//! Business rules for resident roles: id and input validation, and the mapping
//! between the on-disk definition and the wire shape.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use aionui_api_types::{
    KeeperBrief, PlaybookDeltaOp, ROLE_SEND_TOOL_NAME, RoleCommit, RoleExpandRequest, RoleExpandResponse, RoleKeeper,
    RoleKeptProject, RoleProgress, RoleProjectClaim, RoleProposal, RoleProposalChange, RoleRecallRequest,
    RoleRecallResponse, RoleReport, RoleReportOutcome, RoleResponse, SetRoleProjectsRequest, TeamToolName,
    UpsertRoleRequest,
};
use aionui_common::now_ms;
use tokio::sync::Mutex;

use crate::brief::{self, BriefSources};
use crate::error::RoleError;
use crate::git::{self, BOSS_TRACKED_PATHS, ROLE_TRACKED_PATHS};
use crate::playbook::{
    PROGRESS_ARCHIVE_IDLE_DAYS, append_archived_playbook, apply_boss_deltas, apply_playbook_deltas, decay_playbook,
    parse_playbook, prompt_playbook_lines, render_playbook,
};
use crate::proposals::{classify_role_update, new_proposal_id};
use crate::recall::{RecallIndex, RecallSync, RecallWatermark};
pub use crate::store::ThreadMark;
use crate::store::{RoleDefinition, RoleStore, ThreadRecord};

#[path = "debrief.rs"]
mod debrief;
pub use debrief::{DebriefRecord, MAX_KEPT_DEBRIEFS};

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
/// Cap on how many projects one role keeps.
pub const MAX_KEPT_PROJECTS: usize = 20;
/// Cap on the project name a role keeps beside a project id, in characters.
pub const MAX_KEPT_PROJECT_NAME_CHARS: usize = 80;
const MAX_PROJECT_ID_LEN: usize = 128;

/// What [`RoleService::remember`] did with a note.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RememberedNote {
    /// The line as it stands in `MEMORY.md`.
    pub line: String,
    /// `true` when the memory already held this note and nothing was written.
    pub already_recorded: bool,
}

/// What [`RoleService::curate_memory`] did with the memory.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CuratedMemory {
    /// File the replaced memory was kept as, under `memory/archive/`. `None`
    /// when nothing was replaced: the memory was empty or already read the same.
    pub archived_as: Option<String>,
    pub bytes_before: usize,
    pub bytes_after: usize,
}

/// Where the summaries of a role's thread stand, as
/// [`RoleService::thread_upkeep`] reads them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadUpkeep {
    /// The last message the summaries cover; `None` before the first one.
    pub summarized_to: Option<ThreadMark>,
    /// When the thread last moved to a fresh session (ms), 0 if never.
    pub rotated_at: i64,
    /// Every summary of the thread, oldest first.
    pub leaves: Vec<String>,
}

/// What [`RoleService::set_progress`] did with a task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgressUpdate {
    /// The task's name as it is kept, which is also its file name.
    pub task: String,
    /// `true` when the task was closed and its file removed.
    pub closed: bool,
}

/// What a turn the user takes with the role outside any schedule or team
/// brings along, as [`RoleService::open_thread_turn`] decides it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThreadTurn {
    /// The conversation is a side conversation, not the role's thread.
    Aside,
    /// The conversation is the role's thread. `reports` are what the role told
    /// the user from its runs elsewhere since the thread last heard, oldest
    /// first. `adopted` is set on the turn that made the conversation the
    /// thread.
    Thread { reports: Vec<RoleReport>, adopted: bool },
}

#[derive(Debug, Clone)]
pub struct RoleService {
    store: RoleStore,
    recall: RecallIndex,
    /// Serializes memory writes: members of one role can run in several teams
    /// at once, and the user can save the memory from the roles page meanwhile.
    memory_lock: Arc<Mutex<()>>,
}

