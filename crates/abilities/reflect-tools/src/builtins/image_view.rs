//! `image_view` — 读取本地图片,以 `ContentBlock::Image` 供多模态 LLM 消费。

use std::fs;
use std::path::Path;

use async_trait::async_trait;
use serde_json::Value;

use crate::sandbox::resolve_sandbox_path;
use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};

/// 单张图片软上限(10 MiB),超出拒绝以防 OOM。
const MAX_IMAGE_BYTES: u64 = 10 * 1024 * 1024;

const IMAGE_EXTENSIONS: &[(&str, &str)] = &[
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("gif", "image/gif"),
    ("webp", "image/webp"),
    ("bmp", "image/bmp"),
    ("svg", "image/svg+xml"),
];

pub struct ImageViewTool;

fn mime_for_path(path: &Path) -> Option<&'static str> {
    path.extension()
        .and_then(|e| e.to_str())
        .map(str::to_lowercase)
        .and_then(|ext| {
            IMAGE_EXTENSIONS
                .iter()
                .find(|(e, _)| *e == ext)
                .map(|(_, mime)| *mime)
        })
}

#[async_trait]
impl Tool for ImageViewTool {
    fn name(&self) -> &str {
        "image_view"
    }

    fn description(&self) -> &str {
        reflect_prompt::copy("tool.image_view")
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to an image file relative to workspace, or absolute under workspace"
                }
            },
            "required": ["path"]
        })
    }

    fn is_concurrency_safe(&self) -> bool {
        true
    }

    async fn execute(&self, ctx: ToolContext, args: Value) -> Result<ToolOutput, ToolError> {
        let path_str =
            args.get("path")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::InvalidArgs {
                    message: "missing 'path'".into(),
                })?;
        let path = Path::new(path_str);
        let abs = resolve_sandbox_path(&ctx.workspace_path(), path)?;

        let meta =
            fs::metadata(&abs).map_err(|e| ToolError::Io(format!("stat {path_str}: {e}")))?;
        if !meta.is_file() {
            return Err(ToolError::InvalidArgs {
                message: format!("'{path_str}' is not a file"),
            });
        }
        if meta.len() > MAX_IMAGE_BYTES {
            return Err(ToolError::InvalidArgs {
                message: format!(
                    "image too large ({} bytes; max {MAX_IMAGE_BYTES})",
                    meta.len()
                ),
            });
        }

        let mime_type = mime_for_path(&abs).ok_or_else(|| ToolError::InvalidArgs {
            message: format!(
                "unsupported image extension for '{path_str}'; supported: png, jpg, gif, webp, bmp, svg"
            ),
        })?;

        let data =
            fs::read(&abs).map_err(|e| ToolError::Io(format!("read image {path_str}: {e}")))?;

        Ok(ToolOutput {
            content: vec![reflect_protocol::ContentBlock::Image {
                data,
                mime_type: mime_type.to_string(),
            }],
            is_error: false,
            metadata: serde_json::json!({
                "path": path_str,
                "bytes": meta.len(),
                "mime_type": mime_type,
            }),
            elapsed_ms: 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reads_png_as_image_block() {
        let dir = std::env::temp_dir().join(format!("reflect-img-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let img = dir.join("test.png");
        // 最小合法 1x1 PNG
        let png: &[u8] = &[
            0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
            0x00, 0x1f, 0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0a, 0x49, 0x44, 0x41, 0x54, 0x78,
            0x9c, 0x63, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00,
            0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
        ];
        fs::write(&img, png).unwrap();

        let ctx = ToolContext::default();
        ctx.set_workspace(dir.clone());

        let tool = ImageViewTool;
        let out = tool
            .execute(ctx, serde_json::json!({"path": "test.png"}))
            .await
            .unwrap();
        assert!(matches!(
            &out.content[0],
            reflect_protocol::ContentBlock::Image { mime_type, .. }
                if mime_type == "image/png"
        ));
    }
}
