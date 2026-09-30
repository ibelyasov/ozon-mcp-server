//! Bounded private IPC replies; the frontend turns these into standard MCP content.
use crate::error::{Code, fail};
use anyhow::{Result, ensure};
use rmcp::model::{CallToolResult, ContentBlock, ImageContent};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const MAX_WIRE_BYTES: usize = 8 * 1024 * 1024;
const COMPACT_TEXT: &str = "Complete result: use structuredContent.";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImagePayload {
    pub data: String,
    pub mime_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ToolReply {
    Success {
        structured: Value,
        images: Vec<ImagePayload>,
    },
    Failure {
        failure: Value,
    },
}

impl ToolReply {
    #[cfg(test)]
    pub fn structured(&self) -> Option<&Value> {
        match self {
            Self::Success { structured, .. } => Some(structured),
            Self::Failure { .. } => None,
        }
    }
    #[cfg(test)]
    pub fn failure(&self) -> Option<&Value> {
        match self {
            Self::Failure { failure } => Some(failure),
            Self::Success { .. } => None,
        }
    }
    #[cfg(test)]
    pub fn is_error(&self) -> bool {
        matches!(self, Self::Failure { .. })
    }

    pub fn into_mcp(self) -> Result<CallToolResult> {
        let result = match self {
            Self::Failure { failure } => {
                ensure!(
                    failure.is_object() && failure.get("error").is_some_and(Value::is_object),
                    fail(Code::SourceChanged, "Malformed tool failure")
                );
                CallToolResult::error(vec![ContentBlock::text(failure.to_string())])
            }
            Self::Success { structured, images } => {
                ensure!(
                    images.len() <= 4,
                    fail(Code::ResultTooLarge, "Too many images")
                );
                crate::research::response::bounded_value(&structured)?;
                let mut result = CallToolResult::success(vec![ContentBlock::text(COMPACT_TEXT)]);
                result.structured_content = Some(structured);
                for image in images {
                    ensure!(
                        matches!(
                            image.mime_type.as_str(),
                            "image/jpeg" | "image/png" | "image/webp"
                        ),
                        fail(Code::SourceChanged, "Unsupported image MIME")
                    );
                    ensure!(
                        image.data.len() <= 1_398_104,
                        fail(Code::ResultTooLarge, "Image exceeds encoded limit")
                    );
                    result.content.push(ContentBlock::Image(ImageContent::new(
                        image.data,
                        image.mime_type,
                    )));
                }
                result
            }
        };
        ensure!(
            serde_json::to_vec(&result)?.len() <= MAX_WIRE_BYTES - 4096,
            fail(Code::ResultTooLarge, "MCP envelope exceeds limit")
        );
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn structured_payload_is_present_once_and_images_keep_indexes() {
        let marker = "unique-payload-marker".repeat(1_000);
        let structured = json!({"data": marker});
        let reply = ToolReply::Success {
            structured: structured.clone(),
            images: vec![ImagePayload {
                data: "aGVsbG8=".into(),
                mime_type: "image/png".into(),
            }],
        };
        let result = reply.into_mcp().unwrap();
        assert_eq!(result.structured_content, Some(structured));
        assert_eq!(result.content.len(), 2);
        assert_eq!(result.content[0].as_text().unwrap().text, COMPACT_TEXT);
        assert!(matches!(result.content[1], ContentBlock::Image(_)));
        assert_eq!(
            serde_json::to_string(&result)
                .unwrap()
                .matches("unique-payload-marker")
                .count(),
            1_000
        );
    }

    #[test]
    fn failure_has_one_bounded_public_payload() {
        let failure = json!({"schemaVersion":"3", "error":{"code":"SOURCE_BLOCKED"}});
        let result = ToolReply::Failure {
            failure: failure.clone(),
        }
        .into_mcp()
        .unwrap();
        assert_eq!(
            result.content[0].as_text().unwrap().text,
            failure.to_string()
        );
        assert_eq!(result.is_error, Some(true));
        assert!(result.structured_content.is_none());
    }

    #[test]
    fn invalid_enum_combinations_and_oversized_mcp_are_rejected() {
        assert!(
            serde_json::from_value::<ToolReply>(json!({"kind":"failure","failure":{},"images":[]}))
                .is_err()
        );
        let reply = ToolReply::Failure {
            failure: json!({"error":{"message":"x".repeat(MAX_WIRE_BYTES)}}),
        };
        assert_eq!(
            crate::error::code(&reply.into_mcp().unwrap_err()),
            "RESULT_TOO_LARGE"
        );
    }
}
