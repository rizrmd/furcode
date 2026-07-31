use std::collections::HashMap;
use std::path::PathBuf;

use serde::Deserialize;

use crate::{FurcodeConfig, ModelConfig};

/// Intermediate representation of the legacy `~/.furcode/.config.json` (or pre-migrate ~/forge/.config.json) format.
///
/// This format stores the active provider as a top-level string and models as
/// a map from provider ID to model ID, which differs from the TOML config's
/// nested `session`, `commit`, and `suggest` sub-objects.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LegacyConfig {
    /// The active provider ID (e.g. `"anthropic"`).
    #[serde(default)]
    provider: Option<String>,
    /// Map from provider ID to the model ID to use with that provider.
    #[serde(default)]
    model: HashMap<String, String>,
    /// Commit message generation provider/model pair.
    #[serde(default)]
    commit: Option<LegacyModelRef>,
    /// Shell command suggestion provider/model pair.
    #[serde(default)]
    suggest: Option<LegacyModelRef>,
}

/// A provider/model pair as expressed in the legacy JSON config.
#[derive(Debug, Deserialize)]
struct LegacyModelRef {
    provider: Option<String>,
    model: Option<String>,
}

impl LegacyConfig {
    /// Reads the legacy `~/.furcode/.config.json` (or pre-migrate ~/forge/.config.json) file at `path`, parses it, and
    /// returns the equivalent TOML representation as a [`String`].
    ///
    /// Because every field in [`FurcodeConfig`] is `Option`, fields not covered
    /// by the legacy format are `None` and omitted from the serialized TOML,
    /// so they cannot overwrite values from lower-priority config layers.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be read, the JSON is invalid, or the
    /// resulting config cannot be serialized to TOML.
    pub(crate) fn read(path: &PathBuf) -> crate::Result<String> {
        let contents = std::fs::read_to_string(path)?;
        let config = serde_json::from_str::<LegacyConfig>(&contents)?;
        let furcode_config = config.into_furcode_config();
        let content = toml_edit::ser::to_string_pretty(&furcode_config)?;
        Ok(content)
    }

    /// Converts a [`LegacyConfig`] into the fields of [`FurcodeConfig`] that it
    /// covers, leaving all other fields at their defaults (`None`).
    fn into_furcode_config(self) -> FurcodeConfig {
        let session = self.provider.as_deref().and_then(|provider_id| {
            self.model
                .get(provider_id)
                .cloned()
                .map(|model_id| ModelConfig { provider_id: provider_id.to_string(), model_id })
        });

        let commit = self.commit.and_then(|c| {
            c.provider
                .zip(c.model)
                .map(|(provider_id, model_id)| ModelConfig { provider_id, model_id })
        });

        let suggest = self.suggest.and_then(|s| {
            s.provider
                .zip(s.model)
                .map(|(provider_id, model_id)| ModelConfig { provider_id, model_id })
        });

        FurcodeConfig { session, commit, suggest, ..Default::default() }
    }
}
