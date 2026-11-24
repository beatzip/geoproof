//! Read-only demo: capture the current system state and print it as JSON.
//! Never writes to the system and never persists anything.

use snapshot::engine::Engine;
use snapshot::store::Store;
use snapshot::sys::WindowsStateReader;
use snapshot::traits::StateReader;

fn main() {
    let reader = WindowsStateReader;
    match reader.read_all() {
        Ok(values) => {
            let mut obj = serde_json::Map::new();
            for (id, value) in values {
                obj.insert(
                    id.as_str().to_string(),
                    serde_json::to_value(&value).expect("field json"),
                );
            }
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::Value::Object(obj)).expect("json")
            );
        }
        Err(e) => {
            eprintln!("failed to read system state: {e}");
            std::process::exit(1);
        }
    }
    let _ = Engine::new(Store::new(std::env::temp_dir().join("unused")));
}
