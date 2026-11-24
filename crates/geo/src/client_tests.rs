//! Offline GeoClient tests: scripted MockFetch, no sockets. Retry tests run
//! under tokio::time::pause with the advance/yield pattern proven in the
//! stage-1 watcher harness (boxed async-trait futures need timer advances
//! to be polled).

use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::time::Duration;

use crate::ipapi::mock::MockFetch;
use crate::ipapi::{HttpError, HttpResponse};
use crate::provider::{GeoClientConfig, GeoError, GeoInfo, GeoProvider};
use crate::GeoClient;

fn ip() -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7))
}

fn body_json(cc: &str, tz: &str, langs: &str) -> String {
    format!(r#"{{"country_code":"{cc}","timezone":"{tz}","languages":"{langs}"}}"#)
}

fn ok_body(body: &str) -> Result<HttpResponse, HttpError> {
    Ok(HttpResponse {
        status: 200,
        body: body.to_string(),
    })
}

fn config(retries: u32) -> GeoClientConfig {
    GeoClientConfig {
        timeout: Duration::from_secs(5),
        retries,
        backoff_base: Duration::from_millis(500),
        cache_ttl: Duration::from_secs(24 * 60 * 60),
    }
}

fn expected() -> GeoInfo {
    GeoInfo {
        country_code: "FR".to_string(),
        timezone: "Europe/Paris".to_string(),
        languages: vec!["fr".to_string(), "en".to_string()],
    }
}

#[tokio::test]
async fn happy_path_parses_and_normalizes() {
    let mock = Arc::new(MockFetch::new(vec![ok_body(&body_json(
        "fr",
        "Europe/Paris",
        " fr , en , ",
    ))]));
    let client = GeoClient::new(config(3), mock.clone());
    let info = client.lookup(ip()).await.unwrap();
    assert_eq!(info, expected());
    assert_eq!(mock.calls(), 1);
}

#[tokio::test]
async fn transport_error_retries_then_succeeds() {
    let mock = Arc::new(MockFetch::new(vec![
        Err(HttpError("connect timeout".to_string())),
        ok_body(&body_json("fr", "Europe/Paris", "fr,en")),
    ]));
    let client = GeoClient::new(config(3), mock.clone());
    tokio::time::pause();
    let handle = tokio::spawn(async move { client.lookup(ip()).await });
    let mut info = None;
    for _ in 0..200 {
        tokio::time::advance(Duration::ZERO).await;
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(100)).await;
        if handle.is_finished() {
            info = Some(handle.await.unwrap());
            break;
        }
    }
    assert_eq!(info, Some(Ok(expected())));
    assert_eq!(mock.calls(), 2);
}

#[tokio::test]
async fn http_429_retries_then_succeeds() {
    let mock = Arc::new(MockFetch::new(vec![
        Ok(HttpResponse {
            status: 429,
            body: String::new(),
        }),
        ok_body(&body_json("fr", "Europe/Paris", "fr,en")),
    ]));
    let client = GeoClient::new(config(3), mock.clone());
    tokio::time::pause();
    let handle = tokio::spawn(async move { client.lookup(ip()).await });
    let mut info = None;
    for _ in 0..200 {
        tokio::time::advance(Duration::ZERO).await;
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(100)).await;
        if handle.is_finished() {
            info = Some(handle.await.unwrap());
            break;
        }
    }
    assert_eq!(info, Some(Ok(expected())));
    assert_eq!(mock.calls(), 2);
}

#[tokio::test]
async fn retries_exhausted_returns_rate_limited() {
    let mock = Arc::new(MockFetch::new(vec![
        Ok(HttpResponse {
            status: 429,
            body: String::new(),
        }),
        Ok(HttpResponse {
            status: 429,
            body: String::new(),
        }),
        Ok(HttpResponse {
            status: 429,
            body: String::new(),
        }),
        Ok(HttpResponse {
            status: 429,
            body: String::new(),
        }),
    ]));
    let client = GeoClient::new(config(3), mock.clone());
    tokio::time::pause();
    let handle = tokio::spawn(async move { client.lookup(ip()).await });
    let mut result = None;
    for _ in 0..200 {
        tokio::time::advance(Duration::ZERO).await;
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(100)).await;
        if handle.is_finished() {
            result = Some(handle.await.unwrap());
            break;
        }
    }
    assert_eq!(result, Some(Err(GeoError::RateLimited)));
    assert_eq!(mock.calls(), 4); // 1 initial + 3 retries
}

