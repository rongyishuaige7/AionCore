//! Automatic saves of a kept project's conversations into its keeper's memory.
//!
//! Once a conversation in a kept project goes quiet, the conversation layer
//! has the keeper's own engine read what was said and hands the result here.
//! What it took in lands where the keeper keeps things anyway: notes in
//! `MEMORY.md` tagged with the project, new lessons in the playbook, task
//! states under `memory/progress/`, and where work stopped in the project's
//! ledger. Each save is also kept as one line of `debriefs.jsonl`, so the user
//! can see what was saved and take any single item back.
//!
//! Writes go through the same memory lock as every other memory write, so a
//! save, a retrospective, a curation and the user's own changes never write
//! over one another.

use std::collections::HashSet;

use aionui_api_types::{
    KeeperDebrief, KeeperDebriefContext, KeeperDebriefDraft, KeeperDebriefItem, KeeperDebriefItemKind,
    KeeperDebriefStatus, KeeperDebriefWarning, KeeperLastStop, PlaybookDeltaOp, RolePlaybookEntry,
};

use super::{
    MAX_MEMORY_BYTES, MAX_NOTE_CHARS, MAX_PROGRESS_STATE_CHARS, MAX_PROGRESS_TASKS, RoleService, note_text,
    task_file_name, valid_project_id, validate_role_id,
};
use crate::brief::{self, BriefSources};
use crate::error::RoleError;
use crate::git::{self, ROLE_TRACKED_PATHS};
use crate::playbook::{apply_playbook_deltas, parse_playbook, render_playbook};

/// Saves kept per role; older ones are dropped.
pub const MAX_KEPT_DEBRIEFS: usize = 100;
/// Lessons a summarizer is shown, at most.
const CONTEXT_LESSONS: usize = 20;
/// What one save may keep, at most, of each kind.
const MAX_SAVED_NOTES: usize = 5;
const MAX_SAVED_LESSONS: usize = 3;
const MAX_SAVED_TASKS: usize = 3;
const MAX_SAVED_WARNINGS: usize = 2;
/// Caps, in characters.
const LAST_STOP_CHARS: usize = 400;
const WARNING_NOTE_CHARS: usize = 240;
const WARNING_FILES: usize = 4;
const LESSON_CHARS: usize = 300;
/// Shorter than this, a note says nothing.
const MIN_NOTE_CHARS: usize = 4;
/// Who the role's history says made a save.
const SAVE_ACTOR: &str = "keeper-debrief";

/// What a finished pass hands the keeper to record.
#[derive(Debug, Clone, Copy)]
pub struct DebriefRecord<'a> {
    pub role_id: &'a str,
    pub project_id: &'a str,
    pub conversation_id: &'a str,
    /// The card the conversation shows for this save.
    pub message_id: &'a str,
    /// `Saved` when `draft` came back whole, else why there is none.
    pub status: KeeperDebriefStatus,
    pub draft: Option<&'a KeeperDebriefDraft>,
    /// The caller's local date (`YYYY-MM-DD`), for the notes.
    pub day: &'a str,
    pub at: i64,
}

impl RoleService {
    /// What the keeper of `project_id` already knows of the project, as
    /// material for a save, and how many saves it made since `since` (ms).
    /// `None` when the project has no keeper, or the keeper is `skip_role`
    /// (the conversation is the keeper's own).
    pub async fn keeper_debrief_context(
        &self,
        project_id: &str,
        skip_role: Option<&str>,
        since: i64,
    ) -> Result<Option<KeeperDebriefContext>, RoleError> {
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
        let sources = BriefSources {
            role_id: &role.id,
            project_id,
            project_name: &project_name,
            other_projects: &other_projects,
            charter: &role.charter,
            memory: &role.memory,
            playbook: &role.playbook,
            progress: &role.progress,
            last_stop: None,
        };
        let known = brief::compose(&sources, "");
        let lessons = brief::project_lessons(&sources, CONTEXT_LESSONS);
        let saves_today = saves_since(&self.store.read_debriefs(&role.id).await?, since);
        Ok(Some(KeeperDebriefContext {
            role_id: role.id.clone(),
            project_id: project_id.to_owned(),
            project_name: known.project_name,
            charter: known.charter,
            notes: known.memory,
            lessons,
            progress: known.progress,
            saves_today,
            thread_conversation_id: role.thread_conversation_id.clone(),
        }))
    }

