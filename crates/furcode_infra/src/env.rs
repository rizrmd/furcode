use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use furcode_app::EnvironmentInfra;
use furcode_config::{ConfigReader, FurcodeConfig, ModelConfig};
use furcode_domain::{ConfigOperation, Environment};
use tracing::debug;

/// Builds a [`furcode_domain::Environment`] from runtime context only.
///
/// Only the five fields that cannot be sourced from [`FurcodeConfig`] are set
/// here: `os`, `cwd`, `home`, `shell`, and `base_path`. All configuration
/// values are now accessed through `EnvironmentInfra::get_config()`.
pub fn to_environment(cwd: PathBuf) -> Environment {
    Environment {
        os: std::env::consts::OS.to_string(),
        cwd,
        home: dirs::home_dir(),
        shell: if cfg!(target_os = "windows") {
            std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string())
        } else {
            std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
        },
        base_path: ConfigReader::base_path(),
    }
}

/// Applies a single [`ConfigOperation`] directly to a [`FurcodeConfig`].
///
/// Used by [`FurcodeEnvironmentInfra::update_environment`] to mutate the
/// persisted config without an intermediate `Environment` round-trip.
fn apply_config_op(fc: &mut FurcodeConfig, op: ConfigOperation) {
    match op {
        ConfigOperation::SetSessionConfig(mc) => {
            let pid_str = mc.provider.as_ref().to_string();
            let mid_str = mc.model.to_string();
            fc.session = Some(ModelConfig { provider_id: pid_str, model_id: mid_str });
        }
        ConfigOperation::SetCommitConfig(mc) => {
            fc.commit = mc.map(|m| ModelConfig {
                provider_id: m.provider.as_ref().to_string(),
                model_id: m.model.to_string(),
            });
        }
        ConfigOperation::SetSuggestConfig(mc) => {
            fc.suggest = Some(ModelConfig {
                provider_id: mc.provider.as_ref().to_string(),
                model_id: mc.model.to_string(),
            });
        }
        ConfigOperation::SetReasoningEffort(effort) => {
            let config_effort = match effort {
                furcode_domain::Effort::None => furcode_config::Effort::None,
                furcode_domain::Effort::Minimal => furcode_config::Effort::Minimal,
                furcode_domain::Effort::Low => furcode_config::Effort::Low,
                furcode_domain::Effort::Medium => furcode_config::Effort::Medium,
                furcode_domain::Effort::High => furcode_config::Effort::High,
                furcode_domain::Effort::XHigh => furcode_config::Effort::XHigh,
                furcode_domain::Effort::Max => furcode_config::Effort::Max,
            };
            let reasoning = fc
                .reasoning
                .get_or_insert_with(furcode_config::ReasoningConfig::default);
            reasoning.effort = Some(config_effort);
        }
    }
}

/// Infrastructure implementation for managing application configuration with
/// caching support.
///
/// Uses [`FurcodeConfig::read`] and [`FurcodeConfig::write`] for all file I/O and
/// maintains an in-memory cache to reduce disk access. Also handles
/// environment variable discovery via `.env` files and OS APIs.
pub struct FurcodeEnvironmentInfra {
    cwd: PathBuf,
    cache: Arc<std::sync::Mutex<Option<FurcodeConfig>>>,
}

impl FurcodeEnvironmentInfra {
    /// Creates a new [`FurcodeEnvironmentInfra`] with the given pre-read config.
    ///
    /// The cache is pre-seeded with `config` so no disk I/O occurs on the
    /// first [`EnvironmentInfra::get_config`] call.
    ///
    /// # Arguments
    /// * `cwd` - The working directory path; used to resolve `.env` files
    /// * `config` - The pre-read [`FurcodeConfig`] to seed the in-memory cache
    pub fn new(cwd: PathBuf, config: FurcodeConfig) -> Self {
        Self { cwd, cache: Arc::new(std::sync::Mutex::new(Some(config))) }
    }

    /// Returns the cached [`FurcodeConfig`], re-reading from disk if the cache
    /// has been invalidated by [`Self::update_environment`].
    ///
    /// # Errors
    ///
    /// Returns an error if the cache is empty and the disk read fails.
    pub fn cached_config(&self) -> anyhow::Result<FurcodeConfig> {
        let mut cache = self.cache.lock().expect("cache mutex poisoned");
        if let Some(ref config) = *cache {
            Ok(config.clone())
        } else {
            let config = ConfigReader::default()
                .read_defaults()
                .read_global()
                .read_env()
                .build()?;
            *cache = Some(config.clone());
            Ok(config)
        }
    }
}

impl EnvironmentInfra for FurcodeEnvironmentInfra {
    type Config = FurcodeConfig;

    fn get_env_var(&self, key: &str) -> Option<String> {
        std::env::var(key).ok()
    }

    fn get_env_vars(&self) -> BTreeMap<String, String> {
        std::env::vars().collect()
    }

