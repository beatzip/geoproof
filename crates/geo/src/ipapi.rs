//! ipapi.co transport: [HttpFetch] abstraction, reqwest implementation,
//! response parsing and retry/backoff logic.
use std::net::IpAddr;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;

use crate::provider::{GeoClientConfig, GeoError, GeoInfo};

/// User-Agent sent to ipapi.co.
pub const USER_AGENT: &str = "vpose/0.1";
/// Default per-request timeout.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

/// One HTTP GET response.
#[derive(Debug, Clone)]
pub struct HttpResponse {
    /// HTTP status code.
    pub status: u16,
    /// Response body as text.
    pub body: String,
}

/// Transport-level error (connect, timeout, body read).
#[derive(Debug, thiserror::Error)]
#[error("transport error: {0}")]
pub struct HttpError(pub String);

/// HTTP GET abstraction. Production uses reqwest; tests use scripted mocks.
/// No test may open a socket.
#[async_trait]
pub trait HttpFetch: Send + Sync {
    /// Perform a GET of `url`, returning status and body.
    async fn get(&self, url: &str) -> Result<HttpResponse, HttpError>;
}

/// Production [HttpFetch] over reqwest with rustls.
pub struct ReqwestFetch {
    client: reqwest::Client,
}

impl ReqwestFetch {
    /// Build the transport with a per-request timeout.
    pub fn new(timeout: Duration) -> Self {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .expect("reqwest client builder with valid defaults");
        Self { client }
    }
}

#[async_trait]
impl HttpFetch for ReqwestFetch {
    async fn get(&self, url: &str) -> Result<HttpResponse, HttpError> {
        let resp = self
            .client
            .get(url)
            .header(reqwest::header::USER_AGENT, USER_AGENT)
            .send()
            .await
            .map_err(|e| HttpError(e.to_string()))?;
        let status = resp.status().as_u16();
        let body = resp.text().await.map_err(|e| HttpError(e.to_string()))?;
        Ok(HttpResponse { status, body })
    }
}

/// Production [crate::provider::GeoProvider] implementation backed by
/// [ReqwestFetch]: ipapi.co with retries and backoff (caching lives in
/// [crate::GeoClient]).
pub struct IpApiCoAlias {
    http: ReqwestFetch,
    config: GeoClientConfig,
}

impl IpApiCoAlias {
    /// Build the production provider with the given config.
    pub fn new(config: GeoClientConfig) -> Self {
        Self {
            http: ReqwestFetch::new(config.timeout),
            config,
        }
    }
}

#[async_trait]
impl crate::provider::GeoProvider for IpApiCoAlias {
    async fn lookup(&self, ip: IpAddr) -> Result<GeoInfo, GeoError> {
        lookup_with_retries(&self.http, ip, &self.config).await
    }
}

/// Raw ipapi.co JSON payload. `languages` is a comma-separated string and may
/// be absent or null.
#[derive(Debug, Deserialize)]
struct IpApiBody {
    #[serde(default)]
    error: bool,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    country_code: Option<String>,
    #[serde(default)]
    timezone: Option<String>,
    #[serde(default)]
    languages: Option<String>,
}

/// Classification of one HTTP attempt: retryable or final.
enum Attempt {
    Retry(GeoError),
    Final(GeoError),
    Done(GeoInfo),
}

/// GET the url for `ip` with retries: up to `config.retries` attempts on top
/// of the first, backoff base * 2^n. Retried: transport errors, HTTP 429,
/// HTTP 5xx, reason "RateLimited". Not retried: other 4xx, Provider, Parse.
pub(crate) async fn lookup_with_retries(
    http: &dyn HttpFetch,
    ip: IpAddr,
    config: &GeoClientConfig,
) -> Result<GeoInfo, GeoError> {
    let url = format!("https://ipapi.co/{ip}/json/");
    let attempts = config.retries.saturating_add(1);
    let mut last_err: Option<GeoError> = None;
    for attempt in 0..attempts {
        if attempt > 0 {
            let delay = config.backoff_base * (1 << (attempt - 1));
            tokio::time::sleep(delay).await;
        }
        match one_attempt(http, &url).await {
            Attempt::Done(info) => return Ok(info),
            Attempt::Retry(err) => {
                tracing::warn!(attempt, error = %err, "geo lookup retryable failure");
                last_err = Some(err);
            }
            Attempt::Final(err) => return Err(err),
        }
    }
    Err(last_err.unwrap_or(GeoError::Http("no attempts made".to_string())))
}

