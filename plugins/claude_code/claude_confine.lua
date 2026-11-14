local native = require("maki.claude_code.internal")
local path_utils = require("claude_paths")
local M = {}
M.DENY_HOME_OUTSIDE = "a deny_read_home path must stay inside the home directory"
local NO_STATE_DIR = "maki cannot find its state directory as a UTF-8 path for the coding sandbox"

-- Claude Code passes its environment to Bash, in the sandbox too (tested on
-- 2.1.284), and the sandbox cannot drop a variable.
-- Logins and keys under the home directory that the sandboxed shell cannot
-- read. Without `CLAUDE_CONFIG_DIR`, Claude Code keeps its login in
-- `.claude`, and its global state, MCP server `env` included, in
-- `.claude.json`. maki's own config can hold keys in `init.lua` and
-- `mcp.toml`.
local HOME_CREDENTIALS = {
  ".claude",
  native.home_state_prefix,
  ".maki",
  ".config/maki",
  ".ssh",
  ".aws",
  ".gnupg",
  ".netrc",
  ".git-credentials",
  ".gitconfig",
  ".config/git",
  ".Xauthority",
  ".local/share/keyrings",
  ".password-store",
  ".config/containers/auth.json",
  ".m2/settings.xml",
  ".gradle/gradle.properties",
  ".terraform.d",
  ".azure",
  ".config/gh",
  ".config/gcloud",
  ".docker",
  ".kube",
  ".npmrc",
  ".pypirc",
  ".cargo/credentials.toml",
  ".cargo/credentials",
  ".bashrc",
  ".bash_profile",
  ".zshrc",
  ".zshenv",
  ".zprofile",
  ".profile",
  ".config/environment.d",
  ".bash_history",
  ".zsh_history",
  ".python_history",
  ".node_repl_history",
  ".mysql_history",
  ".psql_history",
  ".sqlite_history",
  ".viminfo",
  ".lesshst",
  ".config/fish",
  ".local/share/fish/fish_history",
  ".vault-token",
  ".pgpass",
  ".config/rclone",
  ".mozilla",
  ".config/google-chrome",
  ".config/chromium",
  ".config/BraveSoftware",
  ".config/microsoft-edge",
  ".var/app",
  "snap",
}

M.NO_HOME = "maki cannot find your home directory, so it cannot keep the coding worker's shell away from "
  .. "the logins and keys there. Set HOME, or use the read profile."

function M.denied_home_paths(extra)
  return path_utils.comma_list(extra, function(path)
    if path:sub(1, 1) == "/" or path:sub(1, 1) == "~" or path_utils.climbs(path) then
      return nil, M.DENY_HOME_OUTSIDE
    end
    return path
  end)
end

--- Returns the logins and keys under {home} that a coding worker's shell
--- cannot read, or nil and the reason when there is no home directory.
function M.home_credentials(home, extra, environ)
  if not home or home == "" then
    return nil, M.NO_HOME
  end
  local cwd, cwd_err = maki.uv.cwd()
  if not cwd then
    return nil, "maki cannot resolve relocated credential paths: " .. tostring(cwd_err)
  end
  local paths, seen = {}, {}
  local resolved_home = maki.fs.realpath(home) or home
  local function add(path)
    if not seen[path] then
      seen[path] = true
      paths[#paths + 1] = path
    end
  end
  local function deny(path)
    add(maki.fs.joinpath(home, path))
    local absolute = maki.fs.joinpath(resolved_home, path)
    add(absolute)
    local resolved = maki.fs.realpath(absolute)
    if resolved then
      add(resolved)
    end
  end
  for _, path in ipairs(HOME_CREDENTIALS) do
    deny(path)
  end
  local state_files, state_err = native.home_state_files(home)
  if not state_files then
    return nil, "maki cannot inspect Claude Code state copies: " .. tostring(state_err)
  end
  add(maki.fs.joinpath(home, native.home_state_prefix .. "*"))
  add(maki.fs.joinpath(resolved_home, native.home_state_prefix .. "*"))
  for _, path in ipairs(state_files) do
    local resolved = maki.fs.realpath(maki.fs.joinpath(resolved_home, path))
    if resolved and not path_utils.within(resolved, resolved_home) then
      add(resolved)
    end
  end
  for _, path in ipairs(extra or {}) do
    deny(path)
  end
  local env = environ or {}
  local function relocated(path: string?)
    if path and path ~= "" then
      if path:sub(1, 1) ~= "/" then
        path = maki.fs.joinpath(cwd, path)
      end
      add(path)
      local resolved = maki.fs.realpath(path)
      if resolved then
        add(resolved)
      end
    end
  end
  for _, location in ipairs({ { "%.config", env.XDG_CONFIG_HOME }, { "%.local/share", env.XDG_DATA_HOME } }) do
    local prefix, directory = location[1], location[2]
    if directory and directory ~= "" then
      for _, path in ipairs(HOME_CREDENTIALS) do
        local relative = path:match("^" .. prefix .. "/(.+)$")
        if relative then
          relocated(maki.fs.joinpath(directory, relative))
        end
      end
    end
  end
  if env.CARGO_HOME and env.CARGO_HOME ~= "" then
    relocated(maki.fs.joinpath(env.CARGO_HOME, "credentials"))
    relocated(maki.fs.joinpath(env.CARGO_HOME, "credentials.toml"))
  end
  for _, name in ipairs({
    "GNUPGHOME",
    "DOCKER_CONFIG",
    "CLAUDE_CONFIG_DIR",
    "AWS_SHARED_CREDENTIALS_FILE",
    "AWS_CONFIG_FILE",
    "GH_CONFIG_DIR",
    "PASSWORD_STORE_DIR",
    "ZDOTDIR",
    "PGPASSFILE",
    "AZURE_CONFIG_DIR",
    "CLOUDSDK_CONFIG",
    "RCLONE_CONFIG",
  }) do
    relocated(env[name])
  end
  for path in (env.KUBECONFIG or ""):gmatch("[^:]+") do
    relocated(path)
  end
  return paths
end

function M.deny_rules(extra)
  local rules = {}
  for i, pattern in ipairs(extra) do
    rules[i] = "Read(./" .. pattern .. ")"
  end
  return rules
end

-- A read-only mount of the entire artifact would also block snapshot writes. The sandbox
-- skips nonexistent denied paths.
function M.coding(spec, artifact)
  local state_dir = maki.env.state_dir()
  if not state_dir then
    return nil, NO_STATE_DIR
  end
  local deny = table.clone(spec.denied)
  local function hide(path)
    if path then
      deny[#deny + 1] = path
      local resolved = maki.fs.realpath(path)
      if resolved and resolved ~= path then
        deny[#deny + 1] = resolved
      end
    end
  end
  hide(spec.config_dir)
  hide(maki.fs.dirname(artifact.dir))
  hide(state_dir)
  hide(maki.env.config_dir())
  hide(maki.env.logs_dir())
  for _, name in ipairs({ "XDG_RUNTIME_DIR", "SSH_AUTH_SOCK" }) do
    local path = spec.environ[name]
    if path and path:sub(1, 1) == "/" then
      hide(path)
    end
  end
  table.move(spec.home_credentials, 1, #spec.home_credentials, #deny + 1, deny)
  return { deny_read = deny, allow_read = { artifact.git }, allow_write = { artifact.tmp } }
end

return M
