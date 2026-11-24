//! geo: geo-lookup by IP (ipapi.co) and IANA -> Windows timezone mapping.
//!
//! Public API: [GeoClient] (implements [GeoProvider], caches results),
//! [GeoInfo], [GeoError], and [tz_windows::iana_to_windows].
mod cache;
mod ipapi;
mod provider;
pub mod tz_windows;

pub use ipapi::{IpApiCoAlias as IpApiCo, ReqwestFetch};
pub use provider::{GeoClientConfig, GeoError, GeoInfo, GeoProvider};

#[cfg(test)]
mod client_tests;

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::Mutex;

use crate::cache::LookupCache;
use crate::ipapi::HttpFetch;

/// Geo lookup client with caching and retry/backoff.
///
/// Wraps an [HttpFetch] transport (reqwest in production, scripted mocks in
/// tests) talking to ipapi.co. Successful lookups are cached for
/// [GeoClientConfig::cache_ttl]; failures are never cached.
pub struct GeoClient {
    config: GeoClientConfig,
    http: Arc<dyn HttpFetch>,
    cache: Mutex<LookupCache>,
}

impl GeoClient {
    /// Create a client over `http` with the given config.
    pub fn new(config: GeoClientConfig, http: Arc<dyn HttpFetch>) -> Self {
        let cache_ttl = config.cache_ttl;
        Self {
            config,
            http,
            cache: Mutex::new(LookupCache::new(cache_ttl)),
        }
    }
}

#[async_trait]
impl GeoProvider for GeoClient {
    async fn lookup(&self, ip: IpAddr) -> Result<GeoInfo, GeoError> {
        let mut cache = self.cache.lock().await;
        if let Some(info) = cache.get(&ip) {
            return Ok(info);
        }
        drop(cache);

        let info = ipapi::lookup_with_retries(&*self.http, ip, &self.config).await?;

        let mut cache = self.cache.lock().await;
        cache.insert(ip, info.clone());
        Ok(info)
    }
}

/// Convenience constructor for the production transport.
impl GeoClient {
    /// Build a client with a real reqwest transport and `timeout` applied.
    pub fn with_reqwest(config: GeoClientConfig) -> Self {
        let http: Arc<dyn HttpFetch> = Arc::new(ipapi::ReqwestFetch::new(config.timeout));
        Self::new(config, http)
    }
}

/// Re-exported so examples can name the default config values.
pub const DEFAULT_TIMEOUT: Duration = ipapi::DEFAULT_TIMEOUT;
