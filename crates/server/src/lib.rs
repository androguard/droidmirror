//! Device server library.
//!
//! Host builds compile the session, key map, and uinput event sequences.
//! Capture, the abstract socket, and `/dev/uinput` are Android-only.

pub mod annexb;
pub mod control;
pub mod inject;
pub mod keys;

#[cfg(target_os = "android")]
mod android;

#[cfg(target_os = "android")]
pub use android::exec_via_app_process;
