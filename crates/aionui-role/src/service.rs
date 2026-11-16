//! Business rules for resident roles: id and input validation, and the mapping
//! between the on-disk definition and the wire shape.

use std::path::PathBuf;
use std::sync::Arc;

use aionui_api_types::{RoleProgress, RoleReport, RoleReportOutcome, RoleResponse, TeamToolName, UpsertRoleRequest};
use aionui_common::now_ms;
use tokio::sync::Mutex;

use crate::error::RoleError;
use crate::store::{RoleDefinition, RoleStore};

/// Cap on a charter, in characters.
pub const MAX_CHARTER_CHARS: usize = 4_000;
/// Cap on `MEMORY.md`, in bytes.
pub const MAX_MEMORY_BYTES: usize = 64 * 1024;
/// Cap on one note added through [`RoleService::remember`], in characters.
pub const MAX_NOTE_CHARS: usize = 600;
/// Replaced memories kept under `memory/archive/`; older ones are dropped.
pub const MAX_MEMORY_ARCHIVES: usize = 10;
/// Cap on the state of one task in progress, in characters.
pub const MAX_PROGRESS_STATE_CHARS: usize = 4_000;
/// Cap on how many tasks a role keeps in progress at once.
pub const MAX_PROGRESS_TASKS: usize = 40;
/// Cap on a task name, in characters.
pub const MAX_TASK_NAME_CHARS: usize = 48;
/// Cap on the one line a role tells the user at the end of a run, in characters.
pub const MAX_REPORT_SUMMARY_CHARS: usize = 300;
/// How many of the things a role told the user are kept; older ones are dropped.
pub const MAX_KEPT_REPORTS: usize = 30;
const MAX_ROLE_ID_LEN: usize = 128;

/// What [`RoleService::remember`] did with a note.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RememberedNote {
    /// The line as it stands in `MEMORY.md`.
    pub line: String,
    /// `true` when the memory already held this note and nothing was written.
    pub already_recorded: bool,
}

/// What [`RoleService::curate_memory`] did with the memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CuratedMemory {
    /// File the replaced memory was kept as, under `memory/archive/`. `None`
    /// when nothing was replaced: the memory was empty or already read the same.
    pub archived_as: Option<String>,
    pub bytes_before: usize,
    pub bytes_after: usize,
}

/// What [`RoleService::set_progress`] did with a task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgressUpdate {
    /// The task's name as it is kept, which is also its file name.
    pub task: String,
    /// `true` when the task was closed and its file removed.
    pub closed: bool,
}

#[derive(Debug, Clone)]
pub struct RoleService {
    store: RoleStore,
    /// Serializes memory writes: members of one role can run in several teams
    /// at once, and the user can save the memory from the roles page meanwhile.
    memory_lock: Arc<Mutex<()>>,
}

impl RoleService {
    /// `root` is the directory that holds one sub-directory per role
    /// (`<data_dir>/roles`). It is created on first write.
    pub fn new(root: PathBuf) -> Self {
        Self {
            store: RoleStore::new(root),
            memory_lock: Arc::new(Mutex::new(())),
        }
    }

    /// Every defined role, oldest first. A role whose definition file cannot be
    /// read is skipped (and logged) rather than failing the whole list.
    pub async fn list(&self) -> Result<Vec<RoleResponse>, RoleError> {
        let mut roles = Vec::new();
        for id in self.store.list_ids().await? {
            if validate_role_id(&id).is_err() {
                continue;
            }
            match self.find(&id).await {
                Ok(Some(role)) => roles.push(role),
                Ok(None) => {}
                Err(error) => tracing::warn!(role_id = %id, error = %error, "role: skipped unreadable role"),
            }
        }
        roles.sort_by(|a, b| a.created_at.cmp(&b.created_at).then_with(|| a.id.cmp(&b.id)));
        Ok(roles)
    }

    pub async fn get(&self, id: &str) -> Result<RoleResponse, RoleError> {
        self.find(id).await?.ok_or_else(|| RoleError::NotFound(id.to_owned()))
    }

    /// The role built on `id`, or `None` when that assistant is not a role.
    pub async fn find(&self, id: &str) -> Result<Option<RoleResponse>, RoleError> {
        validate_role_id(id)?;
        let Some(definition) = self.store.read_definition(id).await? else {
            return Ok(None);
        };
        Ok(Some(self.to_response(id, definition).await?))
    }

