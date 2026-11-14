use super::super::checks::{self, handshake_input};
use super::super::error::Error;
use super::super::settings::{self, MCP_CONFIG_FLAG};
use super::super::stream::{
    CONTROL_RESPONSE, Handshake, INITIALIZE, SUCCESS, Step, control_request, parse_event,
};
use super::super::transcript::SERVER;
use super::private_files::private_dir_async;
use super::supervision::{Group, Next, ReadDeadline, send, supervised, unreadable};
use super::{Limits, Listed};
use crate::ProviderUsage;
use futures::future::join_all;
use futures::{Stream, future};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::Metadata;
use std::future::Future;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{self, ExitStatus};
use std::sync::{LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime};
use tokio::process::ChildStdin;
use tracing::warn;

const WINDOW_PROCESSES: usize = 4;
const SET_MODEL: &str = "set_model";
const CONTEXT_USAGE: &str = "get_context_usage";
const PROBE_MODEL: &str = "haiku";
const VERSION_FLAG: &str = "--version";
const MAX_TURNS_FLAG: &str = "--max-turns";
const TURN_LIMIT: &str = "1";
const PARTIAL_MESSAGES_FLAG: &str = "--include-partial-messages";

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
    inode: u64,
    device: u64,
    changed: (i64, i64),
}

impl ExecutableIdentity {
    fn new(metadata: &Metadata) -> Self {
        Self {
            length: metadata.len(),
            modified: metadata.modified().ok(),
            inode: metadata.ino(),
            device: metadata.dev(),
            changed: (metadata.ctime(), metadata.ctime_nsec()),
        }
    }
}

static VERSION_CACHE: LazyLock<Mutex<HashMap<PathBuf, (ExecutableIdentity, String)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub fn invalidate_version(executable: &Path) {
    VERSION_CACHE
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .remove(executable);
}

pub(super) fn base_args(model: &str) -> Vec<String> {
    let mut settings = settings::base_settings();
    settings[checks::AUTO_COMPACT] = json!(false);
    settings[checks::PERMISSIONS_KEY] = json!({ "allow": [format!("mcp__{SERVER}")] });
    let mut args = settings::base_args(model, "", checks::DEFAULT_MODE, &settings.to_string());
    args.extend([PARTIAL_MESSAGES_FLAG, MAX_TURNS_FLAG, TURN_LIMIT].map(str::to_owned));
    args
}

pub(crate) fn command(
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
    command
}

/// Runs `work` on a Claude Code with no prompt and no MCP server, in an empty
/// directory checked to be outside the project.
async fn unprompted<T>(
    launch: &Launch<'_>,
    args: &[String],
    work: impl AsyncFnOnce(&mut Group) -> Result<T, Error>,
) -> Result<T, Error> {
    let (dir, dir_path) = private_dir_async(launch.temp_dir, launch.project, launch.denied).await?;
    let result = supervised(
        command(launch.executable, launch.env, &dir_path, args),
        work,
    )
    .await;
    dir.close().await;
    result
}

fn probe_args() -> Vec<String> {
    let mut args = base_args(PROBE_MODEL);
    args.extend([
        MCP_CONFIG_FLAG.to_owned(),
        settings::EMPTY_MCP_CONFIG.to_owned(),
    ]);
    args
}

pub(crate) async fn probe_with(
    group: &mut Group,
    startup: Duration,
    mut feed: impl FnMut(&Value) -> Result<Step, Error>,
) -> Result<(), Error> {
    let (mut stdin, mut lines) = group.pipes()?;
    let deadline = Instant::now() + startup;
    if let Err(error) = send(&mut stdin, &handshake_input(), deadline).await
        && !matches!(error.cause(), Error::Io { source, .. } if source.kind() == io::ErrorKind::BrokenPipe)
    {
        return Err(error);
    }
    drop(stdin);
    let mut passed = false;
    let mut timer = ReadDeadline::new(deadline);
    loop {
        match timer.next(&mut lines, None, deadline).await {
            Next::Line(Some(Ok(line))) => match feed(&parse_event(&line)?)? {
                Step::Ready => passed = true,
                Step::Nothing | Step::Alive => {}
                _ => return Err(Error::UncheckedStart),
            },
            Next::Line(Some(Err(source))) => return Err(unreadable(source)),
            Next::Line(None) => break,
            Next::Handoff(_) | Next::Late => return Err(Error::ProbeLate(startup.as_secs())),
        }
    }
    probe_exit(group.wait(deadline).await?, passed)
}

