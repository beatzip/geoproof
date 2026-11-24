//! Core geo types: [GeoInfo], [GeoError], [GeoProvider], [GeoClientConfig].
use std::net::IpAddr;
use std::time::Duration;

use async_trait::async_trait;

/// Geo data for one IP, as returned by the provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeoInfo {
    /// ISO 3166-1 alpha-2, normalized to uppercase.
    pub country_code: String,
    /// IANA timezone, as returned by the provider.
    pub timezone: String,
    /// BCP-47 languages in provider order (empty when absent).
    pub languages: Vec<String>,
}

/// Errors surfaced by a geo lookup.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GeoError {
    /// Transport-level failure (connect/timeout/body read).
    #[error("http transport error: {0}")]
    Http(String),
    /// Non-retryable HTTP status.
    #[error("http status {0}")]
    Status(u16),
    /// HTTP 429 or provider reason "RateLimited" after retries.
    #[error("rate limited by provider")]
    RateLimited,
    /// Response body could not be parsed into GeoInfo.
    #[error("malformed geo response: {0}")]
    Parse(String),
    /// Provider answered with an application error (error:true + reason).
    #[error("provider error: {0}")]
    Provider(String),
}

/// Source of geo data for IP addresses.
#[async_trait]
pub trait GeoProvider: Send + Sync {
    /// Look up geo data for `ip`.
    async fn lookup(&self, ip: IpAddr) -> Result<GeoInfo, GeoError>;
}

/// Static configuration for [crate::GeoClient], with defaults per spec:
/// timeout 5 s, 3 retries on top of the first attempt, 0.5 s backoff base,
/// 24 h cache TTL.
#[derive(Debug, Clone)]
pub struct GeoClientConfig {
    /// Per-request HTTP timeout.
    pub timeout: Duration,
    /// Retries performed on top of the first attempt.
    pub retries: u32,
    /// Base delay for exponential backoff: base * 2^n (0.5/1/2 s).
    pub backoff_base: Duration,
    /// How long a successful lookup stays cached.
    pub cache_ttl: Duration,
}

impl Default for GeoClientConfig {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(5),
            retries: 3,
            backoff_base: Duration::from_millis(500),
            cache_ttl: Duration::from_secs(24 * 60 * 60),
        }
    }
}
