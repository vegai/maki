//! Linux-only plugin tests use a fake CLI and real filesystem imports. Live qualification
//! tests are ignored by default.
#![cfg(target_os = "linux")]

mod import;
mod policy;
mod qualify;
mod support;
