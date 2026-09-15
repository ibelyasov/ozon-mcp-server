//! Local, bounded pagination for refinement rows already captured by search.

use crate::evidence::envelope;
use crate::response::serialized_utf16_len;
use crate::store::Store;
use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};

const MAX_REFINEMENT_LIMIT: usize = 100;
const MAX_REFINEMENT_PAGE_UTF16: usize = 8_000;

const REFINEMENT_CURSOR_KIND: &str = "search_refinements";
const REFINEMENT_CURSOR_TTL_SECONDS: u64 = 1_800;

/// Replaces the full normalized refinement list with its first bounded page.
///
/// The remaining rows are captured in an expiring local cursor. The rest of
/// `data` is preserved exactly as normalized by the marketplace layer.
#[allow(clippy::too_many_arguments)]
pub fn first_page(
    store: &mut Store,
    rid: &str,
    data: &mut Value,
    context: &Value,
    observed: &str,
    view: &str,
    repeat_mode: &str,
    limit: usize,
) -> Result<()> {
    validate_limit(limit)?;
    validate_binding_text(view, "view")?;
    validate_binding_text(repeat_mode, "repeatMode")?;

    let data_object = data
        .as_object()
        .ok_or_else(|| anyhow!("SOURCE_CHANGED: normalized search data must be an object"))?;
    let mut rows = data_object
        .get("refinements")
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| anyhow!("SOURCE_CHANGED: normalized refinements must be an array"))?;
    let original_count = rows.len();
    rows.truncate(MAX_REFINEMENT_LIMIT);
    let source_truncated = data_object
        .get("refinementsTruncated")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || original_count > MAX_REFINEMENT_LIMIT;
    let total = rows.len();
    let end = page_end(&rows, 0, limit)?;
    let more = end < total;

    let next_cursor = if more {
        let state = cursor_state(
            &rows,
            end,
            source_truncated,
            context,
            observed,
            view,
            repeat_mode,
            limit,
        );
        Value::String(store.put_ref(
            rid,
            REFINEMENT_CURSOR_KIND,
            &state,
            Some(REFINEMENT_CURSOR_TTL_SECONDS),
        )?)
    } else {
        Value::Null
    };

    let data_object = data
        .as_object_mut()
        .expect("search data was checked as an object");
    data_object.insert("refinements".into(), Value::Array(rows[..end].to_vec()));
    data_object.insert("refinementsNextCursor".into(), next_cursor);
    data_object.insert("refinementsTotal".into(), json!(total));
    data_object.insert("refinementsSourceTruncated".into(), json!(source_truncated));
    data_object.insert(
        "refinementsTruncated".into(),
        json!(more || source_truncated),
    );
    Ok(())
}

