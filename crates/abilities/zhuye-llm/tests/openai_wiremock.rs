//! `OpenAI` provider 的 wiremock 集成测试。

use futures::StreamExt;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};
use zhuye_llm::providers::{OpenAIClient, OpenAIConfig};
use zhuye_llm::{
    ChatEvent, ChatMessage, ChatRequest, ContentBlock, LlmError, ModelClient, SystemBlocks,
    UserContent,
};

fn basic_request() -> ChatRequest {
    ChatRequest {
        model: "gpt-4o".into(),
        messages: vec![ChatMessage::User(UserContent {
            blocks: vec![ContentBlock::text("say hi")],
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
        response_format: None,
    }
}

#[tokio::test]
async fn openai_happy_path_parses_sse() {
    let server = MockServer::start().await;
    let body = "data: {\"id\":\"chatcmpl-1\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hello\"}}]}\n\ndata: {\"choices\":[{\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("Authorization", "Bearer test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
        .mount(&server)
        .await;

    let client = OpenAIClient::new(OpenAIConfig {
        api_key: "test-key".into(),
        base_url: Some(server.uri()),
        timeout: Duration::from_secs(5),
        temperature: None,
        max_tokens: None,
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
    // 期望事件顺序：`MessageStart` → `ContentDelta("Hello")` → `MessageStop`。
    assert!(
        matches!(&events[0], ChatEvent::MessageStart { id, model } if id == "chatcmpl-1" && model == "gpt-4o")
    );
    assert!(matches!(&events[1], ChatEvent::ContentDelta(s) if s == "Hello"));
    assert!(matches!(&events[2], ChatEvent::MessageStop));
}

#[tokio::test]
async fn openai_401_maps_to_auth_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(401).set_body_string("unauthorized"))
        .mount(&server)
        .await;

    let client = OpenAIClient::new(OpenAIConfig {
        api_key: "k".into(),
        base_url: Some(server.uri()),
        timeout: Duration::from_secs(5),
        temperature: None,
        max_tokens: None,
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
async fn openai_429_honors_retry_after() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "5")
                .set_body_string("rate limited"),
        )
        .mount(&server)
        .await;

    let client = OpenAIClient::new(OpenAIConfig {
        api_key: "k".into(),
        base_url: Some(server.uri()),
        timeout: Duration::from_secs(5),
        temperature: None,
        max_tokens: None,
    })
    .unwrap();
    let err = match client
        .stream(basic_request(), CancellationToken::new())
        .await
    {
        Ok(_) => panic!("expected error, got Ok"),
        Err(e) => e,
    };
    match err {
        LlmError::RateLimited { retry_after_ms } => assert_eq!(retry_after_ms, 5000),
        other => panic!("expected RateLimited, got {other:?}"),
    }
}

#[tokio::test]
async fn openai_stream_with_tool_calls_and_usage() {
    let server = MockServer::start().await;
    let body = "data: {\"id\":\"chatcmpl-2\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":null,\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"bash\",\"arguments\":\"\"}}]}}]}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"cmd\\\":\\\"ls\\\"}\"}}]}}]}\n\ndata: {\"choices\":[{\"finish_reason\":\"tool_calls\"}]}\n\ndata: {\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":2,\"total_tokens\":12,\"prompt_tokens_details\":{\"cached_tokens\":3}}}\n\ndata: [DONE]\n\n";
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
        .mount(&server)
        .await;

    let client = OpenAIClient::new(OpenAIConfig {
        api_key: "k".into(),
        base_url: Some(server.uri()),
        timeout: Duration::from_secs(5),
        temperature: None,
        max_tokens: None,
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
    let saw_tool_start = events
        .iter()
        .any(|e| matches!(e, ChatEvent::ToolUseStart { id, name, .. } if id == "call_1" && name == "bash"));
    let saw_tool_delta = events
        .iter()
        .any(|e| matches!(e, ChatEvent::ToolUseDelta(s) if s.contains("cmd")));
    let saw_usage = events.iter().any(|e| {
        matches!(e, ChatEvent::Usage { input_tokens, output_tokens, cached_tokens, .. }
            if *input_tokens == 10 && *output_tokens == 2 && *cached_tokens == 3)
    });
    let saw_stop = events.iter().any(|e| matches!(e, ChatEvent::MessageStop));
    assert!(saw_tool_start, "expected ToolUseStart(bash)");
    assert!(saw_tool_delta, "expected ToolUseDelta with cmd");
    assert!(saw_usage, "expected Usage(10,2,3)");
    assert!(saw_stop, "expected MessageStop");
}

#[tokio::test]
async fn openai_500_maps_to_provider_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(500).set_body_string("internal"))
        .mount(&server)
        .await;

    let client = OpenAIClient::new(OpenAIConfig {
        api_key: "k".into(),
        base_url: Some(server.uri()),
        timeout: Duration::from_secs(5),
        temperature: None,
        max_tokens: None,
    })
    .unwrap();
    let err = match client
        .stream(basic_request(), CancellationToken::new())
        .await
    {
        Ok(_) => panic!("expected error"),
        Err(e) => e,
    };
    match err {
        LlmError::Provider { status, .. } => assert_eq!(status, 500),
        other => panic!("expected Provider(500), got {other:?}"),
    }
}

#[tokio::test]
async fn openai_request_body_has_model_and_stream() {
    use parking_lot::Mutex;
    use std::sync::Arc;

    let server = MockServer::start().await;
    let captured: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let captured_c = captured.clone();
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            "data: {\"choices\":[{\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
            "text/event-stream",
        ))
        .mount(&server)
        .await;
    // 期望通过回调捕获请求 body —— wiremock 的 matchers 在响应前执行，
    // 我们改用 `MockServer::received_requests` API 读取已发请求。
    let client = OpenAIClient::new(OpenAIConfig {
        api_key: "k".into(),
        base_url: Some(server.uri()),
        timeout: Duration::from_secs(5),
        temperature: None,
        max_tokens: None,
    })
    .unwrap();
    let _ = client
        .stream(basic_request(), CancellationToken::new())
        .await;
    let _ = captured_c; // not used; demo of pattern
    let received = server.received_requests().await.unwrap();
    assert!(
        !received.is_empty(),
        "expected at least one received request"
    );
    let body = &received[0].body;
    let s = String::from_utf8_lossy(body);
    assert!(
        s.contains("\"model\":\"gpt-4o\""),
        "body missing model: {s}"
    );
    assert!(
        s.contains("\"stream\":true"),
        "body missing stream:true: {s}"
    );
    let auth = received[0]
        .headers
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert_eq!(auth, "Bearer k");
}

/// R2:空 `api_key` 时**不**发 Authorization 头 —— 免 Key 的 OpenAI 兼容
/// 端点(免费模型池)对「空 Bearer」宽容、对「非空无效 token」严格。
#[tokio::test]
async fn openai_empty_api_key_omits_authorization_header() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            "data: {\"choices\":[{\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
            "text/event-stream",
        ))
        .mount(&server)
        .await;

    let client = OpenAIClient::new(OpenAIConfig {
        api_key: String::new(),
        base_url: Some(server.uri()),
        timeout: Duration::from_secs(5),
        temperature: None,
        max_tokens: None,
    })
    .unwrap();
    let _ = client
        .stream(basic_request(), CancellationToken::new())
        .await;

    let received = server.received_requests().await.unwrap();
    assert!(!received.is_empty(), "expected a request");
    assert!(
        received[0].headers.get("Authorization").is_none(),
        "空 api_key 时不应带 Authorization 头,实际:{:?}",
        received[0].headers.get("Authorization")
    );
}

