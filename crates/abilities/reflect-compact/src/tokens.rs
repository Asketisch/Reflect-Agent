//! Token 估算。默认启发式:`len(text) / 3.5` 字符每 token,image 块按
//! 1000 token 计(对应 reflect 的 `graph.py:_estimate_tokens`)。
//!
//! v1.4 D1:估算器可插拔 —— [`TokenEstimator`] trait + 进程级全局实例
//! (OnceLock,set-once)。默认 [`HeuristicEstimator`] 保持历史行为、
//! 零依赖;开启 `tokenizer` feature 时可用 [`TiktokenEstimator`]
//! (tiktoken-rs,按模型名前缀选 BPE 词表),由 exec bootstrap 注册。
//!
//! 估算值用于触发压缩策略,**不能**作为计费级计数器。

use std::sync::Arc;

use reflect_llm::{ChatMessage, ContentBlock, ToolResult};

/// 单图块的固定 token 预算。
const IMAGE_TOKENS: u32 = 1000;

/// v1.4 D1:token 估算器抽象。`estimate_text` 是原子入口(分词器只看
/// 文本);`estimate_messages` 走结构化遍历(图片块固定预算、tool_calls
/// 的 JSON 参数计入),内部逐文本段调 `estimate_text`。
pub trait TokenEstimator: Send + Sync {
    /// 估算一段纯文本的 token 数。
    fn estimate_text(&self, text: &str) -> u32;

    /// 估算一组聊天消息的总 token 数。默认实现 = 结构化遍历 + 委托
    /// [`Self::estimate_text`];特殊分词器(如多模态感知的)可覆写。
    fn estimate_messages(&self, messages: &[ChatMessage]) -> u32 {
        messages.iter().map(|m| self.estimate_message(m)).sum()
    }

    /// 估算单条消息。默认实现:结构化遍历。
    fn estimate_message(&self, msg: &ChatMessage) -> u32 {
        match msg {
            ChatMessage::System(s) => self.estimate_text(s),
            ChatMessage::User(u) => u.blocks.iter().map(|b| self.estimate_block(b)).sum(),
            ChatMessage::Assistant(a) => {
                let mut t = 0;
                if let Some(text) = &a.text {
                    t += self.estimate_text(text);
                }
                if let Some(thinking) = &a.thinking {
                    t += self.estimate_text(thinking);
                }
                // tool_calls:每个调用的 JSON 参数均计入。
                for tc in &a.tool_calls {
                    t += self.estimate_text(&tc.arguments.to_string());
                    t += self.estimate_text(&tc.name);
                }
                t
            }
            ChatMessage::Tool(r) => self.estimate_text(&tool_result_text(r)),
        }
    }

    /// 估算单个内容块。默认实现:文本委托 `estimate_text`,图片固定预算。
    fn estimate_block(&self, b: &ContentBlock) -> u32 {
        match b {
            ContentBlock::Text { text } => self.estimate_text(text),
            // 在 reflect 中,图片属于固定大预算块。
            ContentBlock::Image { .. } => IMAGE_TOKENS,
        }
    }
}

/// 默认启发式估算器:chars / 3.5,向上取整。
pub struct HeuristicEstimator;

impl TokenEstimator for HeuristicEstimator {
    fn estimate_text(&self, text: &str) -> u32 {
        ((text.chars().count() as f64) / 3.5).ceil() as u32
    }
}

// ── 进程级全局估算器(set-once) ────────────────────────────────────

static GLOBAL_ESTIMATOR: std::sync::OnceLock<Arc<dyn TokenEstimator>> = std::sync::OnceLock::new();

/// 注册进程级全局估算器。仅首次调用生效(后续调用为 no-op 并返回
/// false)—— exec bootstrap 在启动期调用一次;测试可各建独立实例。
pub fn set_global_estimator(estimator: Arc<dyn TokenEstimator>) -> bool {
    GLOBAL_ESTIMATOR.set(estimator).is_ok()
}

/// 当前全局估算器;未注册时为 [`HeuristicEstimator`]。
pub fn global_estimator() -> Arc<dyn TokenEstimator> {
    GLOBAL_ESTIMATOR
        .get()
        .cloned()
        .unwrap_or_else(|| Arc::new(HeuristicEstimator))
}

// ── 自由函数 API(全部 call site 保持不变) ────────────────────────

/// 估算一段聊天消息的总 token 数(走全局估算器)。纯函数 —— 不调用
/// 任何 LLM。
pub fn estimate_messages(messages: &[ChatMessage]) -> u32 {
    global_estimator().estimate_messages(messages)
}

