//! Bounded private IPC replies; the frontend turns these into standard MCP content.
use anyhow::{Result, ensure};
use rmcp::model::{CallToolResult, ContentBlock, ImageContent};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const MAX_WIRE_BYTES: usize = 8 * 1024 * 1024;

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
            let mut result = CallToolResult::structured(value);
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
