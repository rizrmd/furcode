use std::path::PathBuf;
use std::sync::Arc;

use furcode_app::{CommandInfra, CustomInstructionsService, EnvironmentInfra, FileReaderInfra};
use furcode_config::{FurcodeConfig, SelfImproveConfig};
use furcode_domain::wrap_learnings_for_prompt;

/// This service looks for AGENTS.md files in three locations in order of
/// priority:
/// 1. Base path (environment.base_path)
/// 2. Git root directory (if available)
/// 3. Current working directory (environment.cwd)
///
/// When self-improve is enabled, it also loads durable learnings from the user
/// config directory (`learnings.md` by default) on every call so new entries
/// apply without restarting.
#[derive(Clone)]
pub struct FurcodeCustomInstructionsService<F> {
    infra: Arc<F>,
    cache: tokio::sync::OnceCell<Vec<String>>,
}

impl<F> FurcodeCustomInstructionsService<F> {
    /// Creates a new custom instructions service.
    pub fn new(infra: Arc<F>) -> Self {
        Self { infra, cache: Default::default() }
    }
}

impl<F: EnvironmentInfra<Config = FurcodeConfig> + FileReaderInfra + CommandInfra>
    FurcodeCustomInstructionsService<F>
{
    async fn discover_agents_files(&self) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        let environment = self.infra.get_environment();

        // Base custom instructions
        let base_agent_md = environment.global_agentsmd_path();
        if !paths.contains(&base_agent_md) {
            paths.push(base_agent_md);
        }

        // Repo custom instructions
        if let Some(git_root_path) = self.get_git_root().await {
            let git_agent_md = git_root_path.join("AGENTS.md");
            if !paths.contains(&git_agent_md) {
                paths.push(git_agent_md);
            }
        }

        // Working dir custom instructions
        let cwd_agent_md = environment.local_agentsmd_path();
        if !paths.contains(&cwd_agent_md) {
            paths.push(cwd_agent_md);
        }

        paths
    }

    async fn get_git_root(&self) -> Option<PathBuf> {
        let output = self
            .infra
            .execute_command(
                "git rev-parse --show-toplevel".to_owned(),
                self.infra.get_environment().cwd,
                true, // silent mode - don't print git output
                None, // no environment variables needed for git command
            )
            .await
            .ok()?;

        if output.success() {
            Some(PathBuf::from(output.stdout.trim()))
        } else {
            None
        }
    }

    async fn init_agents(&self) -> Vec<String> {
        let paths = self.discover_agents_files().await;

        let mut custom_instructions = Vec::new();

        for path in paths {
            if let Ok(content) = self.infra.read_utf8(&path).await {
                custom_instructions.push(content);
            }
        }

        custom_instructions
    }

    /// Loads learnings from user config when self-improve is enabled.
    ///
    /// Re-read on every call so `/learn` and `remember` take effect without a
    /// process restart. Failures to read the file are ignored (missing file is
    /// normal).
    async fn load_learnings(&self) -> Option<String> {
        let config = self.self_improve_config();
        if !config.enabled {
            return None;
        }

        let env = self.infra.get_environment();
        let path = env.learnings_path(config.path.as_deref()).ok()?;

        let content = self.infra.read_utf8(&path).await.ok()?;
        let wrapped = wrap_learnings_for_prompt(&content, config.max_chars);
        if wrapped.is_empty() {
            None
        } else {
            Some(wrapped)
        }
    }

    fn self_improve_config(&self) -> SelfImproveConfig {
        self.infra
            .get_config()
            .map(|cfg| cfg.self_improve)
            .unwrap_or_default()
    }
}

#[async_trait::async_trait]
impl<F: EnvironmentInfra<Config = FurcodeConfig> + FileReaderInfra + CommandInfra>
    CustomInstructionsService for FurcodeCustomInstructionsService<F>
{
    async fn get_custom_instructions(&self) -> Vec<String> {
        let mut instructions = self
            .cache
            .get_or_init(|| self.init_agents())
            .await
            .clone();

        if let Some(learnings) = self.load_learnings().await {
            instructions.push(learnings);
        }

        instructions
    }
}