/// R0-1:`list_models` 走 `GET {base}/v1/models` 并解析清单条目。
#[tokio::test]
async fn openai_list_models_parses_listing() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "object": "list",
            "data": [
                {"id": "cohere/north-mini-code:free", "isFree": true, "context_length": 256000},
                {"id": "paid-model", "isFree": false}
            ]
        })))
        .mount(&server)
        .await;

    let client = OpenAIClient::new(OpenAIConfig {
        api_key: String::new(),
        base_url: Some(server.uri()),
        timeout: Duration::from_secs(5),
        temperature: None,
        max_tokens: None,
    })
    .unwrap();
    let list = client
        .list_models(CancellationToken::new())
        .await
        .expect("清单拉取不应失败")
        .expect("该 provider 应提供清单");

    assert_eq!(list.len(), 2);
    assert_eq!(list[0].id, "cohere/north-mini-code:free");
    assert_eq!(list[0].is_free, Some(true));
    assert_eq!(list[0].context_window, Some(256000));
    assert_eq!(list[1].is_free, Some(false));
}

/// R0-1:清单端点不存在(404)→ 返回 `Ok(None)`(该 provider 无清单能力),
/// 而非把 404 当成故障。
#[tokio::test]
async fn openai_list_models_404_returns_none() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(404).set_body_string("not found"))
        .mount(&server)
        .await;

    let client = OpenAIClient::new(OpenAIConfig {
        api_key: String::new(),
        base_url: Some(server.uri()),
        timeout: Duration::from_secs(5),
        temperature: None,
        max_tokens: None,
    })
    .unwrap();
    let result = client.list_models(CancellationToken::new()).await;
    assert!(
        matches!(result, Ok(None)),
        "404 应降级为 Ok(None),实际:{result:?}"
    );
}