/// One HTTP attempt + parse + classification.
async fn one_attempt(http: &dyn HttpFetch, url: &str) -> Attempt {
    let resp = match http.get(url).await {
        Ok(r) => r,
        Err(HttpError(msg)) => return Attempt::Retry(GeoError::Http(msg)),
    };
    if !(200..300).contains(&resp.status) {
        return match resp.status {
            429 => Attempt::Retry(GeoError::RateLimited),
            500..=599 => Attempt::Retry(GeoError::Status(resp.status)),
            _ => Attempt::Final(GeoError::Status(resp.status)),
        };
    }
    match parse_body(&resp.body) {
        Ok(info) => Attempt::Done(info),
        Err(e) => match e {
            GeoError::RateLimited => Attempt::Retry(GeoError::RateLimited),
            other => Attempt::Final(other),
        },
    }
}

/// Parse an ipapi.co JSON body into [GeoInfo].
fn parse_body(body: &str) -> Result<GeoInfo, GeoError> {
    let parsed: IpApiBody =
        serde_json::from_str(body).map_err(|e| GeoError::Parse(format!("invalid JSON: {e}")))?;
    if parsed.error {
        let reason = parsed.reason.unwrap_or_else(|| "unknown".to_string());
        return Err(match reason.as_str() {
            "RateLimited" => GeoError::RateLimited,
            _ => GeoError::Provider(reason),
        });
    }
    let country = parsed.country_code.unwrap_or_default();
    if country.len() != 2 || !country.bytes().all(|b| b.is_ascii_alphabetic()) {
        return Err(GeoError::Parse(format!(
            "invalid country_code: {country:?}"
        )));
    }
    let timezone = parsed.timezone.unwrap_or_default();
    if timezone.is_empty() {
        return Err(GeoError::Parse("empty timezone".to_string()));
    }
    let languages = parsed
        .languages
        .map(|s| {
            s.split(',')
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default();
    Ok(GeoInfo {
        country_code: country.to_uppercase(),
        timezone,
        languages,
    })
}

#[cfg(test)]
pub mod mock {
    //! Scripted mock transport for offline tests: a queue of responses plus a
    //! call counter. When the queue is exhausted, the last entry repeats.
    use super::*;
    use std::sync::Mutex;

    pub struct MockFetch {
        script: Mutex<Vec<Result<HttpResponse, HttpError>>>,
        next: std::sync::atomic::AtomicUsize,
        calls: std::sync::atomic::AtomicUsize,
    }

    impl MockFetch {
        /// New mock with a scripted sequence of responses.
        pub fn new(script: Vec<Result<HttpResponse, HttpError>>) -> Self {
            Self {
                script: Mutex::new(script),
                next: std::sync::atomic::AtomicUsize::new(0),
                calls: std::sync::atomic::AtomicUsize::new(0),
            }
        }

        /// Total number of GETs performed.
        pub fn calls(&self) -> usize {
            self.calls.load(std::sync::atomic::Ordering::Relaxed)
        }

        fn next_response(&self) -> Result<HttpResponse, HttpError> {
            let guard = self.script.lock().unwrap_or_else(|e| e.into_inner());
            let idx = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            match guard.get(idx).or_else(|| guard.last()) {
                Some(Ok(resp)) => Ok(HttpResponse {
                    status: resp.status,
                    body: resp.body.clone(),
                }),
                Some(Err(e)) => Err(HttpError(e.0.clone())),
                None => Err(HttpError("empty mock script".to_string())),
            }
        }
    }

    #[async_trait]
    impl HttpFetch for MockFetch {
        async fn get(&self, _url: &str) -> Result<HttpResponse, HttpError> {
            self.calls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.next_response()
        }
    }
}