    /// Create the role or replace its definition. The memory is left untouched,
    /// and so is `created_at` of an existing role.
    pub async fn upsert(&self, id: &str, request: UpsertRoleRequest) -> Result<RoleResponse, RoleError> {
        validate_role_id(id)?;
        let charter = request.charter.trim().to_owned();
        if charter.chars().count() > MAX_CHARTER_CHARS {
            return Err(RoleError::InvalidRequest(format!(
                "charter exceeds {MAX_CHARTER_CHARS} characters"
            )));
        }
        let denied_tools = normalize_denied_tools(&request.denied_tools)?;

        // A corrupt definition must not block the user from saving a good one.
        let existing = match self.store.read_definition(id).await {
            Ok(existing) => existing,
            Err(RoleError::Corrupt(_)) => None,
            Err(error) => return Err(error),
        };
        let now = now_ms();
        let definition = RoleDefinition {
            charter,
            denied_tools,
            verifies: request.verifies,
            asks_when_scheduled: request.asks_when_scheduled,
            created_at: existing.map_or(now, |existing| existing.created_at),
            updated_at: now,
        };
        self.store.write_definition(id, &definition).await?;
        self.to_response(id, definition).await
    }

    /// Replace the role's `MEMORY.md`.
    pub async fn set_memory(&self, id: &str, memory: &str) -> Result<RoleResponse, RoleError> {
        validate_role_id(id)?;
        if memory.len() > MAX_MEMORY_BYTES {
            return Err(RoleError::InvalidRequest(format!(
                "memory exceeds {MAX_MEMORY_BYTES} bytes"
            )));
        }
        let Some(definition) = self.store.read_definition(id).await? else {
            return Err(RoleError::NotFound(id.to_owned()));
        };
        let _guard = self.memory_lock.lock().await;
        self.store.write_memory(id, memory).await?;
        self.to_response(id, definition).await
    }

    /// Append one note to the role's `MEMORY.md` as `- <day> <note>`. The note
    /// is flattened to a single line, and a note the memory already holds is
    /// not written twice. `day` is the caller's local date (`YYYY-MM-DD`).
    pub async fn remember(&self, id: &str, note: &str, day: &str) -> Result<RememberedNote, RoleError> {
        validate_role_id(id)?;
        let note = note.split_whitespace().collect::<Vec<_>>().join(" ");
        if note.is_empty() {
            return Err(RoleError::InvalidRequest("a note cannot be empty".to_owned()));
        }
        if note.chars().count() > MAX_NOTE_CHARS {
            return Err(RoleError::InvalidRequest(format!(
                "a note exceeds {MAX_NOTE_CHARS} characters"
            )));
        }
        if self.store.read_definition(id).await?.is_none() {
            return Err(RoleError::NotFound(id.to_owned()));
        }

        let _guard = self.memory_lock.lock().await;
        let memory = self.store.read_memory(id).await?;
        if let Some(line) = memory.lines().find(|line| note_text(line) == note) {
            return Ok(RememberedNote {
                line: line.trim().to_owned(),
                already_recorded: true,
            });
        }
        let line = format!("- {day} {note}");
        let kept = memory.trim_end();
        let updated = if kept.is_empty() {
            format!("{line}\n")
        } else {
            format!("{kept}\n{line}\n")
        };
        if updated.len() > MAX_MEMORY_BYTES {
            return Err(RoleError::InvalidRequest(format!(
                "memory is full ({MAX_MEMORY_BYTES} bytes), the user has to prune it first"
            )));
        }
        self.store.write_memory(id, &updated).await?;
        Ok(RememberedNote {
            line,
            already_recorded: false,
        })
    }

    /// Replace the whole memory with a tidied version of itself: what a role
    /// does on a curation run, merging what repeats and dropping what no longer
    /// holds. Unlike the user's own edit through [`Self::set_memory`], nobody is
    /// watching, so the memory it replaces is first kept under
    /// `memory/archive/` and an empty memory is refused. `stamp` names that
    /// copy (the caller's local time, digits and `-` only).
    pub async fn curate_memory(&self, id: &str, memory: &str, stamp: &str) -> Result<CuratedMemory, RoleError> {
        validate_role_id(id)?;
        let curated = memory.trim_end();
        if curated.trim().is_empty() {
            return Err(RoleError::InvalidRequest("a curated memory cannot be empty".to_owned()));
        }
        let curated = format!("{curated}\n");
        if curated.len() > MAX_MEMORY_BYTES {
            return Err(RoleError::InvalidRequest(format!(
                "memory exceeds {MAX_MEMORY_BYTES} bytes"
            )));
        }
        if stamp.is_empty() || !stamp.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return Err(RoleError::InvalidRequest("invalid archive stamp".to_owned()));
        }
        if self.store.read_definition(id).await?.is_none() {
            return Err(RoleError::NotFound(id.to_owned()));
        }

