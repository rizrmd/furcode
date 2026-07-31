use std::sync::Arc;

use anyhow::Context;
use bytes::Bytes;
use chrono::Local;
use furcode_app::{
    EnvironmentInfra, FileDirectoryInfra, FileInfoInfra, FileReaderInfra, FileWriterInfra,
    RememberOutput, RememberService,
};
use furcode_config::FurcodeConfig;
use furcode_domain::{
    append_learning_content, format_learning_section, is_near_duplicate,
};

/// Persists durable harness improvements to the user config learnings file.
///
/// Writes only under the configured learnings path (default
/// `~/.furcode/learnings.md`). Does not accept arbitrary paths.
pub struct FurcodeRemember<F>(Arc<F>);

impl<F> FurcodeRemember<F> {
    /// Creates a new remember service.
    pub fn new(infra: Arc<F>) -> Self {
        Self(infra)
    }
}

#[async_trait::async_trait]
impl<
    F: FileDirectoryInfra
        + FileInfoInfra
        + FileReaderInfra
        + FileWriterInfra
        + EnvironmentInfra<Config = FurcodeConfig>
        + Send
        + Sync,
> RememberService for FurcodeRemember<F>
{
    async fn remember(
        &self,
        content: String,
        title: Option<String>,
    ) -> anyhow::Result<RememberOutput> {
        let content = content.trim().to_string();
        if content.is_empty() {
            anyhow::bail!("remember content must not be empty");
        }

        let config = self.0.get_config().context("Failed to read config")?;
        if !config.self_improve.enabled {
            anyhow::bail!(
                "self-improve is disabled in user config ([self_improve].enabled = false)"
            );
        }

        let env = self.0.get_environment();
        let path = env
            .learnings_path(config.self_improve.path.as_deref())
            .context("Invalid learnings path in config")?;

        if let Some(parent) = path.parent() {
            self.0
                .create_dirs(parent)
                .await
                .with_context(|| format!("Failed to create directory: {}", parent.display()))?;
        }

        let existing = if self.0.is_file(&path).await.unwrap_or(false) {
            self.0
                .read_utf8(&path)
                .await
                .with_context(|| format!("Failed to read learnings file: {}", path.display()))?
        } else {
            String::new()
        };

        if is_near_duplicate(&existing, &content) {
            return Ok(RememberOutput {
                path: path.clone(),
                duplicated: true,
                section: content,
            });
        }

        let date = Local::now().format("%Y-%m-%d").to_string();
        let section = format_learning_section(&date, title.as_deref(), &content);
        let new_content = append_learning_content(&existing, &section);

        self.0
            .write(&path, Bytes::from(new_content))
            .await
            .with_context(|| format!("Failed to write learnings file: {}", path.display()))?;

        Ok(RememberOutput {
            path,
            duplicated: false,
            section: section.trim().to_string(),
        })
    }
}
