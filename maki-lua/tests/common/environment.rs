use std::env;
use std::ffi::{OsStr, OsString};
use std::iter;
use std::path::Path;

const PATH: &str = "PATH";
const NEXTEST: &str = "NEXTEST";

pub fn require_nextest() {
    assert!(
        env::var_os(NEXTEST).is_some(),
        "these tests change the process environment, so run them with cargo nextest, which gives each test its own process"
    );
}

pub fn set(name: &str, value: impl AsRef<OsStr>) {
    require_nextest();
    // SAFETY: nextest isolates each test in its own process. Changes precede plugin threads.
    unsafe { env::set_var(name, value) };
}

pub fn remove(name: &str) {
    require_nextest();
    // SAFETY: the same isolated-process contract as set.
    unsafe { env::remove_var(name) };
}

pub struct EnvVar {
    name: &'static str,
    before: Option<OsString>,
}

impl EnvVar {
    pub fn prepend_path(dir: &Path) -> Self {
        let current = env::var_os(PATH).unwrap_or_default();
        let entries = iter::once(dir.to_path_buf()).chain(env::split_paths(&current));
        Self::set(PATH, env::join_paths(entries).unwrap())
    }

    pub fn set(name: &'static str, value: impl AsRef<OsStr>) -> Self {
        let before = env::var_os(name);
        set(name, value);
        Self { name, before }
    }
}

impl Drop for EnvVar {
    fn drop(&mut self) {
        match &self.before {
            Some(value) => set(self.name, value),
            None => remove(self.name),
        }
    }
}