impl RoleService {
    /// `root` is the directory that holds one sub-directory per role
    /// (`<data_dir>/roles`). It is created on first write.
    pub fn new(root: PathBuf) -> Self {
        let recall = RecallIndex::new(&root);
        Self {
            store: RoleStore::new(root),
            recall,
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

    /// Create the role or replace its definition (human user caller). The
    /// memory is left untouched, and so is `created_at` of an existing role.
    pub async fn upsert(&self, id: &str, request: UpsertRoleRequest) -> Result<RoleResponse, RoleError> {
        self.upsert_by_caller(id, request, None).await
    }

    /// Create or update the role definition. When `agent_conversation_id` is
    /// `Some(conv_id)` and the role already exists, tightening changes apply
    /// immediately while major changes (modifying `charter`, relaxing
    /// `denied_tools`, turning off `asks_when_scheduled` or `verifies`) are held
    /// as a pending `RoleProposal` until the user approves them.
    pub async fn upsert_by_caller(
        &self,
        id: &str,
        request: UpsertRoleRequest,
        agent_conversation_id: Option<&str>,
    ) -> Result<RoleResponse, RoleError> {
        validate_role_id(id)?;
        let charter = request.charter.trim().to_owned();
        if charter.chars().count() > MAX_CHARTER_CHARS {
            return Err(RoleError::InvalidRequest(format!(
                "charter exceeds {MAX_CHARTER_CHARS} characters"
            )));
        }
        let denied_tools = normalize_denied_tools(&request.denied_tools)?;

        let _guard = self.memory_lock.lock().await;
        // A corrupt definition must not block the user from saving a good one.
        let existing = match self.store.read_definition(id).await {
            Ok(existing) => existing,
            Err(RoleError::Corrupt(_)) => None,
            Err(error) => return Err(error),
        };
        let now = now_ms();
        let role_dir = self.store.role_dir(id);
        git::ensure_baseline(role_dir.clone(), ROLE_TRACKED_PATHS, now).await?;

        if let (Some(existing_def), Some(conv_id)) = (&existing, agent_conversation_id) {
            let classified = classify_role_update(
                existing_def,
                charter,
                denied_tools,
                request.verifies,
                request.asks_when_scheduled,
                now,
            );
            let mut effective = existing_def.clone();
            if let Some((tightened, reason)) = classified.immediate {
                self.store.write_definition(id, &tightened).await?;
                git::commit(role_dir.clone(), ROLE_TRACKED_PATHS, "agent".into(), reason, now).await?;
                effective = tightened;
            }
            if let Some((summary, detail, change)) = classified.proposal {
                self.stage_proposal_locked(id, summary, detail, Some(conv_id.to_owned()), change, now)
                    .await?;
            }
            return self.to_response(id, effective).await;
        }

        let is_new = existing.is_none();
        // What the role keeps is set apart from its charter, so saving the charter leaves it alone.
        let (created_at, projects) =
            existing.map_or((now, Vec::new()), |existing| (existing.created_at, existing.projects));
        let definition = RoleDefinition {
            charter,
            denied_tools,
            verifies: request.verifies,
            asks_when_scheduled: request.asks_when_scheduled,
            projects,
            created_at,
            updated_at: now,
        };
        self.store.write_definition(id, &definition).await?;
        let (actor, msg) = match (agent_conversation_id.is_some(), is_new) {
            (true, _) => ("agent", "role: hired resident role"),
            (false, true) => ("user", "role: created resident role"),
            (false, false) => ("user", "role: updated charter and permissions"),
        };
        git::commit(role_dir, ROLE_TRACKED_PATHS, actor.into(), msg.into(), now).await?;
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
        let now = now_ms();
        let role_dir = self.store.role_dir(id);
        git::ensure_baseline(role_dir.clone(), ROLE_TRACKED_PATHS, now).await?;
        self.store.write_memory(id, memory).await?;
        git::commit(
            role_dir,
            ROLE_TRACKED_PATHS,
            "user".into(),
            "memory: edited memory".into(),
            now,
        )
        .await?;
        self.to_response(id, definition).await
    }

    /// Append one note to the role's `MEMORY.md` as `- <day> <note>`. The note
    /// is flattened to a single line, and a note the memory already holds is
    /// not written twice. `day` is the caller's local date (`YYYY-MM-DD`).
    pub async fn remember(&self, id: &str, note: &str, day: &str) -> Result<RememberedNote, RoleError> {
        validate_role_id(id)?;
        let flattened = note.split_whitespace().collect::<Vec<_>>().join(" ");
        let note = note_text(&flattened);
        if note.is_empty() || is_day(note) {
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
        let now = now_ms();
        let role_dir = self.store.role_dir(id);
        git::ensure_baseline(role_dir.clone(), ROLE_TRACKED_PATHS, now).await?;
        self.store.write_memory(id, &updated).await?;
        let preview: String = note.chars().take(56).collect();
        git::commit(
            role_dir,
            ROLE_TRACKED_PATHS,
            "agent".into(),
            format!("memory: {preview}"),
            now,
        )
        .await?;
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
        self.curate_memory_as(id, memory, stamp, "agent", "memory: curated and tidied")
            .await
    }

    /// Like [`Self::curate_memory`], recording the given `actor` and `commit_msg`
    /// in the role's git repository.
    pub async fn curate_memory_as(
        &self,
        id: &str,
        memory: &str,
        stamp: &str,
        actor: &str,
        commit_msg: &str,
    ) -> Result<CuratedMemory, RoleError> {
        self.curate_memory_checked(id, None, memory, stamp, actor, commit_msg)
            .await
            .map(|curated| curated.unwrap_or_default())
    }

    /// Like [`Self::curate_memory_as`], but only while the memory still reads
    /// `seen`, the version the tidied one was made from: a note saved or taken
    /// back while the curation ran is never written over. `None` when the
    /// memory changed meanwhile and nothing was written.
    pub async fn curate_memory_from(
        &self,
        id: &str,
        seen: &str,
        memory: &str,
        stamp: &str,
        actor: &str,
        commit_msg: &str,
    ) -> Result<Option<CuratedMemory>, RoleError> {
        self.curate_memory_checked(id, Some(seen), memory, stamp, actor, commit_msg)
            .await
    }

    async fn curate_memory_checked(
        &self,
        id: &str,
        seen: Option<&str>,
        memory: &str,
        stamp: &str,
        actor: &str,
        commit_msg: &str,
    ) -> Result<Option<CuratedMemory>, RoleError> {
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
        if seen.is_some_and(|seen| seen.trim_end() != previous.trim_end()) {
            return Ok(None);
        }
        let bytes_before = previous.len();
        if previous.trim_end() == curated.trim_end() {
            return Ok(Some(CuratedMemory {
                archived_as: None,
                bytes_before,
                bytes_after: bytes_before,
            }));
        }
        let now = now_ms();
        let role_dir = self.store.role_dir(id);
        git::ensure_baseline(role_dir.clone(), ROLE_TRACKED_PATHS, now).await?;
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
        git::commit(
            role_dir,
            ROLE_TRACKED_PATHS,
            actor.to_owned(),
            commit_msg.to_owned(),
            now,
        )
        .await?;
        Ok(Some(CuratedMemory {
            archived_as,
            bytes_before,
            bytes_after: curated.len(),
        }))
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

    /// The conversation that is the role's thread with the user, if it has one.
    pub async fn thread(&self, id: &str) -> Result<Option<String>, RoleError> {
        validate_role_id(id)?;
        Ok(self.store.read_thread(id).await?.map(|record| record.conversation_id))
    }

    /// Open a turn the user takes in `conversation_id`, a conversation with the
    /// role outside any schedule or team. A role keeps one thread with the
    /// user: the first such conversation to take a turn becomes it, and so
    /// does the next one once the recorded thread is gone (`thread_gone`, which
    /// the caller checks against the conversations it knows). In the thread,
    /// the turn takes along what the role told the user from its runs
    /// elsewhere since the thread last heard, and those count as handed over.
    pub async fn open_thread_turn(
        &self,
        id: &str,
        conversation_id: &str,
        thread_gone: bool,
        now: i64,
    ) -> Result<ThreadTurn, RoleError> {
        validate_role_id(id)?;
        if self.store.read_definition(id).await?.is_none() {
            return Err(RoleError::NotFound(id.to_owned()));
        }
        let _guard = self.memory_lock.lock().await;
        let adopt = || ThreadRecord {
            conversation_id: conversation_id.to_owned(),
            delivered_at: now,
            summarized_to: None,
            rotated_at: 0,
        };
        let record = match self.store.read_thread(id).await? {
            Some(record) if record.conversation_id == conversation_id => record,
            Some(_) if !thread_gone => return Ok(ThreadTurn::Aside),
            // A new thread starts from the standing section, which already
            // lists what the role last told the user.
            _ => {
                self.store.write_thread(id, &adopt()).await?;
                return Ok(ThreadTurn::Thread {
                    reports: Vec::new(),
                    adopted: true,
                });
            }
        };
        let reports: Vec<RoleReport> = self
            .store
            .read_reports(id)
            .await?
            .into_iter()
            .filter(|report| report.at > record.delivered_at && report.conversation_id != conversation_id)
            .collect();
        if !reports.is_empty() {
            self.store
                .write_thread(
                    id,
                    &ThreadRecord {
                        delivered_at: now,
                        ..record
                    },
                )
                .await?;
        }
        Ok(ThreadTurn::Thread {
            reports,
            adopted: false,
        })
    }

    /// Where the summaries of the role's thread stand, when `conversation_id`
    /// is that thread; `None` for any other conversation. When S5 leaf
    /// condensation has merged older leaves into `condensed/`, returns the
    /// condensed frontier followed by the newest uncondensed `D0` leaves.
    pub async fn thread_upkeep(&self, id: &str, conversation_id: &str) -> Result<Option<ThreadUpkeep>, RoleError> {
        validate_role_id(id)?;
        validate_conversation_id(conversation_id)?;
        let Some(record) = self.store.read_thread(id).await? else {
            return Ok(None);
        };
        if record.conversation_id != conversation_id {
            return Ok(None);
        }
        let d0 = self.store.read_leaves(id, conversation_id).await?;
        let condensed = self.store.read_condensed_leaf_files(id, conversation_id).await?;
        let dreamer = self.store.read_dreamer_state(id).await?;
        let leaves = if !condensed.is_empty() && dreamer.condensed_conversation_id == conversation_id {
            let consumed = dreamer.condensed_leaves_count.min(d0.len());
            let mut combined: Vec<String> = condensed.into_iter().map(|file| file.content).collect();
            combined.extend_from_slice(&d0[consumed..]);
            combined
        } else {
            d0
        };
        Ok(Some(ThreadUpkeep {
            summarized_to: record.summarized_to,
            rotated_at: record.rotated_at,
            leaves,
        }))
    }

    /// Keep `leaf` as the next summary of the role's thread, covering the
    /// messages up to and including `through`. Returns `false`, keeping
    /// nothing, when `conversation_id` is no longer the thread or the
    /// summaries already reach as far.
    pub async fn add_thread_leaf(
        &self,
        id: &str,
        conversation_id: &str,
        leaf: &str,
        through: ThreadMark,
    ) -> Result<bool, RoleError> {
        validate_role_id(id)?;
        validate_conversation_id(conversation_id)?;
        let _guard = self.memory_lock.lock().await;
        let Some(record) = self.store.read_thread(id).await? else {
            return Ok(false);
        };
        let behind = record
            .summarized_to
            .as_ref()
            .is_none_or(|mark| (mark.created_at, mark.id.as_str()) < (through.created_at, through.id.as_str()));
        if record.conversation_id != conversation_id || !behind || leaf.trim().is_empty() {
            return Ok(false);
        }
        self.store.append_leaf(id, conversation_id, leaf).await?;
        self.store
            .write_thread(
                id,
                &ThreadRecord {
                    summarized_to: Some(through),
                    ..record
                },
            )
            .await?;
        Ok(true)
    }

    /// Note that the role's thread moved to a fresh session at `at` (ms).
    /// Returns `false` when `conversation_id` is no longer the thread.
    pub async fn mark_thread_rotated(&self, id: &str, conversation_id: &str, at: i64) -> Result<bool, RoleError> {
        validate_role_id(id)?;
        let _guard = self.memory_lock.lock().await;
        let Some(record) = self.store.read_thread(id).await? else {
            return Ok(false);
        };
        if record.conversation_id != conversation_id {
            return Ok(false);
        }
        self.store
            .write_thread(
                id,
                &ThreadRecord {
                    rotated_at: at,
                    ..record
                },
            )
            .await?;
        Ok(true)
    }

    /// Watermarks recorded in `recall.db` for `id`, keyed by `conversation_id`.
    pub async fn recall_watermarks(&self, id: &str) -> Result<HashMap<String, RecallWatermark>, RoleError> {
        validate_role_id(id)?;
        self.recall.watermarks(id).await
    }

    /// Apply an incremental catch-up batch (messages + summary leaves) to `id`'s
    /// full-text recall index.
    pub async fn sync_recall(&self, id: &str, sync: RecallSync) -> Result<(), RoleError> {
        validate_role_id(id)?;
        if self.store.read_definition(id).await?.is_none() {
            return Err(RoleError::NotFound(id.to_owned()));
        }
        let leaves = match sync.active_thread.as_deref() {
            Some(thread_id) => {
                validate_conversation_id(thread_id)?;
                let mut files = self.store.read_leaf_files(id, thread_id).await?;
                files.extend(self.store.read_condensed_leaf_files(id, thread_id).await?);
                files
            }
            None => Vec::new(),
        };
        self.recall.sync(id, sync, leaves).await
    }

    /// Search `id`'s thread messages, scheduled runs and summary leaves.
    pub async fn recall(
        &self,
        id: &str,
        request: &RoleRecallRequest,
        now: i64,
    ) -> Result<RoleRecallResponse, RoleError> {
        validate_role_id(id)?;
        if self.store.read_definition(id).await?.is_none() {
            return Err(RoleError::NotFound(id.to_owned()));
        }
        self.recall.recall(id, request, now).await
    }

    /// Expand a `ref_id` returned by [`Self::recall`] to its surrounding
    /// messages or full summary leaf.
    pub async fn expand(&self, id: &str, request: &RoleExpandRequest) -> Result<RoleExpandResponse, RoleError> {
        validate_role_id(id)?;
        if self.store.read_definition(id).await?.is_none() {
            return Err(RoleError::NotFound(id.to_owned()));
        }
        self.recall.expand(id, request).await
    }

    /// Shared boss profile (`roles/_boss/USER.md`) injected into every resident
    /// role's standing prompt.
    pub async fn boss_profile(&self) -> Result<String, RoleError> {
        self.store.read_boss_profile().await
    }

    /// Active playbook lines formatted for prompt injection.
    pub async fn prompt_playbook(&self, id: &str) -> Result<Vec<String>, RoleError> {
        validate_role_id(id)?;
        let raw = self.store.read_playbook(id).await?;
        let meta = self.store.read_playbook_meta(id).await?;
        Ok(prompt_playbook_lines(&parse_playbook(&raw, &meta)))
    }

    /// Apply incremental ACE retrospective deltas to `id`'s `PLAYBOOK.md` and
    /// to the shared `roles/_boss/USER.md`, committing each change to git.
    pub async fn apply_retrospective(
        &self,
        id: &str,
        playbook_ops: &[PlaybookDeltaOp],
        boss_ops: &[PlaybookDeltaOp],
        actor: &str,
        reason: &str,
        now: i64,
    ) -> Result<(usize, bool), RoleError> {
        validate_role_id(id)?;
        if self.store.read_definition(id).await?.is_none() {
            return Err(RoleError::NotFound(id.to_owned()));
        }
        let _guard = self.memory_lock.lock().await;

        let role_dir = self.store.role_dir(id);
        git::ensure_baseline(role_dir.clone(), ROLE_TRACKED_PATHS, now).await?;

        let raw_pb = self.store.read_playbook(id).await?;
        let archived_pb = self.store.read_archived_playbook(id).await?;
        let mut meta = self.store.read_playbook_meta(id).await?;
        let mut entries = parse_playbook(&raw_pb, &meta);
        let applied = apply_playbook_deltas(&mut entries, &mut meta, &archived_pb, playbook_ops, now);
        if applied > 0 {
            self.store.write_playbook(id, &render_playbook(&entries)).await?;
            self.store.write_playbook_meta(id, &meta).await?;
            git::commit(
                role_dir,
                ROLE_TRACKED_PATHS,
                actor.to_owned(),
                format!("playbook: {reason} ({applied} update(s))"),
                now,
            )
            .await?;
        }

        let boss_raw = self.store.read_boss_profile().await?;
        let boss_changed = if let Some(updated_boss) = apply_boss_deltas(&boss_raw, boss_ops) {
            let boss_dir = self.store.boss_dir();
            git::ensure_baseline(boss_dir.clone(), BOSS_TRACKED_PATHS, now).await?;
            self.store.write_boss_profile(&updated_boss).await?;
            git::commit(
                boss_dir,
                BOSS_TRACKED_PATHS,
                actor.to_owned(),
                format!("boss({id}): {reason}"),
                now,
            )
            .await?;
            true
        } else {
            false
        };

        let mut dreamer = self.store.read_dreamer_state(id).await?;
        dreamer.last_retrospective_at = now;
        self.store.write_dreamer_state(id, &dreamer).await?;

        Ok((applied, boss_changed))
    }

    /// Remove one playbook entry by id (`p-NNNN`) on user request.
    pub async fn remove_playbook_entry(&self, id: &str, entry_id: &str, now: i64) -> Result<RoleResponse, RoleError> {
        validate_role_id(id)?;
        let Some(definition) = self.store.read_definition(id).await? else {
            return Err(RoleError::NotFound(id.to_owned()));
        };
        let _guard = self.memory_lock.lock().await;
        let role_dir = self.store.role_dir(id);
        git::ensure_baseline(role_dir.clone(), ROLE_TRACKED_PATHS, now).await?;

        let raw_pb = self.store.read_playbook(id).await?;
        let archived_pb = self.store.read_archived_playbook(id).await?;
        let mut meta = self.store.read_playbook_meta(id).await?;
        let mut entries = parse_playbook(&raw_pb, &meta);
        let removed = apply_playbook_deltas(
            &mut entries,
            &mut meta,
            &archived_pb,
            &[PlaybookDeltaOp::Remove {
                id: entry_id.trim().to_owned(),
            }],
            now,
        );
        if removed > 0 {
            self.store.write_playbook(id, &render_playbook(&entries)).await?;
            self.store.write_playbook_meta(id, &meta).await?;
            git::commit(
                role_dir,
                ROLE_TRACKED_PATHS,
                "user".into(),
                format!("playbook: removed {}", entry_id.trim()),
                now,
            )
            .await?;
        }
        self.to_response(id, definition).await
    }

    /// Stage an agent-proposed major change (role definition or scheduled task)
    /// for user review in the Roles drawer.
    pub async fn stage_proposal(
        &self,
        id: &str,
        summary: String,
        detail: String,
        from_conversation_id: Option<String>,
        change: RoleProposalChange,
        now: i64,
    ) -> Result<RoleProposal, RoleError> {
        validate_role_id(id)?;
        if self.store.read_definition(id).await?.is_none() {
            return Err(RoleError::NotFound(id.to_owned()));
        }
        let _guard = self.memory_lock.lock().await;
        self.stage_proposal_locked(id, summary, detail, from_conversation_id, change, now)
            .await
    }

    async fn stage_proposal_locked(
        &self,
        id: &str,
        summary: String,
        detail: String,
        from_conversation_id: Option<String>,
        change: RoleProposalChange,
        now: i64,
    ) -> Result<RoleProposal, RoleError> {
        let mut proposals = self.store.read_proposals(id).await?;
        if let Some(existing) = proposals.iter_mut().find(|p| p.change == change) {
            existing.summary = summary;
            existing.detail = detail;
            existing.from_conversation_id = from_conversation_id;
            existing.created_at = now;
            let updated = existing.clone();
            self.store.write_proposals(id, &proposals).await?;
            return Ok(updated);
        }
        let proposal = RoleProposal {
            id: new_proposal_id(now, &proposals),
            role_id: id.to_owned(),
            summary,
            detail,
            from_conversation_id,
            created_at: now,
            change,
        };
        proposals.push(proposal.clone());
        self.store.write_proposals(id, &proposals).await?;
        Ok(proposal)
    }

    /// Remove and return a pending proposal by id (`prop-...`).
    pub async fn take_proposal(&self, id: &str, proposal_id: &str) -> Result<RoleProposal, RoleError> {
        validate_role_id(id)?;
        if self.store.read_definition(id).await?.is_none() {
            return Err(RoleError::NotFound(id.to_owned()));
        }
        let _guard = self.memory_lock.lock().await;
        let mut proposals = self.store.read_proposals(id).await?;
        let Some(pos) = proposals.iter().position(|p| p.id == proposal_id) else {
            return Err(RoleError::InvalidRequest(format!("unknown proposal: {proposal_id}")));
        };
        let taken = proposals.remove(pos);
        self.store.write_proposals(id, &proposals).await?;
        Ok(taken)
    }

    /// Apply a user-approved `RoleProposalChange::UpdateRole` and commit to git.
    pub async fn apply_approved_role_change(
        &self,
        id: &str,
        summary: &str,
        change: &RoleProposalChange,
        now: i64,
    ) -> Result<RoleResponse, RoleError> {
        validate_role_id(id)?;
        let _guard = self.memory_lock.lock().await;
        let Some(mut definition) = self.store.read_definition(id).await? else {
            return Err(RoleError::NotFound(id.to_owned()));
        };
        if let RoleProposalChange::UpdateRole {
            charter,
            denied_tools,
            verifies,
            asks_when_scheduled,
        } = change
        {
            if let Some(c) = charter {
                definition.charter = c.trim().to_owned();
            }
            if let Some(d) = denied_tools {
                definition.denied_tools = normalize_denied_tools(d)?;
            }
            if let Some(v) = verifies {
                definition.verifies = *v;
            }
            if let Some(a) = asks_when_scheduled {
                definition.asks_when_scheduled = *a;
            }
            definition.updated_at = now;
            let role_dir = self.store.role_dir(id);
            git::ensure_baseline(role_dir.clone(), ROLE_TRACKED_PATHS, now).await?;
            self.store.write_definition(id, &definition).await?;
            git::commit(
                role_dir,
                ROLE_TRACKED_PATHS,
                "user".into(),
                format!("proposal: approved — {summary}"),
                now,
            )
            .await?;
        }
        self.to_response(id, definition).await
    }

    /// Reject and remove a pending proposal.
    pub async fn reject_proposal(&self, id: &str, proposal_id: &str) -> Result<RoleResponse, RoleError> {
        let _ = self.take_proposal(id, proposal_id).await?;
        self.get(id).await
    }

    /// Read git version history for `id`, newest commit first.
    pub async fn timeline(&self, id: &str, limit: usize) -> Result<Vec<RoleCommit>, RoleError> {
        validate_role_id(id)?;
        if self.store.read_definition(id).await?.is_none() {
            return Err(RoleError::NotFound(id.to_owned()));
        }
        let role_dir = self.store.role_dir(id);
        git::ensure_baseline(role_dir.clone(), ROLE_TRACKED_PATHS, now_ms()).await?;
        git::timeline(role_dir, limit.clamp(1, 100)).await
    }

    /// Revert commit `sha` in `id`'s git repository by creating a new forward
    /// revert commit and checking out the tracked files.
    pub async fn revert(&self, id: &str, sha: &str, now: i64) -> Result<RoleResponse, RoleError> {
        validate_role_id(id)?;
        if self.store.read_definition(id).await?.is_none() {
            return Err(RoleError::NotFound(id.to_owned()));
        }
        let _guard = self.memory_lock.lock().await;
        let role_dir = self.store.role_dir(id);
        git::revert(role_dir, ROLE_TRACKED_PATHS, sha.to_owned(), "user".into(), now).await?;
        let Some(definition) = self.store.read_definition(id).await? else {
            return Err(RoleError::NotFound(id.to_owned()));
        };
        self.to_response(id, definition).await
    }

    /// Run S5 zero-LLM decay on `id`: score and archive stale/contradicted
    /// playbook items to `PLAYBOOK-archived.md` (with a git commit), and move
    /// untouched `progress/<task>.md` files to `progress/.archive/`.
    pub async fn run_zero_llm_decay(&self, id: &str, day: &str, now: i64) -> Result<(usize, usize), RoleError> {
        validate_role_id(id)?;
        if self.store.read_definition(id).await?.is_none() {
            return Err(RoleError::NotFound(id.to_owned()));
        }
        let _guard = self.memory_lock.lock().await;
        let role_dir = self.store.role_dir(id);
        git::ensure_baseline(role_dir.clone(), ROLE_TRACKED_PATHS, now).await?;

        let raw_pb = self.store.read_playbook(id).await?;
        let mut meta = self.store.read_playbook_meta(id).await?;
        let mut entries = parse_playbook(&raw_pb, &meta);
        let archived = decay_playbook(&mut entries, &mut meta, now);
        self.store.write_playbook_meta(id, &meta).await?;

        let decayed_count = archived.len();
        if decayed_count > 0 {
            let prev_archived = self.store.read_archived_playbook(id).await?;
            let next_archived = append_archived_playbook(&prev_archived, &archived, day);
            self.store.write_playbook(id, &render_playbook(&entries)).await?;
            self.store.write_archived_playbook(id, &next_archived).await?;
            git::commit(
                role_dir,
                ROLE_TRACKED_PATHS,
                "dreamer".into(),
                format!("decay: archived {decayed_count} stale playbook item(s)"),
                now,
            )
            .await?;
        }

        let cutoff = now.saturating_sub(PROGRESS_ARCHIVE_IDLE_DAYS * 24 * 60 * 60 * 1_000);
        let archived_progress = self.store.archive_stale_progress(id, cutoff).await?;
        Ok((decayed_count, archived_progress))
    }

    /// Read `(last_retrospective_at, last_dreamer_at)` for `id`.
    pub async fn dreamer_timestamps(&self, id: &str) -> Result<(i64, i64), RoleError> {
        validate_role_id(id)?;
        let state = self.store.read_dreamer_state(id).await?;
        Ok((state.last_retrospective_at, state.last_dreamer_at))
    }

    /// Record that a dreamer maintenance cycle completed at `now`.
    pub async fn mark_dreamer_ran(&self, id: &str, now: i64) -> Result<(), RoleError> {
        validate_role_id(id)?;
        let _guard = self.memory_lock.lock().await;
        let mut state = self.store.read_dreamer_state(id).await?;
        state.last_dreamer_at = now;
        self.store.write_dreamer_state(id, &state).await
    }

    /// Next batch of `batch_size` uncondensed `D0` leaves for `id`'s current
    /// thread (`(conversation_id, start_index, leaves)`), if enough have
    /// accumulated.
    pub async fn next_uncondensed_leaf_batch(
        &self,
        id: &str,
        batch_size: usize,
    ) -> Result<Option<(String, usize, Vec<String>)>, RoleError> {
        validate_role_id(id)?;
        let Some(thread) = self.store.read_thread(id).await? else {
            return Ok(None);
        };
        let d0 = self.store.read_leaves(id, &thread.conversation_id).await?;
        let state = self.store.read_dreamer_state(id).await?;
        let start = if state.condensed_conversation_id == thread.conversation_id {
            state.condensed_leaves_count.min(d0.len())
        } else {
            0
        };
        if d0.len().saturating_sub(start) < batch_size || batch_size == 0 {
            return Ok(None);
        }
        Ok(Some((
            thread.conversation_id,
            start,
            d0[start..start + batch_size].to_vec(),
        )))
    }

    /// Persist a condensed leaf summarizing `consumed_count` `D0` leaves starting
    /// at `expected_start` of `conversation_id`.
    pub async fn record_condensed_leaf(
        &self,
        id: &str,
        conversation_id: &str,
        expected_start: usize,
        consumed_count: usize,
        condensed_leaf: &str,
    ) -> Result<bool, RoleError> {
        validate_role_id(id)?;
        validate_conversation_id(conversation_id)?;
        let _guard = self.memory_lock.lock().await;
        let Some(thread) = self.store.read_thread(id).await? else {
            return Ok(false);
        };
        if thread.conversation_id != conversation_id || condensed_leaf.trim().is_empty() {
            return Ok(false);
        }
        let mut state = self.store.read_dreamer_state(id).await?;
        let current_start = if state.condensed_conversation_id == conversation_id {
            state.condensed_leaves_count
        } else {
            0
        };
        if current_start != expected_start {
            return Ok(false);
        }
        self.store
            .append_condensed_leaf(id, conversation_id, condensed_leaf)
            .await?;
        state.condensed_conversation_id = conversation_id.to_owned();
        state.condensed_leaves_count = current_start + consumed_count;
        self.store.write_dreamer_state(id, &state).await?;
        Ok(true)
    }

    /// Every kept project and its keeper, by project id. Two roles can name the
    /// same project only when a revert brought an old claim back; the later
    /// claim wins.
    pub async fn keepers(&self) -> Result<Vec<RoleKeeper>, RoleError> {
        let mut by_project: HashMap<String, RoleKeeper> = HashMap::new();
        for id in self.store.list_ids().await? {
            if validate_role_id(&id).is_err() {
                continue;
            }
            let definition = match self.store.read_definition(&id).await {
                Ok(Some(definition)) => definition,
                Ok(None) => continue,
                Err(error) => {
                    tracing::warn!(role_id = %id, error = %error, "role: skipped unreadable role for keepers");
                    continue;
                }
            };
            for kept in definition.projects {
                let wins = by_project.get(&kept.project_id).is_none_or(|current| {
                    (kept.claimed_at, id.as_str()) > (current.claimed_at, current.role_id.as_str())
                });
                if wins {
                    by_project.insert(
                        kept.project_id.clone(),
                        RoleKeeper {
                            project_id: kept.project_id,
                            role_id: id.clone(),
                            claimed_at: kept.claimed_at,
                        },
                    );
                }
            }
        }
        let mut keepers: Vec<RoleKeeper> = by_project.into_values().collect();
        keepers.sort_by(|a, b| a.project_id.cmp(&b.project_id));
        Ok(keepers)
    }

    /// What the keeper of `project_id` hands a conversation opened there,
    /// whose first message is `opening` (empty for a preview). `None` when the
    /// project has no keeper, when the keeper is `skip_role` (the conversation
    /// is the keeper's own, which opens with its whole standing section), or
    /// when the keeper has nothing on the project to hand over yet.
    pub async fn keeper_brief(
        &self,
        project_id: &str,
        opening: &str,
        skip_role: Option<&str>,
    ) -> Result<Option<KeeperBrief>, RoleError> {
        if !valid_project_id(project_id) {
            return Ok(None);
        }
        let Some(keeper) = self
            .keepers()
            .await?
            .into_iter()
            .find(|keeper| keeper.project_id == project_id)
        else {
            return Ok(None);
        };
        if skip_role == Some(keeper.role_id.as_str()) {
            return Ok(None);
        }
        let Some(role) = self.find(&keeper.role_id).await? else {
            return Ok(None);
        };
        let ledger = self.store.read_project_ledger(&role.id, project_id).await?;
        let project_name = role
            .projects
            .iter()
            .find(|kept| kept.project_id == project_id)
            .map(|kept| kept.name.clone())
            .unwrap_or_default();
        let other_projects: Vec<String> = role
            .projects
            .iter()
            .filter(|kept| kept.project_id != project_id)
            .map(|kept| kept.name.clone())
            .collect();
        let brief = brief::compose(
            &BriefSources {
                role_id: &role.id,
                project_id,
                project_name: &project_name,
                other_projects: &other_projects,
                charter: &role.charter,
                memory: &role.memory,
                playbook: &role.playbook,
                progress: &role.progress,
                last_stop: ledger.and_then(|ledger| ledger.last_stop),
            },
            opening,
        );
        Ok(brief::has_content(&brief).then_some(brief))
    }

    /// Replace the projects the role keeps, in the order given. A project the
    /// role already kept keeps its `claimed_at`. A project another role keeps
    /// is refused with [`RoleError::Conflict`] unless `transfer` is set; then
    /// it leaves that role, which records the handover in its own history.
    pub async fn set_projects(
        &self,
        id: &str,
        request: SetRoleProjectsRequest,
        now: i64,
    ) -> Result<RoleResponse, RoleError> {
        validate_role_id(id)?;
        let claims = normalize_project_claims(&request.projects)?;
        let _guard = self.memory_lock.lock().await;
        let Some(mut definition) = self.store.read_definition(id).await? else {
            return Err(RoleError::NotFound(id.to_owned()));
        };
        let is_new = |claim: &&RoleProjectClaim| !definition.projects.iter().any(|k| k.project_id == claim.project_id);
        let added: Vec<&RoleProjectClaim> = claims.iter().filter(is_new).collect();
        let taking = |project_id: &str| added.iter().any(|claim| claim.project_id == project_id);

        let mut holders = Vec::new();
        if !added.is_empty() {
            for other in self.store.list_ids().await? {
                if other == id || validate_role_id(&other).is_err() {
                    continue;
                }
                let Ok(Some(other_definition)) = self.store.read_definition(&other).await else {
                    continue;
                };
                if other_definition.projects.iter().any(|kept| taking(&kept.project_id)) {
                    holders.push((other, other_definition));
                }
            }
        }
        if let Some((holder, holder_definition)) = holders.first()
            && !request.transfer
        {
            let taken = holder_definition
                .projects
                .iter()
                .find(|kept| taking(&kept.project_id))
                .map(kept_label)
                .unwrap_or_default();
            return Err(RoleError::Conflict(format!("{taken} is already kept by {holder}")));
        }
        for (holder, mut holder_definition) in holders {
            let handed: Vec<String> = holder_definition
                .projects
                .iter()
                .filter(|kept| taking(&kept.project_id))
                .map(kept_label)
                .collect();
            let holder_dir = self.store.role_dir(&holder);
            git::ensure_baseline(holder_dir.clone(), ROLE_TRACKED_PATHS, now).await?;
            holder_definition.projects.retain(|kept| !taking(&kept.project_id));
            holder_definition.updated_at = now;
            self.store.write_definition(&holder, &holder_definition).await?;
            git::commit(
                holder_dir,
                ROLE_TRACKED_PATHS,
                "user".into(),
                format!("projects: handed {} to {id}", handed.join(", ")),
                now,
            )
            .await?;
        }

        let next: Vec<RoleKeptProject> = claims
            .iter()
            .map(|claim| {
                let before = definition.projects.iter().find(|k| k.project_id == claim.project_id);
                RoleKeptProject {
                    project_id: claim.project_id.clone(),
                    name: match before {
                        Some(kept) if claim.name.is_empty() => kept.name.clone(),
                        _ => claim.name.clone(),
                    },
                    claimed_at: before.map_or(now, |kept| kept.claimed_at),
                }
            })
            .collect();
        if next == definition.projects {
            return self.to_response(id, definition).await;
        }
        let took: Vec<String> = next
            .iter()
            .filter(|kept| taking(&kept.project_id))
            .map(kept_label)
            .collect();
        let let_go: Vec<String> = definition
            .projects
            .iter()
            .filter(|kept| !next.iter().any(|n| n.project_id == kept.project_id))
            .map(kept_label)
            .collect();
        let mut parts = Vec::new();
        if !took.is_empty() {
            parts.push(format!("took on {}", took.join(", ")));
        }
        if !let_go.is_empty() {
            parts.push(format!("let go of {}", let_go.join(", ")));
        }
        let message = if parts.is_empty() {
            "projects: renamed".to_owned()
        } else {
            format!("projects: {}", parts.join("; "))
        };

        let role_dir = self.store.role_dir(id);
        git::ensure_baseline(role_dir.clone(), ROLE_TRACKED_PATHS, now).await?;
        definition.projects = next;
        definition.updated_at = now;
        self.store.write_definition(id, &definition).await?;
        git::commit(role_dir, ROLE_TRACKED_PATHS, "user".into(), message, now).await?;
        self.to_response(id, definition).await
    }

    /// Dismiss the role. Its memory directory stays on disk and is picked up
    /// again if the role is defined anew.
    pub async fn remove(&self, id: &str) -> Result<(), RoleError> {
        validate_role_id(id)?;
        let _guard = self.memory_lock.lock().await;
        let now = now_ms();
        let role_dir = self.store.role_dir(id);
        git::ensure_baseline(role_dir.clone(), ROLE_TRACKED_PATHS, now).await?;
        if self.store.remove_definition(id).await? {
            git::commit(
                role_dir,
                ROLE_TRACKED_PATHS,
                "user".into(),
                "role: dismissed".into(),
                now,
            )
            .await?;
            Ok(())
        } else {
            Err(RoleError::NotFound(id.to_owned()))
        }
    }

    async fn to_response(&self, id: &str, definition: RoleDefinition) -> Result<RoleResponse, RoleError> {
        let pb_raw = self.store.read_playbook(id).await?;
        let pb_meta = self.store.read_playbook_meta(id).await?;
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
            thread_conversation_id: self.store.read_thread(id).await?.map(|record| record.conversation_id),
            playbook: parse_playbook(&pb_raw, &pb_meta),
            proposals: self.store.read_proposals(id).await?,
            projects: definition.projects,
            created_at: definition.created_at,
            updated_at: definition.updated_at,
        })
    }
}

/// A project as a role's history names it: its name, or its id before it had one.
fn kept_label(kept: &RoleKeptProject) -> String {
    if kept.name.is_empty() {
        kept.project_id.clone()
    } else {
        kept.name.clone()
    }
}

/// A project id the backend could have issued (`project_<uuid>`): a plain
/// token, safe as a file name.
fn valid_project_id(project_id: &str) -> bool {
    !project_id.is_empty()
        && project_id.len() <= MAX_PROJECT_ID_LEN
        && project_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
}

/// Check project ids (the backend issues `project_<uuid>`), flatten names to
/// one short line, and drop repeats, keeping the first.
fn normalize_project_claims(claims: &[RoleProjectClaim]) -> Result<Vec<RoleProjectClaim>, RoleError> {
    let mut normalized: Vec<RoleProjectClaim> = Vec::with_capacity(claims.len());
    for claim in claims {
        let project_id = claim.project_id.trim();
        if !valid_project_id(project_id) {
            return Err(RoleError::InvalidRequest(format!(
                "invalid project id: {}",
                project_id.chars().take(MAX_PROJECT_ID_LEN).collect::<String>()
            )));
        }
        if normalized.iter().any(|kept| kept.project_id == project_id) {
            continue;
        }
        let name: String = claim
            .name
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(MAX_KEPT_PROJECT_NAME_CHARS)
            .collect();
        normalized.push(RoleProjectClaim {
            project_id: project_id.to_owned(),
            name,
        });
    }
    if normalized.len() > MAX_KEPT_PROJECTS {
        return Err(RoleError::InvalidRequest(format!(
            "a role keeps at most {MAX_KEPT_PROJECTS} projects"
        )));
    }
    Ok(normalized)
}

/// A thread's summaries live in a directory named after its conversation, so
/// the id is held to what conversation ids are made of.
fn validate_conversation_id(id: &str) -> Result<(), RoleError> {
    let safe = !id.is_empty()
        && id.len() <= MAX_ROLE_ID_LEN
        && !id.starts_with('.')
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if safe {
        Ok(())
    } else {
        Err(RoleError::InvalidId(id.to_owned()))
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
    if id.is_empty() || id.len() > MAX_ROLE_ID_LEN || !plain || id.starts_with('.') || id == "_boss" {
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

/// Trim, reject anything that is not a real team tool or `role_send`, then sort
/// and dedupe so the stored table is canonical.
fn normalize_denied_tools(tools: &[String]) -> Result<Vec<String>, RoleError> {
    let mut normalized = Vec::with_capacity(tools.len());
    for tool in tools {
        let tool = tool.trim();
        if TeamToolName::parse(tool).is_none() && tool != ROLE_SEND_TOOL_NAME {
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