    /// Keep what a pass took from a conversation and record the save. Only a
    /// `Saved` pass with a draft writes anything to memory; it ends up
    /// `Nothing` when all it brought was already known, where work stopped
    /// included. Every pass is recorded, so a skipped one shows why.
    pub async fn record_debrief(&self, record: DebriefRecord<'_>) -> Result<KeeperDebrief, RoleError> {
        let id = record.role_id;
        validate_role_id(id)?;
        let Some(definition) = self.store.read_definition(id).await? else {
            return Err(RoleError::NotFound(id.to_owned()));
        };
        let project_name = definition
            .projects
            .iter()
            .find(|kept| kept.project_id == record.project_id)
            .map(|kept| kept.name.clone())
            .unwrap_or_default();

        let _guard = self.memory_lock.lock().await;
        let mut debriefs = self.store.read_debriefs(id).await?;
        let mut debrief = KeeperDebrief {
            id: new_debrief_id(record.at, &debriefs),
            role_id: id.to_owned(),
            project_id: record.project_id.to_owned(),
            project_name: project_name.clone(),
            conversation_id: record.conversation_id.to_owned(),
            message_id: record.message_id.to_owned(),
            at: record.at,
            status: record.status,
            last_stop: None,
            items: Vec::new(),
            warnings: Vec::new(),
        };
        if let (KeeperDebriefStatus::Saved, Some(draft)) = (record.status, record.draft) {
            self.keep_draft(&record, &project_name, draft, &mut debrief).await?;
            if debrief.items.is_empty() && debrief.warnings.is_empty() && debrief.last_stop.is_none() {
                debrief.status = KeeperDebriefStatus::Nothing;
            }
        } else if record.status == KeeperDebriefStatus::Saved {
            debrief.status = KeeperDebriefStatus::Nothing;
        }
        debriefs.push(debrief.clone());
        let keep_from = debriefs.len().saturating_sub(MAX_KEPT_DEBRIEFS);
        self.store.write_debriefs(id, &debriefs[keep_from..]).await?;
        Ok(debrief)
    }

    async fn keep_draft(
        &self,
        record: &DebriefRecord<'_>,
        project_name: &str,
        draft: &KeeperDebriefDraft,
        debrief: &mut KeeperDebrief,
    ) -> Result<(), RoleError> {
        let id = record.role_id;
        let role_dir = self.store.role_dir(id);
        git::ensure_baseline(role_dir.clone(), ROLE_TRACKED_PATHS, record.at).await?;

        let memory = self.store.read_memory(id).await?;
        let (memory, lines) = with_notes(&memory, record.day, project_name, &draft.notes);
        if !lines.is_empty() {
            self.store.write_memory(id, &memory).await?;
        }
        debrief
            .items
            .extend(lines.into_iter().map(|(line, text)| KeeperDebriefItem {
                key: line,
                kind: KeeperDebriefItemKind::Note,
                text,
                forgotten: false,
                previous: None,
            }));

        let raw = self.store.read_playbook(id).await?;
        let archived = self.store.read_archived_playbook(id).await?;
        let mut meta = self.store.read_playbook_meta(id).await?;
        let mut entries = parse_playbook(&raw, &meta);
        let known: HashSet<String> = entries.iter().map(|entry| entry.id.clone()).collect();
        let ops: Vec<PlaybookDeltaOp> = draft
            .lessons
            .iter()
            .map(|lesson| brief::one_line_cut(lesson, LESSON_CHARS))
            .filter(|lesson| lesson.chars().count() >= MIN_NOTE_CHARS)
            .take(MAX_SAVED_LESSONS)
            .map(|content| PlaybookDeltaOp::Add { content })
            .collect();
        let applied = apply_playbook_deltas(&mut entries, &mut meta, &archived, &ops, record.at);
        if applied > 0 {
            self.store.write_playbook(id, &render_playbook(&entries)).await?;
            self.store.write_playbook_meta(id, &meta).await?;
        }
        let new_lessons: Vec<KeeperDebriefItem> = entries
            .iter()
            .filter(|entry| !known.contains(&entry.id))
            .map(|entry| KeeperDebriefItem {
                key: entry.id.clone(),
                kind: KeeperDebriefItemKind::Lesson,
                text: entry.content.clone(),
                forgotten: false,
                previous: None,
            })
            .collect();
        debrief.warnings = warnings_of(draft, &entries, &known);
        debrief.items.extend(new_lessons);

        let open = self.store.list_progress(id).await?;
        let mut open_count = open.len();
        for task in draft.progress.iter().take(MAX_SAVED_TASKS) {
            let Ok(name) = task_file_name(&task.task) else {
                continue;
            };
            let state: String = task.state.trim().chars().take(MAX_PROGRESS_STATE_CHARS).collect();
            if state.is_empty() {
                continue;
            }
            let previous = open
                .iter()
                .find(|file| file.task == name)
                .map(|file| file.state.trim().to_owned());
            if previous.as_deref() == Some(state.as_str()) {
                continue;
            }
            if previous.is_none() {
                if open_count >= MAX_PROGRESS_TASKS {
                    continue;
                }
                open_count += 1;
            }
            self.store.write_progress(id, &name, &format!("{state}\n")).await?;
            debrief.items.push(KeeperDebriefItem {
                key: name,
                kind: KeeperDebriefItemKind::Progress,
                text: state,
                forgotten: false,
                previous,
            });
        }

        let stop = brief::one_line_cut(&draft.last_stop, LAST_STOP_CHARS);
        let mut ledger = self
            .store
            .read_project_ledger(id, record.project_id)
            .await?
            .unwrap_or_default();
        let known_stop = ledger.last_stop.as_ref().map(|known| known.text.trim());
        if !stop.is_empty() && known_stop != Some(stop.as_str()) {
            ledger.last_stop = Some(KeeperLastStop {
                text: stop.clone(),
                conversation_id: record.conversation_id.to_owned(),
                at: record.at,
            });
            self.store.write_project_ledger(id, record.project_id, &ledger).await?;
            debrief.last_stop = Some(stop);
        }

        let tracked = debrief
            .items
            .iter()
            .filter(|item| item.kind != KeeperDebriefItemKind::Progress)
            .count();
        if tracked > 0 {
            let project = if project_name.is_empty() {
                record.project_id
            } else {
                project_name
            };
            git::commit(
                role_dir,
                ROLE_TRACKED_PATHS,
                SAVE_ACTOR.into(),
                format!("memory: auto-saved {tracked} item(s) from a {project} conversation"),
                record.at,
            )
            .await?;
        }
        Ok(())
    }

