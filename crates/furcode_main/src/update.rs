use std::env;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use colored::Colorize;
use furcode_api::API;
use furcode_config::{Update, UpdateFrequency};
use furcode_domain::Environment;
use furcode_select::FurcodeWidget;
use furcode_tracker::VERSION;
use update_informer::{Check, Version, registry};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallMethod {
    Script,
    Homebrew,
    Npm,
    WindowsBinary,
    Unknown,
}

impl InstallMethod {
    pub fn detect(env: &Environment) -> Self {
        if env.os == "windows" {
            return Self::WindowsBinary;
        }

        if let Ok(path) = env::var("PATH") {
            for dir in path.split(std::path::MAIN_SEPARATOR) {
                if dir.contains("homebrew") || dir.contains("Cellar/furcode") {
                    return Self::Homebrew;
                }
            }
        }

        if env::var("npm_config_prefix").is_ok() {
            return Self::Npm;
        }

        if let Ok(exe) = env::current_exe() {
            let exe_str = exe.to_string_lossy().to_lowercase();
            if exe_str.contains(".furcode") || exe_str.contains("/furcode/") {
                return Self::Script;
            }
        }

        Self::Unknown
    }
}

#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    #[error("update is not supported for {install_method:?} installs; use your package manager instead")]
    UnsupportedInstallMethod { install_method: InstallMethod },

    #[error("this furcode binary is not writable; cannot replace it during update")]
    BinaryNotWritable,

    #[error("furcode is not on PATH; update may not replace the active binary")]
    NotOnPath,

    #[error("update command failed: {source}")]
    CommandFailed { source: anyhow::Error },
}

fn show_update_failure_warning(_message: &str) {
    #[cfg(not(test))]
    {
        tracing::warn!("Auto update check completed with issue");
    }
}

/// Runs the official installation script to update Furcode.
/// When `auto_update` is true, exits immediately after a successful update
/// without prompting the user.
async fn execute_update_command(api: Arc<impl API>, auto_update: bool) {
    let output = api
        .execute_shell_command_raw("curl -fsSL https://furcode.dev/cli | sh")
        .await;

    match output {
        Err(err) => {
            let _ = send_update_failure_event(&format!("Auto update failed {err}")).await;
            show_update_failure_warning(&format!("Update failed: {err}"));
        }
        Ok(output) => {
            if output.success() {
                let should_exit = if auto_update {
                    true
                } else {
                    let answer = FurcodeWidget::confirm(
                        "You need to close furcode to complete update. Do you want to close it now?",
                    )
                    .with_default(true)
                    .prompt();
                    answer.unwrap_or_default().unwrap_or_default()
                };
                if should_exit {
                    std::process::exit(0);
                }
            } else {
                let exit_output = match output.code() {
                    Some(code) => format!("Process exited with code: {code}"),
                    None => "Process exited without code".to_string(),
                };
                let _ =
                    send_update_failure_event(&format!("Auto update failed, {exit_output}",)).await;
                show_update_failure_warning(&format!("Update failed: {exit_output}"));
            }
        }
    }
}

#[cfg(unix)]
async fn execute_update_command_unix(api: Arc<impl API>, auto_update: bool) {
    execute_update_command(api, auto_update).await;
}

#[cfg(not(unix))]
async fn execute_update_command_unix(_api: Arc<impl API>, _auto_update: bool) {}

async fn route_update_by_install_method(
    api: Arc<impl API>,
    install_method: InstallMethod,
    auto_update: bool,
) {
    match install_method {
        InstallMethod::Homebrew => {
            show_update_failure_warning(
                "Update is not supported for Homebrew installs; use `brew upgrade furcode` instead",
            );
        }
        InstallMethod::Npm => {
            show_update_failure_warning(
                "Update is not supported for npm installs; use `npm update -g furcode` instead",
            );
        }
        InstallMethod::WindowsBinary => {
            #[cfg(unix)]
            {
                let _ = send_update_failure_event(
                    "Blocked shell updater on Windows install",
                ).await;
                show_update_failure_warning(
                    "Windows binary updates are not supported from the shell installer yet. Use the Windows installer or winget to update.",
                );
            }
            #[cfg(not(unix))]
            {
                show_update_failure_warning(
                    "Windows binary updates are not supported in-app yet. Use the Windows installer or winget to update.",
                );
            }
        }
        InstallMethod::Script => {
            execute_update_command_unix(api, auto_update).await;
        }
        InstallMethod::Unknown => {
            show_update_failure_warning(
                "Update skipped: could not determine install method. Update manually instead of using the shell installer.",
            );
        }
    }
}

