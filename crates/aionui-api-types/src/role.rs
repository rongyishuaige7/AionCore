//! Resident-role DTOs (`/api/roles`).
//!
//! A role is an assistant promoted to a standing team member: on top of the
//! assistant's own definition it carries a charter, a memory that outlives any
//! single team, and a table of team tools it may not call.

use aionui_common::TimestampMs;
use serde::{Deserialize, Serialize};

/// One resident role.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleResponse {
    /// Id of the assistant this role is built on.
    pub id: String,
    /// What the role owns and what it refuses, in the user's own words.
    pub charter: String,
    /// Team tools this role may not call, enforced where team tools execute.
    pub denied_tools: Vec<String>,
    /// Whether this role reviews work: in a team it has joined, a task another
    /// member marks completed is held until this role passes it.
    #[serde(default)]
    pub verifies: bool,
    /// Whether a scheduled run of this role asks the user before it acts,
    /// instead of running in full auto like any other scheduled task.
    #[serde(default)]
    pub asks_when_scheduled: bool,
    /// Contents of the role's `MEMORY.md`.
    pub memory: String,
    /// Absolute path of the role's memory directory on the backend host.
    pub memory_dir: String,
    /// Work the role has under way, most recently touched first.
    #[serde(default)]
    pub progress: Vec<RoleProgress>,
    /// What the role last chose to tell the user from runs nobody watched,
    /// newest first.
    #[serde(default)]
    pub reported: Vec<RoleReport>,
    /// The one conversation the user keeps with the role, where what it tells
    /// from runs elsewhere is brought. Absent until the user has talked to it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_conversation_id: Option<String>,
    /// ACE-style itemized operating lessons (`PLAYBOOK.md`).
    #[serde(default)]
    pub playbook: Vec<RolePlaybookEntry>,
    /// Agent-proposed major changes held until the user approves or rejects them.
    #[serde(default)]
    pub proposals: Vec<RoleProposal>,
    /// Projects this role keeps, in the order it took them on.
    #[serde(default)]
    pub projects: Vec<RoleKeptProject>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// A project a resident role keeps: it briefs conversations started there and
/// takes in what they leave behind. A project has at most one keeper.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleKeptProject {
    pub project_id: String,
    /// The project's name when the role took it on, for the role's own files
    /// and history; the live name comes from the project itself.
    #[serde(default)]
    pub name: String,
    pub claimed_at: TimestampMs,
}

/// One project to keep, as `PUT /api/roles/{id}/projects` names it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleProjectClaim {
    pub project_id: String,
    #[serde(default)]
    pub name: String,
}

/// Body of `PUT /api/roles/{id}/projects` — the full set of projects the role
/// keeps from now on. A project another role keeps is refused unless
/// `transfer` is set, in which case it changes hands.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetRoleProjectsRequest {
    #[serde(default)]
    pub projects: Vec<RoleProjectClaim>,
    #[serde(default)]
    pub transfer: bool,
}

/// Who keeps one project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleKeeper {
    pub project_id: String,
    pub role_id: String,
    pub claimed_at: TimestampMs,
}

/// `GET /api/roles/keepers` — every kept project and its keeper.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleKeepersResponse {
    pub keepers: Vec<RoleKeeper>,
}

/// How a run nobody watched ended, as the role itself declared it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoleReportOutcome {
    /// There is something the user should know.
    Say,
    /// Nothing the user needs to hear; the run should not ask for attention.
    Silent,
    /// The role cannot go on without an answer from the user.
    Ask,
}

impl RoleReportOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Say => "say",
            Self::Silent => "silent",
            Self::Ask => "ask",
        }
    }
}

/// One thing a role told the user at the end of a run: kept so that the next
/// run does not say it again, and so that the user finds it when they come back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleReport {
    pub at: TimestampMs,
    /// `say` or `ask`; a silent run leaves nothing behind.
    pub outcome: RoleReportOutcome,
    /// One line: what the user should know, or the question they are asked.
    pub summary: String,
    /// The conversation the run happened in.
    pub conversation_id: String,
}

/// Where one piece of a role's ongoing work stands. Each task is a file in the
/// role's memory directory (`progress/<task>.md`) that the role replaces as the
/// work moves and removes when it is finished.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleProgress {
    /// Name of the task, which is also its file name.
    pub task: String,
    /// What is done and what is left, as the role last wrote it.
    pub state: String,
    /// When the state was last written, in milliseconds since the epoch.
    pub updated_at: i64,
}

/// Response of `GET /api/roles`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleListResponse {
    pub roles: Vec<RoleResponse>,
}

