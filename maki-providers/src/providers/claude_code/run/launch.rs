use super::super::checks;
use super::super::error::Error;
use super::super::settings;
use super::super::stream::{
    CONTROL_RESPONSE, Handshake, INITIALIZE, SUCCESS, Step, control_request, parse_event,
};
use super::super::transcript::SERVER;
use super::private_files::private_dir_async;
use super::supervision::{
    Group, Next, next, send, send_handshake, stdout_lines, supervised, unreadable,
};
use super::{Limits, Listed};
use crate::ProviderUsage;
use futures::future::join_all;
use futures_lite::Stream;
use serde_json::{Value, json};
use smol::process::ChildStdin;
use std::collections::{HashMap, HashSet};
use std::env;
use std::ffi::OsStr;
use std::fs::Metadata;
use std::io;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::{LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime};
use tracing::warn;

const WINDOW_PROCESSES: usize = 4;
const SET_MODEL: &str = "set_model";
const CONTEXT_USAGE: &str = "get_context_usage";
const PROBE_MODEL: &str = "haiku";
const VERSION_FLAG: &str = "--version";
const PARTIAL_MESSAGES_FLAG: &str = "--include-partial-messages";
pub(super) const MCP_CONFIG_FLAG: &str = "--mcp-config";

pub(crate) struct Launch<'a> {
    pub executable: &'a Path,
    pub env: &'a [(String, String)],
    pub project: &'a Path,
    pub denied: &'a [PathBuf],
    pub temp_dir: &'a Path,
    pub limits: &'a Limits,
}

#[derive(Clone, PartialEq)]
struct ExecutableIdentity {
    length: u64,
    modified: Option<SystemTime>,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    changed: (i64, i64),
}

impl ExecutableIdentity {
    fn new(metadata: &Metadata) -> Self {
        Self {
            length: metadata.len(),
            modified: metadata.modified().ok(),
            #[cfg(unix)]
            inode: metadata.ino(),
            #[cfg(unix)]
            device: metadata.dev(),
            #[cfg(unix)]
            changed: (metadata.ctime(), metadata.ctime_nsec()),
        }
    }
}

static PROFILE_CACHE: LazyLock<Mutex<HashMap<PathBuf, (ExecutableIdentity, String)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub fn invalidate_profile(executable: &Path) {
    PROFILE_CACHE
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .remove(executable);
}

pub(super) fn base_args(model: &str) -> Vec<String> {
    let mut settings = settings::base_settings();
    settings[checks::AUTO_COMPACT] = json!(false);
    settings["permissions"] = json!({ "allow": [format!("mcp__{SERVER}")] });
    let mut args = settings::base_args(model, "", checks::DEFAULT_MODE, &settings.to_string());
    args.extend([PARTIAL_MESSAGES_FLAG, "--max-turns", "1"].map(str::to_owned));
    args
}

pub(super) fn command(
    executable: &Path,
    env: &[(String, String)],
    cwd: &Path,
    args: &[impl AsRef<OsStr>],
) -> process::Command {
    let mut command = process::Command::new(executable);
    command
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .envs(env.iter().map(|(name, value)| (name, value)));
    #[cfg(unix)]
    command.process_group(0);
    command
}

/// Runs `work` on a Claude Code with no prompt and no MCP server, in an empty
/// directory checked to be outside the project.
async fn unprompted<T>(
    launch: &Launch<'_>,
    work: impl AsyncFnOnce(&mut Group) -> Result<T, Error>,
) -> Result<T, Error> {
    let (dir, dir_path) = private_dir_async(launch.temp_dir, launch.project, launch.denied).await?;
    let mut args = base_args(PROBE_MODEL);
    args.extend([
        MCP_CONFIG_FLAG.to_owned(),
        settings::EMPTY_MCP_CONFIG.to_owned(),
    ]);
    let result = supervised(
        command(launch.executable, launch.env, &dir_path, &args),
        async |group| work(group).await,
    )
    .await;
    dir.close().await;
    result
}

