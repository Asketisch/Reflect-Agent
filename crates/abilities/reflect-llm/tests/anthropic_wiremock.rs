//! Anthropic provider 的 wiremock 集成测试。

use futures::StreamExt;
use reflect_llm::providers::{AnthropicClient, AnthropicConfig};
use reflect_llm::{
    ChatEvent, ChatMessage, ChatRequest, ContentBlock, LlmError, ModelClient, SystemBlocks,
    ToolResult, UserContent,
};
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn basic_request() -> ChatRequest {
    ChatRequest {
        model: "claude-3-5-sonnet-latest".into(),
        messages: vec![ChatMessage::User(UserContent {
            blocks: vec![ContentBlock::text("hi")],
        })],
        tools: vec![],
        system: SystemBlocks::default(),
        temperature: None,
        max_tokens: Some(256),
        top_p: None,
        thinking: None,
        cache_control: vec![],
        metadata: Default::default(),
        stop: vec![],
    }
}

fn make_sse(events: &[&str]) -> String {
    let mut out = String::new();
    for ev in events {
        out.push_str(ev);
        out.push_str("\n\n");
    }
    out
}

#[tokio::test]
async fn anthropic_happy_path_parses_sse() {
    let server = MockServer::start().await;
    let sse = make_sse(&[
        "event: message_start\ndata: {\"message\":{\"id\":\"msg_1\",\"model\":\"claude-3-5-sonnet-latest\",\"usage\":{\"input_tokens\":5,\"output_tokens\":1,\"cache_read_input_tokens\":2}}}",
        "event: content_block_start\ndata: {\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}",
        "event: content_block_delta\ndata: {\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}",
        "event: content_block_stop\ndata: {\"index\":0}",
        "event: message_delta\ndata: {\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}",
        "event: message_stop\ndata: {}",
    ]);
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        // 自定义 base_url 使用 Authorization: Bearer
        .and(header("Authorization", "Bearer test-key"))
        .and(header("anthropic-version", "2023-06-01"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse),
        )
        .mount(&server)
        .await;

    let client = AnthropicClient::new(AnthropicConfig {
        api_key: "test-key".into(),
        base_url: Some(server.uri()),
        timeout: Duration::from_secs(5),
    })
    .unwrap();
    let mut stream = client
        .stream(basic_request(), CancellationToken::new())
        .await
        .unwrap();
    let mut events = Vec::new();
    while let Some(e) = stream.next().await {
        events.push(e.unwrap());
    }
    assert!(matches!(&events[0], ChatEvent::MessageStart { id, .. } if id == "msg_1"));
    // 期望事件顺序:MessageStart、(text_block_start → 无事件)、ContentDelta("Hello")、
    // (content_block_stop → 无事件)、Usage、MessageStop
    let saw_delta = events
        .iter()
        .any(|e| matches!(e, ChatEvent::ContentDelta(s) if s == "Hello"));
    assert!(saw_delta, "expected ContentDelta(Hello) in {events:?}");
    let saw_usage = events.iter().any(|e| {
        matches!(e, ChatEvent::Usage {
            input_tokens,
            output_tokens,
            cached_tokens,
            cache_write_tokens,
        } if *input_tokens == 5 && *output_tokens == 2 && *cached_tokens == 2 && *cache_write_tokens == 0)
    });
    assert!(saw_usage, "expected Usage(5,2,2,0) in {events:?}");
    assert!(matches!(events.last(), Some(ChatEvent::MessageStop)));
}

#[tokio::test]
async fn anthropic_401_maps_to_auth_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(401).set_body_string("unauthorized"))
        .mount(&server)
        .await;

    let client = AnthropicClient::new(AnthropicConfig {
        api_key: "k".into(),
        base_url: Some(server.uri()),
        timeout: Duration::from_secs(5),
    })
    .unwrap();
    let err = match client
        .stream(basic_request(), CancellationToken::new())
        .await
    {
        Ok(_) => panic!("expected error, got Ok"),
        Err(e) => e,
    };
    assert!(matches!(err, LlmError::Auth));
}

#[tokio::test]
async fn anthropic_custom_base_url_uses_bearer_auth() {
    // 自定义 base_url（非 api.anthropic.com）应使用 Authorization: Bearer
    let server = MockServer::start().await;
    let sse = make_sse(&["event: message_stop\ndata: {}"]);
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("Authorization", "Bearer custom-key"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse),
        )
        .mount(&server)
        .await;

    let client = AnthropicClient::new(AnthropicConfig {
        api_key: "custom-key".into(),
        base_url: Some(server.uri()),
        timeout: Duration::from_secs(5),
    })
    .unwrap();
    let mut stream = client
        .stream(basic_request(), CancellationToken::new())
        .await
        .unwrap();
    while let Some(e) = stream.next().await {
        e.unwrap();
    }
}

