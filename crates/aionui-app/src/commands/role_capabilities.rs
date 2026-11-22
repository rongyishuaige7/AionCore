use aionui_api_types::ROLE_CLI_SCHEMA_VERSION;
use serde_json::{Value, json};

const RUNTIME_ENV: [&str; 4] = [
    "AIONUI_BASE_URL",
    "AIONUI_USER_ID",
    "AIONUI_CONVERSATION_ID",
    "AIONUI_RUNTIME_TOKEN",
];

/// Stdin fields of `role remember`.
pub(crate) const REMEMBER_FIELDS: Fields = Fields {
    required: &["note"],
    optional: &[],
    whole_numbers: &[],
    timestamps: &[],
};

/// Stdin fields of `role curate`.
pub(crate) const CURATE_FIELDS: Fields = Fields {
    required: &["memory"],
    optional: &[],
    whole_numbers: &[],
    timestamps: &[],
};

/// Stdin fields of `role progress`.
pub(crate) const PROGRESS_FIELDS: Fields = Fields {
    required: &["task"],
    optional: &["state"],
    whole_numbers: &[],
    timestamps: &[],
};

/// Stdin fields of `role follow-up`.
pub(crate) const FOLLOW_UP_FIELDS: Fields = Fields {
    required: &["note", "after_minutes"],
    optional: &[],
    whole_numbers: &["after_minutes"],
    timestamps: &[],
};

/// Stdin fields of `role report`.
pub(crate) const REPORT_FIELDS: Fields = Fields {
    required: &["outcome"],
    optional: &["summary"],
    whole_numbers: &[],
    timestamps: &[],
};

/// Stdin fields of `role recall`.
pub(crate) const RECALL_FIELDS: Fields = Fields {
    required: &["query"],
    optional: &["limit", "scope", "before", "after"],
    whole_numbers: &["limit"],
    timestamps: &["before", "after"],
};

/// Stdin fields of `role expand`.
pub(crate) const EXPAND_FIELDS: Fields = Fields {
    required: &["ref_id"],
    optional: &["before", "after"],
    whole_numbers: &["before", "after"],
    timestamps: &[],
};

/// Stdin fields of `role send` (exactly one of `to_role` or `to_conversation` is required at runtime).
pub(crate) const SEND_FIELDS: Fields = Fields {
    required: &["message"],
    optional: &["to_role", "to_conversation"],
    whole_numbers: &[],
    timestamps: &[],
};

/// The fields a command reads from its stdin JSON object: strings, except the
/// ones named in `whole_numbers` or `timestamps`.
pub(crate) struct Fields {
    pub required: &'static [&'static str],
    pub optional: &'static [&'static str],
    pub whole_numbers: &'static [&'static str],
    pub timestamps: &'static [&'static str],
}