    /// The role's saves, newest first.
    pub async fn debriefs(&self, id: &str) -> Result<Vec<KeeperDebrief>, RoleError> {
        validate_role_id(id)?;
        if self.store.read_definition(id).await?.is_none() {
            return Err(RoleError::NotFound(id.to_owned()));
        }
        let mut debriefs = self.store.read_debriefs(id).await?;
        debriefs.reverse();
        Ok(debriefs)
    }

    /// Take back one item a save kept, on the user's word: the note leaves
    /// the memory, the lesson leaves the playbook, the task goes back to where
    /// it stood (unless it moved on since). Taking back an item twice is not
    /// an error.
    pub async fn forget_debrief_item(
        &self,
        id: &str,
        debrief_id: &str,
        key: &str,
        now: i64,
    ) -> Result<KeeperDebrief, RoleError> {
        validate_role_id(id)?;
        if self.store.read_definition(id).await?.is_none() {
            return Err(RoleError::NotFound(id.to_owned()));
        }
        let _guard = self.memory_lock.lock().await;
        let mut debriefs = self.store.read_debriefs(id).await?;
        let Some(debrief) = debriefs.iter_mut().find(|debrief| debrief.id == debrief_id) else {
            return Err(RoleError::InvalidRequest(format!("no automatic save {debrief_id}")));
        };
        let Some(item) = debrief.items.iter_mut().find(|item| item.key == key) else {
            return Err(RoleError::InvalidRequest(format!("{debrief_id} kept nothing as {key}")));
        };
        if item.forgotten {
            return Ok(debrief.clone());
        }

        let role_dir = self.store.role_dir(id);
        git::ensure_baseline(role_dir.clone(), ROLE_TRACKED_PATHS, now).await?;
        let mut tracked_change = false;
        match item.kind {
            KeeperDebriefItemKind::Note => {
                let memory = self.store.read_memory(id).await?;
                if let Some(updated) = without_note(&memory, &item.key) {
                    self.store.write_memory(id, &updated).await?;
                    tracked_change = true;
                }
            }
            KeeperDebriefItemKind::Lesson => {
                let raw = self.store.read_playbook(id).await?;
                let archived = self.store.read_archived_playbook(id).await?;
                let mut meta = self.store.read_playbook_meta(id).await?;
                let mut entries = parse_playbook(&raw, &meta);
                let removed = apply_playbook_deltas(
                    &mut entries,
                    &mut meta,
                    &archived,
                    &[PlaybookDeltaOp::Remove { id: item.key.clone() }],
                    now,
                );
                if removed > 0 {
                    self.store.write_playbook(id, &render_playbook(&entries)).await?;
                    self.store.write_playbook_meta(id, &meta).await?;
                    tracked_change = true;
                }
            }
            KeeperDebriefItemKind::Progress => {
                let open = self.store.list_progress(id).await?;
                let current = open.iter().find(|file| file.task == item.key);
                if current.is_some_and(|file| file.state.trim() == item.text.trim()) {
                    match item.previous.as_deref() {
                        Some(previous) => {
                            self.store
                                .write_progress(id, &item.key, &format!("{previous}\n"))
                                .await?;
                        }
                        None => {
                            self.store.remove_progress(id, &item.key).await?;
                        }
                    }
                }
            }
        }
        item.forgotten = true;
        let forgotten = item.key.clone();
        let updated = debrief.clone();
        if tracked_change {
            let preview: String = note_text(&forgotten).chars().take(56).collect();
            git::commit(
                role_dir,
                ROLE_TRACKED_PATHS,
                "user".into(),
                format!("memory: forgot an automatic save ({preview})"),
                now,
            )
            .await?;
        }
        self.store.write_debriefs(id, &debriefs).await?;
        Ok(updated)
    }

