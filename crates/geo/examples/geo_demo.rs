//! Manual live check: resolves the geo data for a public IP and prints the
//! Windows timezone name. Makes a real network request — run by hand only,
//! never in tests.
use std::net::Ipv4Addr;

use geo::tz_windows::iana_to_windows;
use geo::{GeoClient, GeoClientConfig, GeoProvider};

#[tokio::main]
async fn main() {
    let ip = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| std::net::IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)));
    let client = GeoClient::with_reqwest(GeoClientConfig::default());
    match client.lookup(ip).await {
        Ok(info) => {
            println!("country: {}", info.country_code);
            println!("iana tz: {}", info.timezone);
            println!("languages: {}", info.languages.join(", "));
            println!("windows tz: {:?}", iana_to_windows(&info.timezone));
        }
        Err(e) => {
            eprintln!("lookup failed: {e}");
            std::process::exit(1);
        }
    }
}
