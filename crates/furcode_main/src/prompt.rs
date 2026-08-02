use std::borrow::Cow;
use std::fmt::Write;
use std::path::PathBuf;

use convert_case::{Case, Casing};
use derive_setters::Setters;
use furcode_api::{AgentId, Effort, ModelId, Usage};
use nu_ansi_term::{Color, Style};

use crate::display_constants::markers;
use crate::utils::humanize_number;

/// Terminal width at which the reasoning effort label switches from the
/// compact three-letter form (e.g. `MED`) to the full uppercase label.
const WIDE_TERMINAL_THRESHOLD: usize = 100;

/// Codex-inspired chat prompt for interactive sessions.
///
/// Visual model:
///
/// ```text
///   forgecode · main
///   › _
///                                    FURCODE  12.4k  $0.08  claude-sonnet-4
/// ```
#[derive(Clone, Setters)]
#[setters(strip_option, borrow_self)]
pub struct FurcodePrompt {
    pub cwd: PathBuf,
    pub usage: Option<Usage>,
    pub agent_id: AgentId,
    pub model: Option<ModelId>,
    /// Currently configured reasoning effort level for the active model.
    /// `Effort::None` is suppressed in rendering.
    pub reasoning_effort: Option<Effort>,
    pub git_branch: Option<String>,
}

impl FurcodePrompt {
    /// Creates a new `FurcodePrompt`, resolving the git branch once at
    /// construction time.
    pub fn new(cwd: PathBuf, agent_id: AgentId) -> Self {
        let git_branch = get_git_branch();
        Self {
            cwd,
            usage: None,
            agent_id,
            model: None,
            reasoning_effort: None,
            git_branch,
        }
    }

    pub fn refresh(&mut self) -> &mut Self {
        let git_branch = get_git_branch();
        self.git_branch = git_branch;
        self
    }

    pub fn render_prompt_left(&self) -> Cow<'_, str> {
        let muted = Style::new().fg(Color::DarkGray);
        let accent = Style::new().fg(Color::LightCyan);
        let chevron = Style::new().fg(Color::LightGreen).bold();

        let current_dir = self
            .cwd
            .file_name()
            .and_then(|name| name.to_str())
            .map(String::from)
            .unwrap_or_else(|| markers::EMPTY.to_string());

        let mut result = String::with_capacity(128);
        write!(result, "{}", accent.paint(&current_dir)).unwrap();

        if let Some(branch) = self.git_branch.as_deref()
            && branch != current_dir
        {
            write!(result, " {}", muted.paint("·")).unwrap();
            write!(result, " {}", muted.paint(branch)).unwrap();
        }

        write!(result, "\n{} ", chevron.paint("›")).unwrap();
        Cow::Owned(result)
    }

    pub fn render_prompt_right(&self) -> Cow<'_, str> {
        let total_tokens = self.usage.as_ref().map(|u| u.total_tokens);
        let active = total_tokens.map(|t| *t > 0).unwrap_or(false);

        // CODE = blue, PLAN = green, RESEARCH = purple
        let agent_color = match self.agent_id.as_str() {
            "coding" => Color::Rgb(80, 160, 255),
            "planning" => Color::Rgb(80, 200, 120),
            "researching" => Color::Rgb(180, 100, 255),
            _ => Color::LightGray,
        };
        let mut result = String::with_capacity(64);

        let agent_str = match self.agent_id.as_str() {
            "coding" => "CODE".to_string(),
            "planning" => "PLAN".to_string(),
            "researching" => "RESEARCH".to_string(),
            other => other.to_case(Case::UpperSnake),
        };
        write!(
            result,
            " {}",
            Style::new().bold().fg(agent_color).paint(agent_str)
        )
        .unwrap();

        if let Some(tokens) = total_tokens
            && active
        {
            let prefix = match tokens {
                furcode_api::TokenCount::Actual(_) => "",
                furcode_api::TokenCount::Approx(_) => "~",
            };
            let count_str = format!("{}{}", prefix, humanize_number(*tokens));
            write!(
                result,
                " {}",
                Style::new().bold().fg(Color::LightGray).paint(count_str)
            )
            .unwrap();
        }

        if let Some(cost) = self.usage.as_ref().and_then(|u| u.cost)
            && active
        {
            let cost_str = format!("${cost:.2}");
            write!(
                result,
                " {}",
                Style::new().bold().fg(Color::Green).paint(cost_str)
            )
            .unwrap();
        }

        if let Some(model) = self.model.as_ref() {
            let model_str = model.to_string();
            let short_model = model_str.split('/').next_back().unwrap_or(model.as_str());
            let color = if active {
                Color::LightMagenta
            } else {
                Color::DarkGray
            };
            write!(result, " {}", Style::new().fg(color).paint(short_model)).unwrap();
        }

        if let Some(ref effort) = self.reasoning_effort
            && !matches!(effort, Effort::None)
        {
            let label = effort_label(effort, term_width());
            let color = if active {
                Color::Yellow
            } else {
                Color::DarkGray
            };
            write!(result, " {}", Style::new().fg(color).paint(label)).unwrap();
        }

        Cow::Owned(result)
    }

    pub fn render_prompt_indicator(&self) -> Cow<'_, str> {
        Cow::Borrowed("")
    }
}