pub(crate) fn data() -> Value {
    json!({
        "schema_version": ROLE_CLI_SCHEMA_VERSION,
        "contract": "agent-facing-role-cli",
        "purpose": "A resident role's own memory, follow-ups and what it tells the user, for a conversation that runs as that role. The role is the one this conversation's assistant is; no command takes a role id.",
        "commands": {
            "capabilities": { "runtime_env_required": [] },
            "show": {
                "runtime_env_required": RUNTIME_ENV,
                "stdin": "none",
                "data": "{ id, charter, memory, memory_dir, denied_tools, verifies, asks_when_scheduled, progress: [{ task, state, updated_at }], reported: [{ at, outcome, summary, conversation_id }] } — the whole memory, the full state of every task under way, and what the role last told the user"
            },
            "remember": {
                "runtime_env_required": RUNTIME_ENV,
                "stdin_json": { "required": REMEMBER_FIELDS.required, "optional": REMEMBER_FIELDS.optional },
                "fields": { "note": "one thing worth keeping, kept as a single dated line" },
                "data": "{ line, already_recorded }"
            },
            "curate": {
                "runtime_env_required": RUNTIME_ENV,
                "stdin_json": { "required": CURATE_FIELDS.required, "optional": CURATE_FIELDS.optional },
                "fields": { "memory": "the whole memory, tidied: what repeats merged, what no longer holds dropped" },
                "data": "{ archived_as?, bytes_before, bytes_after }"
            },
            "progress": {
                "runtime_env_required": RUNTIME_ENV,
                "stdin_json": { "required": PROGRESS_FIELDS.required, "optional": PROGRESS_FIELDS.optional },
                "fields": {
                    "task": "a short name for the task; the same name replaces what was recorded before",
                    "state": "what is done and what is left; empty or absent closes the task"
                },
                "data": "{ task, closed }"
            },
            "follow-up": {
                "runtime_env_required": RUNTIME_ENV,
                "stdin_json": { "required": FOLLOW_UP_FIELDS.required, "optional": FOLLOW_UP_FIELDS.optional },
                "fields": {
                    "note": "what to pick up when woken, written for a reader who remembers nothing of this run",
                    "after_minutes": "a whole number of minutes from now, from 1 to 10080 (a week)"
                },
                "data": "{ job_id, due_at, pending }"
            },
            "report": {
                "runtime_env_required": RUNTIME_ENV,
                "stdin_json": { "required": REPORT_FIELDS.required, "optional": REPORT_FIELDS.optional },
                "fields": {
                    "outcome": "how a run nobody watched ended: `silent` (nothing the user needs to hear), `say` (something they should know) or `ask` (their answer is needed to go on)",
                    "summary": "the one line the user sees first, or the question they are asked; required for say and ask, 300 characters at most"
                },
                "data": "{ outcome, recorded }"
            },
            "recall": {
                "runtime_env_required": RUNTIME_ENV,
                "stdin_json": { "required": RECALL_FIELDS.required, "optional": RECALL_FIELDS.optional },
                "fields": {
                    "query": "keywords or phrase to search across your thread with the user, your scheduled runs, and your thread summary leaves",
                    "limit": "maximum number of hits to return, from 1 to 50 (default 10)",
                    "scope": "`thread` (your thread and its leaves), `runs` (scheduled runs), or `all` (default)",
                    "before": "only hits strictly before this moment (YYYY-MM-DD, YYYY-MM-DD HH:MM, RFC 3339, or epoch ms)",
                    "after": "only hits at or after this moment (YYYY-MM-DD, YYYY-MM-DD HH:MM, RFC 3339, or epoch ms)"
                },
                "data": "{ hits: [{ ref_id, kind, source, conversation_id, speaker, at, span?, snippet }] }"
            },
            "expand": {
                "runtime_env_required": RUNTIME_ENV,
                "stdin_json": { "required": EXPAND_FIELDS.required, "optional": EXPAND_FIELDS.optional },
                "fields": {
                    "ref_id": "a `ref_id` returned by `role recall` (`msg:...` or `leaf:...`)",
                    "before": "preceding messages to include around a message hit, from 0 to 20 (default 3)",
                    "after": "following messages to include around a message hit, from 0 to 20 (default 3)"
                },
                "data": "{ ref_id, kind, source, conversation_id, span?, leaf?, messages?: [{ ref_id, speaker, at, text, hit? }], truncated? }"
            },
            "peers": {
                "runtime_env_required": RUNTIME_ENV,
                "stdin": "none",
                "data": "{ peers: [{ role_id, name, charter_summary, thread_conversation_id?, busy }] } — live roster of other resident roles"
            },
            "send": {
                "runtime_env_required": RUNTIME_ENV,
                "stdin_json": { "required": SEND_FIELDS.required, "optional": SEND_FIELDS.optional },
                "fields": {
                    "to_role": "peer resident role id (lazily creates its thread when needed); mutually exclusive with `to_conversation`",
                    "to_conversation": "conversation id (e.g. `reply_to` from an incoming `[[AION_SESSION_MESSAGE]]`); mutually exclusive with `to_role`",
                    "message": "concise, self-contained message body"
                },
                "data": "{ status, to_conversation_id, to_role_id?, hop, max_hops }"
            }
        },
        "output_envelope": {
            "success": "boolean",
            "data": "object when success=true",
            "error": "object when success=false: { code, message }",
            "meta": { "schema_version": ROLE_CLI_SCHEMA_VERSION }
        },
        "semantics": {
            "memory": "remember appends one note and never rewrites; a note the memory already holds is not written twice",
            "curation": "curate replaces the whole memory; the version it replaces is kept in the archive directory beside it, and an empty memory is refused",
            "follow_up": "follow-up sets a one-shot timer: the note comes back to this conversation once, after the delay, and the timer is gone once delivered; a conversation holds at most 5 waiting at a time",
            "report": "report declares how a run ended, once, before the final reply: a silent run asks the user for no attention; say and ask are kept, the newest 30, so that the next run does not repeat them; a run that declares nothing is shown to the user as if it had something to say",
            "recall": "recall searches only this role's own thread, scheduled runs, and summary leaves (never other roles or unrelated conversations); pass a hit's ref_id to expand to read the surrounding messages or the full summary leaf",
            "permission": "a role the user keeps from writing its memory or its task progress is answered permission_denied, and should say in its reply what it would have recorded"
        },
        "errors": [
            "not_a_role",
            "permission_denied",
            "runtime_auth_failed",
            "schema_validation_failed",
            "target_not_found",
            "target_is_self",
            "feature_disabled",
            "rate_limited",
            "hop_limit_reached",
            "queue_full",
            "transport_unavailable",
            "internal"
        ]
    })
}

#[cfg(test)]
mod tests {
    use aionui_api_types::RoleCliErrorCode;

    use super::*;

    #[test]
    fn every_error_the_contract_lists_is_a_code_the_backend_can_answer_with() {
        for code in data()["errors"].as_array().unwrap() {
            assert!(
                serde_json::from_value::<RoleCliErrorCode>(code.clone()).is_ok(),
                "{code} is not an error code"
            );
        }
    }
}
