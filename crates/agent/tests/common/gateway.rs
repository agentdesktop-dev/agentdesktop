use std::sync::{Arc, Mutex};

use axum::{
    Json, Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::any,
};
use serde_json::{Value, json};
use tokio::task::JoinHandle;

pub const TOKEN: &str = "provider-integration-access-token";

type Requests = Arc<Mutex<Vec<Value>>>;

pub struct Gateway {
    pub url: String,
    requests: Requests,
    task: JoinHandle<()>,
}

impl Gateway {
    pub async fn start() -> anyhow::Result<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://127.0.0.1:{}/", listener.local_addr()?.port());
        let requests = Requests::default();
        let app = Router::new()
            .fallback(any(handle))
            .with_state(requests.clone());
        let task = tokio::spawn(async move {
            if let Err(error) = axum::serve(listener, app).await {
                tracing::error!(%error, "Fixture gateway failed");
            }
        });
        tracing::info!(%url, "Started host gateway fixture");
        Ok(Self {
            url,
            requests,
            task,
        })
    }

    pub fn requests(&self) -> Vec<Value> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for Gateway {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn handle(
    State(requests): State<Requests>,
    method: axum::http::Method,
    uri: Uri,
    headers: HeaderMap,
    raw: Bytes,
) -> Response {
    let path = uri.path();
    let body = if raw.is_empty() {
        Value::Null
    } else {
        match serde_json::from_slice::<Value>(&raw) {
            Ok(body) => body,
            Err(_) => return (StatusCode::BAD_REQUEST, "Invalid fixture request").into_response(),
        }
    };
    let authorization = headers.get("authorization").and_then(|v| v.to_str().ok());
    let api_key = headers.get("x-api-key").and_then(|v| v.to_str().ok());
    tracing::debug!(%method, path, "Gateway fixture request");
    requests.lock().unwrap().push(json!({"path": path, "method": method.as_str(), "authorization": authorization, "apiKey": api_key, "body": body}));
    if path == "/health" {
        return Json(json!({"ready": true})).into_response();
    }
    if path != "/v1/messages" && path != "/v1/messages/count_tokens" {
        return StatusCode::NOT_FOUND.into_response();
    }
    if authorization != Some(format!("Bearer {TOKEN}").as_str()) && api_key != Some(TOKEN) {
        return (StatusCode::UNAUTHORIZED, Json(json!({"type": "error", "error": {"type": "authentication_error", "message": "Expected the test API key"}}))).into_response();
    }
    if path == "/v1/messages/count_tokens" {
        return Json(json!({"input_tokens": 10})).into_response();
    }
    let text = "provider-integration-ok";
    let mut message = json!({
        "id": "msg_provider_integration", "type": "message", "role": "assistant", "model": body["model"],
        "content": [{"type": "text", "text": text}], "stop_reason": "end_turn", "stop_sequence": null,
        "usage": {"input_tokens": 10, "output_tokens": 5, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0},
    });
    if body["stream"] != true {
        return Json(message).into_response();
    }
    message["content"] = json!([]);
    message["stop_reason"] = Value::Null;
    message["usage"]["output_tokens"] = json!(0);
    let events = [
        json!({"type": "message_start", "message": message}),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": text}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "message_delta", "delta": {"stop_reason": "end_turn", "stop_sequence": null}, "usage": {"output_tokens": 5}}),
        json!({"type": "message_stop"}),
    ];
    let stream: String = events
        .iter()
        .map(|event| {
            format!(
                "event: {}\ndata: {event}\n\n",
                event["type"].as_str().unwrap()
            )
        })
        .collect();
    (
        [
            ("content-type", "text/event-stream"),
            ("cache-control", "no-cache"),
        ],
        stream,
    )
        .into_response()
}
