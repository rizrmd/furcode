use std::collections::HashMap;
use std::path::PathBuf;

use derive_setters::Setters;
use fake::Dummy;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::reader::ConfigReader;
use crate::writer::ConfigWriter;
use crate::{
    AutoDumpFormat, Compact, Decimal, HttpConfig, ModelConfig, ReasoningConfig, RetryConfig, Update,
};

/// Wire protocol a provider uses for chat completions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, Dummy)]
pub enum ProviderResponseType {
    OpenAI,
    OpenAIResponses,
    Anthropic,
    Bedrock,
    Google,
    OpenCode,
}

/// Category of a provider.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema, Dummy)]
#[serde(rename_all = "snake_case")]
pub enum ProviderTypeEntry {
    /// LLM provider for chat completions.
    #[default]
    Llm,
    /// Context engine provider for code indexing and search.
    ContextEngine,
}

/// Authentication method supported by a provider.
///
/// Only the simple (non-OAuth) methods are available here; providers that
/// require OAuth device or authorization-code flows must be configured via the
/// file-based `provider.json` override instead.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, Dummy)]
#[serde(rename_all = "snake_case")]
pub enum ProviderAuthMethod {
    ApiKey,
    GoogleAdc,
}

/// A URL parameter variable for a provider, used to substitute template
/// variables in URL strings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, Dummy)]
#[serde(rename_all = "snake_case")]
pub struct ProviderUrlParam {
    /// The environment variable name used as the template variable key.
    pub name: String,
    /// Optional preset values for this parameter shown as suggestions in the
    /// UI.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
    /// Whether this parameter is optional. When `true`, the parameter may be
    /// left blank without causing an error.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub optional: bool,
}

/// Source of models for a provider: either a URL to fetch them from or a
/// static list defined inline.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, Dummy)]
#[serde(untagged)]
pub enum ModelListConfig {
    /// URL template used to fetch the model list dynamically.
    Url(String),
    /// A static list of models defined directly in the configuration.
    Hardcoded(Vec<furcode_domain::Model>),
}

/// A single provider entry defined inline in `furcode.toml` / `.furcode.toml`.
///
/// Inline providers are merged with the built-in provider list; entries with
/// the same `id` override the corresponding built-in entry field-by-field,
/// while entries with a new `id` are appended to the list.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize, JsonSchema, Dummy)]
#[serde(rename_all = "snake_case")]
pub struct ProviderEntry {
    /// Unique provider identifier used in model paths (e.g. `"my_provider"`).
    pub id: String,
    /// Environment variable holding the API key for this provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_var: Option<String>,
    /// URL template for chat completions; may contain `{{VAR}}` placeholders
    /// that are substituted from the credential's url params.
    pub url: String,
    /// Model source: either a URL template for dynamic discovery or a static
    /// list of models defined inline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models: Option<ModelListConfig>,
    /// Wire protocol used by this provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_type: Option<ProviderResponseType>,
    /// Environment variables whose values are substituted into `{{VAR}}`
    /// placeholders in the `url` and `models` templates.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub url_param_vars: Vec<ProviderUrlParam>,
    /// Additional HTTP headers sent with every request to this provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_headers: Option<HashMap<String, String>>,
    /// Provider category; defaults to `llm` when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_type: Option<ProviderTypeEntry>,
    /// Authentication methods supported by this provider; defaults to
    /// `["api_key"]` when omitted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub auth_methods: Vec<ProviderAuthMethod>,
}