/// Body of `PUT /api/roles/{id}` — create the role or replace its definition.
/// The memory is left untouched.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpsertRoleRequest {
    #[serde(default)]
    pub charter: String,
    #[serde(default)]
    pub denied_tools: Vec<String>,
    #[serde(default)]
    pub verifies: bool,
    #[serde(default)]
    pub asks_when_scheduled: bool,
}

/// Body of `PUT /api/roles/{id}/memory` — replace the role's `MEMORY.md`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateRoleMemoryRequest {
    #[serde(default)]
    pub memory: String,
}

/// Schema version of the envelopes the agent-facing `role` helper prints.
pub const ROLE_CLI_SCHEMA_VERSION: u32 = 1;

/// Why a `role` helper command was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoleCliErrorCode {
    /// The runtime token does not open this conversation.
    RuntimeAuthFailed,
    /// The conversation does not run as a resident role.
    NotARole,
    /// The role's permission table keeps it from this write.
    PermissionDenied,
    SchemaValidationFailed,
    TransportUnavailable,
    Internal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleCliError {
    pub code: RoleCliErrorCode,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleCliMeta {
    pub schema_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
}

/// What every `role` helper command prints, and what the runtime role routes
/// answer with.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoleCliEnvelope<T> {
    pub success: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<T>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<RoleCliError>,
    pub meta: RoleCliMeta,
}

impl<T> RoleCliEnvelope<T> {
    pub fn success(data: T, command: &str) -> Self {
        Self {
            success: true,
            data: Some(data),
            error: None,
            meta: RoleCliMeta {
                schema_version: ROLE_CLI_SCHEMA_VERSION,
                command: Some(command.to_owned()),
            },
        }
    }

    pub fn failure(code: RoleCliErrorCode, message: impl Into<String>, command: &str) -> Self {
        Self {
            success: false,
            data: None,
            error: Some(RoleCliError {
                code,
                message: message.into(),
            }),
            meta: RoleCliMeta {
                schema_version: ROLE_CLI_SCHEMA_VERSION,
                command: Some(command.to_owned()),
            },
        }
    }
}

/// Body of `POST /api/runtime/role/remember` — one note for the role's memory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoleRememberRequest {
    pub note: String,
}

/// The line the memory now holds for a note.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleRememberResponse {
    pub line: String,
    /// The memory already held this note, so nothing was written.
    pub already_recorded: bool,
}

/// Body of `POST /api/runtime/role/progress` — where one task stands. An empty
/// `state` closes the task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoleProgressRequest {
    pub task: String,
    #[serde(default)]
    pub state: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleProgressResponse {
    /// Name the task is filed under.
    pub task: String,
    /// The task was closed rather than updated.
    pub closed: bool,
}

/// Body of `POST /api/runtime/role/curate` — the whole memory, tidied, to take
/// the place of the one the role has now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoleCurateRequest {
    pub memory: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleCurateResponse {
    /// File the replaced memory was kept as, in the `archive` directory beside
    /// the memory. Absent when nothing was replaced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived_as: Option<String>,
    pub bytes_before: usize,
    pub bytes_after: usize,
}

/// Stdin of `role follow-up`: a one-shot timer the role sets for itself. The
/// note comes back to the conversation that set it, once, after the delay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoleFollowUpRequest {
    pub note: String,
    pub after_minutes: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleFollowUpResponse {
    /// The scheduled task the follow-up waits as until it is delivered.
    pub job_id: String,
    pub due_at: TimestampMs,
    /// Follow-ups this conversation now has waiting, this one included.
    pub pending: usize,
}

/// Stdin of `role report`: how the run ended. `summary` is the one line the
/// user sees first, and may be left out of a silent run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoleReportRequest {
    pub outcome: RoleReportOutcome,
    #[serde(default)]
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleReportResponse {
    pub outcome: RoleReportOutcome,
    /// The report was kept among what the role has told the user. `false` for
    /// a silent run.
    pub recorded: bool,
}

/// Payload of the `role.reported` event: a run in `conversation_id` declared
/// how it ended, before its turn completes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleReportedEvent {
    pub conversation_id: String,
    pub role_id: String,
    pub outcome: RoleReportOutcome,
    pub summary: String,
    pub at: TimestampMs,
}

/// Which conversations of a role `role recall` searches.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoleRecallScope {
    /// Only the role's one thread with the user (its messages and summaries).
    Thread,
    /// Only the role's scheduled runs.
    Runs,
    /// Both the thread and the scheduled runs.
    #[default]
    All,
}

/// What a `role recall` hit points at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoleRecallKind {
    Message,
    Leaf,
}