/// R2:错误**在流里**下发 —— HTTP 仍是 200,但某一帧是
/// `data: {"error":{...}}`(实测:dsh-our-free-model 网关的 OpenCode 车道
/// 在地区门 / 配额用尽时就是这个形状)。
///
/// 此前该帧被整块丢弃 → 上层收到「成功但零内容」的流,turn 以
/// `success` + 0 token 收场,真实错误静默丢失。这里断言它必须变成
/// Error 事件。
#[tokio::test]
async fn openai_in_stream_error_frame_yields_error_event() {
    let server = MockServer::start().await;
    let body = concat!(
        "data: {\"id\":\"chatcmpl-1\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"}}]}\n\n",
        "data: {\"error\":{\"message\":\"This model is not available in your country\",\"type\":\"server_error\"}}\n\n",
        "data: [DONE]\n\n",
    );
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
        .mount(&server)
        .await;

    let client = OpenAIClient::new(OpenAIConfig {
        api_key: String::new(),
        base_url: Some(server.uri()),
        timeout: Duration::from_secs(5),
        temperature: None,
        max_tokens: None,
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

    let err = events
        .iter()
        .find_map(|e| match e {
            ChatEvent::Error(e) => Some(e),
            _ => None,
        })
        .expect("流内错误帧必须产出 Error 事件");
    match err {
        LlmError::Provider { status, message } => {
            assert_eq!(*status, 200);
            assert!(
                message.contains("not available in your country"),
                "{message}"
            );
        }
        other => panic!("期望 Provider 错误,得到 {other:?}"),
    }
    // 不得产出正文 —— 错误帧不是内容帧。
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, ChatEvent::ContentDelta(_) | ChatEvent::ThinkingDelta(_))),
        "错误帧不应产出内容增量"
    );
}

// ── 非 SSE 校验 / 版本段拼接 / 采样参数默认值 回归 ──────────────────────

/// 流式请求收到 HTTP 200 的普通 JSON(非 SSE)→ `stream()` 必须
/// 显式返回 `LlmError::Provider`(消息含「非 SSE」与响应体预览),
/// 不再让流以「成功但零内容」静默收场。
#[tokio::test]
async fn openai_non_sse_200_response_yields_explicit_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            br#"{"choices":[{"message":{"content":"hello"}}]}"#.to_vec(),
            "application/json",
        ))
        .mount(&server)
        .await;
    let client = OpenAIClient::new(OpenAIConfig {
        api_key: "test-key".into(),
        base_url: Some(server.uri()),
        timeout: Duration::from_secs(5),
        temperature: None,
        max_tokens: None,
    })
    .expect("client");
    let result = client
        .stream(basic_request(), CancellationToken::new())
        .await;
    let Err(err) = result else {
        panic!("非 SSE 响应必须返回错误");
    };
    match err {
        LlmError::Provider { status, message } => {
            assert_eq!(status, 200);
            assert!(message.contains("非 SSE"), "{message}");
            assert!(message.contains("choices"), "应带响应体预览: {message}");
        }
        other => panic!("期望 Provider 错误,得到 {other:?}"),
    }
}