/// Top-level Furcode configuration merged from all sources (defaults, file,
/// environment).
#[derive(Default, Debug, Setters, Clone, PartialEq, Serialize, Deserialize, JsonSchema, Dummy)]
#[serde(rename_all = "snake_case")]
#[setters(strip_option)]
pub struct FurcodeConfig {
    /// Retry settings applied at the system level to all IO operations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<RetryConfig>,
    /// Maximum number of lines returned by a single file search operation.
    #[serde(default)]
    pub max_search_lines: usize,
    /// Maximum number of bytes returned by a single file search operation.
    #[serde(default)]
    pub max_search_result_bytes: usize,
    /// Maximum number of characters returned from a URL fetch.
    #[serde(default)]
    pub max_fetch_chars: usize,
    /// Maximum number of lines captured from the leading portion of shell
    /// command output.
    #[serde(default)]
    pub max_stdout_prefix_lines: usize,
    /// Maximum number of lines captured from the trailing portion of shell
    /// command output.
    #[serde(default)]
    pub max_stdout_suffix_lines: usize,
    /// Maximum number of characters per line in shell command output.
    #[serde(default)]
    pub max_stdout_line_chars: usize,
    /// Maximum number of characters per line when reading a file.
    #[serde(default)]
    pub max_line_chars: usize,
    /// Maximum number of lines read from a file in a single operation.
    #[serde(default)]
    pub max_read_lines: u64,
    /// Maximum number of files read in a single batch operation.
    #[serde(default)]
    pub max_file_read_batch_size: usize,
    /// HTTP client settings including proxy, TLS, and timeout configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<HttpConfig>,
    /// Maximum file size in bytes permitted for read operations.
    #[serde(default)]
    pub max_file_size_bytes: u64,
    /// Maximum image file size in bytes permitted for read operations.
    #[serde(default)]
    pub max_image_size_bytes: u64,
    /// Maximum time in seconds a single tool call may run before being
    /// cancelled.
    #[serde(default)]
    pub tool_timeout_secs: u64,
    /// Whether to automatically open HTML dump files in the browser after
    /// creation.
    #[serde(default)]
    pub auto_open_dump: bool,
    /// Directory where debug request files are written; disabled when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub debug_requests: Option<PathBuf>,
    /// Path to the conversation history file; defaults to the global history
    /// location when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_history_path: Option<PathBuf>,
    /// Maximum number of conversations shown in the conversation list.
    #[serde(default)]
    pub max_conversations: usize,
    /// Maximum number of candidate results returned from the initial semantic
    /// search vector query.
    #[serde(default)]
    pub max_sem_search_results: usize,
    /// Number of top results retained after re-ranking in semantic search.
    #[serde(default)]
    pub sem_search_top_k: usize,
    /// Base URL of the optional remote cloud services API (semantic search,
    /// workspace indexing, usage). Empty by default — cloud services are
    /// disabled in Furcode unless both this URL and [`Self::services_enabled`]
    /// are set.
    #[serde(default)]
    #[dummy(expr = "String::new()")]
    pub services_url: String,
    /// Master switch for remote cloud services (semantic search, workspace
    /// sync/index, account usage). Defaults to `false` so Furcode runs fully
    /// offline against LLM providers only.
    #[serde(default)]
    pub services_enabled: bool,
    /// Maximum number of file extensions included in the agent system prompt.
    #[serde(default)]
    pub max_extensions: usize,
    /// Format used when automatically creating a session dump after task
    /// completion; disabled when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_dump: Option<AutoDumpFormat>,
    /// Maximum number of files read concurrently during batch operations.
    #[serde(default)]
    pub max_parallel_file_reads: usize,
    /// Time-to-live in seconds for the cached model API list.
    #[serde(default)]
    pub model_cache_ttl_secs: u64,
    /// Default model and provider configuration used when not overridden by
    /// individual agents.    
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<ModelConfig>,
    /// Model and provider configuration used for commit message generation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<ModelConfig>,
    /// Whether `furcode commit` should override `GIT_COMMITTER_NAME` and
    /// `GIT_COMMITTER_EMAIL` with the Furcode identity. Defaults to `true` via
    /// the embedded `.furcode.toml` defaults.
    #[serde(default)]
    #[serde(alias = "use_forge_committer")]
    pub use_furcode_committer: bool,
    /// Maximum number of recent commits included as context for commit message
    /// generation.
    #[serde(default)]
    pub max_commit_count: usize,
    /// Model and provider configuration used for shell command suggestion
    /// generation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggest: Option<ModelConfig>,

    // --- Workflow fields ---
    /// Configuration for automatic Furcode updates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updates: Option<Update>,

    /// Output randomness for all agents; lower values are deterministic, higher
    /// values are creative (0.0–2.0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<Decimal>,

    /// Nucleus sampling threshold for all agents; limits token selection to the
    /// top cumulative probability mass (0.0–1.0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<Decimal>,

    /// Top-k vocabulary cutoff for all agents; restricts sampling to the k
    /// highest-probability tokens (1–1000).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u32>,

