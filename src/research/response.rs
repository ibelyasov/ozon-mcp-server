//! Measure and validate the exact success artifact before journal commit.
use crate::{
    contracts,
    error::{Code, fail},
    runtime::wire::{ImagePayload, MAX_WIRE_BYTES, ToolReply},
};
use anyhow::Result;
use serde::Serialize;
use serde_json::Value;

pub const MAX_RESULT_UNITS: usize = 60_000;
#[cfg(test)]
pub const METADATA_BUDGET_UNITS: usize = 58_000;

pub fn serialized_utf16_len(value: &(impl Serialize + ?Sized)) -> Result<usize> {
    Ok(serde_json::to_string(value)?.encode_utf16().count())
}
pub fn bounded_value(value: &(impl Serialize + ?Sized)) -> Result<Value> {
    if serialized_utf16_len(value)? > MAX_RESULT_UNITS {
        return Err(fail(
            Code::ResultTooLarge,
            "Response exceeds the UTF-16 size limit",
        ));
    }
    Ok(serde_json::to_value(value)?)
}
pub fn success(name: &str, structured: Value, images: Vec<ImagePayload>) -> Result<ToolReply> {
    contracts::validate_output(name, &structured)?;
    bounded_value(&structured)?;
    let reply = ToolReply::Success { structured, images };
    if serde_json::to_vec(&reply)?.len() > MAX_WIRE_BYTES - 4096 {
        return Err(fail(
            Code::ResultTooLarge,
            "Private transport reply exceeds its limit",
        ));
    }
    // The exact MCP conversion checks MIME, image count, encoded bytes and the
    // complete envelope. There is no projection or truncation after this point.
    reply.clone().into_mcp()?;
    Ok(reply)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn utf16_counts_serialized_escapes_and_astral_characters() {
        for (text, units) in [("a", 1), ("я", 1), ("😀", 2), ("\n", 2)] {
            assert_eq!(serialized_utf16_len(&text).unwrap(), units + 2);
        }
    }
    #[test]
    fn exact_limit_passes_and_overflow_is_typed_without_truncation() {
        let overhead = serialized_utf16_len(&json!({"text":""})).unwrap();
        for character in ["a", "я", "😀"] {
            let width = character.encode_utf16().count();
            let value = json!({"text":character.repeat((MAX_RESULT_UNITS-overhead)/width)});
            assert_eq!(bounded_value(&value).unwrap(), value);
            let error = bounded_value(
                &json!({"text":character.repeat((MAX_RESULT_UNITS-overhead)/width+1)}),
            )
            .unwrap_err();
            assert_eq!(crate::error::code(&error), "RESULT_TOO_LARGE");
        }
    }
}