/// v1.4 D1:估算一段纯文本的 token 数(走全局估算器)。供 memory
/// 注入预算等非消息场景使用。
pub fn estimate_text(text: &str) -> u32 {
    global_estimator().estimate_text(text)
}

fn tool_result_text(r: &ToolResult) -> String {
    // content 现为 Vec<ContentBlock>(多模态);为 token 估算展平为
    // 文本。Image 块按占位符计算,而非原始字节长度(后者会严重
    // 高估 token 数)。
    r.content_as_text()
}

// ── tiktoken 实现(feature = "tokenizer") ──────────────────────────

#[cfg(feature = "tokenizer")]
pub mod tiktoken {
    //! 真实分词器(tiktoken-rs)。按模型名前缀选 BPE 词表:
    //! `gpt-4o*` / `o200k*` → o200k_base;其余(gpt-3.5 / gpt-4 /
    //! claude / gemini / 未知)→ cl100k_base 近似(claude 等非 OpenAI
    //! 模型的原生词表不公开,cl100k 是工程界通用的近似基准)。

    use std::sync::Arc;

    use super::{IMAGE_TOKENS, TokenEstimator};
    use reflect_llm::ContentBlock;

    /// tiktoken 估算器。`CoreBPE` 内部是可变缓存 + `&self` 方法,
    /// 直接 `Arc` 共享并发安全(tiktoken-rs 保证)。
    pub struct TiktokenEstimator {
        o200k: tiktoken_rs::CoreBPE,
        cl100k: tiktoken_rs::CoreBPE,
    }

    impl TiktokenEstimator {
        /// 构造(加载内嵌 BPE 词表;无 I/O)。词表加载失败(理论上不
        /// 发生,内嵌数据)返回错误字符串。
        pub fn new() -> Result<Self, String> {
            let o200k = tiktoken_rs::o200k_base().map_err(|e| e.to_string())?;
            let cl100k = tiktoken_rs::cl100k_base().map_err(|e| e.to_string())?;
            Ok(Self { o200k, cl100k })
        }

        /// 按模型名前缀选词表。
        fn bpe_for(&self, model: &str) -> &tiktoken_rs::CoreBPE {
            if model.starts_with("gpt-4o") || model.starts_with("o1") || model.starts_with("o3") {
                &self.o200k
            } else {
                &self.cl100k
            }
        }

        /// 带模型感知的消息估算:文本按对应词表精确计数,图片块仍用
        /// 固定预算(图片 token 由 provider 视分辨率计,非文本分词域)。
        pub fn estimate_messages_for_model(
            &self,
            model: &str,
            messages: &[reflect_llm::ChatMessage],
        ) -> u32 {
            let bpe = self.bpe_for(model);
            messages
                .iter()
                .map(|m| {
                    use reflect_llm::ChatMessage as CM;
                    match m {
                        CM::System(s) => count(bpe, s),
                        CM::User(u) => u
                            .blocks
                            .iter()
                            .map(|b| match b {
                                ContentBlock::Text { text } => count(bpe, text),
                                ContentBlock::Image { .. } => IMAGE_TOKENS,
                                _ => 0,
                            })
                            .sum(),
                        CM::Assistant(a) => {
                            let mut t = 0;
                            if let Some(text) = &a.text {
                                t += count(bpe, text);
                            }
                            if let Some(thinking) = &a.thinking {
                                t += count(bpe, thinking);
                            }
                            for tc in &a.tool_calls {
                                t += count(bpe, &tc.arguments.to_string());
                                t += count(bpe, &tc.name);
                            }
                            t
                        }
                        CM::Tool(r) => count(bpe, &r.content_as_text()),
                    }
                })
                .sum()
        }
    }

    fn count(bpe: &tiktoken_rs::CoreBPE, text: &str) -> u32 {
        bpe.encode_ordinary(text).len() as u32
    }

    impl TokenEstimator for TiktokenEstimator {
        /// 无模型上下文的文本估算走 cl100k(通用近似)。
        fn estimate_text(&self, text: &str) -> u32 {
            count(&self.cl100k, text)
        }
    }