#[tokio::test]
async fn anthropic_529_maps_to_overloaded() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(529).set_body_string("overloaded"))
        .mount(&server)
        .await;

    let client = AnthropicClient::new(AnthropicConfig {
        api_key: "k".into(),
        base_url: Some(server.uri()),
        timeout: Duration::from_secs(5),
    })
    .unwrap();
    let err = match client
        .stream(basic_request(), CancellationToken::new())
        .await
    {
        Ok(_) => panic!("expected error"),
        Err(e) => e,
    };
    assert!(matches!(err, LlmError::Overloaded { .. }));
}

#[tokio::test]
async fn anthropic_request_body_has_cache_control() {
    use reflect_llm::request::{SystemBlock, SystemBlocks};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string("event: message_stop\ndata: {}\n\n"),
        )
        .mount(&server)
        .await;

    let mut req = basic_request();
    req.system = SystemBlocks(vec![SystemBlock {
        text: "be helpful".into(),
        cache_control: None,
        ephemeral: false,
    }]);

    let client = AnthropicClient::new(AnthropicConfig {
        api_key: "k".into(),
        base_url: Some(server.uri()),
        timeout: Duration::from_secs(5),
    })
    .unwrap();
    let mut stream = client.stream(req, CancellationToken::new()).await.unwrap();
    // 排空 stream 以确保请求已发出。
    while stream.next().await.is_some() {}
    let received = server.received_requests().await.unwrap();
    assert!(!received.is_empty());
    let body = String::from_utf8_lossy(&received[0].body);
    assert!(
        body.contains("\"cache_control\""),
        "body missing cache_control: {body}"
    );
    assert!(
        body.contains("\"ttl\":\"5m\""),
        "body missing ttl:5m: {body}"
    );
    let auth = received[0]
        .headers
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert_eq!(auth, "Bearer k");
}

/// v1.x:系统块带 `CacheTtl::OneHour` 时,出站 body 的 `cache_control.ttl`
/// 应为 `"1h"`(对齐 Anthropic 长生命周期 system prompt 缓存)。这条 wire
/// 映射由 `cache_control_json` 实现;上层 `inject_cache_control` 默认 1h
/// 已在 reflect-prompt 单测覆盖。
#[tokio::test]
async fn anthropic_system_block_ttl_1h_on_wire() {
    use reflect_llm::request::{CacheControl, CacheControlKind, CacheTtl, SystemBlock};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string("event: message_stop\ndata: {}\n\n"),
        )
        .mount(&server)
        .await;

    let mut req = basic_request();
    req.system = SystemBlocks(vec![SystemBlock {
        text: "be helpful".into(),
        cache_control: Some(CacheControl {
            kind: CacheControlKind::Ephemeral,
            ttl: Some(CacheTtl::OneHour),
        }),
        ephemeral: false,
    }]);

    let client = AnthropicClient::new(AnthropicConfig {
        api_key: "k".into(),
        base_url: Some(server.uri()),
        timeout: Duration::from_secs(5),
    })
    .unwrap();
    let mut stream = client.stream(req, CancellationToken::new()).await.unwrap();
    while stream.next().await.is_some() {}
    let received = server.received_requests().await.unwrap();
    let body = String::from_utf8_lossy(&received[0].body);
    assert!(
        body.contains("\"ttl\":\"1h\""),
        "system 块 ttl 应为 1h;got: {body}"
    );
}

