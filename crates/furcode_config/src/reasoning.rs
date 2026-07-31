use derive_setters::Setters;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use strum_macros::Display as StrumDisplay;

/// Controls how reasoning output is displayed while streaming.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize, JsonSchema, fake::Dummy,
    StrumDisplay,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ReasoningDisplay {
    /// Show the full thinking stream as it arrives.
    #[default]
    Full,
    /// Show only the last 2 lines of thinking with elapsed time, then clear
    /// the reasoning output when reasoning finishes.
    Compact,
}

/// Controls the reasoning behaviour of a model, including effort level, token
/// budget, and visibility of the thinking process.
#[derive(
    Default, Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, fake::Dummy, Setters,
)]
#[serde(rename_all = "snake_case")]
#[setters(strip_option)]
pub struct ReasoningConfig {
    /// Controls the effort level of the model's reasoning.
    /// Supported by openrouter and furcode provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<Effort>,

    /// Controls how many tokens the model can spend thinking.
    /// Should be greater than 1024 but less than the overall max_tokens.
    /// Supported by openrouter, anthropic, and furcode provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<usize>,

    /// When true, the model thinks deeply but the reasoning is hidden from the
    /// caller. Supported by openrouter and furcode provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude: Option<bool>,

    /// Enables reasoning at the "medium" effort level with no exclusions.
    /// Supported by openrouter, anthropic, and furcode provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,

    /// Controls how reasoning is rendered in the terminal while streaming.
    /// Use `compact` for a concise live preview with elapsed time, or `full`
    /// for the previous behavior of streaming the entire thinking output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<ReasoningDisplay>,
}

/// Effort level for model reasoning.
#[derive(
    Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, fake::Dummy, StrumDisplay,
)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum Effort {
    /// No reasoning; skips the thinking step entirely.
    None,
    /// Minimal reasoning; fastest and cheapest.
    Minimal,
    /// Low reasoning effort.
    Low,
    /// Medium reasoning effort; the default for most providers.
    Medium,
    /// High reasoning effort.
    High,
    /// Extra-high reasoning effort (OpenAI / OpenRouter).
    XHigh,
    /// Maximum reasoning effort; only available on select Anthropic models.
    Max,
}
