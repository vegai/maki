-- The route rules that this plugin and the claude-code provider share. The
-- provider embeds this file and reads the JSON between the long brackets, so
-- the file holds one JSON document and nothing else.
local rules, err = maki.json.decode([==[
{
  "minimum_version": [2, 1, 284],
  "systems": ["linux"],
  "handshake": [
    { "id": "account", "subtype": "initialize" },
    { "id": "settings", "subtype": "get_settings" },
    { "id": "hooks", "subtype": "get_hooks_listing" }
  ],
  "passed_env": [
    "PATH", "HOME", "USER", "LOGNAME", "SHELL", "LANG", "TERM", "TMPDIR", "TZ",
    "HTTP_PROXY", "HTTPS_PROXY", "NO_PROXY", "NODE_EXTRA_CA_CERTS", "SSL_CERT_FILE", "SSL_CERT_DIR",
    "CLAUDE_CONFIG_DIR", "CLAUDE_CODE_OAUTH_TOKEN"
  ],
  "passed_env_prefixes": ["LC_", "XDG_"],
  "route_env": ["CLAUDE_CODE_API_BASE_URL", "CLAUDE_CODE_OAUTH_TOKEN", "CLAUDE_CONFIG_DIR"],
  "route_env_prefixes": ["CLAUDE_CODE_USE_", "ANTHROPIC_"],
  "harmless_env": [
    "CLAUDE_CODE_USE_COWORK_PLUGINS", "CLAUDE_CODE_USE_NATIVE_FILE_SEARCH", "CLAUDE_CODE_USE_POWERSHELL_TOOL",
    "ANTHROPIC_MODEL", "ANTHROPIC_DEFAULT_MODEL", "ANTHROPIC_SMALL_FAST_MODEL",
    "ANTHROPIC_DEFAULT_FABLE_MODEL", "ANTHROPIC_DEFAULT_FABLE_MODEL_NAME",
    "ANTHROPIC_DEFAULT_FABLE_MODEL_DESCRIPTION", "ANTHROPIC_DEFAULT_FABLE_MODEL_SUPPORTED_CAPABILITIES",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL", "ANTHROPIC_DEFAULT_HAIKU_MODEL_NAME",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL_DESCRIPTION", "ANTHROPIC_DEFAULT_HAIKU_MODEL_SUPPORTED_CAPABILITIES",
    "ANTHROPIC_DEFAULT_OPUS_MODEL", "ANTHROPIC_DEFAULT_OPUS_MODEL_NAME",
    "ANTHROPIC_DEFAULT_OPUS_MODEL_DESCRIPTION", "ANTHROPIC_DEFAULT_OPUS_MODEL_SUPPORTED_CAPABILITIES",
    "ANTHROPIC_DEFAULT_SONNET_MODEL", "ANTHROPIC_DEFAULT_SONNET_MODEL_NAME",
    "ANTHROPIC_DEFAULT_SONNET_MODEL_DESCRIPTION", "ANTHROPIC_DEFAULT_SONNET_MODEL_SUPPORTED_CAPABILITIES",
    "ANTHROPIC_CUSTOM_MODEL_OPTION", "ANTHROPIC_CUSTOM_MODEL_OPTION_NAME",
    "ANTHROPIC_CUSTOM_MODEL_OPTION_DESCRIPTION", "ANTHROPIC_CUSTOM_MODEL_OPTION_SUPPORTED_CAPABILITIES"
  ],
  "route_settings": ["apiKeyHelper", "awsAuthRefresh", "awsCredentialExport", "gcpAuthRefresh", "forceLoginOrgUUID"],
  "login_method_key": "forceLoginMethod",
  "subscription_login_method": "claudeai",
  "harmless_policy": [
    "availableModels", "cleanupPeriodDays", "companyAnnouncements", "disableAllHooks",
    "disableClaudeAiConnectors", "forceLoginOrgUUID", "includeCoAuthoredBy", "model", "permissions"
  ],
  "permissions_key": "permissions",
  "harmless_policy_permissions": ["deny", "disableBypassPermissionsMode"],
  "env_key": "env",
  "first_party": "firstParty",
  "no_key_source": "none",
  "flag_source": "flagSettings",
  "policy_source": "policySettings",
  "claude_dir": ".claude",
  "settings_file": "settings.json",
  "local_settings_file": "settings.local.json",
  "builtin_plugin_marker": "builtin"
}
]==])
if not rules then
  error(err, 0)
end
return rules
