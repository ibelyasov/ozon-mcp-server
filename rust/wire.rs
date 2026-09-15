//! Bounded private IPC replies; the frontend turns these into standard MCP content.
use std::ffi::OsStr;

use anyhow::{Result, ensure};
use rmcp::model::{CallToolResult, ContentBlock, ImageContent};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const MAX_WIRE_BYTES: usize = 8 * 1024 * 1024;
const COMPACT_TEXT: &str = "Complete result: use structuredContent.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TextMode {
    Compact,
    Json,
}

impl TextMode {
    fn from_env() -> Result<Self> {
        Self::parse(std::env::var_os("OZON_MCP_TEXT_MODE").as_deref())
    }

    fn parse(value: Option<&OsStr>) -> Result<Self> {
        match value.and_then(OsStr::to_str) {
            None if value.is_none() => Ok(Self::Compact),
            Some("compact") => Ok(Self::Compact),
            Some("json") => Ok(Self::Json),
            _ => anyhow::bail!(
                "INVALID_CONFIGURATION: OZON_MCP_TEXT_MODE must be `compact` or `json`"
            ),
        }
    }
}

pub(crate) fn validate_text_mode() -> Result<()> {
    TextMode::from_env().map(|_| ())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImagePayload {
    pub data: String,
    pub mime_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolReply {
    pub structured: Option<Value>,
    pub error: bool,
    pub text: Option<String>,
    pub images: Vec<ImagePayload>,
}

impl ToolReply {
    pub fn into_mcp(self) -> Result<CallToolResult> {
        // Failure content is already a bounded, typed JSON message and must not
        // depend on the success transport compatibility mode.
        let text_mode = if self.error {
            TextMode::Compact
        } else {
            TextMode::from_env()?
        };
        self.into_mcp_with_mode(text_mode)
    }

    fn into_mcp_with_mode(self, text_mode: TextMode) -> Result<CallToolResult> {
        ensure!(self.images.len() <= 4, "RESULT_TOO_LARGE: too many images");
        let result = if self.error {
            ensure!(
                self.structured.is_none() && self.images.is_empty(),
                "SOURCE_CHANGED: malformed failure"
            );
            CallToolResult::error(vec![ContentBlock::text(self.text.unwrap_or_default())])
        } else {
            let value = self
                .structured
                .ok_or_else(|| anyhow::anyhow!("SOURCE_CHANGED: missing structured result"))?;
            crate::response::bounded_value(&value)?;
            let text = match text_mode {
                TextMode::Compact => COMPACT_TEXT.to_owned(),
                TextMode::Json => value.to_string(),
            };
            let mut result = CallToolResult::success(vec![ContentBlock::text(text)]);
            result.structured_content = Some(value);
            for image in self.images {
                ensure!(
                    matches!(
                        image.mime_type.as_str(),
                        "image/jpeg" | "image/png" | "image/webp"
                    ),
                    "SOURCE_CHANGED: unsupported image MIME"
                );
                ensure!(
                    image.data.len() <= 1_398_104,
                    "RESULT_TOO_LARGE: image exceeds encoded limit"
                );
                result.content.push(ContentBlock::Image(ImageContent::new(
                    image.data,
                    image.mime_type,
                )));
            }
            result
        };
        ensure!(
            serde_json::to_vec(&result)?.len() <= MAX_WIRE_BYTES - 4096,
            "RESULT_TOO_LARGE: MCP envelope exceeds limit"
        );
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn success(value: Value) -> ToolReply {
        ToolReply {
            structured: Some(value),
            error: false,
            text: None,
            images: Vec::new(),
        }
    }

    #[test]
    fn text_mode_defaults_to_compact_and_rejects_invalid_values() {
        assert_eq!(TextMode::parse(None).unwrap(), TextMode::Compact);
        assert_eq!(
            TextMode::parse(Some(OsStr::new("compact"))).unwrap(),
            TextMode::Compact
        );
        assert_eq!(
            TextMode::parse(Some(OsStr::new("json"))).unwrap(),
            TextMode::Json
        );
        assert!(
            TextMode::parse(Some(OsStr::new("")))
                .unwrap_err()
                .to_string()
                .starts_with("INVALID_CONFIGURATION:")
        );
        assert!(TextMode::parse(Some(OsStr::new("full"))).is_err());
    }

    #[test]
    fn compact_mode_keeps_one_structured_copy_and_content_indexes() {
        let marker = "unique-payload-marker".repeat(1_000);
        let value = json!({"data": marker});
        let mut reply = success(value.clone());
        reply.images.push(ImagePayload {
            data: "aGVsbG8=".into(),
            mime_type: "image/png".into(),
        });

        let result = reply.into_mcp_with_mode(TextMode::Compact).unwrap();

        assert_eq!(result.structured_content, Some(value));
        assert_eq!(result.content.len(), 2);
        assert_eq!(result.content[0].as_text().unwrap().text, COMPACT_TEXT);
        assert!(matches!(result.content[1], ContentBlock::Image(_)));
        let envelope = serde_json::to_string(&result).unwrap();
        assert_eq!(envelope.matches("unique-payload-marker").count(), 1_000);
        assert!(COMPACT_TEXT.len() < 64);
    }

    #[test]
    fn json_mode_preserves_legacy_full_text_compatibility() {
        let value = json!({"schemaVersion":"1","data":{"items":[1,2,3]}});
        let expected_text = value.to_string();

        let result = success(value.clone())
            .into_mcp_with_mode(TextMode::Json)
            .unwrap();

        assert_eq!(result.structured_content, Some(value));
        assert_eq!(result.content.len(), 1);
        assert_eq!(result.content[0].as_text().unwrap().text, expected_text);
        assert_eq!(result.is_error, Some(false));
    }

    #[test]
    fn errors_are_unchanged_by_text_mode() {
        let make_error = || ToolReply {
            structured: None,
            error: true,
            text: Some("{\"error\":\"SOURCE_BLOCKED\"}".into()),
            images: Vec::new(),
        };

        let compact = make_error().into_mcp_with_mode(TextMode::Compact).unwrap();
        let json = make_error().into_mcp_with_mode(TextMode::Json).unwrap();

        assert_eq!(compact, json);
        assert_eq!(compact.is_error, Some(true));
        assert!(compact.structured_content.is_none());
    }
}