/// Returns a search-shaped envelope from a captured local refinement cursor.
pub fn continue_page(
    store: &mut Store,
    cursor: &str,
    research: Option<&str>,
    context_id: &str,
    view: Option<&str>,
    repeat_mode: Option<&str>,
    limit: Option<usize>,
) -> Result<Value> {
    let stored = store.get_ref(cursor, REFINEMENT_CURSOR_KIND)?;
    validate_stored_binding(
        &stored.research_id,
        &stored.context_id,
        &stored.value,
        research,
        context_id,
        view,
        repeat_mode,
        limit,
    )?;

    let rows = required_array(&stored.value, "rows")?;
    if rows.len() > MAX_REFINEMENT_LIMIT {
        bail!("SOURCE_CHANGED: refinement cursor exceeds its row bound");
    }
    let offset = required_usize(&stored.value, "offset")?;
    if offset >= rows.len() {
        bail!("INVALID_REFERENCE: refinement cursor has no remaining rows");
    }
    let bound_limit = required_usize(&stored.value, "limit")?;
    validate_limit(bound_limit)?;
    let source_truncated = required_bool(&stored.value, "sourceTruncated")?;
    let observed = required_text(&stored.value, "observedAt")?;
    let bound_view = required_text(&stored.value, "view")?;
    let bound_repeat_mode = required_text(&stored.value, "repeatMode")?;
    let captured_context = stored
        .value
        .get("context")
        .ok_or_else(|| anyhow!("SOURCE_CHANGED: refinement cursor has no context"))?;
    if captured_context.get("contextId").and_then(Value::as_str) != Some(context_id) {
        bail!("SOURCE_CHANGED: refinement cursor context is inconsistent");
    }

    let end = page_end(rows, offset, bound_limit)?;
    let more = end < rows.len();
    let next_cursor = if more {
        let state = cursor_state(
            rows,
            end,
            source_truncated,
            captured_context,
            observed,
            bound_view,
            bound_repeat_mode,
            bound_limit,
        );
        Value::String(store.put_ref(
            &stored.research_id,
            REFINEMENT_CURSOR_KIND,
            &state,
            Some(REFINEMENT_CURSOR_TTL_SECONDS),
        )?)
    } else {
        Value::Null
    };
    let warnings = if source_truncated {
        vec![json!({
            "code": "REFINEMENTS_SOURCE_TRUNCATED",
            "message": "The source truncated the captured refinement list.",
            "evidenceRefs": []
        })]
    } else {
        Vec::new()
    };
    let data = json!({
        "items": [],
        "refinements": rows[offset..end].to_vec(),
        "refinementsIncluded": true,
        "refinementsTruncated": more || source_truncated,
        "refinementsNextCursor": next_cursor,
        "refinementsTotal": rows.len(),
        "refinementsSourceTruncated": source_truncated,
        "nextCursor": null,
        "hasNext": null,
        "coverage": {
            "returned": 0,
            "uniqueSeen": 0,
            "total": null,
            "snapshotGuaranteed": false,
            "completeness": "unknown"
        }
    });
    Ok(envelope(
        Some(&stored.research_id),
        data,
        captured_context,
        Vec::new(),
        warnings,
        observed,
    ))
}

#[allow(clippy::too_many_arguments)]
fn cursor_state(
    rows: &[Value],
    offset: usize,
    source_truncated: bool,
    context: &Value,
    observed: &str,
    view: &str,
    repeat_mode: &str,
    limit: usize,
) -> Value {
    json!({
        "rows": rows,
        "offset": offset,
        "sourceTruncated": source_truncated,
        "context": context,
        "observedAt": observed,
        "view": view,
        "repeatMode": repeat_mode,
        "limit": limit
    })
}

#[allow(clippy::too_many_arguments)]
fn validate_stored_binding(
    stored_research: &str,
    stored_context: &str,
    state: &Value,
    research: Option<&str>,
    context_id: &str,
    view: Option<&str>,
    repeat_mode: Option<&str>,
    limit: Option<usize>,
) -> Result<()> {
    if stored_context != context_id {
        bail!("CONTEXT_CHANGED: refinement cursor belongs to another context");
    }
    if research.is_some_and(|value| value != stored_research) {
        bail!("INVALID_REFERENCE: refinement cursor belongs to another research");
    }
    if view.is_some_and(|value| Some(value) != state.get("view").and_then(Value::as_str)) {
        bail!("INVALID_REFERENCE: refinement cursor view does not match");
    }
    if repeat_mode
        .is_some_and(|value| Some(value) != state.get("repeatMode").and_then(Value::as_str))
    {
        bail!("INVALID_REFERENCE: refinement cursor repeatMode does not match");
    }
    if limit.is_some_and(|value| Some(value as u64) != state.get("limit").and_then(Value::as_u64)) {
        bail!("INVALID_REFERENCE: refinement cursor limit does not match");
    }
    Ok(())
}

fn page_end(rows: &[Value], offset: usize, limit: usize) -> Result<usize> {
    let mut page = Vec::new();
    for row in rows.iter().skip(offset).take(limit) {
        page.push(row.clone());
        if serialized_utf16_len(&page)? > MAX_REFINEMENT_PAGE_UTF16 {
            page.pop();
            if page.is_empty() {
                bail!("RESULT_TOO_LARGE: one refinement exceeds the page size limit");
            }
            break;
        }
    }
    Ok(offset + page.len())
}

