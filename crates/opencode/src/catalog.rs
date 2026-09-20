//! models.dev catalog wiring for the CLI: the production HTTP fetcher
//! (reqwest on a dedicated runtime thread, like
//! `opencode_core::skill::ReqwestFetcher`) and the [`CatalogService`]
//! factory shared by the `models` and `providers` commands.

use std::sync::Arc;
use std::time::Duration;

use opencode_core::catalog::FetchError;
use opencode_core::{CatalogConfig, CatalogService, GlobalPaths};

/// Production fetcher for `GET {source}/api.json`. Network errors and 5xx are
/// transient (the service retries with backoff); 4xx are permanent. The TS
/// reference enforces a 10s request timeout through its HTTP client.
pub struct HttpFetcher;

impl opencode_core::catalog::Fetcher for HttpFetcher {
    fn get(&self, url: &str, user_agent: &str) -> Result<String, FetchError> {
        std::thread::scope(|scope| {
            scope
                .spawn(move || {
                    tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .map_err(|err| FetchError::Transient(err.to_string()))?
                        .block_on(async {
                            let client = reqwest::Client::builder()
                                .timeout(Duration::from_secs(10))
                                .build()
                                .map_err(|err| FetchError::Transient(err.to_string()))?;
                            let response = client
                                .get(url)
                                .header("User-Agent", user_agent)
                                .send()
                                .await
                                .map_err(|err| FetchError::Transient(err.to_string()))?;
                            let status = response.status();
                            if status.is_client_error() {
                                return Err(FetchError::Permanent(status.to_string()));
                            }
                            if status.is_server_error() {
                                return Err(FetchError::Transient(status.to_string()));
                            }
                            response
                                .text()
                                .await
                                .map_err(|err| FetchError::Transient(err.to_string()))
                        })
                })
                .join()
                .map_err(|_| FetchError::Transient("fetch panicked".to_string()))?
        })
    }
}

/// The process-wide catalog service over the global cache directory.
pub fn catalog_service() -> Arc<CatalogService> {
    let paths = GlobalPaths::from_env();
    Arc::new(CatalogService::new(
        paths.cache,
        CatalogConfig::from_env(),
        Arc::new(opencode_core::catalog::SystemClock),
        Arc::new(HttpFetcher),
    ))
}
