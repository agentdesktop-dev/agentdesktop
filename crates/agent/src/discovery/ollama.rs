use std::time::Duration;

use agentdesktop_core::model::{LocalModel, ModelRuntime};
use serde::Deserialize;

const ENDPOINT: &str = "http://127.0.0.1:11434";

#[derive(Deserialize)]
struct TagsResponse {
    models: Vec<OllamaModel>,
}

#[derive(Deserialize)]
struct OllamaModel {
    name: String,
}

pub(super) async fn discover() -> Option<ModelRuntime> {
    discover_at(ENDPOINT).await
}

async fn discover_at(endpoint: &str) -> Option<ModelRuntime> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(1))
        .build()
        .ok()?;
    let response = client
        .get(format!("{endpoint}/api/tags"))
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?
        .json::<TagsResponse>()
        .await
        .ok()?;
    let mut models = response
        .models
        .into_iter()
        .map(|model| LocalModel { name: model.name })
        .collect::<Vec<_>>();
    models.sort_by(|left, right| left.name.cmp(&right.name));
    models.dedup_by(|left, right| left.name == right.name);
    Some(ModelRuntime {
        kind: "ollama".to_owned(),
        models,
    })
}

#[cfg(test)]
mod tests {
    use axum::{Json, Router, routing::get};
    use serde_json::json;

    use super::discover_at;

    #[tokio::test]
    async fn discovers_and_sorts_models() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route(
                    "/api/tags",
                    get(|| async {
                        Json(json!({
                            "models": [
                                { "name": "qwen3:8b" },
                                { "name": "gemma3:4b" },
                                { "name": "qwen3:8b" }
                            ]
                        }))
                    }),
                ),
            )
            .await
            .unwrap();
        });

        let runtime = discover_at(&format!("http://{address}"))
            .await
            .expect("discover Ollama");
        assert_eq!(runtime.kind, "ollama");
        assert_eq!(
            runtime
                .models
                .iter()
                .map(|model| model.name.as_str())
                .collect::<Vec<_>>(),
            ["gemma3:4b", "qwen3:8b"]
        );

        server.abort();
    }
}
