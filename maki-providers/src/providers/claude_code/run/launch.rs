use super::super::checks::{self, InitExpect, Profile};
use super::super::error::Error;
use super::super::stream::{
    CONTROL_RESPONSE, INITIALIZE, Offered, SUCCESS, Step, Turn, control_request,
};
use super::super::transcript::{Catalog, SERVER};
use super::Listed;
use super::private_files::private_dir;
use super::supervision::{
    Group, Next, next, send, send_handshake, stdout_lines, supervised, unreadable,
};
use crate::ProviderUsage;
use futures::future::join_all;
use futures_lite::Stream;
use serde_json::{Value, json};
use smol::process::ChildStdin;
use std::collections::HashMap;
use std::env;
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
pub(super) const MCP_CONFIG_FLAG: &str = "--mcp-config";
const SETTINGS: &str = r#"{"disableAllHooks":true,"autoMemoryEnabled":false,"autoCompactEnabled":false,"claudeMdExcludes":["**"],"disableClaudeAiConnectors":true,"permissions":{"allow":["mcp__maki"]}}"#;
const EMPTY_MCP_CONFIG: &str = r#"{"mcpServers":{}}"#;

pub(crate) struct Launch<'a> {
    pub executable: &'a Path,
    pub env: &'a [(String, String)],
    pub project: &'a Path,
    pub temp_dir: &'a Path,
    pub startup: Duration,
}

#[derive(PartialEq)]
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

static PROFILE_CACHE: LazyLock<Mutex<HashMap<PathBuf, (ExecutableIdentity, Profile)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub(crate) fn invalidate_profile(executable: &Path) {
    PROFILE_CACHE
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .remove(executable);
}

pub(super) fn base_args(model: &str) -> Vec<String> {
    [
        "--print",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--include-partial-messages",
        "--no-session-persistence",
        "--restricted",
        "--tools",
        "",
        "--max-turns",
        "1",
        "--permission-mode",
        "default",
        "--permission-prompts",
        "none",
        "--setting-sources",
        "",
        "--settings",
        SETTINGS,
        "--strict-mcp-config",
        "--model",
        model,
    ]
    .map(str::to_owned)
    .to_vec()
}

pub(super) fn command(
    executable: &Path,
    env: &[(String, String)],
    cwd: &Path,
    args: &[String],
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
    work: impl AsyncFnOnce(&mut Group, &Path) -> Result<T, Error>,
) -> Result<T, Error> {
    let (_dir, dir_path) = private_dir(launch.temp_dir, launch.project)?;
    let mut args = base_args(PROBE_MODEL);
    args.extend([MCP_CONFIG_FLAG.to_owned(), EMPTY_MCP_CONFIG.to_owned()]);
    supervised(
        command(launch.executable, launch.env, &dir_path, &args),
        async |group| work(group, &dir_path).await,
    )
    .await
}

/// A hook that policy keeps on runs at startup, so the handshake first runs
/// in an empty directory, without a prompt.
pub(super) async fn probe(
    launch: &Launch<'_>,
    profile: &Profile,
    plan_usage: &Mutex<Option<ProviderUsage>>,
) -> Result<Vec<Offered>, Error> {
    let empty = Catalog::new(&json!([]))?;
    unprompted(launch, async |group, dir| {
        let mut stdin = group.child.stdin.take();
        let mut lines = stdout_lines(group.child.stdout.take())?;
        let tools = empty.exposed();
        let expect = InitExpect {
            profile,
            model: PROBE_MODEL,
            cwd: dir,
            server: SERVER,
            tools: &tools,
        };
        let mut turn = Turn::new(&empty, expect, plan_usage);
        let deadline = Instant::now() + launch.startup;
        send_handshake(&mut stdin, deadline).await?;
        drop(stdin);
        let mut passed = false;
        loop {
            match next(&mut lines, None, deadline).await {
                Next::Line(Some(Ok(line))) => match turn.feed(&line)? {
                    Step::Ready => passed = true,
                    Step::Nothing | Step::Alive => {}
                    _ => return Err(Error::ProbeRan),
                },
                Next::Line(Some(Err(source))) => return Err(unreadable(source)),
                Next::Line(None) => break,
                Next::Handoff(_) | Next::Late => {
                    return Err(Error::ProbeLate(launch.startup.as_secs()));
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
        Ok(turn.offered())
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

pub(crate) async fn cached_profile(executable: &Path) -> Result<Option<Profile>, Error> {
    let identity = executable_identity(executable).await?;
    Ok(PROFILE_CACHE
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(executable)
        .filter(|(saved, _)| *saved == identity)
        .map(|(_, profile)| profile.clone()))
}

pub(crate) async fn cache_profile(executable: &Path, profile: Profile) -> Result<(), Error> {
    let identity = executable_identity(executable).await?;
    PROFILE_CACHE
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(executable.to_owned(), (identity, profile));
    Ok(())
}

pub(super) async fn checked_profile(launch: &Launch<'_>) -> Result<Profile, Error> {
    if let Some(profile) = cached_profile(launch.executable).await? {
        return Ok(profile);
    }
    let (_dir, dir_path) = private_dir(launch.temp_dir, launch.project)?;
    let output = version(launch.executable, launch.env, &dir_path, launch.startup).await?;
    let profile = checks::profile(&output, env::consts::OS)?;
    cache_profile(launch.executable, profile.clone()).await?;
    Ok(profile)
}

/// Returns the models by the ids they run, after the same checks a request
/// must pass.
pub(crate) async fn models(
    executable: &Path,
    env: &[(String, String)],
    project: &Path,
    temp_dir: &Path,
    plan_usage: &Mutex<Option<ProviderUsage>>,
    startup: Duration,
) -> Result<Vec<Listed>, Error> {
    let launch = Launch {
        executable,
        env,
        project,
        temp_dir,
        startup,
    };
    let profile = checked_profile(&launch).await?;
    let offered = probe(&launch, &profile, plan_usage).await?;
    let mut ids: Vec<String> = Vec::new();
    for offer in offered {
        if !ids.contains(&offer.model) {
            ids.push(offer.model);
        }
    }
    // Each model switch can take Claude Code 2 s, so several processes split
    // the list. An unknown window falls back to the standard one, so the list
    // stays correct without it.
    let shares: Vec<&[String]> = ids
        .chunks(ids.len().div_ceil(WINDOW_PROCESSES).max(1))
        .collect();
    let answers = join_all(shares.iter().map(|share| windows(&launch, share))).await;
    let windows: Vec<Option<u32>> = shares
        .iter()
        .zip(answers)
        .flat_map(|(share, answer)| {
            answer.unwrap_or_else(|error| {
                warn!(%error, "claude-code: maki cannot read the context windows");
                vec![None; share.len()]
            })
        })
        .collect();
    Ok(ids
        .into_iter()
        .zip(windows)
        .map(|(id, window)| Listed { id, window })
        .collect())
}

/// Returns the context window Claude Code opens for each name in `models`,
/// which is 1M for a new model even without `[1m]`. One process does it,
/// without a prompt, in an empty directory like the probe. The result is
/// `None` where the answer names a different model.
async fn windows(launch: &Launch<'_>, models: &[String]) -> Result<Vec<Option<u32>>, Error> {
    unprompted(launch, async |group, _| {
        let mut stdin = group.child.stdin.take();
        let mut lines = stdout_lines(group.child.stdout.take())?;
        let deadline = Instant::now() + launch.startup;
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
/// because Claude Code can answer a later request first. Other lines are
/// ignored, because this process has no prompt and its events do not matter.
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
                let event: Value = serde_json::from_str(&line).unwrap_or_default();
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
