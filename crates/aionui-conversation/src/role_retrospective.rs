//! S4 Retrospective (ACE playbook + shared boss profile incremental updates)
//! and S5 Nightly Dreamer (memory verify/curate + LCM leaf condensation).
//!
//! Runs through the isolated, tool-less `TitleGenerator::summarize` channel so
//! reflection never blocks user turns or mutates files non-deterministically.

use std::sync::Arc;
use std::time::Duration;

use aionui_ai_agent::IWorkerTaskManager;
use aionui_ai_agent::title::{SUMMARY_MAX_BYTES, SUMMARY_TIMEOUT, TitleGenerator};
use aionui_api_types::PlaybookDeltaOp;
use aionui_common::now_ms;
use aionui_db::{MessagePageDirection, MessagePageParams};
use serde::Deserialize;
use tracing::{info, warn};

use crate::role_thread::{RoleThreadMark, clean_leaf, lines_of};
use crate::service::{ConversationRolePort, ConversationService};

/// Minimum interval between automatic retrospectives for one role (24 hours).
pub(crate) const RETROSPECTIVE_MIN_INTERVAL_MS: i64 = 24 * 60 * 60 * 1_000;
/// Weekly retrospective interval for S5 nightly dreamer.
const WEEKLY_RETROSPECTIVE_MS: i64 = 7 * 24 * 60 * 60 * 1_000;
/// Byte threshold at which S5 dreamer curates `MEMORY.md` even mid-week (48 KiB of 64 KiB cap).
pub const DREAMER_CURATE_THRESHOLD_BYTES: usize = 48 * 1024;
/// Number of `D0` leaves merged into one condensed summary leaf (`D1`).
pub const CONDENSE_BATCH_SIZE: usize = 6;

const HOUR_MS: i64 = 60 * 60 * 1_000;
/// A dreamer pass never comes sooner than this after the last one.
const DREAMER_MIN_GAP_MS: i64 = 2 * HOUR_MS;
/// How long the user has sent nothing before the daily pass may run: the
/// user's own night, wherever the machine's clock says it is.
pub const DREAMER_QUIET_MS: i64 = 2 * HOUR_MS;
/// The daily pass, once the user is quiet.
const DREAMER_DAILY_MS: i64 = 20 * HOUR_MS;
/// A role whose user never goes quiet that long still gets its pass.
const DREAMER_OVERDUE_MS: i64 = 44 * HOUR_MS;

/// Whether a role's dreamer pass is due at `now`. `last_dreamer_at` is when
/// the last pass ended (0 if never), `last_failed` whether a step it was due
/// to do failed, `user_quiet_for` how long the user has sent no message, and
/// `near_memory_cap` whether the memory is close to its cap. Passes stay at
/// least two hours apart; a failed one is retried as soon as the user is
/// quiet again.
pub fn dreamer_due(
    now: i64,
    last_dreamer_at: i64,
    last_failed: bool,
    user_quiet_for: i64,
    near_memory_cap: bool,
) -> bool {
    let since = now.saturating_sub(last_dreamer_at);
    if since < DREAMER_MIN_GAP_MS {
        return false;
    }
    if near_memory_cap || since >= DREAMER_OVERDUE_MS {
        return true;
    }
    user_quiet_for >= DREAMER_QUIET_MS && (since >= DREAMER_DAILY_MS || last_failed)
}
const END_MARKER: &str = "[end]";

/// Current role state supplied to a retrospective pass by [`ConversationRolePort`].
#[derive(Debug, Clone, Default)]
pub struct RoleRetrospectiveContext {
    pub last_retrospective_at: i64,
    pub playbook_lines: Vec<String>,
    pub boss_profile: String,
    pub failed_runs: Vec<String>,
}

/// Outcome of the LLM-assisted steps in an S5 dreamer pass.
#[derive(Debug, Clone, Default)]
pub struct RoleDreamTasksOutcome {
    pub curated_memory: Option<String>,
    pub retrospective_ran: bool,
    pub condensed_batches: usize,
    /// Why a step that was due did not get done, for the role's dreamer state.
    pub error: Option<String>,
}

