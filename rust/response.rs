//! One measurement for public JSON and optional search metadata.
use anyhow::{Result, bail};
use serde::Serialize;
use serde_json::Value;

pub const MAX_RESULT_UNITS: usize = 60_000;
pub const METADATA_BUDGET_UNITS: usize = 58_000;

pub fn serialized_utf16_len(value: &impl Serialize) -> Result<usize> {
    Ok(serde_json::to_string(value)?.encode_utf16().count())
}

pub fn bounded_value(value: &impl Serialize) -> Result<Value> {
    if serialized_utf16_len(value)? > MAX_RESULT_UNITS {
        bail!("RESULT_TOO_LARGE: response exceeds size limit");
    }
    Ok(serde_json::to_value(value)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn measures_the_serialized_wire_format_in_utf16() {
        for (text, units) in [("a", 1), ("я", 1), ("😀", 2), ("\n", 2)] {
            assert_eq!(serialized_utf16_len(&text).unwrap(), units + 2);
        }
    }

    #[test]
    fn accepts_exact_limit_and_rejects_overflow_without_truncating() {
        for character in ["a", "я", "😀"] {
            let width = character.encode_utf16().count();
            let empty = json!({"text":""});
            let overhead = serialized_utf16_len(&empty).unwrap();
            let value = json!({"text":character.repeat((MAX_RESULT_UNITS-overhead)/width)});
            assert_eq!(bounded_value(&value).unwrap(), value);
            let too_large = json!({"text":character.repeat((MAX_RESULT_UNITS-overhead)/width + 1)});
            assert!(bounded_value(&too_large).is_err());
        }
    }
}