#[tokio::test]
async fn anthropic_parses_cache_creation_input_tokens() {
    // M8:从 message_start 事件捕获 `cache_creation_input_tokens`,
    // 并将其作为 `Usage.cache_write_tokens` 上报。
    let server = MockServer::start().await;
    let sse = make_sse(&[
        "event: message_start\ndata: {\"message\":{\"id\":\"msg_2\",\"model\":\"claude-3-5-sonnet-latest\",\"usage\":{\"input_tokens\":12,\"output_tokens\":1,\"cache_read_input_tokens\":3,\"cache_creation_input_tokens\":7}}}",
        "event: content_block_start\ndata: {\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}",
        "event: content_block_delta\ndata: {\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}",
        "event: content_block_stop\ndata: {\"index\":0}",
        "event: message_delta\ndata: {\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}",
        "event: message_stop\ndata: {}",
    ]);
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse),
        )
        .mount(&server)
        .await;

    let client = AnthropicClient::new(AnthropicConfig {
        api_key: "k".into(),
        base_url: Some(server.uri()),
        timeout: Duration::from_secs(5),
    })
    .unwrap();
    let mut stream = client
        .stream(basic_request(), CancellationToken::new())
        .await
        .unwrap();
    let mut events = Vec::new();
    while let Some(e) = stream.next().await {
        events.push(e.unwrap());
    }
    let saw_usage = events.iter().any(|e| {
        matches!(e, ChatEvent::Usage {
            input_tokens,
            output_tokens,
            cached_tokens,
            cache_write_tokens,
        } if *input_tokens == 12
            && *output_tokens == 2
            && *cached_tokens == 3
            && *cache_write_tokens == 7)
    });
    assert!(
        saw_usage,
        "expected Usage(12,2,3,7) — cache_creation_input_tokens should surface as cache_write_tokens; got {events:?}"
    );
}

/// v1.x:当 `message_delta.delta.stop_reason == "max_tokens"` 时,输出被
/// provider 的输出上限截断,解析器应发出 `MessageStopTruncated`(而非普通
/// `MessageStop`),让引擎据此 auto-continue 补完被截断的作答。
#[tokio::test]
async fn anthropic_max_tokens_stop_reason_emits_truncated() {
    let server = MockServer::start().await;
    let sse = make_sse(&[
        "event: message_start\ndata: {\"message\":{\"id\":\"msg_3\",\"model\":\"claude-3-5-sonnet-latest\",\"usage\":{\"input_tokens\":5,\"output_tokens\":1}}}",
        "event: content_block_start\ndata: {\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}",
        "event: content_block_delta\ndata: {\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"partial answer...\"}}",
        "event: content_block_stop\ndata: {\"index\":0}",
        "event: message_delta\ndata: {\"delta\":{\"stop_reason\":\"max_tokens\"},\"usage\":{\"output_tokens\":4096}}",
        "event: message_stop\ndata: {}",
    ]);
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse),
        )
        .mount(&server)
        .await;

    let client = AnthropicClient::new(AnthropicConfig {
        api_key: "k".into(),
        base_url: Some(server.uri()),
        timeout: Duration::from_secs(5),
    })
    .unwrap();
    let mut stream = client
        .stream(basic_request(), CancellationToken::new())
        .await
        .unwrap();
    let mut events = Vec::new();
    while let Some(e) = stream.next().await {
        events.push(e.unwrap());
    }
    // 末尾事件必须是 MessageStopTruncated{max_tokens},而非普通 MessageStop。
    assert!(
        matches!(events.last(), Some(ChatEvent::MessageStopTruncated { stop_reason }) if stop_reason == "max_tokens"),
        "expected MessageStopTruncated{{max_tokens}} at end, got {:?}",
        events.last()
    );
    assert!(
        !events.iter().any(|e| matches!(e, ChatEvent::MessageStop)),
        "must NOT emit a plain MessageStop when stop_reason == max_tokens"
    );
}

/// 反例:`stop_reason == "end_turn"` 仍应发普通 `MessageStop`(回归保护)。
#[tokio::test]
async fn anthropic_end_turn_emits_plain_message_stop() {
    let server = MockServer::start().await;
    let sse = make_sse(&[
        "event: message_start\ndata: {\"message\":{\"id\":\"msg_4\",\"model\":\"claude-3-5-sonnet-latest\",\"usage\":{\"input_tokens\":5,\"output_tokens\":1}}}",
        "event: content_block_start\ndata: {\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}",
        "event: content_block_delta\ndata: {\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"done.\"}}",
        "event: content_block_stop\ndata: {\"index\":0}",
        "event: message_delta\ndata: {\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}",
        "event: message_stop\ndata: {}",
    ]);
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse),
        )
        .mount(&server)
        .await;

    let client = AnthropicClient::new(AnthropicConfig {
        api_key: "k".into(),
        base_url: Some(server.uri()),
        timeout: Duration::from_secs(5),
    })
    .unwrap();
    let mut stream = client
        .stream(basic_request(), CancellationToken::new())
        .await
        .unwrap();
    let mut events = Vec::new();
    while let Some(e) = stream.next().await {
        events.push(e.unwrap());
    }
    assert!(
        matches!(events.last(), Some(ChatEvent::MessageStop)),
        "expected plain MessageStop for end_turn, got {:?}",
        events.last()
    );
}