    fn get_environment(&self) -> Environment {
        to_environment(self.cwd.clone())
    }

    fn get_config(&self) -> anyhow::Result<FurcodeConfig> {
        self.cached_config()
    }

    async fn update_environment(&self, ops: Vec<ConfigOperation>) -> anyhow::Result<()> {
        // Load the global config (with defaults applied) for the update round-trip
        let mut fc = ConfigReader::default()
            .read_defaults()
            .read_global()
            .build()?;

        debug!(config = ?fc, ?ops, "applying app config operations");

        for op in ops {
            apply_config_op(&mut fc, op);
        }

        fc.write()?;
        debug!(config = ?fc, "written .furcode.toml");

        // Reset cache so next get_config() re-reads the updated values from disk
        *self.cache.lock().expect("cache mutex poisoned") = None;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use furcode_config::FurcodeConfig;
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn test_to_environment_sets_cwd() {
        let fixture_cwd = PathBuf::from("/test/cwd");
        let actual = to_environment(fixture_cwd.clone());
        assert_eq!(actual.cwd, fixture_cwd);
    }

    #[test]
    fn test_to_environment_base_path_is_stable_after_env_var_change() {
        let fixture_cwd = PathBuf::from("/any/cwd");
        let expected = to_environment(fixture_cwd.clone()).base_path;

        let previous = std::env::var("FURCODE_CONFIG").ok();
        unsafe { std::env::set_var("FURCODE_CONFIG", "/custom/config/dir") };

        let actual = to_environment(fixture_cwd).base_path;

        if let Some(value) = previous {
            unsafe { std::env::set_var("FURCODE_CONFIG", value) };
        } else {
            unsafe { std::env::remove_var("FURCODE_CONFIG") };
        }

        assert_eq!(actual, expected);
    }

    #[test]
    fn test_to_environment_falls_back_to_home_dir_when_env_var_absent() {
        let actual = to_environment(PathBuf::from("/any/cwd"));
        // Without FURCODE_CONFIG the base_path must be either ".furcode" (new default)
        // or "furcode" (legacy fallback when ~/furcode exists on this machine).
        let name = actual.base_path.file_name().unwrap();
        assert!(
            name == ".furcode" || name == "furcode",
            "Expected base_path to end with '.furcode' or 'furcode', got: {:?}",
            name
        );
    }

    #[test]
    fn test_apply_config_op_set_model() {
        use furcode_domain::{ModelConfig as DomainModelConfig, ModelId, ProviderId};

        let mut fixture = FurcodeConfig::default();
        apply_config_op(
            &mut fixture,
            ConfigOperation::SetSessionConfig(DomainModelConfig::new(
                ProviderId::ANTHROPIC,
                ModelId::new("claude-3-5-sonnet"),
            )),
        );

        let actual_provider = fixture.session.as_ref().map(|s| s.provider_id.as_str());
        let actual_model = fixture.session.as_ref().map(|s| s.model_id.as_str());

        assert_eq!(actual_provider, Some("anthropic"));
        assert_eq!(actual_model, Some("claude-3-5-sonnet"));
    }

    #[test]
    fn test_apply_config_op_set_session_config_replaces_existing() {
        use furcode_config::ModelConfig as FurcodeCfgModelConfig;
        use furcode_domain::{ModelConfig as DomainModelConfig, ModelId, ProviderId};

        let mut fixture = FurcodeConfig {
            session: Some(FurcodeCfgModelConfig {
                provider_id: "openai".to_string(),
                model_id: "gpt-4".to_string(),
            }),
            ..Default::default()
        };

        apply_config_op(
            &mut fixture,
            ConfigOperation::SetSessionConfig(DomainModelConfig::new(
                ProviderId::ANTHROPIC,
                ModelId::new("claude-3-5-sonnet-20241022"),
            )),
        );

        let actual_provider = fixture.session.as_ref().map(|s| s.provider_id.as_str());
        let actual_model = fixture.session.as_ref().map(|s| s.model_id.as_str());

        assert_eq!(actual_provider, Some("anthropic"));
        assert_eq!(actual_model, Some("claude-3-5-sonnet-20241022"));
    }

    #[test]
    fn test_apply_config_op_set_session_config_creates_new_session() {
        use furcode_domain::{ModelConfig as DomainModelConfig, ModelId, ProviderId};

        let mut fixture = FurcodeConfig::default();

        apply_config_op(
            &mut fixture,
            ConfigOperation::SetSessionConfig(DomainModelConfig::new(
                ProviderId::ANTHROPIC,
                ModelId::new("claude-3-5-sonnet-20241022"),
            )),
        );

        let actual_provider = fixture.session.as_ref().map(|s| s.provider_id.as_str());
        let actual_model = fixture.session.as_ref().map(|s| s.model_id.as_str());

        assert_eq!(actual_provider, Some("anthropic"));
        assert_eq!(actual_model, Some("claude-3-5-sonnet-20241022"));
    }
}
