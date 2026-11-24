//! Thin Windows FFI wrappers - the ONLY place where unsafe is allowed in
//! this crate. Every unsafe block carries a // SAFETY: comment. No business
//! logic here: call the API, convert, return.

#![cfg(windows)]
#![allow(unsafe_code)]

pub mod registry;
pub mod registry_locale;
pub mod service;
pub mod time;
pub mod winapi;
pub mod wm;

pub use registry::WindowsRegistry;
pub use registry_locale::WindowsLocaleRegistry;
pub use service::WindowsServiceController;
pub use time::{current_system_date, SystemDate};
pub use winapi::WindowsAdminCheck;
pub use winapi::WindowsTzSetter;
pub use wm::WindowsSettingChangeBroadcast;