    /// The lesson a save warned about no longer holds, on the user's word:
    /// it counts once against the lesson, so it fades from briefs and warnings.
    pub async fn mark_debrief_lesson_outdated(
        &self,
        id: &str,
        debrief_id: &str,
        lesson_id: &str,
        now: i64,
    ) -> Result<KeeperDebrief, RoleError> {
        validate_role_id(id)?;
        if self.store.read_definition(id).await?.is_none() {
            return Err(RoleError::NotFound(id.to_owned()));
        }
        let _guard = self.memory_lock.lock().await;
        let mut debriefs = self.store.read_debriefs(id).await?;
        let Some(debrief) = debriefs.iter_mut().find(|debrief| debrief.id == debrief_id) else {
            return Err(RoleError::InvalidRequest(format!("no automatic save {debrief_id}")));
        };
        let Some(warning) = debrief
            .warnings
            .iter_mut()
            .find(|warning| warning.lesson_id == lesson_id)
        else {
            return Err(RoleError::InvalidRequest(format!(
                "{debrief_id} raised nothing about {lesson_id}"
            )));
        };
        if warning.outdated {
            return Ok(debrief.clone());
        }

        let role_dir = self.store.role_dir(id);
        git::ensure_baseline(role_dir.clone(), ROLE_TRACKED_PATHS, now).await?;
        let raw = self.store.read_playbook(id).await?;
        let archived = self.store.read_archived_playbook(id).await?;
        let mut meta = self.store.read_playbook_meta(id).await?;
        let mut entries = parse_playbook(&raw, &meta);
        let applied = apply_playbook_deltas(
            &mut entries,
            &mut meta,
            &archived,
            &[PlaybookDeltaOp::Update {
                id: lesson_id.to_owned(),
                content: None,
                helpful_delta: 0,
                harmful_delta: 1,
            }],
            now,
        );
        if applied > 0 {
            self.store.write_playbook(id, &render_playbook(&entries)).await?;
            self.store.write_playbook_meta(id, &meta).await?;
            git::commit(
                role_dir,
                ROLE_TRACKED_PATHS,
                "user".into(),
                format!("playbook: {lesson_id} marked outdated"),
                now,
            )
            .await?;
        }
        warning.outdated = true;
        let updated = debrief.clone();
        self.store.write_debriefs(id, &debriefs).await?;
        Ok(updated)
    }
}

/// Saves since `since` (ms) that ran the keeper's engine: the ones counted
/// against the day's cap.
pub(crate) fn saves_since(debriefs: &[KeeperDebrief], since: i64) -> usize {
    debriefs
        .iter()
        .filter(|debrief| debrief.at >= since && debrief.status != KeeperDebriefStatus::Unsupported)
        .count()
}