#[tokio::test]
async fn backoff_delays_are_half_second_doubling() {
    // 429 forever: total sleep must be 0.5 + 1 + 2 = 3.5 s under paused clock.
    let mock = Arc::new(MockFetch::new(vec![
        Ok(HttpResponse {
            status: 429,
            body: String::new(),
        }),
        Ok(HttpResponse {
            status: 429,
            body: String::new(),
        }),
        Ok(HttpResponse {
            status: 429,
            body: String::new(),
        }),
        Ok(HttpResponse {
            status: 429,
            body: String::new(),
        }),
    ]));
    let client = GeoClient::new(config(3), mock.clone());
    tokio::time::pause();
    let handle = tokio::spawn(async move { client.lookup(ip()).await });
    let mut advanced: Duration = Duration::ZERO;
    loop {
        tokio::time::advance(Duration::ZERO).await;
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(10)).await;
        advanced += Duration::from_millis(10);
        if handle.is_finished() {
            break;
        }
        assert!(advanced < Duration::from_secs(60), "test runaway");
    }
    let _ = handle.await;
    // 0.5 + 1 + 2 s = 3.5 s of scripted backoff; the poll loop may run one
    // extra 10 ms tick after the last sleep resolves.
    assert!(
        advanced >= Duration::from_millis(3500) && advanced < Duration::from_millis(3600),
        "backoff total {advanced:?} outside 3.5s..3.6s"
    );
}

#[tokio::test]
async fn other_4xx_is_not_retried() {
    let mock = Arc::new(MockFetch::new(vec![Ok(HttpResponse {
        status: 404,
        body: "nope".to_string(),
    })]));
    let client = GeoClient::new(config(3), mock.clone());
    let result = client.lookup(ip()).await;
    assert_eq!(result, Err(GeoError::Status(404)));
    assert_eq!(mock.calls(), 1);
}

#[tokio::test]
async fn malformed_json_is_parse_error_without_retry() {
    let mock = Arc::new(MockFetch::new(vec![ok_body("not json at all")]));
    let client = GeoClient::new(config(3), mock.clone());
    let result = client.lookup(ip()).await;
    assert!(matches!(result, Err(GeoError::Parse(_))));
    assert_eq!(mock.calls(), 1);
}

#[tokio::test]
async fn provider_error_body_with_http_200_no_retry() {
    let mock = Arc::new(MockFetch::new(vec![ok_body(
        r#"{"error":true,"reason":"Invalid IP"}"#,
    )]));
    let client = GeoClient::new(config(3), mock.clone());
    let result = client.lookup(ip()).await;
    assert_eq!(result, Err(GeoError::Provider("Invalid IP".to_string())));
    assert_eq!(mock.calls(), 1);
}

#[tokio::test]
async fn rate_limited_reason_is_retryable() {
    let mock = Arc::new(MockFetch::new(vec![
        ok_body(r#"{"error":true,"reason":"RateLimited"}"#),
        ok_body(&body_json("fr", "Europe/Paris", "fr,en")),
    ]));
    let client = GeoClient::new(config(3), mock.clone());
    tokio::time::pause();
    let handle = tokio::spawn(async move { client.lookup(ip()).await });
    let mut info = None;
    for _ in 0..200 {
        tokio::time::advance(Duration::ZERO).await;
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(100)).await;
        if handle.is_finished() {
            info = Some(handle.await.unwrap());
            break;
        }
    }
    assert_eq!(info, Some(Ok(expected())));
    assert_eq!(mock.calls(), 2);
}

