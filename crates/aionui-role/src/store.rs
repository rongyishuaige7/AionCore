//! On-disk layout of resident roles. Callers pass ids that already went
//! through `service::validate_role_id`; the store itself never validates.
//!
//! An id may contain `:` (`bare:<agent id>`), which is awkward in a file name,
//! so a role's directory spells it `@`. Validation never lets `@` into an id,
//! which keeps the mapping reversible.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::RoleError;

const DEFINITION_FILE: &str = "role.json";
const MEMORY_DIR: &str = "memory";
const MEMORY_FILE: &str = "MEMORY.md";
const PROGRESS_DIR: &str = "progress";
const PROGRESS_EXTENSION: &str = "md";
const ID_SEPARATOR: &str = ":";
const DIR_SEPARATOR: &str = "@";

/// Contents of `role.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RoleDefinition {
    #[serde(default)]
    pub charter: String,
    #[serde(default)]
    pub denied_tools: Vec<String>,
    /// Whether the role reviews work other members hand in. Absent in role
    /// files written before the flag existed.
    #[serde(default)]
    pub verifies: bool,
    pub created_at: i64,
    pub updated_at: i64,
}

/// One task file under `memory/progress/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProgressFile {
    /// The file's name without its extension.
    pub task: String,
    pub state: String,
    /// Last write, in milliseconds since the epoch.
    pub updated_at: i64,
}

#[derive(Debug, Clone)]
pub(crate) struct RoleStore {
    root: PathBuf,
}

impl RoleStore {
    pub(crate) fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn role_dir(&self, id: &str) -> PathBuf {
        self.root.join(id.replace(ID_SEPARATOR, DIR_SEPARATOR))
    }

    pub(crate) fn memory_dir(&self, id: &str) -> PathBuf {
        self.role_dir(id).join(MEMORY_DIR)
    }

    /// Ids of every directory under the root, sorted. A directory is only a
    /// role once it also holds a definition file.
    pub(crate) async fn list_ids(&self) -> Result<Vec<String>, RoleError> {
        let mut entries = match tokio::fs::read_dir(&self.root).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let mut ids = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            if !entry.file_type().await?.is_dir() {
                continue;
            }
            if let Ok(name) = entry.file_name().into_string() {
                ids.push(name.replace(DIR_SEPARATOR, ID_SEPARATOR));
            }
        }
        ids.sort();
        Ok(ids)
    }

    pub(crate) async fn read_definition(&self, id: &str) -> Result<Option<RoleDefinition>, RoleError> {
        let path = self.role_dir(id).join(DEFINITION_FILE);
        let raw = match tokio::fs::read_to_string(&path).await {
            Ok(raw) => raw,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        serde_json::from_str(&raw)
            .map(Some)
            .map_err(|error| RoleError::Corrupt(format!("{id}: {error}")))
    }

    pub(crate) async fn write_definition(&self, id: &str, definition: &RoleDefinition) -> Result<(), RoleError> {
        let raw =
            serde_json::to_vec_pretty(definition).map_err(|error| RoleError::Corrupt(format!("{id}: {error}")))?;
        write_atomic(&self.role_dir(id).join(DEFINITION_FILE), &raw).await?;
        tokio::fs::create_dir_all(self.memory_dir(id)).await?;
        Ok(())
    }

    /// Remove the definition file only. The memory directory is kept so that
    /// dismissing a role never destroys what it has learned; defining the role
    /// again picks the memory back up. Returns `false` when there was no role.
    pub(crate) async fn remove_definition(&self, id: &str) -> Result<bool, RoleError> {
        match tokio::fs::remove_file(self.role_dir(id).join(DEFINITION_FILE)).await {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    pub(crate) async fn read_memory(&self, id: &str) -> Result<String, RoleError> {
        match tokio::fs::read_to_string(self.memory_dir(id).join(MEMORY_FILE)).await {
            Ok(memory) => Ok(memory),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(String::new()),
            Err(error) => Err(error.into()),
        }
    }

    pub(crate) async fn write_memory(&self, id: &str, memory: &str) -> Result<(), RoleError> {
        write_atomic(&self.memory_dir(id).join(MEMORY_FILE), memory.as_bytes()).await
    }

    fn progress_dir(&self, id: &str) -> PathBuf {
        self.memory_dir(id).join(PROGRESS_DIR)
    }

    fn progress_path(&self, id: &str, task: &str) -> PathBuf {
        self.progress_dir(id).join(format!("{task}.{PROGRESS_EXTENSION}"))
    }

    /// Every task file of the role, most recently written first. Files the
    /// user put there by hand are listed like the ones the role wrote.
    pub(crate) async fn list_progress(&self, id: &str) -> Result<Vec<ProgressFile>, RoleError> {
        let mut entries = match tokio::fs::read_dir(self.progress_dir(id)).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let mut files = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some(PROGRESS_EXTENSION) {
                continue;
            }
            let Some(task) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            let metadata = entry.metadata().await?;
            if !metadata.is_file() {
                continue;
            }
            let raw = tokio::fs::read(&path).await?;
            files.push(ProgressFile {
                task: task.to_owned(),
                state: String::from_utf8_lossy(&raw).into_owned(),
                updated_at: modified_ms(&metadata),
            });
        }
        files.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then_with(|| a.task.cmp(&b.task)));
        Ok(files)
    }

    pub(crate) async fn write_progress(&self, id: &str, task: &str, state: &str) -> Result<(), RoleError> {
        write_atomic(&self.progress_path(id, task), state.as_bytes()).await
    }

    /// Returns `false` when the task had no file.
    pub(crate) async fn remove_progress(&self, id: &str, task: &str) -> Result<bool, RoleError> {
        match tokio::fs::remove_file(self.progress_path(id, task)).await {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        }
    }
}

/// A file's last write in milliseconds since the epoch, `0` where the platform
/// does not say.
fn modified_ms(metadata: &std::fs::Metadata) -> i64 {
    metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |elapsed| i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX))
}

/// Write through a sibling temp file and rename, so a reader (a member being
/// woken) never sees a half-written definition or memory.
async fn write_atomic(path: &Path, contents: &[u8]) -> Result<(), RoleError> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let tmp = path.with_extension("tmp");
    tokio::fs::write(&tmp, contents).await?;
    tokio::fs::rename(&tmp, path).await?;
    Ok(())
}