fn new_debrief_id(at: i64, existing: &[KeeperDebrief]) -> String {
    let base = format!("d-{at}");
    let taken = |candidate: &str| existing.iter().any(|debrief| debrief.id == candidate);
    if !taken(&base) {
        return base;
    }
    (2..)
        .map(|suffix| format!("{base}-{suffix}"))
        .find(|candidate| !taken(candidate))
        .unwrap_or(base)
}

/// `memory` with `notes` appended as `- <day> [<project>] <note>`, skipping
/// notes it already holds (tagged or not) and stopping when the memory is
/// full. Also returns each line written (what a forget looks for) with the
/// note as the user reads it: no day, no project tag.
pub(crate) fn with_notes(
    memory: &str,
    day: &str,
    project_name: &str,
    notes: &[String],
) -> (String, Vec<(String, String)>) {
    let mut known: HashSet<String> = memory.lines().map(|line| note_text(line).to_owned()).collect();
    let mut updated = memory.trim_end().to_owned();
    let mut written = Vec::new();
    for note in notes {
        if written.len() >= MAX_SAVED_NOTES {
            break;
        }
        let Some(plain) = plain_note(note, project_name) else {
            continue;
        };
        let tagged = if project_name.trim().is_empty() {
            plain.clone()
        } else {
            format!("[{}] {plain}", project_name.trim())
        };
        if known.contains(&tagged) || known.contains(&plain) {
            continue;
        }
        let line = format!("- {day} {tagged}");
        let grown = updated.len() + line.len() + 2;
        if grown > MAX_MEMORY_BYTES {
            break;
        }
        if !updated.is_empty() {
            updated.push('\n');
        }
        updated.push_str(&line);
        known.insert(tagged);
        written.push((line, plain));
    }
    if !updated.is_empty() {
        updated.push('\n');
    }
    (updated, written)
}

/// A summarizer's note as one plain line: no bullet, day or project tag of
/// its own, cut to a note's cap. `None` when nothing is left.
fn plain_note(note: &str, project_name: &str) -> Option<String> {
    let flat = note.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut text = note_text(&flat).trim();
    let tag = format!("[{}]", project_name.trim());
    if !project_name.trim().is_empty()
        && let Some(rest) = text.strip_prefix(&tag)
    {
        text = rest.trim_start();
    }
    if text.chars().count() < MIN_NOTE_CHARS {
        return None;
    }
    Some(brief::one_line_cut(text, MAX_NOTE_CHARS))
}

/// `memory` without the note `line` a save wrote, matched exactly or, when it
/// was rewritten since, by its text. `None` when it is no longer there.
pub(crate) fn without_note(memory: &str, line: &str) -> Option<String> {
    let line = line.trim();
    let text = note_text(line);
    let mut removed = false;
    let kept: Vec<&str> = memory
        .lines()
        .filter(|candidate| {
            let matches = !removed && (candidate.trim() == line || note_text(candidate) == text);
            removed |= matches;
            !matches
        })
        .collect();
    if !removed {
        return None;
    }
    let mut updated = kept.join("\n");
    if !updated.is_empty() {
        updated.push('\n');
    }
    Some(updated)
}

/// The draft's warnings about lessons the playbook held before this save,
/// with each lesson as it stands.
fn warnings_of(
    draft: &KeeperDebriefDraft,
    entries: &[RolePlaybookEntry],
    known: &HashSet<String>,
) -> Vec<KeeperDebriefWarning> {
    let mut seen = HashSet::new();
    draft
        .warnings
        .iter()
        .filter_map(|warning| {
            let lesson_id = warning.lesson_id.trim();
            if !known.contains(lesson_id) || !seen.insert(lesson_id.to_owned()) {
                return None;
            }
            let entry = entries.iter().find(|entry| entry.id == lesson_id)?;
            Some(KeeperDebriefWarning {
                lesson_id: lesson_id.to_owned(),
                lesson: brief::one_line_cut(&entry.content, LESSON_CHARS),
                files: warning
                    .files
                    .iter()
                    .map(|file| file.trim().to_owned())
                    .filter(|file| !file.is_empty())
                    .take(WARNING_FILES)
                    .collect(),
                note: brief::one_line_cut(&warning.note, WARNING_NOTE_CHARS),
                outdated: false,
            })
        })
        .take(MAX_SAVED_WARNINGS)
        .collect()
}

#[cfg(test)]
#[path = "debrief_test.rs"]
mod debrief_test;