const RETROSPECTIVE_INSTRUCTIONS: &str = "\
You are running a retrospective for a resident assistant role that works with one boss over months. \
Read the current playbook, the shared boss profile, and the evidence since the last review, then output \
ONLY incremental JSON delta operations to improve how this role works.

Strict rules (Hermes & ACE):
1. Write ONLY durable rules backed by concrete evidence in the material below. When nothing new and durable was learned, return empty arrays.
2. Treat user corrections, pushbacks, and answers to `[ask]` report cards as primary playbook signals: ADD a specific rule, or UPDATE an existing rule's `helpful_delta` / `harmful_delta` (integers 0..3) and wording.
3. NEVER record transient environment, network, proxy, rate-limit, or API outages.
4. NEVER record negative conclusions claiming a tool, skill, or command \"does not work\" or \"is broken\".
5. Put role-specific operating checklists and domain lessons in `playbook`; put cross-role facts about the boss (communication style, formatting preferences, priorities, review habits) in `boss_profile`.
6. Write in the language the user predominantly uses.
7. Output a single JSON object (no markdown commentary) followed on its own final line by `[end]`:
{\"playbook\": [{\"op\": \"ADD\", \"content\": \"...\"}, {\"op\": \"UPDATE\", \"id\": \"p-0001\", \"helpful_delta\": 1, \"harmful_delta\": 0}, {\"op\": \"REMOVE\", \"id\": \"p-0002\"}], \"boss_profile\": [{\"op\": \"ADD\", \"content\": \"...\"}]}
[end]";

const CONDENSE_INSTRUCTIONS: &str = "\
You are condensing six consecutive summary leaves of a long-running conversation between a user and a resident role \
into one higher-level summary. The assistant will read this condensed summary in place of the six leaves.

Rules:
- Keep durable decisions and why they were made, user facts/preferences, commitments, and exact file paths, commands, names, numbers, and dates.
- Drop superseded intermediate steps and resolved trivia; when something changed across the six stretches, keep where it ended up.
- Mark unconfirmed points as such; never invent facts.
- Write in the language the summaries use, at most ~600 words, no preamble.
- End with a line that reads exactly [end].";

const VERIFY_AND_CURATE_INSTRUCTIONS: &str = "\
You are verifying and curating a resident role's `MEMORY.md` against its recent conversation summaries and run reports.

Rules:
- Merge duplicate or overlapping notes into single concise `- YYYY-MM-DD <fact>` bullets (keeping the newest date when merged).
- Remove claims that recent evidence directly contradicts or shows are obsolete.
- Keep exact file paths, identifiers, preferences, and standing decisions intact.
- Do NOT add speculative notes or environment error noise; if the memory is already clean, return it unchanged.
- Output ONLY the tidied markdown lines (no code fences, no preamble), followed on its own final line by `[end]`.";

#[derive(Debug, Default, Deserialize)]
struct ParsedRetrospective {
    #[serde(default)]
    playbook: Vec<PlaybookDeltaOp>,
    #[serde(default)]
    boss_profile: Vec<PlaybookDeltaOp>,
}

pub(crate) fn parse_retrospective_output(raw: &str) -> Option<(Vec<PlaybookDeltaOp>, Vec<PlaybookDeltaOp>)> {
    let stripped = crate::strip_think_tags(raw);
    let body = stripped.trim_end().strip_suffix(END_MARKER).unwrap_or(&stripped).trim();
    if body.is_empty() || body.len() > SUMMARY_MAX_BYTES {
        return None;
    }
    let json_slice = extract_json_object(body)?;
    let parsed: ParsedRetrospective = serde_json::from_str(json_slice).ok()?;
    Some((parsed.playbook, parsed.boss_profile))
}

fn extract_json_object(text: &str) -> Option<&str> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    (start <= end).then_some(&text[start..=end])
}