    /// 便捷构造 `Arc<dyn TokenEstimator>`(给 `set_global_estimator`)。
    pub fn global_tiktoken_estimator() -> Result<Arc<dyn TokenEstimator>, String> {
        Ok(Arc::new(TiktokenEstimator::new()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reflect_llm::{AssistantContent, ContentBlock, ToolCallRequest, ToolResult, UserContent};

    #[test]
    fn empty_messages_yield_zero() {
        assert_eq!(estimate_messages(&[]), 0);
    }

    #[test]
    fn short_text_rounds_up() {
        // 4 字符 / 3.5 = 1.14,向上取整 = 2
        let msgs = vec![ChatMessage::System("abcd".into())];
        assert_eq!(estimate_messages(&msgs), 2);
    }

    #[test]
    fn long_text_uses_3_5_ratio() {
        // 35 字符 / 3.5 = 10
        let s: String = "a".repeat(35);
        let msgs = vec![ChatMessage::System(s)];
        assert_eq!(estimate_messages(&msgs), 10);
    }

    #[test]
    fn image_block_equals_1000_tokens() {
        let msgs = vec![ChatMessage::User(UserContent {
            blocks: vec![ContentBlock::Image {
                data: vec![0xff; 100],
                mime_type: "image/png".into(),
            }],
        })];
        assert_eq!(estimate_messages(&msgs), 1000);
    }

    #[test]
    fn assistant_counts_text_thinking_and_tool_calls() {
        let a = AssistantContent {
            text: Some("hello".into()),
            tool_calls: vec![ToolCallRequest {
                id: "c1".into(),
                name: "read".into(),
                arguments: serde_json::json!({"path": "/tmp/x"}),
            }],
            thinking: Some("thinking about it".into()),
        };
        let msgs = vec![ChatMessage::Assistant(a)];
        let t = estimate_messages(&msgs);
        // "hello" = ceil(5/3.5)=2;"thinking about it" = ceil(18/3.5)=6;
        // "read" = ceil(4/3.5)=2;args JSON 若干
        assert!(t >= 10);
    }

    #[test]
    fn tool_result_counts_content() {
        let r = ToolResult {
            call_id: "c1".into(),
            content: vec![ContentBlock::text("ok result")],
            is_error: false,
        };
        let msgs = vec![ChatMessage::Tool(r)];
        let t = estimate_messages(&msgs);
        // "ok result" = ceil(9/3.5) = 3
        assert_eq!(t, 3);
    }

    #[test]
    fn sums_across_messages() {
        let msgs = vec![
            ChatMessage::System("hello".into()),
            ChatMessage::User(UserContent {
                blocks: vec![ContentBlock::text("world")],
            }),
        ];
        let t = estimate_messages(&msgs);
        // "hello"=2,"world"=2
        assert_eq!(t, 4);
    }

    // ── v1.4 D1:可插拔估算器 ───────────────────────────────────
    //
    // 注意:不在测试里调 `set_global_estimator` —— OnceLock 是进程级
    // set-once,注册自定义估算器会污染同二进制内其他依赖默认启发式
    // 数值的测试。全局注册路径由 exec bootstrap(生产,启动期一次性)
    // 覆盖;这里只直接测各实现。

    #[test]
    fn estimate_text_defaults_to_heuristic_when_unset() {
        // 测试进程未注册全局估算器 → 自由函数走启发式。
        assert_eq!(estimate_text("abcd"), 2);
    }

    #[test]
    fn heuristic_estimator_direct() {
        let e = HeuristicEstimator;
        assert_eq!(e.estimate_text(""), 0);
        assert_eq!(e.estimate_text("abcde"), 2); // ceil(5/3.5)
    }

    #[cfg(feature = "tokenizer")]
    mod tiktoken_tests {
        use super::*;

        /// tiktoken:英文短句的真实 token 数(cl100k 下 "hello world" = 2)。
        #[test]
        fn tiktoken_counts_english() {
            let est = super::super::tiktoken::TiktokenEstimator::new().unwrap();
            assert_eq!(est.estimate_text("hello world"), 2);
        }

        /// o200k 选择:gpt-4o 前缀走 o200k 词表(与 cl100k 计数可能不同,
        /// 这里只验证可运行且非零)。
        #[test]
        fn tiktoken_model_aware_messages() {
            use reflect_llm::{ChatMessage, UserContent};
            let est = super::super::tiktoken::TiktokenEstimator::new().unwrap();
            let msgs = vec![ChatMessage::User(UserContent {
                blocks: vec![ContentBlock::text("hello world")],
            })];
            let t = est.estimate_messages_for_model("gpt-4o", &msgs);
            assert!(t > 0 && t < 10);
            let t2 = est.estimate_messages_for_model("claude-3-5-sonnet", &msgs);
            assert_eq!(t2, 2, "cl100k 下 hello world = 2");
        }
    }
}
