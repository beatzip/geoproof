//! Windows read smoke test. Ignored by default: it touches the real system
//! state (read-only). Run explicitly with:
//!   cargo test -p snapshot --test windows_smoke -- --ignored
#![cfg(windows)]

use snapshot::traits::StateReader;
use snapshot::WindowsStateReader;

#[test]
#[ignore = "reads real Windows state; run manually"]
fn windows_read_returns_all_fields() {
    let reader = WindowsStateReader;
    let values = reader.read_all().expect("read_all must succeed on Windows");
    assert_eq!(values.len(), 4);
    for (id, value) in values {
        let json = serde_json::to_string(&value).expect("serializable");
        println!("{id}: {json}");
    }
}