#[tokio::test]
async fn anthropic_request_attaches_cache_control_to_last_tools() {
    // M8:当 `req.metadata["cache_break_tool"] = "true"` 时,出站 body
    // 中最后 N 个工具 def 都应携带 `cache_control: ephemeral`。
    use reflect_llm::request::{SystemBlock, SystemBlocks, ToolSpec};
    use serde_json::json;

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string("event: message_stop\ndata: {}\n\n"),
        )
        .mount(&server)
        .await;

    let mut req = basic_request();
    req.system = SystemBlocks(vec![SystemBlock {
        text: "be helpful".into(),
        cache_control: None,
        ephemeral: false,
    }]);
    // 5 个工具 —— 最后 3 个应附加 cache_control。
    for (name, _) in [
        ("read", 0),
        ("write", 1),
        ("edit", 2),
        ("grep", 3),
        ("glob", 4),
    ] {
        req.tools.push(ToolSpec::Function {
            name: name.into(),
            description: "".into(),
            parameters: json!({"type": "object"}),
        });
    }
    req.metadata
        .insert("cache_break_tool".to_string(), "true".to_string());

    let client = AnthropicClient::new(AnthropicConfig {
        api_key: "k".into(),
        base_url: Some(server.uri()),
        timeout: Duration::from_secs(5),
    })
    .unwrap();
    let mut stream = client.stream(req, CancellationToken::new()).await.unwrap();
    while stream.next().await.is_some() {}
    let received = server.received_requests().await.unwrap();
    let body = String::from_utf8_lossy(&received[0].body);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    let tools = v["tools"].as_array().expect("tools[] present");
    assert_eq!(tools.len(), 5);
    // 前 2 个工具:无 cache_control。
    for (i, t) in tools.iter().enumerate().take(2) {
        assert!(
            t.get("cache_control").is_none(),
            "tool[{i}] should NOT have cache_control, got {t}"
        );
    }
    // 后 3 个工具:cache_control.ephemeral。
    for (i, t) in tools.iter().enumerate().skip(2) {
        let cc = t.get("cache_control").expect("cache_control present");
        assert_eq!(cc["type"], "ephemeral", "tool[{i}] cache_control.type");
        assert_eq!(cc["ttl"], "5m", "tool[{i}] cache_control.ttl");
    }
}

#[tokio::test]
async fn anthropic_request_attaches_cache_control_to_prefix_anchor_message() {
    // M8:当 `req.cache_control` 携带指向消息索引 N 的 `CacheBreak` 项时,
    // 该消息最后一个 content block 上会附加 `cache_control: ephemeral`。
    use reflect_llm::CacheTtl;
    use reflect_llm::request::CacheBreak;

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string("event: message_stop\ndata: {}\n\n"),
        )
        .mount(&server)
        .await;

    let mut req = basic_request();
    req.messages = vec![
        ChatMessage::User(UserContent {
            blocks: vec![ContentBlock::text("first user turn")],
        }),
        ChatMessage::Assistant(reflect_llm::AssistantContent {
            text: Some("first assistant".into()),
            tool_calls: vec![],
            thinking: None,
        }),
        ChatMessage::User(UserContent {
            blocks: vec![ContentBlock::text("second user turn — anchor")],
        }),
    ];
    // 把 cache_control breakpoint 锚在最后一条 user 消息上(idx 2)。
    req.cache_control.push(CacheBreak {
        after_message_index: 2,
        ttl: CacheTtl::FiveMinutes,
    });

    let client = AnthropicClient::new(AnthropicConfig {
        api_key: "k".into(),
        base_url: Some(server.uri()),
        timeout: Duration::from_secs(5),
    })
    .unwrap();
    let mut stream = client.stream(req, CancellationToken::new()).await.unwrap();
    while stream.next().await.is_some() {}
    let received = server.received_requests().await.unwrap();
    let body = String::from_utf8_lossy(&received[0].body);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    let messages = v["messages"].as_array().expect("messages[] present");
    assert_eq!(messages.len(), 3);
    // 前两条消息:任意 block 上都没有 cache_control。
    for (i, m) in messages.iter().enumerate().take(2) {
        let blocks = m["content"].as_array().expect("content[] present");
        for (j, b) in blocks.iter().enumerate() {
            assert!(
                b.get("cache_control").is_none(),
                "messages[{i}].content[{j}] should NOT have cache_control, got {b}"
            );
        }
    }
    // 第三条消息:最后一个 content block 上有 cache_control。
    let anchor_blocks = messages[2]["content"].as_array().unwrap();
    let last = anchor_blocks.last().unwrap();
    let cc = last
        .get("cache_control")
        .expect("anchor block has cache_control");
    assert_eq!(cc["type"], "ephemeral");
    assert_eq!(cc["ttl"], "5m");
}