        let _guard = self.memory_lock.lock().await;
        let previous = self.store.read_memory(id).await?;
        let bytes_before = previous.len();
        if previous.trim_end() == curated.trim_end() {
            return Ok(CuratedMemory {
                archived_as: None,
                bytes_before,
                bytes_after: bytes_before,
            });
        }
        let archived_as = if previous.trim().is_empty() {
            None
        } else {
            Some(
                self.store
                    .archive_memory(id, &previous, stamp, MAX_MEMORY_ARCHIVES)
                    .await?,
            )
        };
        self.store.write_memory(id, &curated).await?;
        Ok(CuratedMemory {
            archived_as,
            bytes_before,
            bytes_after: curated.len(),
        })
    }

    /// Record where a task stands, replacing what was recorded for it before.
    /// An empty `state` closes the task: its file is removed, and closing a
    /// task that has none is not an error.
    pub async fn set_progress(&self, id: &str, task: &str, state: &str) -> Result<ProgressUpdate, RoleError> {
        validate_role_id(id)?;
        let task = task_file_name(task)?;
        let state = state.trim();
        if state.chars().count() > MAX_PROGRESS_STATE_CHARS {
            return Err(RoleError::InvalidRequest(format!(
                "a task's state exceeds {MAX_PROGRESS_STATE_CHARS} characters"
            )));
        }
        if self.store.read_definition(id).await?.is_none() {
            return Err(RoleError::NotFound(id.to_owned()));
        }

        let _guard = self.memory_lock.lock().await;
        if state.is_empty() {
            self.store.remove_progress(id, &task).await?;
            return Ok(ProgressUpdate { task, closed: true });
        }
        let open = self.store.list_progress(id).await?;
        if open.len() >= MAX_PROGRESS_TASKS && !open.iter().any(|file| file.task == task) {
            return Err(RoleError::InvalidRequest(format!(
                "{MAX_PROGRESS_TASKS} tasks are already in progress, close one first"
            )));
        }
        self.store.write_progress(id, &task, &format!("{state}\n")).await?;
        Ok(ProgressUpdate { task, closed: false })
    }

    /// Keep what the role told the user at the end of a run that nobody
    /// watched, so that the next run knows it has been said and the user finds
    /// it when they come back. A silent run tells the user nothing and leaves
    /// nothing behind: `None`. The summary is flattened to one line; `say` and
    /// `ask` need one.
    pub async fn record_report(
        &self,
        id: &str,
        outcome: RoleReportOutcome,
        summary: &str,
        conversation_id: &str,
        at: i64,
    ) -> Result<Option<RoleReport>, RoleError> {
        validate_role_id(id)?;
        let summary = summary.split_whitespace().collect::<Vec<_>>().join(" ");
        if summary.chars().count() > MAX_REPORT_SUMMARY_CHARS {
            return Err(RoleError::InvalidRequest(format!(
                "a summary exceeds {MAX_REPORT_SUMMARY_CHARS} characters; the detail belongs in your reply"
            )));
        }
        if outcome == RoleReportOutcome::Silent {
            return Ok(None);
        }
        if summary.is_empty() {
            return Err(RoleError::InvalidRequest(format!(
                "`{}` needs a summary: the one line the user sees first",
                outcome.as_str()
            )));
        }
        if self.store.read_definition(id).await?.is_none() {
            return Err(RoleError::NotFound(id.to_owned()));
        }

        let report = RoleReport {
            at,
            outcome,
            summary,
            conversation_id: conversation_id.to_owned(),
        };
        let _guard = self.memory_lock.lock().await;
        let mut reports = self.store.read_reports(id).await?;
        reports.push(report.clone());
        let dropped = reports.len().saturating_sub(MAX_KEPT_REPORTS);
        self.store.write_reports(id, &reports[dropped..]).await?;
        Ok(Some(report))
    }

    /// Dismiss the role. Its memory directory stays on disk and is picked up
    /// again if the role is defined anew.
    pub async fn remove(&self, id: &str) -> Result<(), RoleError> {
        validate_role_id(id)?;
        if self.store.remove_definition(id).await? {
            Ok(())
        } else {
            Err(RoleError::NotFound(id.to_owned()))
        }
    }

    async fn to_response(&self, id: &str, definition: RoleDefinition) -> Result<RoleResponse, RoleError> {
        Ok(RoleResponse {
            id: id.to_owned(),
            charter: definition.charter,
            denied_tools: definition.denied_tools,
            verifies: definition.verifies,
            asks_when_scheduled: definition.asks_when_scheduled,
            memory: self.store.read_memory(id).await?,
            memory_dir: self.store.memory_dir(id).to_string_lossy().into_owned(),
            progress: self
                .store
                .list_progress(id)
                .await?
                .into_iter()
                .map(|file| RoleProgress {
                    task: file.task,
                    state: file.state,
                    updated_at: file.updated_at,
                })
                .collect(),
            reported: {
                let mut reported = self.store.read_reports(id).await?;
                reported.reverse();
                reported
            },
            created_at: definition.created_at,
            updated_at: definition.updated_at,
        })
    }
}