    /// Maximum tokens the model may generate per response for all agents
    /// (1–100,000).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,

    /// Maximum tool failures per turn before the orchestrator forces
    /// completion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tool_failure_per_turn: Option<usize>,

    /// Maximum number of requests that can be made in a single turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_requests_per_turn: Option<usize>,

    /// Context compaction settings applied to all agents; falls back to each
    /// agent's individual setting when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compact: Option<Compact>,

    /// Whether restricted mode is active; when enabled, tool execution requires
    /// explicit permission grants.
    #[serde(default)]
    pub restricted: bool,

    /// Whether tool use is supported in the current environment; when false,
    /// all tool calls are disabled.
    #[serde(default)]
    pub tool_supported: bool,

    /// Reasoning configuration applied to all agents; controls effort level,
    /// token budget, and visibility of the model's thinking process.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<ReasoningConfig>,

    /// Additional provider definitions merged with the built-in provider list.
    ///
    /// Entries with an `id` matching a built-in provider override its fields;
    /// entries with a new `id` are appended and become available for model
    /// selection.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub providers: Vec<ProviderEntry>,

    /// Currency symbol displayed in the shell rprompt next to the session cost
    /// (e.g. `"$"`, `"€"`, `"₹"`). Defaults to `"$"`.
    #[serde(default)]
    pub currency_symbol: String,

    /// Conversion rate applied to costs before display in the shell rprompt.
    /// The raw USD cost is multiplied by this value, allowing costs to be shown
    /// in a local currency. Defaults to `1.0` (no conversion).
    #[serde(default)]
    pub currency_conversion_rate: Decimal,

    /// Enables the pending todos hook that checks for incomplete todo items
    /// when a task ends and reminds the LLM about them.
    #[serde(default)]
    pub verify_todos: bool,

    /// Switches patch replacement fallback from the legacy fuzzy-search range
    /// lookup to the newer text-patch gRPC API.
    /// Defaults to `false` so patching continues to use the legacy fallback
    /// behavior unless explicitly enabled in `furcode.toml` / `.furcode.toml`.
    #[serde(default)]
    pub use_text_patch_fallback: bool,

    /// Whether the deep research agent is available.
    ///
    /// When set to `true`, the Sage agent is added to the agent list and
    /// the `:sage` app command is enabled. Defaults to `false`.
    #[serde(default)]
    pub research_subagent: bool,

    /// Enables subagent support via the task tool; when true the forge agent
    /// gains access to the `task` tool for delegating work to specialised
    /// sub-agents, and the `sage` research-only agent tool is removed.
    /// When false the `task` tool is disabled and `sage` is available instead.
    #[serde(default)]
    pub subagents: bool,

    /// Enables automatic VS Code extension installation when Furcode runs inside
    /// VS Code and the extension is not already installed.
    #[serde(default)]
    pub auto_install_vscode_extension: bool,

    /// When `true`, all system messages in the conversation are merged into a
    /// single leading system message before the request is sent. Enable this
    /// for providers that reject requests containing system messages after
    /// user or assistant turns (e.g. vLLM, NVIDIA NIM).
    #[serde(default)]
    pub merge_system_messages: bool,

    /// Runtime self-improvement settings. Learnings are stored under the user
    /// config directory and injected into the system prompt without recompiling.
    #[serde(default)]
    pub self_improve: SelfImproveConfig,

    /// How thinking and tool activity are shown in the interactive TUI.
    ///
    /// - [`ActivityDisplay::Compact`] (default): animated status, last 2 lines
    ///   of live text, then a one-line summary when each phase ends.
    /// - [`ActivityDisplay::OutLoud`]: stream full reasoning and tool output
    ///   (classic “thinking out loud” behaviour).
    #[serde(default)]
    pub activity_display: ActivityDisplay,
}

/// Interactive activity display style for the TUI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema, Dummy)]
#[serde(rename_all = "snake_case")]
pub enum ActivityDisplay {
    /// Compact animated feed (last 2 live lines → one-line summary).
    #[default]
    Compact,
    /// Stream full reasoning and tool dumps (“thinking out loud”).
    #[serde(alias = "thinking_out_loud", alias = "verbose", alias = "out-loud")]
    OutLoud,
}