/// GAIA-fix:工具结果(image_view)里的 `ContentBlock::Image` 必须以 Anthropic
/// 原生的 base64 image 块形式进入 `tool_result.content`,而非被拍平成
/// `[{"type":"image","data":[137,80,...]}]` 字节数组字符串(此前模型看到的不是
/// 图而是数字文本,所有图片题失败)。
#[tokio::test]
async fn anthropic_tool_result_image_emits_base64_block() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string("event: message_stop\ndata: {}\n\n"),
        )
        .mount(&server)
        .await;

    // 构造一个 image_view 风格的工具结果:content 含一个 Image 块。
    let png_header: Vec<u8> = vec![0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];
    let mut req = basic_request();
    req.messages.push(ChatMessage::Tool(ToolResult {
        call_id: "call_img1".into(),
        content: vec![ContentBlock::Image {
            data: png_header.clone(),
            mime_type: "image/png".into(),
        }],
        is_error: false,
    }));

    let client = AnthropicClient::new(AnthropicConfig {
        api_key: "k".into(),
        base_url: Some(server.uri()),
        timeout: Duration::from_secs(5),
    })
    .unwrap();
    let mut stream = client.stream(req, CancellationToken::new()).await.unwrap();
    while stream.next().await.is_some() {}

    let received = server.received_requests().await.unwrap();
    let body = String::from_utf8_lossy(&received[0].body);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    let messages = v["messages"].as_array().expect("messages[] present");
    // 末条应是 role=user 的 tool_result。
    let tool_msg = messages.last().unwrap();
    assert_eq!(tool_msg["role"], "user");
    let tr = &tool_msg["content"][0];
    assert_eq!(tr["type"], "tool_result", "expected tool_result block");
    assert_eq!(tr["tool_use_id"], "call_img1");
    // content 必须是 content-blocks 数组(不是字符串),且含 image base64 块。
    let content = tr["content"]
        .as_array()
        .expect("tool_result.content must be an array");
    assert_eq!(content.len(), 1, "expected exactly one content block");
    let img = &content[0];
    assert_eq!(img["type"], "image", "expected an image block, got {img}");
    assert_eq!(img["source"]["type"], "base64");
    assert_eq!(img["source"]["media_type"], "image/png");
    // base64 数据不应是字节数组文本 `[137,80,...]`。
    let b64 = img["source"]["data"].as_str().expect("base64 data string");
    assert!(
        !b64.contains('[') && !b64.contains('8'),
        "base64 data must not look like a byte-array text, got: {b64}"
    );
    assert!(!b64.is_empty(), "base64 data must be non-empty");
}

/// 反例:纯文本工具结果仍应发 `{"type":"text"}` 块(回归保护,确保图片分支没
/// 破坏文本路径)。
#[tokio::test]
async fn anthropic_tool_result_text_emits_text_block() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string("event: message_stop\ndata: {}\n\n"),
        )
        .mount(&server)
        .await;

    let mut req = basic_request();
    req.messages.push(ChatMessage::Tool(ToolResult {
        call_id: "call_t1".into(),
        content: vec![ContentBlock::text("hello world")],
        is_error: false,
    }));

    let client = AnthropicClient::new(AnthropicConfig {
        api_key: "k".into(),
        base_url: Some(server.uri()),
        timeout: Duration::from_secs(5),
    })
    .unwrap();
    let mut stream = client.stream(req, CancellationToken::new()).await.unwrap();
    while stream.next().await.is_some() {}

    let received = server.received_requests().await.unwrap();
    let body = String::from_utf8_lossy(&received[0].body);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    let messages = v["messages"].as_array().unwrap();
    let tr = &messages.last().unwrap()["content"][0];
    assert_eq!(tr["type"], "tool_result");
    let content = tr["content"].as_array().unwrap();
    assert_eq!(content[0]["type"], "text");
    assert_eq!(content[0]["text"], "hello world");
}