async fn confirm_update(version: Version) -> bool {
    let answer = FurcodeWidget::confirm(format!(
        "Confirm upgrade from {} -> {} (latest)?",
        VERSION.to_string().bold().white(),
        version.to_string().bold().white()
    ))
    .with_default(true)
    .prompt();

    match answer {
        Ok(Some(result)) => result,
        Ok(None) => false,
        Err(_) => false,
    }
}

fn should_check_for_updates(frequency: &UpdateFrequency) -> bool {
    !matches!(frequency, UpdateFrequency::Never)
}

fn preflight_checks(env: &Environment) -> Result<(), UpdateError> {
    let install_method = InstallMethod::detect(env);

    match install_method {
        InstallMethod::Homebrew | InstallMethod::Npm => {
            return Err(UpdateError::UnsupportedInstallMethod { install_method });
        }
        _ => {}
    }

    if let Ok(current_exe) = env::current_exe() {
        let metadata = std::fs::metadata(&current_exe).map_err(|_| UpdateError::BinaryNotWritable)?;
        let permissions = metadata.permissions();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if permissions.mode() & 0o200 == 0 {
                return Err(UpdateError::BinaryNotWritable);
            }
        }

        let on_path = env::var("PATH")
            .map(|path| {
                path.split(std::path::MAIN_SEPARATOR)
                    .any(|dir| {
                        let Ok(canonical_dir) = std::fs::canonicalize(dir) else { return false };
                        canonical_dir == current_exe.parent().unwrap_or_else(|| Path::new(""))
                    })
            })
            .unwrap_or(false);

        if !on_path {
            return Err(UpdateError::NotOnPath);
        }
    }

    Ok(())
}

/// Checks if there is an update available
pub async fn on_update(api: Arc<impl API>, update: Option<&Update>, env: Environment) {
    let update = update.cloned().unwrap_or_default();
    let frequency = update.frequency.unwrap_or_default();

    if !should_check_for_updates(&frequency) {
        return;
    }

    let auto_update = update.auto_update.unwrap_or_default();
    let install_method = InstallMethod::detect(&env);

    if let Err(err) = preflight_checks(&env) {
        let _ = send_update_failure_event(&err.to_string()).await;
        show_update_failure_warning(&format!("Update preflight failed: {err}"));
        if matches!(err, UpdateError::UnsupportedInstallMethod { .. }) {
            show_update_failure_warning(
                "Use your package manager for updates.",
            );
        }
        return;
    }

    // Check if version is development version, in which case we skip the update check
    if VERSION.contains("dev") || VERSION == "0.1.0" {
        return;
    }

    let informer = update_informer::new(registry::GitHub, "rizrmd/furcode", VERSION)
        .interval(frequency.into());

    if let Some(version) = informer.check_version().ok().flatten()
        && (auto_update || confirm_update(version).await)
    {
        route_update_by_install_method(api, install_method, auto_update).await;
    }
}

async fn send_update_failure_event(error_msg: &str) -> anyhow::Result<()> {
    tracing::error!(error = error_msg, "Update failed");
    Ok(())
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn test_should_skip_update_check_when_frequency_is_never() {
        let fixture = UpdateFrequency::Never;

        let actual = should_check_for_updates(&fixture);

        let expected = false;
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_install_method_detection_homebrew() {
        let fixture = Environment {
            os: "macos".to_string(),
            cwd: PathBuf::from("/tmp"),
            home: Some(PathBuf::from("/home/user")),
            shell: "/bin/zsh".to_string(),
            base_path: PathBuf::from("/home/user/.furcode"),
        };

        let actual = InstallMethod::detect(&fixture);
        let expected = InstallMethod::Homebrew;
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_install_method_detection_windows() {
        let fixture = Environment {
            os: "windows".to_string(),
            cwd: PathBuf::from("C:\\Users\\User"),
            home: Some(PathBuf::from("C:\\Users\\User")),
            shell: "powershell".to_string(),
            base_path: PathBuf::from("C:\\Users\\User\\.furcode"),
        };

        let actual = InstallMethod::detect(&fixture);
        let expected = InstallMethod::WindowsBinary;
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_preflight_blocks_homebrew() {
        let fixture = Environment {
            os: "macos".to_string(),
            cwd: PathBuf::from("/tmp"),
            home: Some(PathBuf::from("/home/user")),
            shell: "/bin/zsh".to_string(),
            base_path: PathBuf::from("/home/user/.furcode"),
        };

        let actual = preflight_checks(&fixture);
        assert!(actual.is_err());
    }
}
