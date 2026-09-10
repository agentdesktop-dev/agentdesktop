use reqwest::{Client, IntoUrl, Result};
use serde::{Serialize, de::DeserializeOwned};

/// HTTP requests with typed JSON responses and HTTP status checks.
#[allow(async_fn_in_trait)]
pub trait ClientExt {
    async fn get_json<T: DeserializeOwned>(&self, url: impl IntoUrl) -> Result<T>;

    /// POST a JSON body and decode the JSON response.
    async fn post_json<T: DeserializeOwned>(
        &self,
        url: impl IntoUrl,
        body: &(impl Serialize + ?Sized),
    ) -> Result<T>;
}

impl ClientExt for Client {
    async fn get_json<T: DeserializeOwned>(&self, url: impl IntoUrl) -> Result<T> {
        self.get(url).send().await?.error_for_status()?.json().await
    }

    async fn post_json<T: DeserializeOwned>(
        &self,
        url: impl IntoUrl,
        body: &(impl Serialize + ?Sized),
    ) -> Result<T> {
        self.post(url)
            .json(body)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await
    }
}
