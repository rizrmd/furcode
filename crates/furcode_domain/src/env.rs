use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::PathBuf;

use derive_more::Display;
use derive_setters::Setters;
use serde::{Deserialize, Serialize};

use crate::{Effort, ModelConfig};

/// All discrete mutations that can be applied to the application configuration.
///
/// Instead of replacing the entire config, callers describe exactly which field
/// they want to change. Implementations receive a list of operations, apply
/// each in order, and persist the result atomically.
#[derive(Debug, Clone, PartialEq)]
pub enum ConfigOperation {
    /// Set the active session provider and model atomically.
    ///
    /// When the provider differs from the current session provider the entire
    /// session (provider + model) is replaced atomically. When they match only
    /// the model field is updated.
    SetSessionConfig(ModelConfig),
    /// Set the commit-message generation configuration.
    ///
    /// `None` clears the commit configuration so the active session
    /// provider/model is used for commit message generation.
    SetCommitConfig(Option<ModelConfig>),
    /// Set the shell-command suggestion configuration.
    SetSuggestConfig(ModelConfig),
    /// Set the reasoning effort level for all agents.
    SetReasoningEffort(Effort),
}

const VERSION: &str = match option_env!("APP_VERSION") {
    Some(val) => val,
    None => env!("CARGO_PKG_VERSION"),
};

/// Represents the minimal runtime environment in which the application is
/// running.
///
/// Contains only the five fields that cannot be sourced from [`FurcodeConfig`]:
/// `os`, `cwd`, `home`, `shell`, and `base_path`. All configuration
/// values previously carried here are now accessed through
/// `EnvironmentInfra::get_config()`.
#[derive(Debug, Setters, Clone, PartialEq, Serialize, Deserialize, fake::Dummy)]
#[serde(rename_all = "camelCase")]
#[setters(strip_option)]
pub struct Environment {
    /// The operating system of the environment.
    pub os: String,
    /// The current working directory.
    pub cwd: PathBuf,
    /// The home directory.
    pub home: Option<PathBuf>,
    /// The shell being used.
    pub shell: String,
    /// The base path relative to which everything else is stored.
    pub base_path: PathBuf,
}

impl Environment {
    pub fn log_path(&self) -> PathBuf {
        self.base_path.join("logs")
    }

    /// Returns the history file path.
    ///
    /// # Arguments
    /// * `custom_path` - An optional custom path sourced from
    ///   `FurcodeConfig::custom_history_path`. When present it overrides the
    ///   default location inside `base_path`.
    pub fn history_path(&self, custom_path: Option<&PathBuf>) -> PathBuf {
        custom_path
            .cloned()
            .unwrap_or(self.base_path.join(".furcode_history"))
    }
    pub fn snapshot_path(&self) -> PathBuf {
        self.base_path.join("snapshots")
    }

    /// Directory for exact frame-TUI transcript snapshots (per conversation).
    pub fn transcript_dir(&self) -> PathBuf {
        self.base_path.join("transcripts")
    }

    /// Path of the display transcript snapshot for a conversation id string.
    pub fn transcript_path(&self, conversation_id: &str) -> PathBuf {
        self.transcript_dir().join(format!("{conversation_id}.json"))
    }
    pub fn mcp_user_config(&self) -> PathBuf {
        self.base_path.join(".mcp.json")
    }

    /// Returns the path where the MCP trust store is persisted across restarts.
    pub fn mcp_trust_path(&self) -> PathBuf {
        self.base_path.join(".mcp_trust.json")
    }

    pub fn agent_path(&self) -> PathBuf {
        self.base_path.join("agents")
    }
    pub fn agent_cwd_path(&self) -> PathBuf {
        self.cwd.join(".furcode/agents")
    }

    pub fn permissions_path(&self) -> PathBuf {
        self.base_path.join("permissions.yaml")
    }

    pub fn mcp_local_config(&self) -> PathBuf {
        self.cwd.join(".mcp.json")
    }

    pub fn version(&self) -> String {
        VERSION.to_string()
    }

    pub fn app_config(&self) -> PathBuf {
        self.base_path.join(".config.json")
    }

    pub fn database_path(&self) -> PathBuf {
        self.base_path.join(".furcode.db")
    }

    /// Returns the path to the cache directory
    pub fn cache_dir(&self) -> PathBuf {
        self.base_path.join("cache")
    }

    /// Returns the global skills directory path (~/.furcode/skills)
    pub fn global_skills_path(&self) -> PathBuf {
        self.base_path.join("skills")
    }

    /// Returns the agents skills directory path (~/.agents/skills)
    ///
    /// Returns `None` when the home directory cannot be determined.
    pub fn agents_skills_path(&self) -> Option<PathBuf> {
        self.home.as_ref().map(|home| home.join(".agents/skills"))
    }

    /// Returns the project-local skills directory path (.furcode/skills)
    pub fn local_skills_path(&self) -> PathBuf {
        self.cwd.join(".furcode/skills")
    }

