//! Provenance is persisted locally; public operation envelopes carry references.
use chrono::{SecondsFormat, Utc};
use serde_json::{Value, json};
use url::Url;
use uuid::Uuid;

pub fn now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}
pub fn unique(prefix: &str) -> String {
    format!("{prefix}-{}", Uuid::new_v4())
}

pub fn envelope(
    research_id: Option<&str>,
    data: Value,
    context: &Value,
    warnings: Vec<Value>,
    observed_at: &str,
) -> Value {
    json!({"schemaVersion":"3","researchId":research_id,"data":data,"context":context,"observedAt":observed_at,"evidence":[],"warnings":warnings})
}

pub fn observed_evidence(
    source_url: &str,
    context_id: &str,
    sku: Option<&str>,
    facts: Vec<Value>,
    observed_at: &str,
) -> anyhow::Result<Value> {
    let source_url = canonical_source_url(source_url).ok_or_else(|| {
        crate::error::fail(
            crate::error::Code::SourceChanged,
            "Missing or invalid observation source URL",
        )
    })?;
    Ok(
        json!({"evidenceRef":unique("evidence"),"sourceKind":"ozon_page","sourceUrl":source_url,"observedAt":observed_at,"contextId":context_id,"sku":sku,"facts":facts}),
    )
}

pub fn canonical_source_url(input: &str) -> Option<String> {
    let mut url = Url::parse(input).ok()?;
    if url.scheme() != "https"
        || url.port().is_some()
        || !matches!(url.host_str(), Some("ozon.ru" | "www.ozon.ru"))
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return None;
    }
    url.set_query(None);
    url.set_fragment(None);
    Some(url.into())
}

pub fn fact(path: impl Into<String>, value: impl Into<Value>) -> Value {
    json!({"fieldPath":path.into(),"value":value.into()})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn strips_navigation_state_without_accepting_credentials_or_foreign_origins() {
        assert_eq!(
            canonical_source_url("https://www.ozon.ru/product/a/?tracking=x#reviews").as_deref(),
            Some("https://www.ozon.ru/product/a/")
        );
        for url in [
            "https://evil.example/product/a",
            "https://secret@www.ozon.ru/product/a/",
            "http://www.ozon.ru/product/a/",
        ] {
            assert!(canonical_source_url(url).is_none());
        }
    }
}
