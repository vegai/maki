+++
title = "Claude Code"
weight = 24
[extra]
group = "Guides"
+++

# Claude Code

Claude Code is Anthropic's CLI, and you can use it with a Claude subscription. The `claude_code` plugin gives tasks to the installed `claude` CLI and returns its reply to the maki agent. Claude runs on your subscription login, and the main agent can use any model from any provider. Claude receives only the task text, without the main agent's conversation.

A task runs in one of two profiles:

- `read`, the default: Claude investigates your checkout with `Read`, `Glob` and `Grep`.
- `code`: Claude edits files and runs commands in a private snapshot of the session's directory. It returns its changes, the agent reviews them, and the agent applies the ones it accepts with `claude_code_import`.

The checkout is your git work tree. The session's directory is where you started maki, which can be a subdirectory of the checkout.

If your Anthropic account uses API keys, use the [anthropic provider](/docs/providers/). The plugin runs only on a Claude subscription login.

## Installation

1. Install Claude Code.
2. Log in with your subscription:

   ```bash
   claude auth login
   claude auth status
   ```

3. Make sure that `auth status` shows `"authMethod": "claude.ai"`.
4. For the `code` profile, use a git checkout. Install `bubblewrap` and `socat` for the sandbox (`apt install bubblewrap socat`).
5. Enable the plugin in `init.lua`:

   ```lua
   maki.setup({
     plugins = {
       claude_code = { enabled = true },
     },
   })
   ```

A call needs the `run` [permission](/docs/permissions/), like `bash`. Its scope contains the profile and model, for example `claude read sonnet` or `claude code sonnet`. You can allow every read task and approve each coding task separately.

## What happens in a call

Before maki sends a prompt, it validates the Claude Code version and your settings. It starts `claude -p` in an empty directory. That process answers requests about its login, settings and hooks. The run in the session's directory answers the same requests again. maki sends the prompt only when:

- The login is a claude.ai subscription.
- There is no API key.
- No hook is active.
- The checks accept the [policy](#what-claude-sees).

If the run's answers fail the same checks, maki stops it at once. The provider and plugin share the Rust launch checks. maki also checks that the reply names the model your account resolved.

maki reuses a validated version while the executable is unchanged. Every process checks its init version, and each request validates the current login and policy.

The tool view of a finished call shows the login and the token usage. It also shows your usage against the plan's 5-hour and 7-day limits. It lists each tool call that Claude Code did not allow. "maki found no route conflict" means that the checks passed. [Limits](#limits) says what maki cannot see about billing.

## What Claude sees

- **Environment**: only a short list of variables reaches Claude Code, such as `PATH`, `HOME`, the locale and the proxy variables. A key you export for maki, such as `ANTHROPIC_API_KEY`, stays in maki, and the tool view names it.
- **Settings**: maki ignores your user, project and local Claude Code settings. Hooks, auto memory and connectors are off, and Claude Code loads no `CLAUDE.md` or `AGENTS.md` itself. maki sends the instructions its own prompt uses: the project's files and your global `AGENTS.md`. If an ignored file can change the login, the call stops. The error names the file and key without the value.
- **Organization policy**: managed settings still apply. maki accepts keys that only restrict Claude Code or inform it, such as `availableModels` or extra `deny` rules. Any other policy key stops the call.
- **Files**: the read profile uses Claude Code permission rules to restrict access to the session's directory. These rules deny `.env*` files, `secrets/` directories and the paths you add with `deny_read`. Protection for `Glob` and `Grep` is best effort. Claude Code cannot fully enforce these rules for those tools. A secret under another name remains readable. Add it to `deny_read`.

To keep the ignored settings for your everyday Claude Code, give maki a separate Claude login with `config_dir`:

```bash
CLAUDE_CONFIG_DIR=~/.claude-maki claude auth login
```

```lua
claude_code = { enabled = true, config_dir = "/home/me/.claude-maki" },
```

## Coding tasks

```
checkout --snapshot--> artifact --claude -p--> changes --claude_code_import--> checkout
```

1. maki takes a snapshot of the session's directory in a new artifact directory. It copies tracked files from disk, including uncommitted edits. The call can list additional untracked paths in `include`. The snapshot excludes `.env*`, `secrets/`, `deny_read` paths, `.claude`, `.maki`, sparse-checkout omissions, submodules and external links. Protected agent directory names are checked without regard to case. The reply names skipped submodules.

   A secret under another name, such as a tracked `.npmrc`, enters the snapshot. Add it to `deny_read`. The snapshot gets a fresh git repository. If a file changes during the copy, the call stops.

2. maki copies untracked `dependencies` directories, such as `node_modules`, into the snapshot. It uses copy-on-write when the filesystem supports it. Then your `prepare` command runs. maki skips Python virtual environments because their scripts contain the checkout path. Recreate them in `prepare`.

   Untracked dependency files stay outside the import, including files that `prepare` creates. Preparation changes form the worker baseline even if the command fails or times out. The worker starts only after maki records that baseline.

3. Claude works in the snapshot. Its shell commands run in the Claude Code sandbox, with bubblewrap on Linux and no network access. The shell can write only to the snapshot and a temporary directory beside it. It cannot read the checkout, maki state, config and logs, other artifacts, shell histories, browser profiles or common credentials such as `~/.ssh`, git credentials and keyrings. Add home-relative paths with `deny_read_home`. Other home files remain readable.

4. Claude Code passes its environment to the shell. The `code` profile refuses to start when `CLAUDE_CODE_OAUTH_TOKEN` is set. It also refuses `HTTP_PROXY` or `HTTPS_PROXY` URLs that contain a login. Use `claude auth login` for authentication.

5. After the worker exits, maki moves root and nested Git metadata into the artifact's `git-metadata` directory. Ordinary files stay in the snapshot and remain eligible for import. Empty nested repositories do not prevent collection. The change report gives a hardened diff command.

6. maki lists the changes. `claude_code_import` applies them with one `bash` command that you approve. It validates all target files before any change. A conflict at that point stops the import.

   The artifact's `originals` folder keeps hard links to the validated files. Its `displaced` folder keeps removed files. Both folders separate backups by import attempt. These backups preserve writes through open descriptors and later atomic editor saves. A save after removal can stop the import partway. The reply lists applied changes.

   A retry preserves earlier backups. Each retry uses a new backup directory. Approval, import and manifest updates hold an artifact lock. Concurrent imports refuse to start, and expiry skips the artifact while that lock is held. Apply symlink, submodule, file-type and non-UTF-8 path changes manually from the artifact.

Artifacts live in the maki state directory, or in `artifact_dir`, which must be an absolute path outside the checkout. To import replacements or deletions, choose an artifact directory on the checkout's filesystem. If maki cannot make hard links, the import stops before any checkout change. The next coding call removes every artifact that has not been written for `artifact_ttl_hours`.

## Experimental provider

The `claude-code` provider runs maki's agent loop on Claude Code models. The `claude_code` plugin enables the provider. Both use the plugin's `executable` and `config_dir`. It limits its requests to `max_concurrent` separately from the plugin's calls, so up to twice that many can run at once.

Claude Code lists the models available to your account. The provider uses Anthropic model ids, such as `claude-code/claude-sonnet-5`. A listing uses one request slot and starts up to six short `claude` processes. maki keeps the list for a day. Opening the model picker uses the cached list. `maki models --refresh` or Ctrl+r in the picker reads it again. A failed listing waits five minutes before a retry unless you refresh. Tiers and list prices are the anthropic provider's, as the [providers page](/docs/providers/) lists them.

Organization policy can make Claude Code run a different model. A reply from a different model stops the turn.

Each model gets the context window Claude Code opens for it. maki reads the window with the model list. New models such as Sonnet 5.5 use 1M. Older ones such as Opus 4.6 use 200K and offer a `-1m` id for their 1M window, such as `claude-code/claude-opus-4-6-1m`. Until the first listing, every model has the standard 200K window. Run `maki models --refresh` to read the list right away.

`/thinking` sets Claude Code's effort on models with adaptive thinking, such as Sonnet 5, and a thinking budget on Haiku 4.5. `off` disables thinking, and `adaptive` keeps Claude Code's default.

Claude Code's own tools are off. When the model calls a tool, maki stops Claude Code and runs the call with its own tools and your permission rules. It sends the results with the next request. Claude Code's own retries are off too.

maki retries rate limits, overloads and server errors as it does for the anthropic provider. It also retries interrupted generations and a reply that stopped to call tools but called none. Held tool calls run only after the whole request succeeds. Login, policy, refusal and invariant errors stop the turn. No output for `stream_timeout_secs` causes a stream timeout and a retry. Each turn starts a policy probe and a worker. Account and managed policy can change without changes to local files, so each turn checks them again. A changed executable also gets a version check, so this provider is slower than the anthropic one.

Each turn runs on your subscription login, and maki counts it as $0. It shows the API list price of the same tokens beside it, such as `$0.000 (~$0.123 Claude subscription)`.

## Options

All options live under `plugins.claude_code` in [configuration](/docs/configuration/).

## Limits

- The plugin and provider run only on Linux. They need Claude Code 2.1.284 or newer. Each call validates the current CLI contract. An unchanged executable reuses its version check.
- maki cannot see how Anthropic bills a call or a turn. Past your plan's limits, Anthropic can bill it as extra usage, which `/usage` shows. The USD value maki shows is the API list price of the tokens.
- A plugin call has 30 seconds to start Claude Code. Then the call's `timeout` applies, 600 seconds by default. Claude Code tools can run for a long time without output.
- If your organization policy keeps a hook on, the hook runs once in the empty directory before maki stops the call. The hook's output remains there. The reply names the directory.
- maki cannot send images to Claude Code.
- A long task with many tool calls runs several times slower on the provider than in Claude Code itself. Each request starts a new `claude` that reads the conversation as a transcript without the model's earlier thinking. The model needs more turns to reach the same result.
- When Anthropic's safety classifier stops a provider reply, the turn stops with its explanation. maki does not retry it.
- In a plugin call Claude uses Claude Code's own tools, so maki's [token economy](/docs/token-economy/) does not apply. Each call starts a fresh `claude`. The prompt must contain all data necessary for the task.
- A coding worker's shell has no network access. Fetch what it needs in `prepare`, which runs as you, outside the sandbox.
- The changes exclude new files that match your `.gitignore` and anything the worker writes to a path named `.claude` or `.maki`, including case variants.