fn probe_exit(status: ExitStatus, complete: bool) -> Result<(), Error> {
    if status.code().is_none() {
        return Err(Error::StoppedInChecks);
    }
    if !status.success() {
        return Err(Error::ExitedInChecks(status));
    }
    if !complete {
        return Err(Error::ChecksUnanswered);
    }
    Ok(())
}

pub(super) async fn probe(
    launch: &Launch<'_>,
    plan_usage: &Mutex<Option<ProviderUsage>>,
) -> Result<Handshake, Error> {
    let mut handshake = Handshake::default();
    unprompted(launch, &probe_args(), async |group| {
        probe_with(group, launch.limits.startup, |event| {
            handshake.feed(event, plan_usage)
        })
        .await
    })
    .await?;
    Ok(handshake)
}

async fn executable_identity(executable: &Path) -> Result<ExecutableIdentity, Error> {
    let metadata = tokio::fs::metadata(executable)
        .await
        .map_err(Error::path("inspect Claude Code", executable))?;
    Ok(ExecutableIdentity::new(&metadata))
}

pub(crate) async fn checked_profile(
    launch: &Launch<'_>,
    cancel: impl Future<Output = ()> + Send,
) -> Result<String, Error> {
    let identity = executable_identity(launch.executable).await?;
    if let Some(version) = VERSION_CACHE
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(launch.executable)
        .filter(|(saved, _)| *saved == identity)
        .map(|(_, version)| version.clone())
    {
        return Ok(version);
    }
    let (dir, dir_path) = private_dir_async(launch.temp_dir, launch.project, launch.denied).await?;
    let output = version(
        launch.executable,
        launch.env,
        &dir_path,
        launch.limits.startup,
        cancel,
    )
    .await;
    dir.close().await;
    let profile = checks::profile(&output?)?;
    VERSION_CACHE
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(launch.executable.to_owned(), (identity, profile.clone()));
    Ok(profile)
}

