//! Exact frame-TUI transcript snapshots for `/resume`.
//!
//! Rebuild-from-context is lossy (no tool timings, collapsed bodies, truncated
//! prose, only last N turns). Snapshots store the live [`TranscriptLine`] list
//! byte-for-byte so leave and resume paint the same surface.

use std::fs;
use std::path::Path;

use furcode_domain::Environment;
use serde::{Deserialize, Serialize};

use super::view::TranscriptLine;

const SNAPSHOT_VERSION: u32 = 1;

/// On-disk payload written after each idle turn and on session exit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptSnapshot {
    pub version: u32,
    pub conversation_id: String,
    pub lines: Vec<TranscriptLine>,
}

/// Persist the live transcript for exact resume later.
///
/// # Errors
/// Returns I/O or serialization errors. Callers should log and continue —
/// snapshot failure must never break the interactive session.
pub fn save_transcript(
    env: &Environment,
    conversation_id: &str,
    lines: &[TranscriptLine],
) -> anyhow::Result<()> {
    let dir = env.transcript_dir();
    fs::create_dir_all(&dir)?;
    let path = env.transcript_path(conversation_id);
    let snap = TranscriptSnapshot {
        version: SNAPSHOT_VERSION,
        conversation_id: conversation_id.to_string(),
        lines: lines.to_vec(),
    };
    let json = serde_json::to_string(&snap)?;
    // Atomic-ish write: temp then rename.
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, json)?;
    fs::rename(&tmp, &path)?;
    Ok(())
}

/// Load a previously saved transcript snapshot, if any.
///
/// Returns `None` when the file is missing or unreadable (caller falls back to
/// slim context rebuild).
pub fn load_transcript(env: &Environment, conversation_id: &str) -> Option<Vec<TranscriptLine>> {
    let path = env.transcript_path(conversation_id);
    load_transcript_path(&path, conversation_id)
}

fn load_transcript_path(path: &Path, conversation_id: &str) -> Option<Vec<TranscriptLine>> {
    let raw = fs::read_to_string(path).ok()?;
    let snap: TranscriptSnapshot = serde_json::from_str(&raw).ok()?;
    if snap.version != SNAPSHOT_VERSION {
        tracing::warn!(
            path = %path.display(),
            version = snap.version,
            "unsupported transcript snapshot version"
        );
        return None;
    }
    if !snap.conversation_id.is_empty() && snap.conversation_id != conversation_id {
        tracing::warn!(
            path = %path.display(),
            expected = conversation_id,
            found = %snap.conversation_id,
            "transcript snapshot conversation id mismatch"
        );
        return None;
    }
    Some(snap.lines)
}

/// Remove a snapshot (e.g. after conversation delete). Best-effort.
pub fn delete_transcript(env: &Environment, conversation_id: &str) {
    let path = env.transcript_path(conversation_id);
    let _ = fs::remove_file(path);
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use pretty_assertions::assert_eq;
    use tempfile::TempDir;

    use super::*;
    use crate::tui::view::TranscriptKind;

    fn test_env(base: PathBuf) -> Environment {
        Environment {
            os: "test".into(),
            cwd: base.clone(),
            home: Some(base.clone()),
            shell: "sh".into(),
            base_path: base,
        }
    }

    #[test]
    fn round_trip_preserves_lines() {
        let tmp = TempDir::new().unwrap();
        let env = test_env(tmp.path().to_path_buf());
        let id = "550e8400-e29b-41d4-a716-446655440000";
        let lines = vec![
            TranscriptLine {
                kind: TranscriptKind::User,
                text: "implement auto update".into(),
            },
            TranscriptLine {
                kind: TranscriptKind::UserTime,
                text: "22:09".into(),
            },
            TranscriptLine {
                kind: TranscriptKind::Spacer,
                text: String::new(),
            },
            TranscriptLine {
                kind: TranscriptKind::Thought,
                text: "Thought for 8.9s".into(),
            },
            TranscriptLine {
                kind: TranscriptKind::ToolDone,
                text: "Read plans/auto-update.md · 0.2s".into(),
            },
            TranscriptLine {
                kind: TranscriptKind::Assistant,
                text: "Done.".into(),
            },
            TranscriptLine {
                kind: TranscriptKind::Info,
                text: "Turn complete · model finished (no more tools) · send a message to continue"
                    .into(),
            },
        ];

        save_transcript(&env, id, &lines).unwrap();
        let loaded = load_transcript(&env, id).unwrap();
        assert_eq!(loaded, lines);
    }

    #[test]
    fn missing_file_returns_none() {
        let tmp = TempDir::new().unwrap();
        let env = test_env(tmp.path().to_path_buf());
        assert!(load_transcript(&env, "does-not-exist").is_none());
    }
}
