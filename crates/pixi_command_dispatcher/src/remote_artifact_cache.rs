use std::path::Path;

use rattler_networking::LazyClient;
use reqwest_middleware::ClientWithMiddleware;
use serde::Deserialize;
use thiserror::Error;
use url::Url;

#[derive(Debug, Error)]
pub enum RemoteArtifactCacheError {
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest_middleware::Error),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("request error: {0}")]
    Reqwest(#[from] reqwest::Error),
}

#[derive(Debug, Deserialize)]
pub struct ArtifactInfo {
    pub sha256: String,
    pub size: i64,
}

/// Client for the remote artifact cache on prefix.dev.
///
/// Uses the authenticated `LazyClient` from rattler-networking so that
/// tokens stored via `pixi auth login` are automatically attached.
#[derive(Clone, Debug)]
pub struct RemoteArtifactCache {
    client: LazyClient,
    base_url: Url,
    owner: String,
    upload_enabled: bool,
}

impl RemoteArtifactCache {
    pub fn new(client: LazyClient, base_url: Url, owner: String, upload_enabled: bool) -> Self {
        Self {
            client,
            base_url,
            owner,
            upload_enabled,
        }
    }

    pub fn upload_enabled(&self) -> bool {
        self.upload_enabled
    }

    /// Get the authenticated HTTP client.
    fn http_client(&self) -> &ClientWithMiddleware {
        self.client.client()
    }

    /// Format a URL for the given API path segment and cache key.
    fn api_url(&self, action: &str, cache_key: &str) -> String {
        format!(
            "{}/api/v1/artifact-cache/{}/{}/{}",
            self.base_url.as_str().trim_end_matches('/'),
            action,
            self.owner,
            cache_key,
        )
    }

    /// Check if an artifact exists in the remote cache.
    pub async fn check(
        &self,
        cache_key: &str,
    ) -> Result<Option<ArtifactInfo>, RemoteArtifactCacheError> {
        let url = self.api_url("check", cache_key);

        let response = self.http_client().get(&url).send().await?;

        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }

        let info = response.error_for_status()?.json::<ArtifactInfo>().await?;
        Ok(Some(info))
    }

    /// Download an artifact from the remote cache to a local path.
    pub async fn download(
        &self,
        cache_key: &str,
        dest: &Path,
    ) -> Result<(), RemoteArtifactCacheError> {
        let url = self.api_url("download", cache_key);

        let response = self.http_client().get(&url).send().await?.error_for_status()?;

        let bytes = response.bytes().await?;
        tokio::fs::write(dest, &bytes).await?;

        Ok(())
    }

    /// Upload a built artifact to the remote cache.
    pub async fn upload(
        &self,
        cache_key: &str,
        file: &Path,
        sha256_hex: &str,
    ) -> Result<(), RemoteArtifactCacheError> {
        let url = self.api_url("upload", cache_key);

        let file_bytes = tokio::fs::read(file).await?;

        self.http_client()
            .put(&url)
            .header("content-type", "application/octet-stream")
            .header("x-file-sha256", sha256_hex)
            .body(file_bytes)
            .send()
            .await?
            .error_for_status()?;

        Ok(())
    }
}