/// A role id is an assistant id and becomes a directory name, so it is
/// restricted to `[A-Za-z0-9._:-]`, not starting with a dot. That covers every
/// assistant id the backend issues: `builtin-office`, `custom-<ms>-<hex>` and
/// `bare:<agent id>`. The store maps `:` to a filename-safe character.
fn validate_role_id(id: &str) -> Result<(), RoleError> {
    let plain = id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'));
    if id.is_empty() || id.len() > MAX_ROLE_ID_LEN || !plain || id.starts_with('.') {
        return Err(RoleError::InvalidId(id.chars().take(MAX_ROLE_ID_LEN).collect()));
    }
    Ok(())
}

/// A task name as its file name: lowercased, with every run of characters that
/// are not letters, digits or `_` turned into one `-`. Letters and digits of
/// any script are kept, so a task can be named in the user's own language, and
/// nothing that could leave the progress directory survives.
fn task_file_name(task: &str) -> Result<String, RoleError> {
    let mut name = String::new();
    for c in task.trim().chars().flat_map(char::to_lowercase) {
        if c.is_alphanumeric() || c == '_' {
            name.push(c);
        } else if !name.is_empty() && !name.ends_with('-') {
            name.push('-');
        }
    }
    let name = name.trim_end_matches('-');
    if name.is_empty() {
        return Err(RoleError::InvalidRequest(
            "a task name needs at least one letter or digit".to_owned(),
        ));
    }
    if name.chars().count() > MAX_TASK_NAME_CHARS {
        return Err(RoleError::InvalidRequest(format!(
            "a task name exceeds {MAX_TASK_NAME_CHARS} characters"
        )));
    }
    Ok(name.to_owned())
}

/// The text of a memory line without its bullet and its day, so the same note
/// is recognized whenever it was written and whoever wrote it.
fn note_text(line: &str) -> &str {
    let line = line.trim();
    let text = line.strip_prefix("- ").unwrap_or(line).trim_start();
    match text.split_once(' ') {
        Some((first, rest)) if is_day(first) => rest.trim_start(),
        _ => text,
    }
}

/// `YYYY-MM-DD`.
fn is_day(text: &str) -> bool {
    text.len() == 10
        && text.bytes().enumerate().all(|(index, byte)| match index {
            4 | 7 => byte == b'-',
            _ => byte.is_ascii_digit(),
        })
}

/// Trim, reject anything that is not a real team tool, then sort and dedupe so
/// the stored table is canonical.
fn normalize_denied_tools(tools: &[String]) -> Result<Vec<String>, RoleError> {
    let mut normalized = Vec::with_capacity(tools.len());
    for tool in tools {
        let tool = tool.trim();
        if TeamToolName::parse(tool).is_none() {
            return Err(RoleError::InvalidRequest(format!("unknown team tool: {tool}")));
        }
        normalized.push(tool.to_owned());
    }
    normalized.sort();
    normalized.dedup();
    Ok(normalized)
}

#[cfg(test)]
#[path = "service_test.rs"]
mod service_test;