    /// Returns the global commands directory path (base_path/commands)
    pub fn command_path(&self) -> PathBuf {
        self.base_path.join("commands")
    }

    /// Returns the project-local commands directory path (.furcode/commands)
    pub fn command_path_local(&self) -> PathBuf {
        self.cwd.join(".furcode/commands")
    }

    /// Returns the global AGENTS.md path (base_path/AGENTS.md)
    pub fn global_agentsmd_path(&self) -> PathBuf {
        self.base_path.join("AGENTS.md")
    }

    /// Returns the project-local AGENTS.md path (cwd/AGENTS.md)
    pub fn local_agentsmd_path(&self) -> PathBuf {
        self.cwd.join("AGENTS.md")
    }

    /// Returns the default global learnings path (`base_path/learnings.md`).
    pub fn global_learnings_path(&self) -> PathBuf {
        self.base_path.join("learnings.md")
    }

    /// Resolves the learnings file path under the user config base directory.
    ///
    /// When `relative` is `None` or empty, returns [`Self::global_learnings_path`].
    /// Relative paths may only resolve under `base_path` (no `..` escape).
    ///
    /// # Errors
    ///
    /// Returns an error when the resolved path would leave `base_path`.
    pub fn learnings_path(&self, relative: Option<&str>) -> anyhow::Result<PathBuf> {
        let rel = relative.map(str::trim).filter(|s| !s.is_empty());
        let Some(rel) = rel else {
            return Ok(self.global_learnings_path());
        };

        let candidate = PathBuf::from(rel);
        if candidate.is_absolute() {
            anyhow::bail!(
                "learnings path must be relative to the user config directory, got absolute path: {rel}"
            );
        }
        if candidate
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            anyhow::bail!("learnings path must not contain '..': {rel}");
        }

        Ok(self.base_path.join(candidate))
    }

    /// Returns the plans directory path relative to the current working
    /// directory (cwd/plans)
    pub fn plans_path(&self) -> PathBuf {
        self.cwd.join("plans")
    }

    /// Returns the path to the custom provider configuration file
    /// (base_path/provider.json)
    pub fn provider_config_path(&self) -> PathBuf {
        self.base_path.join("provider.json")
    }

    /// Returns the path to the credentials file where provider API keys are
    /// stored
    pub fn credentials_path(&self) -> PathBuf {
        self.base_path.join(".credentials.json")
    }

    pub fn workspace_hash(&self) -> WorkspaceHash {
        let mut hasher = DefaultHasher::default();
        self.cwd.hash(&mut hasher);

        WorkspaceHash(hasher.finish())
    }
}

#[derive(Clone, Copy, Display)]
pub struct WorkspaceHash(u64);
impl WorkspaceHash {
    pub fn new(id: u64) -> Self {
        WorkspaceHash(id)
    }

    pub fn id(&self) -> u64 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use fake::{Fake, Faker};
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn test_agent_cwd_path() {
        let fixture: Environment = Faker.fake();
        let fixture = fixture.cwd(PathBuf::from("/current/working/dir"));

        let actual = fixture.agent_cwd_path();
        let expected = PathBuf::from("/current/working/dir/.furcode/agents");

        assert_eq!(actual, expected);
    }

    #[test]
    fn test_agent_cwd_path_independent_from_agent_path() {
        let fixture: Environment = Faker.fake();
        let fixture = fixture
            .cwd(PathBuf::from("/different/current/dir"))
            .base_path(PathBuf::from("/completely/different/base"));

        let agent_path = fixture.agent_path();
        let agent_cwd_path = fixture.agent_cwd_path();
        let expected_agent_path = PathBuf::from("/completely/different/base/agents");
        let expected_agent_cwd_path = PathBuf::from("/different/current/dir/.furcode/agents");

        // Verify that agent_path uses base_path
        assert_eq!(agent_path, expected_agent_path);

        // Verify that agent_cwd_path is independent and always relative to CWD
        assert_eq!(agent_cwd_path, expected_agent_cwd_path);

        // Verify they are different paths
        assert_ne!(agent_path, agent_cwd_path);
    }

    #[test]
    fn test_global_skills_path() {
        let fixture: Environment = Faker.fake();
        let fixture = fixture.base_path(PathBuf::from("/home/user/.furcode"));

        let actual = fixture.global_skills_path();
        let expected = PathBuf::from("/home/user/.furcode/skills");

        assert_eq!(actual, expected);
    }

    #[test]
    fn test_agents_skills_path_with_home() {
        let fixture: Environment = Faker.fake();
        let fixture = fixture.home(PathBuf::from("/home/user"));

        let actual = fixture.agents_skills_path();
        let expected = Some(PathBuf::from("/home/user/.agents/skills"));

        assert_eq!(actual, expected);
    }

    #[test]
    fn test_agents_skills_path_without_home() {
        let fixture: Environment = Faker.fake();
        // Explicitly clear the home field
        let mut fixture = fixture;
        fixture.home = None;

        let actual = fixture.agents_skills_path();

        assert_eq!(actual, None);
    }