/// Returns the models by the ids they run, after the same checks a request
/// must pass.
pub(crate) async fn models(
    launch: &Launch<'_>,
    plan_usage: &Mutex<Option<ProviderUsage>>,
) -> Result<Vec<Listed>, Error> {
    checked_profile(launch, future::pending()).await?;
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
    unprompted(launch, &probe_args(), async |group| {
        let (mut stdin, mut lines) = group.pipes()?;
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
    let mut timer = ReadDeadline::new(deadline);
    let id = request["subtype"].as_str().unwrap_or_default();
    send(stdin, &control_request(id, request), deadline).await?;
    loop {
        match timer.next(lines, None, deadline).await {
            Next::Line(Some(Ok(line))) => {
                let event = parse_event(&line)?;
                let answer = &event["response"];
                if event["type"] == CONTROL_RESPONSE && answer["request_id"] == id {
                    return match answer["subtype"] == SUCCESS {
                        true => Ok(answer["response"].clone()),
                        false => Err(Error::RequestRefused {
                            id: id.to_owned(),
                            error: answer["error"].clone(),
                        }),
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
    cwd: &Path,
    startup: Duration,
    cancel: impl Future<Output = ()> + Send,
) -> Result<String, Error> {
    let args = [VERSION_FLAG.to_owned()];
    supervised(command(executable, env, cwd, &args), async |group| {
        async {
            tokio::select! { biased; value =
            async {
                let (stdin, mut lines) = group.pipes()?;
                drop(stdin);
                let deadline = Instant::now() + startup;
                let first = match ReadDeadline::new(deadline)
                    .next(&mut lines, None, deadline)
                    .await
                {
                    Next::Line(Some(Ok(line))) => Some(line),
                    Next::Line(Some(Err(source))) => return Err(Error::UnreadableVersion(source)),
                    _ => None,
                };
                let status = group.wait(deadline).await?;
                if !status.success() {
                    return Err(Error::VersionFailed(status));
                }
                first.ok_or(Error::NoVersion)
            } => value, value =
            async {
                cancel.await;
                Err(Error::Cancelled)
            } => value, }
        }
        .await
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::{
        CONTROL_RESPONSE, Error, SUCCESS, ask, command, probe_exit, probe_with, supervised,
    };
    use crate::process::wait_without_reaping;
    use crate::providers::claude_code::stream::result_failure;
    use futures::stream;
    use serde_json::json;
    use std::env;
    use std::os::unix::process::ExitStatusExt;
    use std::path::Path;
    use std::process::ExitStatus;
    use std::time::{Duration, Instant};
    use test_case::test_case;
    use tokio::task::spawn_blocking;

    const FAILED_EXIT_CODE: i32 = 1;
    const EXIT_ONE: i32 = FAILED_EXIT_CODE << 8;
    const SIGKILL_STATUS: i32 = 9;
    const SHELL: &str = "/bin/sh";
    const STARTUP: Duration = Duration::from_secs(10);
    const ERROR_TEXT: &str = "probe dependency unavailable";
    const EXITED_PROBE: &str = r#"if [ -n "$1" ]; then printf '%s\n' "$1"; fi; exit 1"#;
    const CONTROL_ID: &str = "test_control";
    const CONTROL_FAILURE: &str = "control request refused";
    const READS_INPUT: &str = "cat >/dev/null";

    #[test_case(true ; "success")]
    #[test_case(false ; "refusal_preserves_the_cli_error")]
    #[tokio::test]
    async fn control_requests_preserve_the_answer(success: bool) {
        let cwd = env::current_dir().unwrap();
        let event = json!({
            "type": CONTROL_RESPONSE,
            "response": {
                "request_id": CONTROL_ID,
                "subtype": if success { SUCCESS } else { "error" },
                "response": {},
                "error": CONTROL_FAILURE,
            },
        });
        let result = supervised(
            command(Path::new(SHELL), &[], &cwd, &["-c", READS_INPUT]),
            async |group| {
                let (mut stdin, _) = group.pipes()?;
                let mut lines = stream::iter([Ok(event.to_string())]);
                ask(
                    &mut stdin,
                    &mut lines,
                    &json!({"subtype": CONTROL_ID}),
                    Instant::now() + STARTUP,
                )
                .await
            },
        )
        .await;
        if success {
            assert_eq!(result.unwrap(), json!({}));
        } else {
            assert!(matches!(
                result.unwrap_err().cause(),
                Error::RequestRefused { id, error }
                    if id == CONTROL_ID && error == CONTROL_FAILURE
            ));
        }
    }

    #[test_case(0, true => matches Ok(()) ; "complete")]
    #[test_case(0, false => matches Err(Error::ChecksUnanswered) ; "incomplete")]
    #[test_case(EXIT_ONE, true => matches Err(Error::ExitedInChecks(_)) ; "failed")]
    #[test_case(SIGKILL_STATUS, true => matches Err(Error::StoppedInChecks) ; "signalled")]
    fn probe_exit_preserves_status_and_completion(
        status: i32,
        complete: bool,
    ) -> Result<(), Error> {
        probe_exit(ExitStatus::from_raw(status), complete)
    }

    #[test_case(false ; "exit_status")]
    #[test_case(true ; "failure_event")]
    #[tokio::test]
    async fn a_probe_that_exits_before_the_handshake_preserves_its_failure(diagnostic: bool) {
        let cwd = env::current_dir().unwrap();
        let event = if diagnostic {
            json!({"type": "result", "subtype": "error_during_execution", "errors": [ERROR_TEXT]})
                .to_string()
        } else {
            String::new()
        };
        let args = ["-c", EXITED_PROBE, "probe", &event];
        let error = supervised(command(Path::new(SHELL), &[], &cwd, &args), async |group| {
            let pid = group.id();
            assert!(
                spawn_blocking(move || wait_without_reaping(pid))
                    .await
                    .unwrap()
            );
            probe_with(group, STARTUP, |event| Err(result_failure(event).unwrap())).await
        })
        .await
        .unwrap_err();
        if diagnostic {
            assert!(
                matches!(error.cause(), Error::Failed(message) if message.contains(ERROR_TEXT)),
                "{error}"
            );
        } else {
            assert!(
                matches!(error.cause(), Error::ExitedInChecks(status) if status.code() == Some(FAILED_EXIT_CODE)),
                "{error}"
            );
        }
    }
}