/// A hook that policy keeps on runs at startup, so the handshake first runs
/// in an empty directory, without a prompt.
pub(super) async fn probe(
    launch: &Launch<'_>,
    plan_usage: &Mutex<Option<ProviderUsage>>,
) -> Result<Handshake, Error> {
    unprompted(launch, async |group| {
        let mut stdin = group.child.stdin.take();
        let mut lines = stdout_lines(group.child.stdout.take())?;
        let mut handshake = Handshake::default();
        let deadline = Instant::now() + launch.limits.startup;
        send_handshake(&mut stdin, deadline).await?;
        drop(stdin);
        let mut passed = false;
        loop {
            match next(&mut lines, None, deadline).await {
                Next::Line(Some(Ok(line))) => {
                    let event = parse_event(&line)?;
                    match handshake.feed(&event, plan_usage, false)? {
                        Step::Ready => passed = true,
                        Step::Nothing | Step::Alive => {}
                        _ => return Err(Error::UncheckedStart),
                    }
                }
                Next::Line(Some(Err(source))) => return Err(unreadable(source)),
                Next::Line(None) => break,
                Next::Handoff(_) | Next::Late => {
                    return Err(Error::ProbeLate(launch.limits.startup.as_secs()));
                }
            }
        }
        let status = group.wait(deadline).await?;
        if !status.success() {
            return Err(Error::ExitedInChecks(status));
        }
        if !passed {
            return Err(Error::ChecksUnanswered);
        }
        Ok(handshake)
    })
    .await
}

async fn executable_identity(executable: &Path) -> Result<ExecutableIdentity, Error> {
    let metadata = smol::fs::metadata(executable)
        .await
        .map_err(|source| Error::Path {
            what: "inspect Claude Code",
            path: executable.to_owned(),
            source,
        })?;
    Ok(ExecutableIdentity::new(&metadata))
}

pub struct VersionCheck {
    executable: PathBuf,
    identity: ExecutableIdentity,
    pub version: Option<String>,
}

impl VersionCheck {
    fn remember(&self, version: &str) {
        PROFILE_CACHE
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(
                self.executable.clone(),
                (self.identity.clone(), version.to_owned()),
            );
    }

    pub fn save(&self, output: &str) -> Result<String, String> {
        let profile =
            checks::profile(output, env::consts::OS).map_err(|error| error.to_string())?;
        self.remember(&profile);
        Ok(profile)
    }
}

pub(crate) async fn version_check(executable: &Path) -> Result<VersionCheck, Error> {
    let identity = executable_identity(executable).await?;
    let version = PROFILE_CACHE
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(executable)
        .filter(|(saved, _)| *saved == identity)
        .map(|(_, version)| version.clone());
    Ok(VersionCheck {
        executable: executable.to_owned(),
        identity,
        version,
    })
}

pub(super) async fn checked_profile(launch: &Launch<'_>) -> Result<String, Error> {
    let check = version_check(launch.executable).await?;
    if let Some(version) = &check.version {
        return Ok(version.clone());
    }
    let (dir, dir_path) = private_dir_async(launch.temp_dir, launch.project, launch.denied).await?;
    let output = version(
        launch.executable,
        launch.env,
        &dir_path,
        launch.limits.startup,
    )
    .await;
    dir.close().await;
    let profile = checks::profile(&output?, env::consts::OS)?;
    check.remember(&profile);
    Ok(profile)
}