impl ActivityDisplay {
    /// Parse a user-facing mode string (`compact`, `out_loud`, …).
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "compact" | "c" | "default" => Some(Self::Compact),
            "out_loud" | "out-loud" | "outloud" | "loud" | "verbose" | "thinking_out_loud"
            | "thinking-out-loud" | "full" => Some(Self::OutLoud),
            _ => None,
        }
    }

    /// Stable config / CLI token for this mode.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Compact => "compact",
            Self::OutLoud => "out_loud",
        }
    }
}

impl std::fmt::Display for ActivityDisplay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Configuration for the self-improving harness.
///
/// Durable improvements are stored as markdown under the user config directory
/// (default `~/.furcode/learnings.md`) and loaded at runtime into the agent
/// system prompt. No binary rebuild is required for new learnings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, Dummy)]
#[serde(rename_all = "snake_case")]
pub struct SelfImproveConfig {
    /// When `true`, learnings are loaded into the system prompt and the
    /// `remember` tool may persist new ones. Defaults to `true`.
    #[serde(default = "default_self_improve_enabled")]
    pub enabled: bool,
    /// Maximum characters of learnings content injected into the system prompt.
    /// Longer files are truncated from the end (newest content is kept when
    /// appended newest-first) or the start as a fallback.
    #[serde(default = "default_self_improve_max_chars")]
    pub max_chars: usize,
    /// Optional path relative to the user config base path. Defaults to
    /// `learnings.md` when absent or empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

fn default_self_improve_enabled() -> bool {
    true
}

fn default_self_improve_max_chars() -> usize {
    12_000
}

impl Default for SelfImproveConfig {
    fn default() -> Self {
        Self {
            enabled: default_self_improve_enabled(),
            max_chars: default_self_improve_max_chars(),
            path: None,
        }
    }
}

/// Error message returned when a cloud-only feature is invoked while services
/// are disabled.
pub const CLOUD_SERVICES_DISABLED_MSG: &str = "\
Cloud services are disabled in Furcode. Semantic search, workspace indexing, \
and account usage APIs are not available. Use local tools (Read, FsSearch, Shell) \
instead. To re-enable later, set services_enabled = true and a valid services_url \
in ~/.furcode/.furcode.toml.";

impl FurcodeConfig {
    /// Whether remote cloud services may be used.
    ///
    /// Requires both `services_enabled = true` and a non-empty `services_url`.
    /// Defaults keep this off so Furcode runs offline against LLM providers
    /// only.
    pub fn cloud_services_enabled(&self) -> bool {
        self.services_enabled && !self.services_url.trim().is_empty()
    }

    /// Returns an error when cloud services are disabled.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CloudServicesDisabled`] when
    /// [`Self::cloud_services_enabled`] is false.
    pub fn require_cloud_services(&self) -> crate::Result<()> {
        if self.cloud_services_enabled() {
            Ok(())
        } else {
            Err(crate::Error::CloudServicesDisabled(CLOUD_SERVICES_DISABLED_MSG))
        }
    }

    /// Reads and merges configuration from all sources, returning the resolved
    /// [`FurcodeConfig`].
    ///
    /// # Errors
    ///
    /// Returns an error if the config path cannot be resolved, the file cannot
    /// be read, or deserialization fails.
    pub fn read() -> crate::Result<FurcodeConfig> {
        ConfigReader::default()
            .read_legacy()
            .read_defaults()
            .read_global()
            .read_env()
            .build()
    }

