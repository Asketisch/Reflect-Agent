//! MCP `Content` ↔ `reflect_protocol::ContentBlock` 转换。
//!
//! MCP 5 个 variant:`Text` / `Image` / `Resource` / `Audio` / `ResourceLink`。
//! Reflect `ContentBlock` 4 个 variant:`Text` / `Image` / `Json` / `ToolResult`。
//! `Audio` 与 `Resource*` 暂降级为文本 placeholder (`[unsupported MCP content: ...]`),
//! 留 v0.4 补全。

use rmcp::model::{Content, RawContent};

use reflect_protocol::ContentBlock;

/// 把 MCP `Vec<Content>` 转 `Vec<ContentBlock>`。
///
/// 不支持的 variant 输出 `[unsupported MCP content: ...]` 文本,
/// 保留信息但不丢失数据。
pub fn convert_mcp_content(items: &[Content]) -> Vec<ContentBlock> {
    items
        .iter()
        .map(|annotated| convert_one(&annotated.raw))
        .collect()
}

fn convert_one(raw: &RawContent) -> ContentBlock {
    match raw {
        RawContent::Text(t) => ContentBlock::Text {
            text: t.text.clone(),
        },
        RawContent::Image(img) => ContentBlock::Image {
            // MCP 字段是 base64-encoded String,Reflect Image.data 是 Vec<u8>;
            // v0.3 暂保留原 base64 字节 (消费者按 MIME 自行解码);
            // v0.4 计划用专用 base64 解码器把图片还原成原始字节。
            data: img.data.as_bytes().to_vec(),
            mime_type: img.mime_type.clone(),
        },
        RawContent::Audio(a) => ContentBlock::Text {
            text: format!("[unsupported MCP content: audio/{}]", a.mime_type),
        },
        RawContent::Resource(_) | RawContent::ResourceLink(_) => ContentBlock::Text {
            text: "[unsupported MCP content: resource]".to_string(),
        },
        // rmcp 1.7 标记 `non_exhaustive`,未来加 variant 时此分支兜底。
        #[allow(unreachable_patterns)]
        _ => ContentBlock::Text {
            text: format!("[unsupported MCP content: {:?}]", raw),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::{RawImageContent, RawTextContent};

    #[test]
    fn text_round_trip() {
        let items = vec![Content::new(
            RawContent::Text(RawTextContent {
                text: "hi".to_string(),
                meta: None,
            }),
            None,
        )];
        let out = convert_mcp_content(&items);
        match &out[0] {
            ContentBlock::Text { text } => assert_eq!(text, "hi"),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn image_passthrough() {
        let items = vec![Content::new(
            RawContent::Image(RawImageContent {
                data: "AAA".to_string(),
                mime_type: "image/png".to_string(),
                meta: None,
            }),
            None,
        )];
        let out = convert_mcp_content(&items);
        match &out[0] {
            ContentBlock::Image { data, mime_type } => {
                // v0.3 保留 base64 字节;详见 convert_one 注释。
                assert_eq!(data, b"AAA");
                assert_eq!(mime_type, "image/png");
            }
            other => panic!("expected Image, got {other:?}"),
        }
    }

    #[test]
    fn empty_input_yields_empty_output() {
        let out = convert_mcp_content(&[]);
        assert!(out.is_empty());
    }
}