/// Returns the models by the ids they run, after the same checks a request
/// must pass.
pub(crate) async fn models(
    launch: &Launch<'_>,
    plan_usage: &Mutex<Option<ProviderUsage>>,
) -> Result<Vec<Listed>, Error> {
    checked_profile(launch).await?;
    let offered = probe(launch, plan_usage).await?;
    let mut seen = HashSet::new();
    let ids: Vec<_> = offered
        .models()
        .map(str::to_owned)
        .filter(|id| seen.insert(id.clone()))
        .collect();
    let chunk_size = ids.len().div_ceil(WINDOW_PROCESSES).max(1);
    let chunks = ids.chunks(chunk_size).map(async |models| {
        let windows = windows(launch, models).await.unwrap_or_else(|error| {
            warn!(%error, "claude-code: maki cannot read the context windows");
            vec![None; models.len()]
        });
        models
            .iter()
            .cloned()
            .zip(windows)
            .map(|(id, window)| Listed { id, window })
            .collect::<Vec<_>>()
    });
    Ok(join_all(chunks).await.into_iter().flatten().collect())
}

/// Returns the context window Claude Code opens for each name in `models`,
/// which is 1M for a new model even without `[1m]`. One process does it,
/// without a prompt, in an empty directory like the probe. The result is
/// `None` where the answer names a different model.
async fn windows(launch: &Launch<'_>, models: &[String]) -> Result<Vec<Option<u32>>, Error> {
    unprompted(launch, async |group| {
        let mut stdin = group.child.stdin.take();
        let mut lines = stdout_lines(group.child.stdout.take())?;
        let deadline = Instant::now() + launch.limits.startup;
        let initialize = json!({ "subtype": INITIALIZE });
        ask(&mut stdin, &mut lines, &initialize, deadline).await?;
        let mut windows = Vec::with_capacity(models.len());
        for model in models {
            let switch = json!({ "subtype": SET_MODEL, "model": model });
            ask(&mut stdin, &mut lines, &switch, deadline).await?;
            let usage = json!({ "subtype": CONTEXT_USAGE });
            let usage = ask(&mut stdin, &mut lines, &usage, deadline).await?;
            let window = usage["maxTokens"]
                .as_u64()
                .and_then(|n| u32::try_from(n).ok());
            windows.push(window.filter(|_| usage["model"] == model.as_str()));
        }
        Ok(windows)
    })
    .await
}

/// Sends `request` and waits for its answer. Requests go one at a time,
/// because Claude Code can answer a later request first. Other lines must
/// parse as JSON before the wait continues.
async fn ask(
    stdin: &mut Option<ChildStdin>,
    lines: &mut (impl Stream<Item = io::Result<String>> + Unpin),
    request: &Value,
    deadline: Instant,
) -> Result<Value, Error> {
    let id = request["subtype"].as_str().unwrap_or_default();
    send(stdin, &control_request(id, request), deadline).await?;
    loop {
        match next(lines, None, deadline).await {
            Next::Line(Some(Ok(line))) => {
                let event = parse_event(&line)?;
                let answer = &event["response"];
                if event["type"] == CONTROL_RESPONSE && answer["request_id"] == id {
                    return match answer["subtype"] == SUCCESS {
                        true => Ok(answer["response"].clone()),
                        false => Err(Error::NotAnswered(id.to_owned())),
                    };
                }
            }
            Next::Line(Some(Err(source))) => return Err(unreadable(source)),
            Next::Line(None) | Next::Handoff(_) | Next::Late => {
                return Err(Error::NotAnswered(id.to_owned()));
            }
        }
    }
}

async fn version(
    executable: &Path,
    env: &[(String, String)],
    temp_dir: &Path,
    startup: Duration,
) -> Result<String, Error> {
    let args = [VERSION_FLAG.to_owned()];
    supervised(command(executable, env, temp_dir, &args), async |group| {
        drop(group.child.stdin.take());
        let mut lines = stdout_lines(group.child.stdout.take())?;
        let deadline = Instant::now() + startup;
        let first = match next(&mut lines, None, deadline).await {
            Next::Line(Some(Ok(line))) => Some(line),
            Next::Line(Some(Err(source))) => return Err(Error::UnreadableVersion(source)),
            _ => None,
        };
        let status = group.wait(deadline).await?;
        if !status.success() {
            return Err(Error::VersionFailed(status));
        }
        first.ok_or(Error::NoVersion)
    })
    .await
}
