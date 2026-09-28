//! End-to-end tests of the claude_code plugin: against a fake `claude`
//! (`policy`), its import command on an on-disk git checkout (`import`), and
//! live qualification of a Claude Code version (`qualify`, ignored). Tests
//! that change the whole process's environment live in `claude_code_env`.
//!
//! Linux only, because the plugin runs only on Linux and elsewhere never
//! starts the fake.
#![cfg(target_os = "linux")]

mod import;
mod policy;
mod qualify;
mod support;