fn validate_limit(limit: usize) -> Result<()> {
    if !(1..=MAX_REFINEMENT_LIMIT).contains(&limit) {
        bail!("INVALID_ARGUMENT: refinement limit must be between 1 and 100");
    }
    Ok(())
}

fn validate_binding_text(value: &str, field: &str) -> Result<()> {
    if value.is_empty() {
        bail!("INVALID_ARGUMENT: refinement {field} must not be empty");
    }
    Ok(())
}

fn required_text<'a>(value: &'a Value, field: &str) -> Result<&'a str> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("SOURCE_CHANGED: invalid refinement cursor {field}"))
}

fn required_array<'a>(value: &'a Value, field: &str) -> Result<&'a [Value]> {
    value
        .get(field)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .ok_or_else(|| anyhow!("SOURCE_CHANGED: invalid refinement cursor {field}"))
}

fn required_usize(value: &Value, field: &str) -> Result<usize> {
    value
        .get(field)
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .ok_or_else(|| anyhow!("SOURCE_CHANGED: invalid refinement cursor {field}"))
}

fn required_bool(value: &Value, field: &str) -> Result<bool> {
    value
        .get(field)
        .and_then(Value::as_bool)
        .ok_or_else(|| anyhow!("SOURCE_CHANGED: invalid refinement cursor {field}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    fn setup() -> (tempfile::TempDir, Store, String, Value) {
        let directory = tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let context = json!({
            "contextId": "context-a",
            "regionLabel": "Москва",
            "regionVerification": "verified",
            "accountState": "authenticated",
            "accessState": "available"
        });
        let mut store = Store::open(directory.path()).unwrap();
        let research = store.create_research("refinements", &context).unwrap();
        (directory, store, research, context)
    }

    fn rows(count: usize) -> Vec<Value> {
        (0..count)
            .map(|index| {
                json!({
                    "kind": "filter",
                    "label": format!("row-{index}"),
                    "groupLabel": "group",
                    "selected": false,
                    "searchRef": format!("search-ref-{index}")
                })
            })
            .collect()
    }

    #[test]
    fn pages_every_captured_row_and_replays_without_remote_state() {
        let (_directory, mut store, research, context) = setup();
        let expected = rows(29);
        let mut data = json!({
            "items": [{"sentinel": true}],
            "refinements": expected,
            "refinementsIncluded": true,
            "refinementsTruncated": false,
            "nextCursor": "product-page",
            "hasNext": true,
            "coverage": {"returned": 1}
        });
        first_page(
            &mut store,
            &research,
            &mut data,
            &context,
            "2026-09-15T10:00:00.000Z",
            "compact",
            "include",
            12,
        )
        .unwrap();
        assert_eq!(data["items"], json!([{"sentinel": true}]));
        assert_eq!(data["nextCursor"], "product-page");
        assert_eq!(data["refinementsTotal"], 29);

        let first_cursor = data["refinementsNextCursor"].as_str().unwrap().to_owned();
        let replay = continue_page(
            &mut store,
            &first_cursor,
            Some(&research),
            "context-a",
            None,
            None,
            None,
        )
        .unwrap();
        let replay_again = continue_page(
            &mut store,
            &first_cursor,
            Some(&research),
            "context-a",
            Some("compact"),
            Some("include"),
            Some(12),
        )
        .unwrap();
        assert_eq!(
            replay["data"]["refinements"],
            replay_again["data"]["refinements"]
        );
        assert_eq!(replay["context"], context);
        assert_eq!(replay["observedAt"], "2026-09-15T10:00:00.000Z");
        assert_eq!(replay["evidence"], json!([]));
        assert_eq!(replay["warnings"], json!([]));

        let last_cursor = replay["data"]["refinementsNextCursor"].as_str().unwrap();
        let last =
            continue_page(&mut store, last_cursor, None, "context-a", None, None, None).unwrap();
        let collected = data["refinements"]
            .as_array()
            .unwrap()
            .iter()
            .chain(replay["data"]["refinements"].as_array().unwrap())
            .chain(last["data"]["refinements"].as_array().unwrap())
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(collected, expected);
        assert!(last["data"]["refinementsNextCursor"].is_null());
        assert_eq!(last["data"]["refinementsTruncated"], false);
    }

    #[test]
    fn rejects_cursor_binding_mismatches_and_expiry() {
        let (_directory, mut store, research, context) = setup();
        let state = cursor_state(
            &rows(2),
            1,
            false,
            &context,
            "2026-09-15T10:00:00.000Z",
            "compact",
            "include",
            1,
        );
        let cursor = store
            .put_ref(&research, REFINEMENT_CURSOR_KIND, &state, Some(1_800))
            .unwrap();
        for error in [
            continue_page(
                &mut store,
                &cursor,
                Some("other-research"),
                "context-a",
                None,
                None,
                None,
            ),
            continue_page(&mut store, &cursor, None, "context-b", None, None, None),
            continue_page(
                &mut store,
                &cursor,
                None,
                "context-a",
                Some("full"),
                None,
                None,
            ),
            continue_page(
                &mut store,
                &cursor,
                None,
                "context-a",
                None,
                Some("omit"),
                None,
            ),
            continue_page(&mut store, &cursor, None, "context-a", None, None, Some(2)),
        ] {
            assert!(error.is_err());
        }

        let expired = store
            .put_ref(&research, REFINEMENT_CURSOR_KIND, &state, Some(0))
            .unwrap();
        assert!(
            continue_page(&mut store, &expired, None, "context-a", None, None, None,)
                .unwrap_err()
                .to_string()
                .starts_with("INVALID_REFERENCE: reference expired")
        );
    }

    #[test]
    fn obeys_utf16_budget_and_reports_source_truncation() {
        let (_directory, mut store, research, context) = setup();
        let wide_rows = (0..4)
            .map(|index| {
                json!({
                    "kind": "filter",
                    "label": format!("{index}-{}", "😀".repeat(1_200)),
                    "groupLabel": null,
                    "selected": null,
                    "searchRef": format!("search-ref-{index}")
                })
            })
            .collect::<Vec<_>>();
        let mut data = json!({"refinements": wide_rows, "refinementsTruncated": true});
        first_page(
            &mut store,
            &research,
            &mut data,
            &context,
            "2026-09-15T10:00:00.000Z",
            "compact",
            "include",
            4,
        )
        .unwrap();
        assert!(
            serialized_utf16_len(data["refinements"].as_array().unwrap()).unwrap()
                <= MAX_REFINEMENT_PAGE_UTF16
        );
        assert_eq!(data["refinementsSourceTruncated"], true);
        assert_eq!(data["refinementsTruncated"], true);
        let next = continue_page(
            &mut store,
            data["refinementsNextCursor"].as_str().unwrap(),
            None,
            "context-a",
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(next["warnings"][0]["code"], "REFINEMENTS_SOURCE_TRUNCATED");
    }

    #[test]
    fn rejects_a_single_row_over_the_utf16_budget() {
        let (_directory, mut store, research, context) = setup();
        let mut data = json!({
            "refinements": [{
                "kind": "filter",
                "label": "😀".repeat(MAX_REFINEMENT_PAGE_UTF16),
                "groupLabel": null,
                "selected": null,
                "searchRef": "ref"
            }],
            "refinementsTruncated": false
        });
        assert!(
            first_page(
                &mut store,
                &research,
                &mut data,
                &context,
                "2026-09-15T10:00:00.000Z",
                "compact",
                "include",
                12,
            )
            .unwrap_err()
            .to_string()
            .starts_with("RESULT_TOO_LARGE:")
        );
    }
}