    #[test]
    fn test_local_skills_path() {
        let fixture: Environment = Faker.fake();
        let fixture = fixture.cwd(PathBuf::from("/projects/my-app"));

        let actual = fixture.local_skills_path();
        let expected = PathBuf::from("/projects/my-app/.furcode/skills");

        assert_eq!(actual, expected);
    }

    #[test]
    fn test_skills_paths_independent() {
        let fixture: Environment = Faker.fake();
        let fixture = fixture
            .cwd(PathBuf::from("/projects/my-app"))
            .base_path(PathBuf::from("/home/user/.furcode"));

        let global_path = fixture.global_skills_path();
        let local_path = fixture.local_skills_path();

        let expected_global = PathBuf::from("/home/user/.furcode/skills");
        let expected_local = PathBuf::from("/projects/my-app/.furcode/skills");

        // Verify global path uses base_path
        assert_eq!(global_path, expected_global);

        // Verify local path uses cwd
        assert_eq!(local_path, expected_local);

        // Verify they are different paths
        assert_ne!(global_path, local_path);
    }

    #[test]
    fn test_command_path() {
        let fixture: Environment = Faker.fake();
        let fixture = fixture.base_path(PathBuf::from("/home/user/.furcode"));

        let actual = fixture.command_path();
        let expected = PathBuf::from("/home/user/.furcode/commands");

        assert_eq!(actual, expected);
    }

    #[test]
    fn test_command_path_local() {
        let fixture: Environment = Faker.fake();
        let fixture = fixture.cwd(PathBuf::from("/projects/my-app"));

        let actual = fixture.command_path_local();
        let expected = PathBuf::from("/projects/my-app/.furcode/commands");

        assert_eq!(actual, expected);
    }

    #[test]
    fn test_command_paths_independent() {
        let fixture: Environment = Faker.fake();
        let fixture = fixture
            .cwd(PathBuf::from("/projects/my-app"))
            .base_path(PathBuf::from("/home/user/.furcode"));

        let global_path = fixture.command_path();
        let local_path = fixture.command_path_local();

        let expected_global = PathBuf::from("/home/user/.furcode/commands");
        let expected_local = PathBuf::from("/projects/my-app/.furcode/commands");

        assert_eq!(global_path, expected_global);
        assert_eq!(local_path, expected_local);
        assert_ne!(global_path, local_path);
    }

    #[test]
    fn test_global_agents_md_path() {
        let fixture: Environment = Faker.fake();
        let fixture = fixture.base_path(PathBuf::from("/home/user/.furcode"));

        let actual = fixture.global_agentsmd_path();
        let expected = PathBuf::from("/home/user/.furcode/AGENTS.md");

        assert_eq!(actual, expected);
    }

    #[test]
    fn test_local_agents_md_path() {
        let fixture: Environment = Faker.fake();
        let fixture = fixture.cwd(PathBuf::from("/projects/my-app"));

        let actual = fixture.local_agentsmd_path();
        let expected = PathBuf::from("/projects/my-app/AGENTS.md");

        assert_eq!(actual, expected);
    }

    #[test]
    fn test_global_learnings_path() {
        let fixture: Environment = Faker.fake();
        let fixture = fixture.base_path(PathBuf::from("/home/user/.furcode"));

        let actual = fixture.global_learnings_path();
        let expected = PathBuf::from("/home/user/.furcode/learnings.md");

        assert_eq!(actual, expected);
    }

    #[test]
    fn test_learnings_path_default_and_relative() {
        let fixture: Environment = Faker.fake();
        let fixture = fixture.base_path(PathBuf::from("/home/user/.furcode"));

        let actual_default = fixture.learnings_path(None).unwrap();
        assert_eq!(
            actual_default,
            PathBuf::from("/home/user/.furcode/learnings.md")
        );

        let actual_custom = fixture.learnings_path(Some("notes/improve.md")).unwrap();
        assert_eq!(
            actual_custom,
            PathBuf::from("/home/user/.furcode/notes/improve.md")
        );

        assert!(fixture.learnings_path(Some("../escape.md")).is_err());
        assert!(fixture.learnings_path(Some("/abs/path.md")).is_err());
    }

    #[test]
    fn test_plans_path() {
        let fixture: Environment = Faker.fake();
        let fixture = fixture.cwd(PathBuf::from("/projects/my-app"));

        let actual = fixture.plans_path();
        let expected = PathBuf::from("/projects/my-app/plans");

        assert_eq!(actual, expected);
    }

    #[test]
    fn test_provider_config_path() {
        let fixture: Environment = Faker.fake();
        let fixture = fixture.base_path(PathBuf::from("/home/user/.furcode"));

        let actual = fixture.provider_config_path();
        let expected = PathBuf::from("/home/user/.furcode/provider.json");

        assert_eq!(actual, expected);
    }
}
