//! A local model server for tests: it speaks OpenAI's Responses streaming
//! protocol, and answers each request as the test scripts it, so agents
//! run without calling a model.

use std::sync::Arc;

use arko_ai::LLMClient;
use serde_json::Value;
use tokio::task::JoinHandle;

/// A local model server speaking OpenAI's Responses streaming
/// protocol: `respond` answers each request (its index and body) with
/// the events of a model turn, after an optional wait.
pub struct ModelServer {
    url: String,
    task: JoinHandle<()>,
}

pub type Respond = dyn Fn(usize, String) -> std::pin::Pin<Box<dyn std::future::Future<Output = String> + Send>>
    + Send
    + Sync;

impl ModelServer {
    pub async fn start(respond: Arc<Respond>) -> Self {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let task = tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let respond = respond.clone();
                let count = count.clone();
                tokio::spawn(async move {
                    let mut bytes = Vec::new();
                    let body = loop {
                        let mut chunk = [0; 8192];
                        let n = socket.read(&mut chunk).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        bytes.extend_from_slice(&chunk[..n]);
                        let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") else {
                            continue;
                        };
                        let head = String::from_utf8_lossy(&bytes[..end]).to_ascii_lowercase();
                        let length: usize = head
                            .lines()
                            .find_map(|line| line.strip_prefix("content-length:"))
                            .map_or(0, |value| value.trim().parse().unwrap());
                        if bytes.len() >= end + 4 + length {
                            break String::from_utf8_lossy(&bytes[end + 4..end + 4 + length])
                                .into_owned();
                        }
                    };
                    let index = count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let stream = respond(index, body).await;
                    // A whole response (an error), or the events of a turn.
                    let reply = if stream.starts_with("HTTP/1.1 ") {
                        stream
                    } else {
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{stream}",
                            stream.len()
                        )
                    };
                    let _ = socket.write_all(reply.as_bytes()).await;
                });
            }
        });
        Self { url, task }
    }

    pub fn client(&self) -> LLMClient {
        LLMClient::new().with_openai_client(
            arko_ai::providers::openai::OpenAIClient::new("test-key").with_base_url(&self.url),
        )
    }

    /// A client of this server over a ChatGPT login (with a token that
    /// doesn't expire), where OpenAI models accept the backend's input
    /// limit.
    pub fn chatgpt_client(&self) -> LLMClient {
        use base64::Engine;
        let part = |json: &str| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json);
        let token = format!(
            "{}.{}.signature",
            part(r#"{"alg":"none"}"#),
            part(r#"{"exp":4102444800}"#)
        );
        LLMClient::new().with_openai_client(
            arko_ai::providers::openai::OpenAIClient::new_chatgpt_oauth(&token, None, "account")
                .with_base_url(&self.url),
        )
    }
}

impl Drop for ModelServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// A model turn calling the tool `name`.
pub fn tool_call(id: &str, name: &str, arguments: Value) -> String {
    let item = serde_json::json!({
        "type": "function_call", "id": format!("fc_{id}"), "call_id": format!("call_{id}"),
        "name": name, "arguments": arguments.to_string(), "status": "completed",
    });
    let completed = serde_json::json!({"type": "response.completed", "response": {
        "id": format!("resp_{id}"), "object": "response", "created_at": 0, "status": "completed",
        "model": "gpt-6-luna", "output": [], "usage": {"input_tokens": 10, "output_tokens": 5},
        "metadata": {},
    }});
    [
        serde_json::json!({"type": "response.output_item.done", "output_index": 0, "item": item}),
        completed,
    ]
    .iter()
    .map(|event| format!("data: {event}\n\n"))
    .collect()
}

/// `turn` (a model turn), reporting `tokens` of input.
pub fn with_input(turn: String, tokens: usize) -> String {
    turn.replace("\"input_tokens\":10", &format!("\"input_tokens\":{tokens}"))
}

/// The answer to a request over the model's input limit, as OpenAI gives
/// it.
pub fn overflow() -> String {
    let body = serde_json::json!({"error": {
        "message": "Your input exceeds the context window of this model. Please adjust your input and try again.",
        "type": "invalid_request_error", "param": "input", "code": "context_length_exceeded",
    }})
    .to_string();
    format!(
        "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

pub fn complete(id: &str, report: &str) -> String {
    tool_call(
        id,
        "complete_task",
        serde_json::json!({ "summary": report }),
    )
}
