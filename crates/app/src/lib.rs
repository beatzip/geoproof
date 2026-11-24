//! app: stage 6 monolith — tray + autostart + orchestration.
//!
//! `sys` is the only unsafe-capable tree; `tray` renders in the elevated
//! process (accepted compromise, see docs/specs/stage-06.md — stage 7
//! splits tray/service via a named pipe).

pub mod autostart;
pub mod country;
pub mod logging;
pub mod orchestrator;
pub mod shutdown;
pub mod tray;

#[cfg(windows)]
pub mod sys;
