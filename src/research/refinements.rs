//! Bounded local pages of refinements captured with the original search.
use crate::{
    error::{Code, fail},
    research::journal::JournalTxn,
    research::response::serialized_utf16_len,
};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const MAX_PAGE_UNITS: usize = 8_000;
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Captured {
    rows: Vec<Value>,
    total: usize,
    source_truncated: bool,
    base: Value,
    observed_at: String,
    limit: usize,
    search_limit: usize,
}

pub fn first_page(
    txn: &mut JournalTxn<'_>,
    rid: &str,
    data: &mut Value,
    observed: &str,
    limit: usize,
    search_limit: usize,
) -> Result<()> {
    let rows = data["refinements"]
        .as_array()
        .cloned()
        .ok_or_else(|| fail(Code::SourceChanged, "Invalid normalized refinements"))?;
    let total = rows.len();
    let source_truncated = data["refinementsSourceTruncated"]
        .as_bool()
        .unwrap_or(false);
    let mut base = data.clone();
    base["items"] = json!([]);
    base["coverage"]["returned"] = json!(0);
    publish(
        txn,
        rid,
        data,
        Captured {
            rows,
            total,
            source_truncated,
            base,
            observed_at: observed.into(),
            limit,
            search_limit,
        },
    )
}
pub fn continue_page(
    txn: &mut JournalTxn<'_>,
    cursor: &str,
    rid: Option<&str>,
    context_id: &str,
    limit: Option<usize>,
    search_limit: Option<usize>,
    include_facets: Option<bool>,
) -> Result<(String, Value, String)> {
    let stored = txn.get_ref(cursor, "search_refinements")?;
    if rid.is_some_and(|rid| rid != stored.research_id) {
        return Err(fail(
            Code::InvalidReference,
            "Refinement cursor belongs to another research",
        ));
    }
    if stored.context_id != context_id {
        return Err(fail(
            Code::ContextChanged,
            "Refinement cursor context changed",
        ));
    }
    let captured: Captured = serde_json::from_value(stored.value)
        .map_err(|_| fail(Code::SourceChanged, "Invalid refinement cursor"))?;
    if limit.is_some_and(|limit| limit != captured.limit)
        || search_limit.is_some_and(|limit| limit != captured.search_limit)
        || include_facets == Some(false)
    {
        return Err(fail(
            Code::InvalidReference,
            "Refinement cursor page limit cannot change",
        ));
    }
    let observed = captured.observed_at.clone();
    let mut data = captured.base.clone();
    publish(txn, &stored.research_id, &mut data, captured)?;
    Ok((stored.research_id, data, observed))
}
fn publish(
    txn: &mut JournalTxn<'_>,
    rid: &str,
    data: &mut Value,
    mut captured: Captured,
) -> Result<()> {
    if !(1..=100).contains(&captured.limit) {
        return Err(fail(
            Code::InvalidArgument,
            "Refinement limit must be 1–100",
        ));
    }
    let mut count = captured.rows.len().min(captured.limit);
    while count > 0 && serialized_utf16_len(&captured.rows[..count])? > MAX_PAGE_UNITS {
        count -= 1;
    }
    if count == 0 && !captured.rows.is_empty() {
        return Err(fail(
            Code::ResultTooLarge,
            "One refinement exceeds its page budget",
        ));
    }
    let remaining = captured.rows.split_off(count);
    let rows = std::mem::replace(&mut captured.rows, remaining);
    let next = if captured.rows.is_empty() {
        None
    } else {
        Some(txn.put_ref(
            rid,
            "search_refinements",
            &serde_json::to_value(&captured)?,
            Some(1800),
        )?)
    };
    data["refinements"] = json!(rows);
    data["refinementsIncluded"] = json!(true);
    data["refinementsTruncated"] = json!(captured.source_truncated || next.is_some());
    data["refinementsNextCursor"] = json!(next);
    data["refinementsTotal"] = json!(captured.total);
    data["refinementsSourceTruncated"] = json!(captured.source_truncated);
    Ok(())
}
