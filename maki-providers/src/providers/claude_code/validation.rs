//! Shared launch validation for the provider and the Lua worker.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::checks;

pub fn version(output: &str, system: &str) -> Result<String, String> {
    checks::profile(output, system)
        .map(|profile| profile.version)
        .map_err(|error| error.to_string())
}

pub fn environment(environ: HashMap<String, String>) -> (HashMap<String, String>, Vec<String>) {
    let (env, withheld) = checks::child_env(environ);
    (env.into_iter().collect(), withheld)
}

pub fn config_dir(configured: Option<String>, home: Option<String>) -> Result<PathBuf, String> {
    checks::config_dir(configured.map(OsString::from), home.map(OsString::from))
        .map_err(|error| error.to_string())
}

pub fn settings_conflicts(settings: &Value) -> Vec<String> {
    checks::settings_conflicts(settings)
}

fn response(value: &Value) -> &Value {
    if value["type"] == "control_response" {
        &value["response"]["response"]
    } else {
        value
    }
}

pub fn account_problem(init: &Value, modes: &[String]) -> Option<String> {
    checks::account_problem_in(response(init), modes).map(|problem| problem.to_string())
}

pub fn policy_problem(settings: &Value, hooks: &Value) -> Option<String> {
    checks::policy_problem_in(response(settings), response(hooks), false)
        .map(|problem| problem.to_string())
}

pub fn init_problem(
    event: &Value,
    version: &str,
    cwd: &Path,
    tools: HashSet<String>,
    modes: &[String],
) -> Option<String> {
    checks::init_problem_in(event, version, cwd, &tools, modes, None)
        .map(|problem| problem.to_string())
}

pub fn same_model(ran: &str, expected: &str) -> bool {
    crate::model::is_same_model(ran, expected)
}

pub fn plugins_problem(plugins: &Value) -> Option<String> {
    checks::plugins_problem(plugins).map(|problem| problem.to_string())
}

pub fn resolved_model(account: &Value, requested: &str) -> Option<String> {
    response(account)["models"]
        .as_array()?
        .iter()
        .find(|offer| offer["value"] == requested)?["resolvedModel"]
        .as_str()
        .filter(|model| !model.is_empty())
        .map(str::to_owned)
}

pub async fn cached_version(executable: &Path) -> Result<Option<String>, String> {
    super::run::cached_profile(executable)
        .await
        .map(|profile| profile.map(|profile| profile.version))
        .map_err(|error| error.to_string())
}

pub async fn cache_version(
    executable: &Path,
    output: &str,
    system: &str,
) -> Result<String, String> {
    let profile = checks::profile(output, system).map_err(|error| error.to_string())?;
    let version = profile.version.clone();
    super::run::cache_profile(executable, profile)
        .await
        .map_err(|error| error.to_string())?;
    Ok(version)
}

pub fn invalidate_version(executable: &Path) {
    super::run::invalidate_profile(executable);
}
