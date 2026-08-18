//! `OpenAI` provider 的 wiremock 集成测试。

use futures::StreamExt;
use reflect_llm::providers::{OpenAIClient, OpenAIConfig};
use reflect_llm::{
    ChatEvent, ChatMessage, ChatRequest, ContentBlock, LlmError, ModelClient, SystemBlocks,
    UserContent,
};
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

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
    }
}

#[tokio::test]
async fn openai_happy_path_parses_sse() {
    let server = MockServer::start().await;
    let body = "data: {\"id\":\"chatcmpl-1\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hello\"}}]}\n\ndata: {\"choices\":[{\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("Authorization", "Bearer test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(&server)
        .await;

    let client = OpenAIClient::new(OpenAIConfig {
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
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(&server)
        .await;

    let client = OpenAIClient::new(OpenAIConfig {
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
        .respond_with(ResponseTemplate::new(200).set_body_string(
            "data: {\"choices\":[{\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
        ))
        .mount(&server)
        .await;
    // 期望通过回调捕获请求 body —— wiremock 的 matchers 在响应前执行，
    // 我们改用 `MockServer::received_requests` API 读取已发请求。
    let client = OpenAIClient::new(OpenAIConfig {
        api_key: "k".into(),
        base_url: Some(server.uri()),
        timeout: Duration::from_secs(5),
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
