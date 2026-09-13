use chrono::{SecondsFormat, Utc};
use serde_json::{Value, json};
use url::Url;

pub const MAX_JSON_UTF16: usize = 60_000;
pub const MAX_IMAGE_WIRE_BYTES: usize = 8 * 1024 * 1024;

pub fn now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

pub fn public_context(raw: &Value) -> Value {
    json!({
        "contextId": raw.get("contextId").and_then(Value::as_str).unwrap_or("context-unknown"),
        "regionLabel": raw.get("regionLabel").cloned().unwrap_or(Value::Null),
        "regionVerification": enum_value(raw, "regionVerification", &["verified", "unverified"], "unverified"),
        "accountState": enum_value(raw, "accountState", &["authenticated", "anonymous", "unknown"], "unknown"),
        "accessState": enum_value(raw, "accessState", &["available", "blocked", "unknown"], "unknown"),
    })
}

pub fn envelope(
    research_id: Option<&str>,
    data: Value,
    context: &Value,
    evidence: Vec<Value>,
    warnings: Vec<Value>,
    observed_at: &str,
) -> Value {
    json!({
        "schemaVersion": "1",
        "researchId": research_id,
        "data": data,
        "context": public_context(context),
        "observedAt": observed_at,
        "evidence": evidence.into_iter().map(envelope_evidence).collect::<Vec<_>>(),
        "warnings": warnings,
    })
}

pub fn observed_evidence(
    source_url: Option<&str>,
    context_id: &str,
    sku: Option<&str>,
    field_paths: &[&str],
    facts: Value,
    observed_at: &str,
) -> Value {
    json!({
        "sourceKind": "ozon_page",
        "sourceUrl": source_url.and_then(canonical_source_url),
        "observedAt": observed_at,
        "contextId": context_id,
        "sku": sku,
        "fieldPaths": field_paths,
        "facts": facts,
    })
}

pub fn envelope_evidence(mut value: Value) -> Value {
    if let Some(obj) = value.as_object_mut() {
        obj.remove("facts");
    }
    value
}

pub fn canonical_source_url(input: &str) -> Option<String> {
    let mut url = Url::parse(input).ok()?;
    if url.scheme() != "https"
        || url.port().is_some()
        || !matches!(url.host_str(), Some("ozon.ru" | "www.ozon.ru"))
    {
        return None;
    }
    url.set_query(None);
    url.set_fragment(None);
    let _ = url.set_username("");
    let _ = url.set_password(None);
    Some(url.into())
}

pub fn utf16_len(value: &Value) -> anyhow::Result<usize> {
    Ok(serde_json::to_string(value)?.encode_utf16().count())
}

pub fn error_code(error: &anyhow::Error) -> &'static str {
    if error.chain().any(is_storage_full) {
        return "STORAGE_FULL";
    }
    let message = error.to_string();
    const KNOWN: &[&str] = &[
        "INVALID_ARGUMENT",
        "INVALID_REFERENCE",
        "CONTEXT_CHANGED",
        "CONTEXT_UNVERIFIED",
        "RESEARCH_EXPIRED",
        "UNSUPPORTED_CAPABILITY",
        "SOURCE_BLOCKED",
        "SOURCE_CHANGED",
        "UPSTREAM_TIMEOUT",
        "SERVER_BUSY",
        "RESULT_TOO_LARGE",
        "PARTIAL_RESULT",
        "NOT_FOUND",
        "CONFLICT",
        "STORAGE_FULL",
        "CANCELLED",
    ];
    KNOWN
        .iter()
        .copied()
        .find(|code| message.starts_with(code))
        .unwrap_or("SOURCE_CHANGED")
}

fn is_storage_full(error: &(dyn std::error::Error + 'static)) -> bool {
    if let Some(rusqlite::Error::SqliteFailure(details, _)) =
        error.downcast_ref::<rusqlite::Error>()
    {
        return details.code == rusqlite::ffi::ErrorCode::DiskFull;
    }
    error
        .downcast_ref::<std::io::Error>()
        .and_then(std::io::Error::raw_os_error)
        == Some(28)
}

pub fn safe_message(error: &anyhow::Error) -> String {
    let raw = error.to_string();
    let (_, message) = raw.split_once(':').unwrap_or(("", raw.as_str()));
    let message = message.trim();
    if message.is_empty() {
        "The operation failed.".into()
    } else {
        message.chars().take(2000).collect()
    }
}

fn enum_value(raw: &Value, key: &str, allowed: &[&str], fallback: &str) -> Value {
    raw.get(key)
        .and_then(Value::as_str)
        .filter(|v| allowed.contains(v))
        .unwrap_or(fallback)
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_url_drops_private_navigation_state() {
        assert_eq!(
            canonical_source_url("https://www.ozon.ru/product/a/?tracking=x#reviews").as_deref(),
            Some("https://www.ozon.ru/product/a/")
        );
        assert!(canonical_source_url("https://evil.example/product/a").is_none());
    }

    #[test]
    fn envelope_evidence_removes_private_facts() {
        let value = envelope_evidence(json!({"evidenceRef":"e","facts":[1],"fieldPaths":[""]}));
        assert!(value.get("facts").is_none());
    }

    #[test]
    fn storage_full_is_detected_through_error_chains() {
        let sqlite = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code: rusqlite::ffi::ErrorCode::DiskFull,
                extended_code: 13,
            },
            None,
        );
        let error = anyhow::Error::new(sqlite).context("write research event");
        assert_eq!(error_code(&error), "STORAGE_FULL");

        let io = std::io::Error::from_raw_os_error(28);
        let error = anyhow::Error::new(io).context("persist research");
        assert_eq!(error_code(&error), "STORAGE_FULL");
    }
}