/// Where a recalled message or summary came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoleRecallSource {
    Thread,
    Run,
}

/// A moment on the timeline for `before` or `after` in `role recall`: either
/// milliseconds since the epoch or a date/time string (`YYYY-MM-DD`,
/// `YYYY-MM-DD HH:MM`, or RFC 3339).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RoleRecallBound {
    Millis(i64),
    Text(String),
}

/// Body of `POST /api/runtime/role/recall`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoleRecallRequest {
    pub query: String,
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub scope: Option<RoleRecallScope>,
    #[serde(default)]
    pub before: Option<RoleRecallBound>,
    #[serde(default)]
    pub after: Option<RoleRecallBound>,
}

/// One hit returned by `role recall`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleRecallHit {
    pub ref_id: String,
    pub kind: RoleRecallKind,
    pub source: RoleRecallSource,
    pub conversation_id: String,
    pub speaker: String,
    pub at: TimestampMs,
    /// For a leaf summary: the span header (`YYYY-MM-DD HH:MM → YYYY-MM-DD HH:MM · N messages`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span: Option<String>,
    pub snippet: String,
}

/// Response of `POST /api/runtime/role/recall`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleRecallResponse {
    pub hits: Vec<RoleRecallHit>,
}

/// Body of `POST /api/runtime/role/expand`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoleExpandRequest {
    pub ref_id: String,
    #[serde(default)]
    pub before: Option<u32>,
    #[serde(default)]
    pub after: Option<u32>,
}

/// One message in the window returned by `role expand`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleExpandMessage {
    pub ref_id: String,
    pub speaker: String,
    pub at: TimestampMs,
    pub text: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hit: bool,
}

/// Response of `POST /api/runtime/role/expand`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleExpandResponse {
    pub ref_id: String,
    pub kind: RoleRecallKind,
    pub source: RoleRecallSource,
    pub conversation_id: String,
    /// For a leaf summary: the span of messages it covers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span: Option<String>,
    /// For a leaf summary: the whole summary body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leaf: Option<String>,
    /// For a message: the hit and its surrounding messages, oldest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub messages: Vec<RoleExpandMessage>,
    /// True when a message or surrounding window was clipped to stay within the size cap.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}

/// One itemized lesson in a role's `memory/PLAYBOOK.md`: `- [p-0001] (+3/-1) <content>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RolePlaybookEntry {
    pub id: String,
    pub helpful: u32,
    pub harmful: u32,
    pub content: String,
    #[serde(default)]
    pub updated_at: i64,
}

/// Incremental ACE delta operation over `PLAYBOOK.md` or `roles/_boss/USER.md`.
/// Never rewrites the whole file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "UPPERCASE")]
pub enum PlaybookDeltaOp {
    Add {
        content: String,
    },
    Update {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content: Option<String>,
        #[serde(default)]
        helpful_delta: u32,
        #[serde(default)]
        harmful_delta: u32,
    },
    Remove {
        id: String,
    },
}

/// What an agent-authored proposal wants to change on a resident role.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RoleProposalChange {
    UpdateRole {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        charter: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        denied_tools: Option<Vec<String>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        verifies: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        asks_when_scheduled: Option<bool>,
    },
    CreateCronJob {
        name: String,
        schedule_desc: String,
        prompt: String,
        payload: serde_json::Value,
    },
    UpdateCronJob {
        job_id: String,
        name: String,
        payload: serde_json::Value,
    },
    DeleteCronJob {
        job_id: String,
        name: String,
    },
}

/// A major change proposed by an agent and held until the user approves or rejects it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleProposal {
    pub id: String,
    pub role_id: String,
    pub summary: String,
    pub detail: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_conversation_id: Option<String>,
    pub created_at: TimestampMs,
    pub change: RoleProposalChange,
}

/// One git commit in `<data_dir>/roles/<id>/.git`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleCommit {
    pub sha: String,
    pub short_sha: String,
    pub at: TimestampMs,
    pub actor: String,
    pub message: String,
    #[serde(default)]
    pub files: Vec<String>,
    pub additions: usize,
    pub deletions: usize,
    pub diff: String,
    pub can_revert: bool,
}

/// Response of `GET /api/roles/{id}/timeline`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleTimelineResponse {
    pub commits: Vec<RoleCommit>,
}

/// Body of `POST /api/roles/{id}/revert`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoleRevertRequest {
    pub sha: String,
}

/// Summary of an S5 dreamer maintenance run on one role.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleDreamResponse {
    pub decayed_playbook: usize,
    pub archived_progress: usize,
    pub curated_memory: bool,
    pub retrospective_ran: bool,
    pub condensed_batches: usize,
}