/// Gets the current git branch name if available
fn get_git_branch() -> Option<String> {
    let repo = gix::discover(".").ok()?;
    let head = repo.head().ok()?;
    head.referent_name().map(|r| r.shorten().to_string())
}

/// Returns the current terminal width in columns, falling back to 80 when
/// the size cannot be detected.
fn term_width() -> usize {
    terminal_size::terminal_size()
        .map(|(w, _)| w.0 as usize)
        .unwrap_or(80)
}

/// Formats an [`Effort`] as its uppercase label, collapsing to the first three
/// characters on narrow terminals (< [`WIDE_TERMINAL_THRESHOLD`] columns).
fn effort_label(effort: &Effort, width: usize) -> String {
    let full = effort.to_string().to_uppercase();
    if width >= WIDE_TERMINAL_THRESHOLD {
        full
    } else {
        full.chars().take(3).collect()
    }
}

#[cfg(test)]
mod tests {
    use nu_ansi_term::Style;
    use pretty_assertions::assert_eq;

    use super::*;

    impl Default for FurcodePrompt {
        fn default() -> Self {
            FurcodePrompt {
                cwd: PathBuf::from("."),
                usage: None,
                agent_id: AgentId::default(),
                model: None,
                reasoning_effort: None,
                git_branch: None,
            }
        }
    }

    enum PromptHistorySearchStatus {
        Passing,
        Failing,
    }

    struct PromptHistorySearch {
        status: PromptHistorySearchStatus,
        term: String,
    }

    fn render_prompt_history_search_indicator(
        history_search: PromptHistorySearch,
    ) -> Cow<'static, str> {
        let prefix = match history_search.status {
            PromptHistorySearchStatus::Passing => "",
            PromptHistorySearchStatus::Failing => "failing ",
        };

        let mut result = String::with_capacity(32);
        if history_search.term.is_empty() {
            write!(result, "({prefix}reverse-search) ").unwrap();
        } else {
            write!(
                result,
                "({}reverse-search: {}) ",
                prefix, history_search.term
            )
            .unwrap();
        }