/// `[openai] temperature` / `max_tokens` 段配置作为默认值进请求体
/// (请求侧显式值优先:`max_tokens` 显式 256 不得被配置覆盖)。
#[tokio::test]
async fn openai_config_temperature_and_max_tokens_fill_request_body() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            "data: {\"choices\":[{\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
            "text/event-stream",
        ))
        .mount(&server)
        .await;
    let client = OpenAIClient::new(OpenAIConfig {
        api_key: "test-key".into(),
        base_url: Some(server.uri()),
        timeout: Duration::from_secs(5),
        temperature: Some(0.3),
        max_tokens: Some(777),
    })
    .expect("client");
    let mut stream = client
        .stream(basic_request(), CancellationToken::new())
        .await
        .expect("stream");
    while let Some(ev) = stream.next().await {
        ev.expect("event");
    }
    let received = server.received_requests().await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&received[0].body).unwrap();
    // f32 序列化有精度噪声(0.3f32 → 0.3000000119…),按容差比较。
    let temp = body["temperature"].as_f64().expect("temperature 应为数值");
    assert!(
        (temp - 0.3).abs() < 1e-3,
        "配置的 temperature 应进请求体: {body}"
    );
    assert_eq!(body["max_tokens"], 256, "请求侧显式值优先于配置默认");
}

/// 请求侧未带 temperature / max_tokens 时,配置默认值生效。
#[tokio::test]
async fn openai_config_defaults_apply_when_request_omits_params() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            "data: {\"choices\":[{\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
            "text/event-stream",
        ))
        .mount(&server)
        .await;
    let client = OpenAIClient::new(OpenAIConfig {
        api_key: "test-key".into(),
        base_url: Some(server.uri()),
        timeout: Duration::from_secs(5),
        temperature: Some(0.9),
        max_tokens: Some(123),
    })
    .expect("client");
    let mut request = basic_request();
    request.temperature = None;
    request.max_tokens = None;
    let mut stream = client
        .stream(request, CancellationToken::new())
        .await
        .expect("stream");
    while let Some(ev) = stream.next().await {
        ev.expect("event");
    }
    let received = server.received_requests().await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&received[0].body).unwrap();
    let temp = body["temperature"].as_f64().expect("temperature 应为数值");
    assert!((temp - 0.9).abs() < 1e-3, "{body}");
    assert_eq!(body["max_tokens"], 123);
}

/// base_url 已含版本段(智谱式 `/api/paas/v4`)→ 只追加
/// `/chat/completions`,不再拼出 `/v4/v1/...`。
#[tokio::test]
async fn openai_base_url_with_version_segment_not_duplicated() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/paas/v4/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            "data: {\"choices\":[{\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
            "text/event-stream",
        ))
        .mount(&server)
        .await;
    let client = OpenAIClient::new(OpenAIConfig {
        api_key: "test-key".into(),
        base_url: Some(format!("{}/api/paas/v4", server.uri())),
        timeout: Duration::from_secs(5),
        temperature: None,
        max_tokens: None,
    })
    .expect("client");
    let mut stream = client
        .stream(basic_request(), CancellationToken::new())
        .await
        .expect("stream");
    while let Some(ev) = stream.next().await {
        ev.expect("event");
    }
    // 命中了 /api/paas/v4/chat/completions(wiremock 404 会让 stream 报错,
    // 走不到这里);再确认没有请求打到 /v1/... 重复段。
    let received = server.received_requests().await.unwrap();
    let urls: Vec<String> = received
        .iter()
        .map(|r| r.url.as_str().to_string())
        .collect();
    assert!(
        urls.iter()
            .all(|u| u.contains("/api/paas/v4/chat/completions")),
        "全部请求应打到版本段路径: {urls:?}"
    );
}
