//! A fake `claude` for the provider's tests.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use rustix::process::{Pid, test_kill_process_group};
use serde_json::{Value, json};
use tempfile::{TempDir, tempdir};

use super::error::Error;
use super::run::{Limits, Request, Thinking, request};
use crate::{Message, ProviderEvent, ProviderUsage, StreamResponse};

const OWNER_ONLY_MODE: &str = "700";
pub(super) const TOOL: &str = "read";
pub(super) const IDLE: Duration = Duration::from_secs(10);
/// Generous for a loaded machine. Only a test that waits out the whole
/// limit uses the short one.
pub(super) const EXIT_LIMIT: Duration = Duration::from_secs(30);
/// Only a failing test waits this long, so it is generous for a loaded
/// machine, where starting a child maki can take seconds.
pub(super) const WAIT: Duration = Duration::from_secs(30);
pub(super) const TEMP_BASE: &str = "tmp";
const PROC: &str = "/proc";
pub(super) const POLL: Duration = Duration::from_millis(10);
pub(super) const SYSTEM: &str = "You are maki.";
/// Only the conversation holds it, so finding it in argv means the
/// transcript went on the command line instead of stdin.
pub(super) const MARKER: &str = "ULTRAMARINE-41";
/// The fake's alias.
pub(super) const ALIAS: &str = "haiku";
/// The models the fake's account answer lists.
pub(super) const OPUS: &str = "claude-opus-5-5";
pub(super) const HAIKU: &str = "claude-haiku-4-5-20251001";
pub(super) const VERSION_ERROR: &str = "claude: cannot read the install\n";

/// Answers the handshake, then plays the scenario in its directory. It
/// reaches maki's handoff server through bash's `/dev/tcp`.
const FAKE: &str = include_str!("fake_claude.sh");

pub(super) struct Fake {
    pub(super) dir: TempDir,
    project: TempDir,
    pub(super) plan_usage: Mutex<Option<ProviderUsage>>,
}

impl Fake {
    /// `install` puts the script in place, so this process never opens it
    /// for writing. A child forked on another test thread could inherit
    /// that open file, and the script would then fail with "Text file
    /// busy".
    pub(super) fn new(scenario: &str) -> Self {
        let dir = tempdir().unwrap();
        let script = FAKE.replace("@DIR@", &dir.path().display().to_string());
        let source = dir.path().join("claude.sh");
        fs::write(&source, script).unwrap();
        let installed = process::Command::new("install")
            .args(["-m", OWNER_ONLY_MODE])
            .arg(&source)
            .arg(dir.path().join("claude"))
            .status()
            .unwrap();
        assert!(installed.success(), "install exited with an error");
        fs::write(dir.path().join("scenario"), scenario).unwrap();
        Self {
            dir,
            project: tempdir().unwrap(),
            plan_usage: Mutex::default(),
        }
    }

    pub(super) fn log(&self, name: &str) -> String {
        fs::read_to_string(self.dir.path().join(name)).unwrap_or_default()
    }

    pub(super) fn executable(&self) -> PathBuf {
        self.dir.path().join("claude")
    }

    pub(super) fn project(&self) -> PathBuf {
        self.project.path().canonicalize().unwrap()
    }

    pub(super) fn env() -> Vec<(String, String)> {
        ["PATH", "HOME"]
            .into_iter()
            .filter_map(|name| Some((name.to_owned(), env::var(name).ok()?)))
            .collect()
    }

    /// Runs one request with a `read` tool and a conversation holding the
    /// marker. Returns its result and the events it sent.
    pub(super) async fn request(&self) -> (Result<StreamResponse, Error>, Vec<ProviderEvent>) {
        self.request_with(
            &format!("find {MARKER}"),
            ALIAS,
            Limits {
                exit: EXIT_LIMIT,
                ..Limits::new(IDLE)
            },
        )
        .await
    }

    pub(super) async fn request_with(
        &self,
        prompt: &str,
        model: &str,
        limits: Limits,
    ) -> (Result<StreamResponse, Error>, Vec<ProviderEvent>) {
        self.send(prompt, model, Thinking::Default, limits).await
    }

    pub(super) async fn send(
        &self,
        prompt: &str,
        model: &str,
        thinking: Thinking,
        limits: Limits,
    ) -> (Result<StreamResponse, Error>, Vec<ProviderEvent>) {
        let messages = [Message::user(prompt.to_owned())];
        self.send_messages(&messages, model, thinking, limits).await
    }

    pub(super) async fn send_messages(
        &self,
        messages: &[Message],
        model: &str,
        thinking: Thinking,
        limits: Limits,
    ) -> (Result<StreamResponse, Error>, Vec<ProviderEvent>) {
        let tools = tools();
        let (events, received) = flume::unbounded();
        let project = self.project();
        let result = request(Request {
            executable: &self.executable(),
            env: &Self::env(),
            model,
            cwd: &project,
            system: SYSTEM,
            messages,
            tools: &tools,
            events: &events,
            plan_usage: &self.plan_usage,
            temp_dir: &self.temp_base(),
            max_output: None,
            thinking,
            limits: &limits,
            slot_wait: Duration::ZERO,
        })
        .await;
        (result, received.try_iter().collect())
    }

    /// Where the fake's requests make their private directories, apart
    /// from every other test's.
    pub(super) fn temp_base(&self) -> PathBuf {
        let base = self.dir.path().join(TEMP_BASE);
        fs::create_dir_all(&base).unwrap();
        base.canonicalize().unwrap()
    }

    /// A leader that exited and is not reaped stays in `/proc`.
    pub(super) fn leader_reaped(&self) -> bool {
        let pid = self.log("pid");
        !Path::new(PROC).join(pid.trim()).exists()
    }

    pub(super) fn group_gone(&self) -> bool {
        let pid: i32 = self.log("pid").trim().parse().unwrap();
        let pid = Pid::from_raw(pid).unwrap();
        wait_until(|| test_kill_process_group(pid).is_err())
    }
}

pub(super) fn wait_until(mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + WAIT;
    while Instant::now() < deadline {
        if done() {
            return true;
        }
        thread::sleep(POLL);
    }
    false
}

/// The only tool maki offers in these tests.
pub(super) fn tools() -> Value {
    json!([{ "name": TOOL, "description": "Read a file", "input_schema": { "type": "object" } }])
}