    /// Writes the configuration to the user config file.
    ///
    /// # Errors
    ///
    /// Returns an error if the configuration cannot be serialized or written to
    /// disk.
    pub fn write(&self) -> crate::Result<()> {
        let path = ConfigReader::config_path();
        ConfigWriter::new(self.clone()).write(&path)
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;
    use crate::reader::ConfigReader;

    #[test]
    fn test_f32_temperature_round_trip() {
        let fixture = FurcodeConfig { temperature: Some(Decimal(0.1)), ..Default::default() };

        let toml = toml_edit::ser::to_string_pretty(&fixture).unwrap();

        assert!(
            toml.contains("temperature = 0.1\n"),
            "expected `temperature = 0.1` in TOML output, got:\n{toml}"
        );
    }

    #[test]
    fn test_f32_top_p_round_trip() {
        let fixture = FurcodeConfig { top_p: Some(Decimal(0.9)), ..Default::default() };

        let toml = toml_edit::ser::to_string_pretty(&fixture).unwrap();

        assert!(
            toml.contains("top_p = 0.9\n"),
            "expected `top_p = 0.9` in TOML output, got:\n{toml}"
        );
    }

    #[test]
    fn test_f32_temperature_deserialize_round_trip() {
        let fixture = FurcodeConfig { temperature: Some(Decimal(0.1)), ..Default::default() };

        let toml = toml_edit::ser::to_string_pretty(&fixture).unwrap();

        let actual = ConfigReader::default().read_toml(&toml).build().unwrap();

        assert_eq!(actual.temperature, fixture.temperature);
    }

    #[test]
    fn test_provider_static_model_list_deserialization() {
        let fixture = r#"
[[providers]]
id = "ollama"
url = "http://127.0.0.1:8000/v1/chat/completions"
response_type = "OpenAI"
auth_methods = ["api_key"]

[[providers.models]]
id = "Qwen3.6-35B-A3b-q3-mlx"
name = "Qwen3.5-35B"
description = "Qwen local reasoning model with advanced problem-solving capabilities"
context_length = 262144
tools_supported = true
supports_parallel_tool_calls = true
supports_reasoning = true
input_modalities = ["text"]

[[providers.models]]
id = "llama3.2-3b"
name = "Llama 3.2 3B"
description = "Meta Llama 3.2 3B lightweight local model"
context_length = 131072
tools_supported = true
supports_parallel_tool_calls = false
supports_reasoning = false
input_modalities = ["text"]
"#;

        let actual = ConfigReader::default().read_toml(fixture).build().unwrap();

        let expected = vec![ProviderEntry {
            id: "ollama".to_string(),
            url: "http://127.0.0.1:8000/v1/chat/completions".to_string(),
            response_type: Some(ProviderResponseType::OpenAI),
            auth_methods: vec![ProviderAuthMethod::ApiKey],
            models: Some(ModelListConfig::Hardcoded(vec![
                furcode_domain::Model::new("Qwen3.6-35B-A3b-q3-mlx")
                    .name("Qwen3.5-35B".to_string())
                    .description(
                        "Qwen local reasoning model with advanced problem-solving capabilities"
                            .to_string(),
                    )
                    .context_length(262144)
                    .tools_supported(true)
                    .supports_parallel_tool_calls(true)
                    .supports_reasoning(true)
                    .input_modalities(vec![furcode_domain::InputModality::Text]),
                furcode_domain::Model::new("llama3.2-3b")
                    .name("Llama 3.2 3B".to_string())
                    .description("Meta Llama 3.2 3B lightweight local model".to_string())
                    .context_length(131072)
                    .tools_supported(true)
                    .supports_parallel_tool_calls(false)
                    .supports_reasoning(false)
                    .input_modalities(vec![furcode_domain::InputModality::Text]),
            ])),
            ..Default::default()
        }];

        assert_eq!(actual.providers, expected);
    }

    #[test]
    fn test_provider_url_model_list_deserialization() {
        let fixture = r#"
[[providers]]
id = "my_provider"
url = "http://example.com/v1/chat/completions"
models = "http://example.com/v1/models"
"#;

        let actual = ConfigReader::default().read_toml(fixture).build().unwrap();

        let expected = vec![ProviderEntry {
            id: "my_provider".to_string(),
            url: "http://example.com/v1/chat/completions".to_string(),
            models: Some(ModelListConfig::Url(
                "http://example.com/v1/models".to_string(),
            )),
            ..Default::default()
        }];

        assert_eq!(actual.providers, expected);
    }

    #[test]
    fn test_auto_install_vscode_extension_defaults_to_true() {
        let actual = ConfigReader::default().read_defaults().build().unwrap();

        assert_eq!(actual.auto_install_vscode_extension, true);
    }

    #[test]
    fn test_auto_install_vscode_extension_can_be_disabled() {
        let toml = "auto_install_vscode_extension = false\n";

        let actual = ConfigReader::default()
            .read_defaults()
            .read_toml(toml)
            .build()
            .unwrap();

        assert_eq!(actual.auto_install_vscode_extension, false);
    }
}
