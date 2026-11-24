//! The only module tree allowed to hold `unsafe`: thin FFI wrappers for
//! console/session-end signal handlers, the single-instance mutex, the
//! hidden session window + message pump, and Shell helpers. Every unsafe
//! block carries a `// SAFETY:` comment. Business logic never lives here.

pub mod console;
pub mod scheduler;
pub mod session_window;
pub mod shell;
pub mod single_instance;