#[tokio::test]
async fn missing_languages_is_empty_vec() {
    let mock = Arc::new(MockFetch::new(vec![ok_body(
        r#"{"country_code":"JP","timezone":"Asia/Tokyo"}"#,
    )]));
    let client = GeoClient::new(config(3), mock.clone());
    let info = client.lookup(ip()).await.unwrap();
    assert_eq!(info.languages, Vec::<String>::new());
    assert_eq!(info.country_code, "JP");
}

#[tokio::test]
async fn invalid_country_code_is_parse_error() {
    let mock = Arc::new(MockFetch::new(vec![ok_body(
        r#"{"country_code":"FRA","timezone":"Europe/Paris"}"#,
    )]));
    let client = GeoClient::new(config(3), mock.clone());
    let result = client.lookup(ip()).await;
    assert!(matches!(result, Err(GeoError::Parse(_))));
}

#[tokio::test]
async fn empty_timezone_is_parse_error() {
    let mock = Arc::new(MockFetch::new(vec![ok_body(
        r#"{"country_code":"FR","timezone":""}"#,
    )]));
    let client = GeoClient::new(config(3), mock.clone());
    let result = client.lookup(ip()).await;
    assert!(matches!(result, Err(GeoError::Parse(_))));
}

#[tokio::test]
async fn cache_hit_makes_single_http_call() {
    let mock = Arc::new(MockFetch::new(vec![ok_body(&body_json(
        "fr",
        "Europe/Paris",
        "fr,en",
    ))]));
    let client = GeoClient::new(config(3), mock.clone());
    let first = client.lookup(ip()).await.unwrap();
    let second = client.lookup(ip()).await.unwrap();
    assert_eq!(first, second);
    assert_eq!(mock.calls(), 1);
}

#[tokio::test]
async fn ttl_expiry_refetches_under_paused_clock() {
    let mock = Arc::new(MockFetch::new(vec![ok_body(&body_json(
        "fr",
        "Europe/Paris",
        "fr,en",
    ))]));
    let client = Arc::new(GeoClient::new(config(3), mock.clone()));
    tokio::time::pause();
    let handle = tokio::spawn({
        let client = Arc::clone(&client);
        async move { client.lookup(ip()).await }
    });
    for _ in 0..50 {
        tokio::time::advance(Duration::ZERO).await;
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(100)).await;
        if handle.is_finished() {
            handle.await.unwrap().unwrap();
            break;
        }
    }
    // Advance past the 24 h TTL.
    tokio::time::advance(Duration::from_secs(25 * 60 * 60)).await;
    tokio::task::yield_now().await;
    let second = client.lookup(ip()).await.unwrap();
    assert_eq!(second.timezone, "Europe/Paris");
    assert_eq!(mock.calls(), 2);
}

#[tokio::test]
async fn failures_are_not_cached() {
    let mock = Arc::new(MockFetch::new(vec![
        ok_body(r#"{"error":true,"reason":"Invalid IP"}"#),
        ok_body(&body_json("fr", "Europe/Paris", "fr,en")),
    ]));
    let client = GeoClient::new(config(3), mock.clone());
    assert!(client.lookup(ip()).await.is_err());
    let info = client.lookup(ip()).await.unwrap();
    assert_eq!(info.country_code, "FR");
    assert_eq!(mock.calls(), 2);
}

/// End-to-end: a GeoProvider mock producing Europe/Paris maps to the Windows
/// name via iana_to_windows.
#[tokio::test]
async fn end_to_end_paris_to_romance() {
    struct Fixed;
    #[async_trait::async_trait]
    impl GeoProvider for Fixed {
        async fn lookup(&self, _ip: IpAddr) -> Result<GeoInfo, GeoError> {
            Ok(GeoInfo {
                country_code: "FR".to_string(),
                timezone: "Europe/Paris".to_string(),
                languages: vec!["fr".to_string()],
            })
        }
    }
    let info = Fixed.lookup(ip()).await.unwrap();
    assert_eq!(
        crate::tz_windows::iana_to_windows(&info.timezone),
        Some("Romance Standard Time")
    );
}
