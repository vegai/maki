-- Cases that both the plugin's checks (`spec.lua`) and the provider's checks (`checks.rs`)
-- must decide the same way. A case with a `problem` must be refused with that message text,
-- and any other case accepted. The provider embeds this file and reads the JSON between the
-- long brackets, so the file holds one JSON document and nothing else.
return maki.json.decode([==[
{
  "versions": [
    { "output": "2.1.284 (Claude Code)", "system": "linux", "version": "2.1.284" },
    { "output": "2.1.285 (Claude Code)", "system": "Linux", "version": "2.1.285" },
    { "output": "  2.1.1000", "system": "linux", "version": "2.1.1000" },
    { "output": "3.0.0", "system": "linux", "version": "3.0.0" },
    {
      "output": "2.1.283 (Claude Code)",
      "system": "linux",
      "problem": "Claude Code 2.1.283 is older than 2.1.284, the oldest version maki runs"
    },
    { "output": "1.9.999", "system": "linux", "problem": "is older than 2.1.284" },
    { "output": "2.1.284-beta (Claude Code)", "system": "linux", "problem": "maki cannot read a Claude Code version" },
    { "output": "2.1.284.1", "system": "linux", "problem": "maki cannot read a Claude Code version" },
    { "output": "Update available", "system": "linux", "problem": "maki cannot read a Claude Code version" },
    { "output": "claude: not found", "system": "linux", "problem": "maki cannot read a Claude Code version" },
    { "output": "", "system": "linux", "problem": "maki cannot read a Claude Code version" },
    { "output": "2.1.284", "system": "darwin", "problem": "maki runs Claude Code only on linux, not on darwin" },
    { "output": "2.1.284", "system": "Windows_NT", "problem": "not on Windows_NT" }
  ],
  "env": [
    { "name": "PATH", "verdict": "passed" },
    { "name": "https_proxy", "verdict": "passed" },
    { "name": "LC_ALL", "verdict": "passed" },
    { "name": "XDG_RUNTIME_DIR", "verdict": "passed" },
    { "name": "CLAUDE_CONFIG_DIR", "verdict": "passed" },
    { "name": "ANTHROPIC_API_KEY", "verdict": "withheld" },
    { "name": "ANTHROPIC_BASE_URL", "verdict": "withheld" },
    { "name": "ANTHROPIC_SMALL_FAST_MODEL_AWS_REGION", "verdict": "withheld" },
    { "name": "CLAUDE_CODE_API_BASE_URL", "verdict": "withheld" },
    { "name": "CLAUDE_CODE_USE_BEDROCK", "verdict": "withheld" },
    { "name": "CLAUDE_CODE_USE_NEWCLOUD", "verdict": "withheld" },
    { "name": "CLAUDE_CODE_USE_POWERSHELL_TOOL", "verdict": "dropped" },
    { "name": "ANTHROPIC_MODEL", "verdict": "dropped" },
    { "name": "CARGO_HOME", "verdict": "dropped" }
  ],
  "config_dirs": [
    { "configured": "/cfg", "home": "/home/u", "dir": "/cfg" },
    { "home": "/home/u", "dir": "/home/u/.claude" },
    { "configured": "", "home": "/home/u", "dir": "/home/u/.claude" },
    {
      "configured": "cfg",
      "home": "/home/u",
      "problem": "the Claude Code config directory cfg is not an absolute path. Set the claude_code plugin's `config_dir` option, CLAUDE_CONFIG_DIR or HOME to an absolute path."
    },
    { "home": "home", "problem": "the Claude Code config directory home/.claude is not an absolute path" },
    { "home": "", "problem": "maki cannot find the Claude Code config directory" },
    { "problem": "maki cannot find the Claude Code config directory" }
  ],
  "settings": [
    {
      "settings": {
        "apiKeyHelper": "/bin/helper",
        "forceLoginMethod": "console",
        "env": { "ANTHROPIC_API_KEY": "sk-ant-secret", "ANTHROPIC_MODEL": "opus", "FOO": "bar" }
      },
      "conflicts": ["apiKeyHelper", "forceLoginMethod", "env.ANTHROPIC_API_KEY"]
    },
    { "settings": { "gcpAuthRefresh": "g", "awsAuthRefresh": "a" }, "conflicts": ["awsAuthRefresh", "gcpAuthRefresh"] },
    {
      "settings": { "env": { "CLAUDE_CODE_USE_VERTEX": "1", "ANTHROPIC_BASE_URL": "u" } },
      "conflicts": ["env.ANTHROPIC_BASE_URL", "env.CLAUDE_CODE_USE_VERTEX"]
    },
    { "settings": { "forceLoginMethod": "claudeai" }, "conflicts": [] },
    { "settings": { "model": "opus" }, "conflicts": [] }
  ],
  "policies": [
    { "policy": { "companyAnnouncements": ["hi"], "permissions": { "deny": ["Bash"] } } },
    { "policy": { "model": "opus", "availableModels": ["opus"], "forceLoginMethod": "claudeai" } },
    { "policy": { "permissions": { "allow": ["Bash"] } }, "problem": "policy sets permissions.allow, which" },
    {
      "policy": { "permissions": { "disableBypassPermissionsMode": "disable", "defaultMode": "plan" } },
      "problem": "policy sets permissions.defaultMode, which"
    },
    { "policy": { "apiKeyHelper": "x" }, "problem": "policy sets apiKeyHelper, which" },
    { "policy": { "forceLoginMethod": "console" }, "problem": "policy sets forceLoginMethod, which" },
    { "policy": { "env": { "EDITOR": "vi" } }, "problem": "policy sets env, which" }
  ],
  "accounts": [
    {
      "init": { "current_permission_mode": "default", "account": { "apiProvider": "firstParty", "subscriptionType": "Claude Pro" } },
      "accepted": true
    },
    {
      "init": {
        "current_permission_mode": "default",
        "account": { "apiProvider": "firstParty", "subscriptionType": "Claude Pro", "apiKeySource": "none" }
      },
      "accepted": true
    },
    { "init": {}, "problem": "Claude Code did not report its login" },
    {
      "init": { "current_permission_mode": "default", "account": { "apiProvider": "firstParty" } },
      "problem": "Claude Code is not logged in with a claude.ai subscription. Run `claude auth login`"
    },
    {
      "init": {
        "current_permission_mode": "default",
        "account": { "apiProvider": "firstParty", "subscriptionType": "Claude Pro", "apiKeySource": "ANTHROPIC_API_KEY" }
      },
      "problem": "Claude Code would use an API key from \"ANTHROPIC_API_KEY\" instead of the subscription"
    },
    {
      "init": {
        "current_permission_mode": "default",
        "account": { "apiProvider": "firstParty", "apiKeySource": "/login managed key" }
      },
      "problem": "Claude Code would use an API key from \"/login managed key\""
    },
    {
      "init": { "current_permission_mode": "default", "account": { "apiProvider": "bedrock", "subscriptionType": "Claude Pro" } },
      "problem": "Claude Code sends requests to \"bedrock\" instead of Anthropic"
    },
    {
      "init": {
        "current_permission_mode": "bypassPermissions",
        "account": { "apiProvider": "firstParty", "subscriptionType": "Claude Pro" }
      },
      "problem": "Claude Code starts in permission mode \"bypassPermissions\""
    }
  ],
  "plugin_lists": [
    { "plugins": [] },
    { "plugins": [{ "name": "agents-md", "path": "builtin", "source": "agents-md@builtin" }] },
    { "plugins": [{ "name": "cc-plugin-new", "path": "builtin", "source": "cc-plugin-new@builtin" }] },
    {
      "plugins": [{ "name": "new", "path": "/home/u/.claude/plugins/new", "source": "new@builtin" }],
      "problem": "Claude Code loaded the plugin \"new@builtin\". maki does not run Claude Code with a plugin"
    },
    {
      "plugins": [{ "name": "evil", "path": "builtin", "source": "evil@market" }],
      "problem": "Claude Code loaded the plugin \"evil@market\""
    },
    {
      "plugins": [{ "name": "agents-md", "source": "agents-md@builtin" }],
      "problem": "Claude Code loaded the plugin \"agents-md@builtin\""
    },
    { "plugins": ["agents-md@builtin"], "problem": "Claude Code loaded the plugin null" },
    { "plugins": "agents-md@builtin", "problem": "Claude Code did not list its plugins" },
    { "problem": "Claude Code did not list its plugins" }
  ],
  "hooks": [
    { "hooks": { "hooks": [], "policy": { "allDisabled": true } } },
    { "hooks": { "hooks": [{ "source": "userSettings", "disabled": true }], "policy": { "allDisabled": true } } },
    {
      "hooks": {
        "hooks": [{ "event": "PreToolUse", "source": "policySettings", "disabled": false }],
        "policy": { "allDisabled": true, "policyHookCount": 1 }
      },
      "problem": "Claude Code would run a hook from \"policySettings\", which can change the checkout"
    },
    {
      "hooks": { "hooks": [{ "source": "userSettings" }], "policy": { "allDisabled": true } },
      "problem": "Claude Code would run a hook from \"userSettings\""
    },
    {
      "hooks": { "hooks": { "h": { "source": "policySettings" } }, "policy": { "allDisabled": true } },
      "problem": "Claude Code did not list its hooks"
    },
    { "hooks": { "hooks": [], "policy": { "allDisabled": false } }, "problem": "Claude Code cannot turn its hooks off" },
    { "hooks": { "hooks": [], "policy": {} }, "problem": "Claude Code cannot turn its hooks off" },
    { "hooks": { "hooks": [] }, "problem": "Claude Code did not list its hooks" },
    { "hooks": { "policy": { "allDisabled": true } }, "problem": "Claude Code did not list its hooks" },
    { "problem": "Claude Code did not list its hooks" }
  ]
}
]==])
