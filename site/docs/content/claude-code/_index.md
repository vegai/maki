+++
title = "Claude Code"
weight = 24
[extra]
group = "Guides"
+++

# Claude Code

Claude Code is Anthropic's CLI, and you can use it with a Claude subscription. The `claude_code` plugin lets the maki agent give a task to the `claude` you installed and receive the reply. Claude runs on your subscription login, and the main agent can use any model from any provider. Claude receives only the task text, without the main agent's conversation.

A task runs in one of two profiles:

- `read`, the default: Claude investigates your checkout with `Read`, `Glob` and `Grep`.
- `code`: Claude edits files and runs commands in a private snapshot of the session's directory. It returns its changes, the agent reviews them, and the agent applies the ones it accepts with `claude_code_import`.

The checkout is your git work tree. The session's directory is where you started maki, which can be a subdirectory of the checkout.

If your Anthropic account uses API keys, use the [anthropic provider](/docs/providers/) instead. The plugin runs only on a Claude subscription login.

## Installation

1. Install Claude Code.
2. Log in with your subscription:

   ```bash
   claude auth login
   claude auth status
   ```

3. Check that `auth status` shows `"authMethod": "claude.ai"`.
4. For the `code` profile, work in a git checkout, and install `bubblewrap` and `socat` for the Claude Code sandbox (`apt install bubblewrap socat`).
5. Enable the plugin in `init.lua`:

   ```lua
   maki.setup({
     plugins = {
       claude_code = { enabled = true },
     },
   })
   ```

A call needs the `run` [permission](/docs/permissions/), like `bash`. The scope is the profile and the model, for example `claude read sonnet` or `claude code sonnet`, so you can allow every read task and still approve each coding task.

## What happens in a call

Before maki sends a prompt, it checks the Claude Code version and your settings. It then starts a first `claude -p` in an empty directory, which answers requests about its login, settings and hooks. The run in the session's directory answers the same requests again. maki sends the prompt only when:

- The login is a claude.ai subscription.
- There is no API key.
- No hook is active.
- The checks accept the [policy](#what-claude-sees).

If the run's answers fail the same checks, maki stops it at once.

The tool view of a finished call shows the login and the token usage. It also shows how much of your plan's 5-hour and 7-day limits you used, and each tool call that Claude Code did not allow. "maki found no route conflict" means that the checks passed. [Limits](#limits) says what maki cannot see about billing.

## What Claude sees

- **Environment**: only a short list of variables reaches Claude Code, such as `PATH`, `HOME`, the locale and the proxy variables. A key you export for maki, such as `ANTHROPIC_API_KEY`, stays in maki, and the tool view names it.
- **Settings**: maki ignores your user, project and local Claude Code settings. Hooks, auto memory and connectors are off, and Claude Code loads no `CLAUDE.md` or `AGENTS.md` itself. Instead maki sends the instructions its own prompt uses: the project's files and your global `AGENTS.md`. If an ignored file could change the login, the call stops, and the error names the file and the key but not the value.
- **Organization policy**: managed settings still apply. maki accepts keys that only restrict Claude Code or inform it, such as `availableModels` or extra `deny` rules. Any other policy key stops the call.
- **Files**: `Read`, `Glob` and `Grep` stay inside the session's directory and cannot read `.env*` files, `secrets/` directories or the paths you add with `deny_read`. Claude Code enforces these rules in its own process and cannot fully apply them to `Glob` and `Grep`, so they are weaker than an operating system sandbox. A secret under another name is still readable, so add it to `deny_read`.

To keep the ignored settings for your everyday Claude Code, give maki a separate Claude login with `config_dir`:

```bash
CLAUDE_CONFIG_DIR=~/.claude-maki claude auth login
```

```lua
claude_code = { enabled = true, config_dir = "/home/me/.claude-maki" },
```

## Coding tasks

```
checkout ──snapshot──> artifact ──claude -p (edits, sandboxed Bash)──> changes ──claude_code_import──> checkout
```

1. maki takes a snapshot of the session's directory in a new artifact directory: the tracked files as they are on disk, uncommitted edits included, plus any untracked paths the call lists in `include`. The snapshot leaves out `.env*` files, `secrets/` directories, the `deny_read` paths, anything named `.claude`, submodules, and links that point out of the snapshot. The reply says when a submodule was skipped. A secret under another name, such as a tracked `.npmrc`, is in the snapshot, so add it to `deny_read`. The snapshot gets a fresh git repository. If a file changes while maki takes the snapshot, the call stops.
2. maki clones the directories in `dependencies`, such as `node_modules`, into the snapshot, copy-on-write when the filesystem supports it and as a full copy otherwise. Then your `prepare` command runs. A Python virtual environment is not copied, because its scripts hard-code its path in the checkout, so recreate it in `prepare`.
3. Claude works in the snapshot. Its shell commands run in the Claude Code sandbox (bubblewrap on Linux), with no network access. The shell can write only to the snapshot and to a temporary directory next to it. It cannot read your checkout, maki's state and config, other artifacts, the logins or common credential files such as `~/.ssh`.
4. Claude Code passes its environment to the shell, so the `code` profile refuses to start when `CLAUDE_CODE_OAUTH_TOKEN` is set, or when `HTTP_PROXY` or `HTTPS_PROXY` has a login in its URL. Log in with `claude auth login` instead of the token.
5. maki lists the changes, and `claude_code_import` applies them with one `bash` command that you approve. If a file it changes also changed in your checkout since the snapshot, the command changes nothing. Files it replaces or deletes stay in the artifact's `originals` folder. Apply these changes yourself from the artifact: symlink and submodule changes, a file that became a folder or the reverse, and files whose names are not UTF-8.

Artifacts live in the maki state directory, or in `artifact_dir`, which must be an absolute path outside the checkout. The next coding call removes every artifact that has not been written for `artifact_ttl_hours`.

## Experimental provider

maki can also run its own agent on the Claude Code models through the `claude-code` provider. The provider is on whenever the `claude_code` plugin is enabled, and it uses the plugin's `executable` and `config_dir`. It limits its requests to `max_concurrent` separately from the plugin's calls, so up to twice that many can run at once.

The models are the ones Claude Code offers your account, under the anthropic provider's ids, such as `claude-code/claude-sonnet-5`. Listing them takes one request slot and starts up to six short `claude` processes. maki keeps the list for a day, and `maki models --refresh` or a model refresh in the TUI reads it again. A failed listing is not retried for five minutes unless you refresh. Tiers and list prices are the anthropic provider's, as the [providers page](/docs/providers/) lists them. Organization policy can make Claude Code run a different model, and a reply from a model you did not pick stops the turn.

Each model gets the context window Claude Code opens for it, read along with the model list. That is 1M for new models such as Sonnet 5.5 and 200K for older ones such as Opus 4.6, which also have a `-1m` id for their 1M window, such as `claude-code/claude-opus-4-6-1m`. Until the first listing, every model has the standard 200K window. Run `maki models --refresh` to read the list right away.

`/thinking` sets Claude Code's effort on models with adaptive thinking, such as Sonnet 5, and a thinking budget on Haiku 4.5. `off` disables thinking, and `adaptive` keeps Claude Code's default.

Claude Code's own tools are all off, so when the model calls tools, maki stops Claude Code, runs the calls with its own tools and your permission rules, and sends the results with the next request. Claude Code's own retries are off too: maki retries a rate limit, an overload or a server error the same way it does for the anthropic provider. It also retries a reply that stopped to call tools but called none, and a reply that sent nothing for `stream_timeout_secs`, which counts as a stream timeout. Each turn starts three `claude` processes, so this provider is slower than the anthropic one.

Each turn runs on your subscription login, and maki counts it as $0, with the API list price of the same tokens beside it, such as `$0.000 (~$0.123 Claude subscription)`.

## Options

All options live under `plugins.claude_code` in [configuration](/docs/configuration/).

## Limits

- The plugin and the provider run only on Linux, with Claude Code 2.1.284 or newer. maki keeps no list of tested versions, and each call checks the installed Claude Code again.
- maki cannot see how Anthropic bills a call or a turn. Past your plan's limits, Anthropic can bill it as extra usage, which `/usage` shows. The USD value maki shows is the API list price of the tokens.
- A plugin call has 30 seconds to start Claude Code. After that only the call's `timeout` stops it, 600 seconds by default, because Claude Code's own tools can run for a long time without output.
- If your organization policy keeps a hook on, the hook runs once in the empty directory before maki stops the call. Anything the hook writes there stays, and the reply names the directory.
- maki cannot send images to Claude Code.
- A long task with many tool calls runs several times slower on the provider than in Claude Code itself. Each request is a new `claude` that reads the conversation as a transcript without the model's earlier thinking, so the model needs more turns to reach the same result.
- When Anthropic's safety classifier stops a reply from the provider, the turn stops with the classifier's explanation, and maki does not retry it.
- In a plugin call Claude uses Claude Code's own tools, so maki's [token economy](/docs/token-economy/) does not apply. Each call starts a fresh `claude`, so the prompt must carry everything the task needs.
- A coding worker's shell has no network access. Fetch what it needs in `prepare`, which runs as you, outside the sandbox.
- The changes do not include a new file from the worker that your `.gitignore` matches, or anything the worker writes to a path named `.claude`.
