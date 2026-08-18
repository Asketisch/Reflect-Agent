//! `Ollama` provider 的 wiremock 集成测试。
//!
//! Ollama 服务端发 NDJSON(每行一 JSON 对象,无 SSE `data:` 前缀);
//! mock server 直接拼接多行 JSON,验证 `OllamaClient` 的行分隔、
//! `prompt_eval_count` / `eval_count` → `ChatEvent::Usage` 映射、
//! 错误码分类。
//!
//! 跑 `cargo test -p reflect-llm --test ollama_wiremock`。

use futures::StreamExt;
use reflect_llm::providers::{OllamaClient, OllamaConfig};
use reflect_llm::{
    ChatEvent, ChatMessage, ChatRequest, ContentBlock, LlmError, ModelClient, SystemBlocks,
    ToolSpec, UserContent,
};
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn basic_request() -> ChatRequest {
    ChatRequest {
        model: "llama3.2".into(),
        messages: vec![ChatMessage::User(UserContent {
            blocks: vec![ContentBlock::text("say hi")],
        })],
        tools: vec![],
        system: SystemBlocks::default(),
        temperature: Some(0.5),
        max_tokens: Some(256),
        top_p: None,
        thinking: None,
        cache_control: vec![],
        metadata: Default::default(),
        stop: vec![],
    }
}

fn make_client(server: &MockServer) -> OllamaClient {
    OllamaClient::new(OllamaConfig {
        base_url: Some(server.uri()),
        timeout: Duration::from_secs(5),
        ..Default::default()
    })
    .expect("client")
}

#[tokio::test]
async fn ollama_happy_path_parses_ndjson_text_stream() {
    let server = MockServer::start().await;
    let body = "{\"model\":\"llama3.2\",\"created_at\":\"2026-06-23T00:00:00Z\",\"message\":{\"role\":\"assistant\",\"content\":\"Hello\"},\"done\":false}\n\
                 {\"model\":\"llama3.2\",\"created_at\":\"2026-06-23T00:00:01Z\",\"message\":{\"role\":\"assistant\",\"content\":\" world\"},\"done\":false}\n\
                 {\"model\":\"llama3.2\",\"created_at\":\"2026-06-23T00:00:02Z\",\"message\":{\"role\":\"assistant\",\"content\":\"\"},\"done\":true,\"done_reason\":\"stop\",\"prompt_eval_count\":12,\"eval_count\":5,\"total_duration\":1234567890}\n";
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(&server)
        .await;

    let client = make_client(&server);
    let mut stream = client
        .stream(basic_request(), CancellationToken::new())
        .await
        .unwrap();
    let mut events = Vec::new();
    while let Some(e) = stream.next().await {
        events.push(e.expect("event"));
    }

    // 第一个 chunk 产生 MessageStart(因为 model 字段非空) + ContentDelta。
    let saw_start = events
        .iter()
        .any(|e| matches!(e, ChatEvent::MessageStart { model, .. } if model == "llama3.2"));
    let saw_text = events
        .iter()
        .any(|e| matches!(e, ChatEvent::ContentDelta(s) if s == "Hello"));
    let saw_text2 = events
        .iter()
        .any(|e| matches!(e, ChatEvent::ContentDelta(s) if s == " world"));
    let saw_usage = events.iter().any(|e| {
        matches!(e, ChatEvent::Usage { input_tokens, output_tokens, .. }
            if *input_tokens == 12 && *output_tokens == 5)
    });
    let saw_stop = events.iter().any(|e| matches!(e, ChatEvent::MessageStop));

    assert!(saw_start, "expected MessageStart with model=llama3.2");
    assert!(saw_text, "expected ContentDelta(\"Hello\")");
    assert!(saw_text2, "expected ContentDelta(\" world\")");
    assert!(saw_usage, "expected Usage(12, 5)");
    assert!(saw_stop, "expected MessageStop");
}