/// Extract `(start_span, end_span)` from `<!-- start → end · ... -->` headers of
/// the first and last leaf in a condensed batch.
pub(crate) fn condensed_span_header(leaves: &[String]) -> String {
    fn span_of(leaf: &str) -> Option<(&str, &str)> {
        let first = leaf.trim().lines().next()?;
        let inner = first.strip_prefix("<!--")?.strip_suffix("-->")?.trim();
        let (left, right) = inner.split_once('→')?;
        let end = right.split('·').next()?.trim();
        Some((left.trim(), end))
    }
    let start = leaves
        .first()
        .and_then(|l| span_of(l).map(|(s, _)| s))
        .unwrap_or("earlier");
    let end = leaves
        .last()
        .and_then(|l| span_of(l).map(|(_, e)| e))
        .unwrap_or("later");
    format!("<!-- {start} → {end} · condensed {} summaries -->", leaves.len())
}

impl ConversationService {
    /// Run a retrospective pass for `assistant_id` if at least 24h have elapsed
    /// since `last_retrospective_at` (or immediately when `force` is true).
    /// `Ok(false)` when there was nothing to review, `Err` with a short reason
    /// when the engine could not write one.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn maybe_run_retrospective(
        &self,
        user_id: &str,
        assistant_id: &str,
        thread_conversation_id: Option<&str>,
        generator: &dyn TitleGenerator,
        actor: &str,
        reason: &str,
        force: bool,
    ) -> Result<bool, String> {
        let Some(port) = self.role_port() else {
            return Ok(false);
        };
        let Some(ctx) = port.retrospective_context(user_id, assistant_id).await else {
            return Ok(false);
        };
        let now = now_ms();
        if !force
            && ctx.last_retrospective_at > 0
            && now.saturating_sub(ctx.last_retrospective_at) < RETROSPECTIVE_MIN_INTERVAL_MS
        {
            return Ok(false);
        }

        let evidence = self
            .gather_retrospective_evidence(
                &port,
                user_id,
                assistant_id,
                thread_conversation_id,
                ctx.last_retrospective_at,
                &ctx.failed_runs,
            )
            .await;
        if evidence.trim().is_empty() {
            return Ok(false);
        }

        let mut prompt = String::from(RETROSPECTIVE_INSTRUCTIONS);
        prompt.push_str("\n\n<current-playbook>\n");
        if ctx.playbook_lines.is_empty() {
            prompt.push_str("(empty)\n");
        } else {
            for line in &ctx.playbook_lines {
                prompt.push_str("- ");
                prompt.push_str(line);
                prompt.push('\n');
            }
        }
        prompt.push_str("</current-playbook>\n\n<current-boss-profile>\n");
        if ctx.boss_profile.trim().is_empty() {
            prompt.push_str("(empty)\n");
        } else {
            prompt.push_str(ctx.boss_profile.trim());
            prompt.push('\n');
        }
        prompt.push_str("</current-boss-profile>\n\n<evidence>\n");
        prompt.push_str(&evidence);
        prompt.push_str("\n</evidence>");

        let answer =
            match tokio::time::timeout(SUMMARY_TIMEOUT + Duration::from_secs(30), generator.summarize(&prompt)).await {
                Ok(Ok(ans)) => ans,
                Ok(Err(error)) => {
                    warn!(assistant_id, error = %error, "role retrospective: summarize failed");
                    return Err(format!("retrospective: {error}"));
                }
                Err(_) => {
                    warn!(assistant_id, "role retrospective: summarize timed out");
                    return Err("retrospective: timed out".to_owned());
                }
            };

        let Some((playbook_ops, boss_ops)) = parse_retrospective_output(&answer) else {
            warn!(
                assistant_id,
                "role retrospective: model answer was not valid delta JSON"
            );
            return Err("retrospective: the answer was not valid JSON".to_owned());
        };

        port.apply_retrospective(assistant_id, &playbook_ops, &boss_ops, actor, reason, now)
            .await;
        info!(
            assistant_id,
            playbook_ops = playbook_ops.len(),
            boss_ops = boss_ops.len(),
            "role retrospective completed"
        );
        Ok(true)
    }

    async fn gather_retrospective_evidence(
        &self,
        port: &Arc<dyn ConversationRolePort>,
        user_id: &str,
        assistant_id: &str,
        thread_conversation_id: Option<&str>,
        since_ms: i64,
        failed_runs: &[String],
    ) -> String {
        let mut sections = Vec::new();

        if let Some(tid) = thread_conversation_id {
            if let Some(state) = port.thread_state(assistant_id, tid).await
                && !state.leaves.is_empty()
            {
                let tail_leaves: Vec<&str> = state.leaves.iter().rev().take(3).rev().map(String::as_str).collect();
                sections.push(format!("Recent thread summaries:\n{}", tail_leaves.join("\n\n")));
            }

            let after = (since_ms > 0).then(|| RoleThreadMark {
                created_at: since_ms,
                id: String::new(),
            });
            if let Ok(page) = self
                .conversation_repo()
                .list_messages_page(
                    user_id,
                    tid,
                    &MessagePageParams {
                        limit: 80,
                        direction: match after {
                            Some(mark) => MessagePageDirection::After {
                                cursor: aionui_db::MessagePageCursor {
                                    created_at: mark.created_at,
                                    id: mark.id,
                                },
                            },
                            None => MessagePageDirection::InitialLatest,
                        },
                    },
                )
                .await
            {
                let lines = lines_of(&page.items);
                if !lines.is_empty() {
                    let mut exchange = String::new();
                    for line in lines.iter().rev().take(30).rev() {
                        let snippet: String = line.text.chars().take(500).collect();
                        exchange.push_str(&format!("[{}] {}\n", line.speaker.as_str(), snippet));
                    }
                    sections.push(format!("Recent thread exchanges & report replies:\n{exchange}"));
                }

                // Tool denials / rejected approvals in the thread.
                let mut denials = Vec::new();
                for row in &page.items {
                    if row.hidden {
                        continue;
                    }
                    let rejected_status = matches!(row.status.as_deref(), Some("canceled" | "error"));
                    let mentions_denial = row.content.contains("permission_denied")
                        || row.content.contains("rejected")
                        || row.content.contains("denied");
                    if (row.r#type != "text" && rejected_status) || (row.r#type == "tool_call" && mentions_denial) {
                        let clipped: String = row.content.chars().take(240).collect();
                        denials.push(format!("- ({}) {clipped}", row.r#type));
                    }
                }
                if !denials.is_empty() {
                    sections.push(format!(
                        "Rejected approvals or denied tool calls:\n{}",
                        denials.into_iter().take(10).collect::<Vec<_>>().join("\n")
                    ));
                }
            }
        }

        if !failed_runs.is_empty() {
            sections.push(format!("Recent failed scheduled runs:\n{}", failed_runs.join("\n")));
        }

        sections.join("\n\n")
    }

    /// Condense batches of [`CONDENSE_BATCH_SIZE`] uncondensed `D0` leaves into
    /// `condensed/` higher-level summaries. Returns how many batches were condensed.
    pub(crate) async fn condense_leaves_for_role(&self, assistant_id: &str, generator: &dyn TitleGenerator) -> usize {
        let Some(port) = self.role_port() else {
            return 0;
        };
        let mut batches_done = 0;
        // Cap per pass so one call stays bounded.
        for _ in 0..4 {
            let Some((conv_id, start_idx, batch)) = port
                .next_uncondensed_leaf_batch(assistant_id, CONDENSE_BATCH_SIZE)
                .await
            else {
                break;
            };
            let mut prompt = String::from(CONDENSE_INSTRUCTIONS);
            prompt.push_str("\n\n<leaves>\n");
            prompt.push_str(&batch.join("\n\n---\n\n"));
            prompt.push_str("\n</leaves>");

            let answer =
                match tokio::time::timeout(SUMMARY_TIMEOUT + Duration::from_secs(30), generator.summarize(&prompt))
                    .await
                {
                    Ok(Ok(ans)) => ans,
                    Ok(Err(error)) => {
                        warn!(assistant_id, error = %error, "role leaves: condensing failed");
                        break;
                    }
                    Err(_) => {
                        warn!(assistant_id, "role leaves: condensing timed out");
                        break;
                    }
                };
            let Some(summary) = clean_leaf(&answer) else {
                warn!(assistant_id, "role leaves: condensed summary was cut off");
                break;
            };
            let header = condensed_span_header(&batch);
            let condensed = format!("{header}\n{summary}\n");
            if !port
                .record_condensed_leaf(assistant_id, &conv_id, start_idx, batch.len(), &condensed)
                .await
            {
                break;
            }
            batches_done += 1;
        }
        batches_done
    }

    /// Execute the LLM-assisted portions of S5 Nightly Dreamer for `assistant_id`
    /// (memory verify & curate when needed, weekly retrospective + boss profile,
    /// and LCM leaf condensation).
    pub async fn run_role_dreamer_tasks(
        &self,
        user_id: &str,
        assistant_id: &str,
        thread_conversation_id: Option<&str>,
        current_memory: &str,
        force_retrospective: bool,
        task_manager: &Arc<dyn IWorkerTaskManager>,
    ) -> RoleDreamTasksOutcome {
        let mut outcome = RoleDreamTasksOutcome::default();

        // Pick a conversation of this role to snapshot a `TitleGenerator` from
        // (thread first, else most recent scheduled run).
        let candidate_conv = match thread_conversation_id {
            Some(tid) => Some(tid.to_owned()),
            None => self
                .conversation_repo()
                .list_scheduled_for_assistant(user_id, assistant_id)
                .await
                .ok()
                .and_then(|mut runs| runs.pop()),
        };
        let Some(conv_id) = candidate_conv else {
            return outcome;
        };

        // The dreamer writes on the role's behalf, so it takes the isolated
        // summarizer, which needs no reply of this runtime to have succeeded:
        // a runtime woken for the pass has had none.
        let generator = if let Some(live) = task_manager.get_task(&conv_id) {
            live.summarizer().await
        } else {
            match self
                .ensure_runtime_agent(user_id, &conv_id, task_manager, "role_dreamer")
                .await
            {
                Ok((agent, _)) => agent.summarizer().await,
                Err(error) => {
                    warn!(assistant_id, conversation_id = %conv_id, error = %error, "role dreamer: runtime unavailable");
                    outcome.error = Some(format!("engine unavailable: {error}"));
                    return outcome;
                }
            }
        };
        let Some(generator) = generator else {
            warn!(assistant_id, conversation_id = %conv_id, "role dreamer: the engine has no summarizer");
            outcome.error = Some("the engine has no summarizer".to_owned());
            return outcome;
        };
        let mut errors = Vec::new();

        // 1. Verify & curate `MEMORY.md` when approaching the 64 KiB cap (>= 48 KiB)
        //    or when it has grown past 25 lines during a dreamer pass.
        let line_count = current_memory.lines().filter(|l| !l.trim().is_empty()).count();
        if current_memory.len() >= DREAMER_CURATE_THRESHOLD_BYTES || line_count >= 25 {
            match self
                .verify_and_curate_memory(assistant_id, thread_conversation_id, current_memory, generator.as_ref())
                .await
            {
                Ok(curated) => outcome.curated_memory = Some(curated),
                Err(error) => {
                    warn!(assistant_id, error = %error, "role dreamer: memory not curated");
                    errors.push(error);
                }
            }
        }

        // 2. Retrospective (weekly in background dreamer, or immediately when forced).
        let due_weekly = if let Some(port) = self.role_port()
            && let Some(ctx) = port.retrospective_context(user_id, assistant_id).await
        {
            force_retrospective
                || ctx.last_retrospective_at == 0
                || now_ms().saturating_sub(ctx.last_retrospective_at) >= WEEKLY_RETROSPECTIVE_MS
        } else {
            false
        };
        if due_weekly {
            match self
                .maybe_run_retrospective(
                    user_id,
                    assistant_id,
                    thread_conversation_id,
                    generator.as_ref(),
                    "dreamer",
                    "nightly retrospective",
                    true,
                )
                .await
            {
                Ok(ran) => outcome.retrospective_ran = ran,
                Err(error) => errors.push(error),
            }
        }

        // 3. LCM leaf condensation (`D0` -> `condensed/`).
        outcome.condensed_batches = self.condense_leaves_for_role(assistant_id, generator.as_ref()).await;
        if outcome.condensed_batches > 0
            && let Some(port) = self.role_port()
        {
            port.catch_up_recall(user_id, assistant_id).await;
        }

        if !errors.is_empty() {
            outcome.error = Some(errors.join("; "));
        }
        outcome
    }

    async fn verify_and_curate_memory(
        &self,
        assistant_id: &str,
        thread_conversation_id: Option<&str>,
        current_memory: &str,
        generator: &dyn TitleGenerator,
    ) -> Result<String, String> {
        let mut prompt = String::from(VERIFY_AND_CURATE_INSTRUCTIONS);
        if let Some(port) = self.role_port()
            && let Some(tid) = thread_conversation_id
            && let Some(state) = port.thread_state(assistant_id, tid).await
            && !state.leaves.is_empty()
        {
            let recent: Vec<&str> = state.leaves.iter().rev().take(2).rev().map(String::as_str).collect();
            prompt.push_str("\n\n<recent-evidence>\n");
            prompt.push_str(&recent.join("\n\n"));
            prompt.push_str("\n</recent-evidence>");
        }
        prompt.push_str("\n\n<memory>\n");
        prompt.push_str(current_memory.trim());
        prompt.push_str("\n</memory>");

        let answer =
            match tokio::time::timeout(SUMMARY_TIMEOUT + Duration::from_secs(30), generator.summarize(&prompt)).await {
                Ok(Ok(answer)) => answer,
                Ok(Err(error)) => return Err(format!("curate: {error}")),
                Err(_) => return Err("curate: timed out".to_owned()),
            };
        clean_leaf(&answer).ok_or_else(|| "curate: the answer was cut off".to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_000 * HOUR_MS;

    #[test]
    fn dreamer_waits_for_the_user_to_go_quiet() {
        let day_ago = NOW - 21 * HOUR_MS;
        assert!(!dreamer_due(NOW, day_ago, false, HOUR_MS, false));
        assert!(dreamer_due(NOW, day_ago, false, DREAMER_QUIET_MS, false));
        // A role never run before goes on the first quiet stretch.
        assert!(dreamer_due(NOW, 0, false, DREAMER_QUIET_MS, false));
    }

    #[test]
    fn dreamer_runs_once_a_day_and_retries_a_failed_pass() {
        let hours_ago = NOW - 3 * HOUR_MS;
        assert!(!dreamer_due(NOW, hours_ago, false, 10 * HOUR_MS, false));
        assert!(dreamer_due(NOW, hours_ago, true, 10 * HOUR_MS, false));
        // Never sooner than the gap, failed or near the cap.
        let just_now = NOW - HOUR_MS;
        assert!(!dreamer_due(NOW, just_now, true, 10 * HOUR_MS, true));
    }

    #[test]
    fn dreamer_goes_anyway_when_overdue_or_near_the_cap() {
        assert!(dreamer_due(NOW, NOW - 45 * HOUR_MS, false, 0, false));
        assert!(dreamer_due(NOW, NOW - 3 * HOUR_MS, false, 0, true));
    }

    #[test]
    fn retrospective_parser_extracts_delta_ops_and_ignores_fences_or_think_tags() {
        let raw = "<think>analyzing</think>\n```json\n{\n  \"playbook\": [\n    {\"op\": \"ADD\", \"content\": \"发周报前核对数据源\"},\n    {\"op\": \"UPDATE\", \"id\": \"p-0001\", \"helpful_delta\": 2}\n  ],\n  \"boss_profile\": [\n    {\"op\": \"ADD\", \"content\": \"先写结论\"}\n  ]\n}\n```\n[end]\n";
        let (pb, boss) = parse_retrospective_output(raw).unwrap();
        assert_eq!(pb.len(), 2);
        assert_eq!(boss.len(), 1);
    }

    #[test]
    fn condensed_span_header_combines_first_start_and_last_end() {
        let leaves = vec![
            "<!-- 2026-11-01 09:00 → 2026-11-01 10:00 · 8 messages -->\n- a".to_owned(),
            "<!-- 2026-11-03 14:00 → 2026-11-03 16:30 · 11 messages -->\n- b".to_owned(),
        ];
        assert_eq!(
            condensed_span_header(&leaves),
            "<!-- 2026-11-01 09:00 → 2026-11-03 16:30 · condensed 2 summaries -->"
        );
    }
}
