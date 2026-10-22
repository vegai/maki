use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

use tempfile::{TempDir, tempdir};

const PASSED: &[&str] = &["PATH", "USER", "LANG", "TERM"];
const USER_DIRS: &[(&str, &str)] = &[
    ("HOME", "home"),
    ("XDG_CONFIG_HOME", "config"),
    ("XDG_STATE_HOME", "state"),
    ("XDG_DATA_HOME", "data"),
    ("XDG_CACHE_HOME", "cache"),
];

pub struct Isolation(TempDir);

impl Isolation {
    pub fn new() -> Self {
        Self(tempdir().unwrap())
    }

    pub fn dir(&self, name: &str) -> PathBuf {
        let path = self.0.path().join(name);
        fs::create_dir_all(&path).unwrap();
        path.canonicalize().unwrap()
    }

    pub fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_maki"));
        command.current_dir(self.dir("project")).env_clear();
        for &(name, directory) in USER_DIRS {
            command.env(name, self.dir(directory));
        }
        for &name in PASSED {
            if let Some(value) = env::var_os(name) {
                command.env(name, value);
            }
        }
        command
    }
}