        Cow::Owned(Style::new().fg(Color::White).paint(&result).to_string())
    }

    #[test]
    fn test_render_prompt_left() {
        let prompt = FurcodePrompt::default();
        let actual = prompt.render_prompt_left();
        assert!(actual.contains("›"));
    }

    #[test]
    fn test_render_prompt_left_with_branch() {
        let prompt = FurcodePrompt {
            git_branch: Some("main".to_string()),
            ..Default::default()
        };
        let actual = prompt.render_prompt_left();
        assert!(actual.contains("main"));
        assert!(actual.contains("›"));
    }

    #[test]
    fn test_render_prompt_right_inactive() {
        let mut prompt = FurcodePrompt::default();
        let _ = prompt.model(ModelId::new("gpt-4"));

        let actual = prompt.render_prompt_right();
        assert!(actual.contains("CODE"));
        assert!(actual.contains("gpt-4"));
        assert!(!actual.contains("1k") && !actual.contains("~"));
    }

    #[test]
    fn test_render_prompt_right_active_with_tokens() {
        let usage = Usage {
            prompt_tokens: furcode_api::TokenCount::Actual(10),
            completion_tokens: furcode_api::TokenCount::Actual(20),
            total_tokens: furcode_api::TokenCount::Approx(30),
            ..Default::default()
        };
        let mut prompt = FurcodePrompt::default();
        let _ = prompt.usage(usage);

        let actual = prompt.render_prompt_right();
        assert!(actual.contains("~30"));
        assert!(actual.contains("CODE"));
    }

    #[test]
    fn test_render_prompt_history_search_indicator_passing() {
        let history_search = PromptHistorySearch {
            status: PromptHistorySearchStatus::Passing,
            term: "test".to_string(),
        };
        let actual = render_prompt_history_search_indicator(history_search);
        let expected = Style::new()
            .fg(Color::White)
            .paint("(reverse-search: test) ")
            .to_string();
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_render_prompt_history_search_indicator_failing() {
        let history_search = PromptHistorySearch {
            status: PromptHistorySearchStatus::Failing,
            term: "test".to_string(),
        };
        let actual = render_prompt_history_search_indicator(history_search);
        let expected = Style::new()
            .fg(Color::White)
            .paint("(failing reverse-search: test) ")
            .to_string();
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_render_prompt_history_search_indicator_empty_term() {
        let history_search = PromptHistorySearch {
            status: PromptHistorySearchStatus::Passing,
            term: "".to_string(),
        };
        let actual = render_prompt_history_search_indicator(history_search);
        let expected = Style::new()
            .fg(Color::White)
            .paint("(reverse-search) ")
            .to_string();
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_render_prompt_right_strips_provider_prefix() {
        let usage = Usage {
            prompt_tokens: furcode_api::TokenCount::Actual(10),
            completion_tokens: furcode_api::TokenCount::Actual(20),
            total_tokens: furcode_api::TokenCount::Actual(30),
            ..Default::default()
        };
        let mut prompt = FurcodePrompt::default();
        let _ = prompt.usage(usage);
        let _ = prompt.model(ModelId::new("anthropic/claude-3"));

        let actual = prompt.render_prompt_right();
        assert!(actual.contains("claude-3"));
        assert!(!actual.contains("anthropic/claude-3"));
        assert!(actual.contains("30"));
    }

    #[test]
    fn test_render_prompt_right_with_cost() {
        let usage = Usage {
            total_tokens: furcode_api::TokenCount::Actual(1500),
            cost: Some(0.01),
            ..Default::default()
        };
        let mut prompt = FurcodePrompt::default();
        let _ = prompt.usage(usage);

        let actual = prompt.render_prompt_right();
        assert!(actual.contains("0.01"));
        assert!(actual.contains("1.5k"));
    }

    #[test]
    fn test_render_prompt_right_with_reasoning_effort() {
        let mut prompt = FurcodePrompt::default();
        let _ = prompt.model(ModelId::new("gpt-4"));
        let _ = prompt.reasoning_effort(Effort::High);

        let actual = prompt.render_prompt_right();
        assert!(actual.contains("HIGH") || actual.contains("HIG"));
    }

    #[test]
    fn test_render_prompt_right_hides_effort_none() {
        let mut prompt = FurcodePrompt::default();
        let _ = prompt.model(ModelId::new("gpt-4"));
        let _ = prompt.reasoning_effort(Effort::None);

        let actual = prompt.render_prompt_right();
        assert!(!actual.to_uppercase().contains("NONE"));
    }

    #[test]
    fn test_effort_label_narrow_vs_wide() {
        assert_eq!(effort_label(&Effort::Medium, 80), "MED");
        assert_eq!(
            effort_label(&Effort::Medium, WIDE_TERMINAL_THRESHOLD),
            "MEDIUM"
        );
    }
}