#[tokio::test]
async fn ollama_stream_with_tool_call() {
    let server = MockServer::start().await;
    let body = "{\"model\":\"qwen2.5\",\"message\":{\"role\":\"assistant\",\"content\":\"\",\"tool_calls\":[{\"id\":\"call_1\",\"function\":{\"name\":\"bash\",\"arguments\":{\"command\":\"ls\"}}}]},\"done\":false}\n\
                 {\"model\":\"qwen2.5\",\"message\":{\"role\":\"assistant\",\"content\":\"\"},\"done\":true,\"prompt_eval_count\":8,\"eval_count\":3}\n";
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(&server)
        .await;

    let client = make_client(&server);
    let mut req = basic_request();
    req.model = "qwen2.5".into();
    req.tools = vec![ToolSpec::Function {
        name: "bash".into(),
        description: "shell".into(),
        parameters: serde_json::json!({"type": "object"}),
    }];
    let mut stream = client.stream(req, CancellationToken::new()).await.unwrap();
    let mut events = Vec::new();
    while let Some(e) = stream.next().await {
        events.push(e.expect("event"));
    }

    let saw_tool_start = events.iter().any(|e| {
        matches!(e, ChatEvent::ToolUseStart { id, name, input_json, .. }
            if id == "call_1" && name == "bash" && input_json.contains("ls"))
    });
    assert!(saw_tool_start, "expected ToolUseStart(bash) with arguments");

    let received = server.received_requests().await.unwrap();
    let req_body = String::from_utf8_lossy(&received[0].body);
    assert!(
        req_body.contains("\"tools\"") && req_body.contains("\"bash\""),
        "request body should include tools schema, got: {req_body}"
    );
}

#[tokio::test]
async fn ollama_401_maps_to_auth_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ResponseTemplate::new(401).set_body_string("unauthorized"))
        .mount(&server)
        .await;

    let client = make_client(&server);
    let result = client
        .stream(basic_request(), CancellationToken::new())
        .await;
    match result {
        Err(LlmError::Auth) => {}
        Err(other) => panic!("expected Auth, got {other:?}"),
        Ok(_) => panic!("expected Auth error, got Ok"),
    }
}

#[tokio::test]
async fn ollama_404_maps_to_invalid_request_model_not_found() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(
            ResponseTemplate::new(404)
                .set_body_string(r#"{"error":"model 'foo' not found, try pulling it first"}"#),
        )
        .mount(&server)
        .await;

    let client = make_client(&server);
    let result = client
        .stream(basic_request(), CancellationToken::new())
        .await;
    match result {
        Err(LlmError::InvalidRequest { message }) => {
            assert!(
                message.contains("model not found"),
                "expected 'model not found' in message, got: {message}"
            );
        }
        Err(other) => panic!("expected InvalidRequest, got {other:?}"),
        Ok(_) => panic!("expected InvalidRequest, got Ok"),
    }
}

#[tokio::test]
async fn ollama_500_maps_to_provider_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ResponseTemplate::new(500).set_body_string("ollama internal: OOM"))
        .mount(&server)
        .await;

    let client = make_client(&server);
    let result = client
        .stream(basic_request(), CancellationToken::new())
        .await;
    match result {
        Err(LlmError::Provider { status, message }) => {
            assert_eq!(status, 500);
            assert!(message.contains("OOM"), "got: {message}");
        }
        Err(other) => panic!("expected Provider(500), got {other:?}"),
        Ok(_) => panic!("expected Provider error, got Ok"),
    }
}

#[tokio::test]
async fn ollama_request_body_includes_model_stream_and_messages() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            "{\"model\":\"llama3.2\",\"message\":{\"role\":\"assistant\",\"content\":\"hi\"},\"done\":true,\"prompt_eval_count\":1,\"eval_count\":1}\n",
        ))
        .mount(&server)
        .await;

    let client = make_client(&server);
    let _ = client
        .stream(basic_request(), CancellationToken::new())
        .await
        .expect("stream");
    let received = server.received_requests().await.unwrap();
    assert!(!received.is_empty(), "expected a request");
    let body = String::from_utf8_lossy(&received[0].body);
    assert!(body.contains("\"model\":\"llama3.2\""), "body: {body}");
    assert!(body.contains("\"stream\":true"), "body: {body}");
    assert!(body.contains("\"role\":\"user\""), "body: {body}");
    assert!(body.contains("say hi"), "body: {body}");
    // Ollama local default: 不带 Authorization header(API key 为空)。
    assert!(
        received[0].headers.get("Authorization").is_none(),
        "local Ollama should not send Authorization header"
    );
}

#[tokio::test]
async fn ollama_request_with_api_key_sends_authorization_header() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            "{\"model\":\"llama3.2\",\"message\":{\"role\":\"assistant\",\"content\":\"hi\"},\"done\":true,\"prompt_eval_count\":1,\"eval_count\":1}\n",
        ))
        .mount(&server)
        .await;

    let client = OllamaClient::new(OllamaConfig {
        base_url: Some(server.uri()),
        api_key: Some("test-key".into()),
        timeout: Duration::from_secs(5),
        ..Default::default()
    })
    .unwrap();
    let _ = client
        .stream(basic_request(), CancellationToken::new())
        .await
        .expect("stream");
    let received = server.received_requests().await.unwrap();
    let auth = received[0]
        .headers
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert_eq!(
        auth, "Bearer test-key",
        "should send Bearer token when api_key set"
    );
}
