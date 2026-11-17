//! Linux-only plugin tests use a fake CLI and real filesystem imports. Live qualification
//! tests are ignored by default.
#![cfg(target_os = "linux")]

#[path = "../common/mod.rs"]
mod common;
mod environment;
mod import;
mod policy;
mod qualify;
mod support;
