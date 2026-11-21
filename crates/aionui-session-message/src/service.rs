//! Delivery semantics.
//!
//! The core invariant: delivery DELEGATES to
//! `ConversationService::send_message` — the human send path — instead of
//! writing a second one. That makes "cross-session delivery ≡ a human pressing
//! send" a structural fact, and it makes the team-conversation rejection free:
//! that path already refuses team-owned conversations
//! (`aionui-conversation/src/service.rs`, the `team_id_from_extra` guard in
//! `send_message`), so no matter where an agent obtained a team conversation
//! id, it gets a 403.
//!
//! `send_message`, never `run_agent_turn`: the former ends in
//! `spawn_user_turn` and returns immediately; the latter awaits the whole turn
//! and would let one target block the single drainer for minutes.

use std::sync::Arc;

use aionui_ai_agent::IWorkerTaskManager;
use aionui_api_types::{
    MAX_ROLE_AUTO_HOPS, SendMessageRequest, SendMessageResponse, SessionDeliveryStatus,
    SessionMessageRateLimitedPayload, SessionRateGate, SessionSendMessageRequest, SessionSendMessageResponse,
    WebSocketMessage,
};
use aionui_common::constants::{AIONUI_SESSION_MESSAGE_END_MARKER, AIONUI_SESSION_MESSAGE_MARKER};
use aionui_conversation::session_mentions::{team_id_from_extra_str, workspace_from_extra};
use aionui_conversation::{ConversationError, ConversationService};
use aionui_db::{IConversationRepository, ISettingsRepository, MessagePageDirection, MessagePageParams};
use aionui_realtime::EventBroadcaster;
use async_trait::async_trait;
use tokio::sync::Notify;
use tracing::{info, warn};

use crate::drainer::{DeliveryOutcome, DeliverySink, DrainGate};
use crate::error::SessionMessageError;
use crate::queue::{DeliveryQueue, PendingDelivery, TTL_MS};
use crate::rate_limit::{RateLimiter, RateVerdict};

/// Build the recipient-side block.
///
/// The short reply pointer after `reply_to` is deliberate (~10-15 tokens): the
/// recipient must know it can reply at all, or the reply path is dead. The full
/// schema does not go here — that is what `session capabilities` is for, so the
/// block closes with a pointer to it, framed as a fallback for when the
/// `session-message` skill is unavailable (matching the sender block's wording, so
/// a recipient that already has the skill does not run a needless capabilities
/// round trip). It carries no `send-message` payload command template — the skill
/// body stays the single source of that payload shape.
///
/// The trailing `session capabilities` line has no `from:`/`workspace:`/`reply_to:`
/// prefix, so the frontend's `parseSessionMessageBlock` (`sessionMarkers.ts`)
/// ignores it; the whole block is stripped from the bubble regardless, so it is
/// UI-safe and never breaks the `reply_to` address parse (which splits on `\t`).
///
/// The block is agent-facing — the frontend strips it and renders human-facing
/// labels separately via i18n — so its text (the reply pointer, the `workspace:`
/// warning, and the capabilities pointer) is written in English, not localized.
pub fn build_session_message_block(from_name: &str, from_id: &str, workspace_field: &str, reply_to: &str) -> String {
    build_session_message_block_with_meta(from_name, from_id, workspace_field, reply_to, None, None)
}

/// Build the recipient-side block with optional resident-role provenance
/// (`from_role: <role_id>\t<role_name>`) and autonomous hop counter (`hop: N/M`).
pub fn build_session_message_block_with_meta(
    from_name: &str,
    from_id: &str,
    workspace_field: &str,
    reply_to: &str,
    from_role: Option<(&str, &str)>,
    hop: Option<(u32, u32)>,
) -> String {
    let mut out = format!(
        "{AIONUI_SESSION_MESSAGE_MARKER}\n\
         from: {from_name}\t{from_id}\n"
    );
    if let Some((role_id, role_name)) = from_role {
        out.push_str(&format!("from_role: {role_id}\t{role_name}\n"));
    }
    out.push_str(&format!("workspace: {workspace_field}\n"));
    if let Some((current, max)) = hop {
        out.push_str(&format!("hop: {current}/{max}\n"));
    }
    out.push_str(&format!(
        "reply_to: {reply_to}\t(reply: session send-message, to=reply_to)\n\
         If the session-message skill is unavailable, run `\"$AIONUI_HELPER_BIN\" session capabilities` for the full delivery contract.\n\
         {AIONUI_SESSION_MESSAGE_END_MARKER}"
    ));
    out
}

/// Extract the inbound autonomous `hop` count from a persisted message text.
/// Returns `0` when the message was written by a human (no `[[AION_SESSION_MESSAGE]]`
/// block) or carries no `hop:` header.
pub fn parse_inbound_hop_from_text(text: &str) -> u32 {
    let trimmed = text.trim_start();
    let Some(after_open) = trimmed.strip_prefix(AIONUI_SESSION_MESSAGE_MARKER) else {
        return 0;
    };
    let Some(end_idx) = after_open.find(AIONUI_SESSION_MESSAGE_END_MARKER) else {
        return 0;
    };
    for line in after_open[..end_idx].lines() {
        if let Some(rest) = line.trim().strip_prefix("hop:") {
            let head = rest.trim().split('/').next().unwrap_or_default().trim();
            if let Ok(parsed) = head.parse::<u32>() {
                return parsed;
            }
        }
    }
    0
}

/// Result of [`SessionMessageService::send_detailed`], exposing the causal hop
/// recorded on the delivery for `role send`.
#[derive(Debug, Clone)]
pub struct DetailedDeliveryOutcome {
    pub response: SessionSendMessageResponse,
    pub hop: u32,
    pub max_hops: u32,
}

/// Block first, then the body: the block is context, not an attachment.
pub fn compose_delivery_content(block: &str, message: &str) -> String {
    format!("{block}\n\n{message}")
}

/// `workspace:` value for the recipient block.
///
/// Reports the SENDER's path, because that is what the recipient cannot infer.
/// An unknown sender workspace renders as `unknown` — never `same` — for the
/// same reason as the sender-side block: claiming `same` would tell the agent
/// relative paths are safe when we do not know that.
fn recipient_workspace_field(sender_workspace: Option<&str>, target_workspace: Option<&str>) -> String {
    match (sender_workspace, target_workspace) {
        (Some(sender), Some(target)) if sender == target => "same".to_owned(),
        (Some(sender), _) => format!("{sender} (differs from yours; don't use relative paths, don't assume readable)"),
        (None, _) => "unknown (differs from yours; don't use relative paths, don't assume readable)".to_owned(),
    }
}

pub struct SessionMessageDeps {
    pub conversation_service: ConversationService,
    pub conversation_repo: Arc<dyn IConversationRepository>,
    pub settings_repo: Arc<dyn ISettingsRepository>,
    pub task_manager: Arc<dyn IWorkerTaskManager>,
    pub broadcaster: Arc<dyn EventBroadcaster>,
    pub queue: Arc<DeliveryQueue>,
    pub rate_limiter: Arc<RateLimiter>,
    pub notify: Arc<Notify>,
}

pub struct SessionMessageService {
    deps: SessionMessageDeps,
}

impl SessionMessageService {
    pub fn new(deps: SessionMessageDeps) -> Self {
        Self { deps }
    }

    pub fn queue(&self) -> &Arc<DeliveryQueue> {
        &self.deps.queue
    }

    /// The global toggle, per user (`system_settings.user_id` is its PK).
    /// A read failure is treated as ENABLED — the toggle is an opt-out panic
    /// button, and a transient DB error must not silently disable a feature the
    /// user left on.
    pub async fn is_enabled_for(&self, user_id: &str) -> bool {
        match self.deps.settings_repo.get_settings(user_id).await {
            Ok(Some(settings)) => settings.cross_session_message_enabled,
            // No row yet → defaults, and the default is on.
            Ok(None) => true,
            Err(error) => {
                warn!(
                    user_id,
                    error = %error,
                    "cross-session toggle lookup failed; treating the feature as enabled"
                );
                true
            }
        }
    }

    /// Validation order is FIXED (spec §6.3). When several conditions hold at
    /// once, which code comes back must be deterministic or the tests are
    /// flaky:
    ///
    /// 1. `feature_disabled` — most global, cheapest
    /// 2. `sender_is_team`
    /// 3. `schema_validation_failed`
    /// 4. `rate_limited` — BEFORE the target lookup: during a spin storm we
    ///    must not pay a DB read per junk request
    /// 5. target checks — `target_is_self` / `target_not_found` / `target_is_team`
    /// 6. deliver via the human send path
    /// 7. queue on busy — only here can `queue_full` happen
    pub async fn send(
        &self,
        user_id: &str,
        from_conversation_id: &str,
        req: &SessionSendMessageRequest,
    ) -> Result<SessionSendMessageResponse, SessionMessageError> {
        self.send_detailed(user_id, from_conversation_id, req)
            .await
            .map(|outcome| outcome.response)
    }

    /// Same delivery contract as [`Self::send`], additionally returning the
    /// recorded causal hop count (`hop / max_hops`) for `role send`.
    pub async fn send_detailed(
        &self,
        user_id: &str,
        from_conversation_id: &str,
        req: &SessionSendMessageRequest,
    ) -> Result<DetailedDeliveryOutcome, SessionMessageError> {
        // 1
        if !self.is_enabled_for(user_id).await {
            warn!(
                from_conversation_id,
                outcome = "rejected",
                error_code = "feature_disabled",
                "cross-session delivery refused"
            );
            return Err(SessionMessageError::FeatureDisabled);
        }

        // 2 — the sender's own row. A teammate's agent holds a
        // ConversationHelper token too, so without this check it could use this
        // CLI to reach ordinary conversations, bypassing the team mailbox.
        let sender = self.load_sender(user_id, from_conversation_id).await?;
        if team_id_from_extra_str(&sender.extra).is_some() {
            warn!(
                from_conversation_id,
                outcome = "rejected",
                error_code = "sender_is_team",
                "cross-session delivery refused"
            );
            return Err(SessionMessageError::SenderIsTeam {
                id: from_conversation_id.to_owned(),
            });
        }

        // 3
        let to = req.to.trim();
        if to.is_empty() || to == "*" {
            return Err(SessionMessageError::SchemaValidation {
                reason: "`to` must be a conversation id; names and `*` are not supported".to_owned(),
            });
        }
        if req.message.trim().is_empty() {
            return Err(SessionMessageError::SchemaValidation {
                reason: "`message` must not be empty".to_owned(),
            });
        }

        // 4
        if let RateVerdict::Tripped { gate, window_count } =
            self.deps.rate_limiter.check_and_record(from_conversation_id, to)
        {
            self.broadcast_rate_limited(user_id, &sender.name, from_conversation_id, to, gate, window_count)
                .await;
            warn!(
                from_conversation_id,
                to_conversation_id = to,
                window_count,
                gate = ?gate,
                outcome = "rejected",
                error_code = "rate_limited",
                "cross-session rate gate tripped"
            );
            return Err(SessionMessageError::RateLimited);
        }

        // 5 — role permission, causal hop budget, and target checks.
        if to == from_conversation_id {
            return Err(SessionMessageError::TargetIsSelf { id: to.to_owned() });
        }
        let sender_role = self
            .deps
            .conversation_service
            .sender_role_identity(user_id, from_conversation_id)
            .await;
        if let Some((ref role_id, _, false)) = sender_role {
            warn!(
                from_conversation_id,
                role_id = %role_id,
                outcome = "rejected",
                error_code = "permission_denied",
                "resident role denied outbound message by permission table"
            );
            return Err(SessionMessageError::PermissionDenied);
        }

        let inbound_hop = self.current_inbound_hop(user_id, from_conversation_id).await;
        let outbound_hop = inbound_hop.saturating_add(1);
        if outbound_hop > MAX_ROLE_AUTO_HOPS {
            warn!(
                from_conversation_id,
                to_conversation_id = to,
                outbound_hop,
                max_hops = MAX_ROLE_AUTO_HOPS,
                outcome = "rejected",
                error_code = "hop_limit_reached",
                "autonomous cross-session hop budget exhausted"
            );
            return Err(SessionMessageError::HopLimitReached {
                hop: outbound_hop,
                max_hops: MAX_ROLE_AUTO_HOPS,
            });
        }

        let target = self
            .deps
            .conversation_repo
            .get(user_id, to)
            .await
            .map_err(|error| SessionMessageError::TransportUnavailable {
                reason: error.to_string(),
            })?
            // Scoped by user_id, so another user's id lands here: 404, not 403
            // — refuse without leaking that the id exists.
            .ok_or_else(|| SessionMessageError::TargetNotFound { id: to.to_owned() })?;
        if team_id_from_extra_str(&target.extra).is_some() {
            return Err(SessionMessageError::TargetIsTeam { id: to.to_owned() });
        }

        // 6
        let sender_workspace = workspace_from_extra(&sender.extra);
        let target_workspace = workspace_from_extra(&target.extra);
        let from_role_meta = sender_role
            .as_ref()
            .map(|(role_id, role_name, _)| (role_id.as_str(), role_name.as_str()));
        let sender_label = sender_role
            .as_ref()
            .map(|(_, role_name, _)| role_name.as_str())
            .filter(|label| !label.trim().is_empty())
            .unwrap_or_else(|| sender.name.as_str());
        let block = build_session_message_block_with_meta(
            sender_label,
            from_conversation_id,
            &recipient_workspace_field(sender_workspace.as_deref(), target_workspace.as_deref()),
            from_conversation_id,
            from_role_meta,
            Some((outbound_hop, MAX_ROLE_AUTO_HOPS)),
        );
        let content = compose_delivery_content(&block, &req.message);

        match self.deliver_now(user_id, to, content.clone()).await {
            Ok(sent) => {
                info!(
                    from_conversation_id,
                    to_conversation_id = to,
                    hop = outbound_hop,
                    outcome = "delivered",
                    delivered_midturn = sent.delivered_midturn,
                    "cross-session delivery accepted"
                );
                Ok(DetailedDeliveryOutcome {
                    response: SessionSendMessageResponse {
                        status: SessionDeliveryStatus::Delivered,
                        to: to.to_owned(),
                    },
                    hop: outbound_hop,
                    max_hops: MAX_ROLE_AUTO_HOPS,
                })
            }
            // 7
            Err(DeliverAttemptError::Transient(reason)) => {
                self.deps.queue.push(PendingDelivery {
                    to: to.to_owned(),
                    user_id: user_id.to_owned(),
                    from_conversation_id: from_conversation_id.to_owned(),
                    message: content,
                    expires_at_ms: self.deps.queue.clock().now_ms() + TTL_MS,
                })?;
                self.deps.notify.notify_one();
                info!(
                    from_conversation_id,
                    to_conversation_id = to,
                    hop = outbound_hop,
                    outcome = "queued",
                    reason = %reason,
                    "cross-session delivery queued; target not ready"
                );
                Ok(DetailedDeliveryOutcome {
                    response: SessionSendMessageResponse {
                        status: SessionDeliveryStatus::Queued,
                        to: to.to_owned(),
                    },
                    hop: outbound_hop,
                    max_hops: MAX_ROLE_AUTO_HOPS,
                })
            }
            Err(DeliverAttemptError::Hard(reason)) => Err(SessionMessageError::TransportUnavailable { reason }),
        }
    }

    async fn current_inbound_hop(&self, user_id: &str, conversation_id: &str) -> u32 {
        let Ok(page) = self
            .deps
            .conversation_repo
            .list_messages_page(
                user_id,
                conversation_id,
                &MessagePageParams {
                    limit: 20,
                    direction: MessagePageDirection::InitialLatest,
                },
            )
            .await
        else {
            return 0;
        };
        let Some(latest_user_row) = page
            .items
            .iter()
            .rfind(|row| !row.hidden && row.r#type == "text" && row.position.as_deref() == Some("right"))
        else {
            return 0;
        };
        if let Ok(envelope) = serde_json::from_str::<serde_json::Value>(&latest_user_row.content)
            && let Some(text) = envelope.get("content").and_then(serde_json::Value::as_str)
        {
            return parse_inbound_hop_from_text(text);
        }
        parse_inbound_hop_from_text(&latest_user_row.content)
    }

    /// `list` / `targets` / `mentionable` share the send path's two most global
    /// checks: the feature toggle and the team-sender rule. `capabilities` does
    /// NOT go through this — it is a static contract that touches no
    /// conversation data, so it stays available even when the feature is off.
    ///
    /// An EMPTY `from_conversation_id` skips the team-sender check rather than
    /// failing. The runtime route always has a token-bound caller, but the
    /// `@@` picker's `current_conversation_id` is optional (a picker opened
    /// outside a conversation has no sender to validate). Treating "no sender"
    /// as a missing row would turn the toggle check into a 409.
    pub async fn guard_list_access(
        &self,
        user_id: &str,
        from_conversation_id: &str,
    ) -> Result<(), SessionMessageError> {
        if !self.is_enabled_for(user_id).await {
            return Err(SessionMessageError::FeatureDisabled);
        }
        if from_conversation_id.is_empty() {
            return Ok(());
        }
        let sender = self.load_sender(user_id, from_conversation_id).await?;
        if team_id_from_extra_str(&sender.extra).is_some() {
            return Err(SessionMessageError::SenderIsTeam {
                id: from_conversation_id.to_owned(),
            });
        }
        Ok(())
    }

    /// The caller's own row. A missing row means the runtime token named a
    /// conversation that no longer exists, which is a transport-level problem
    /// rather than a target problem — hence `TransportUnavailable`, not
    /// `TargetNotFound`.
    async fn load_sender(
        &self,
        user_id: &str,
        from_conversation_id: &str,
    ) -> Result<aionui_db::models::ConversationRow, SessionMessageError> {
        self.deps
            .conversation_repo
            .get(user_id, from_conversation_id)
            .await
            .map_err(|error| SessionMessageError::TransportUnavailable {
                reason: error.to_string(),
            })?
            .ok_or_else(|| SessionMessageError::TransportUnavailable {
                reason: format!("sender conversation missing: {from_conversation_id}"),
            })
    }

    /// One attempt through the human send path. `content` already carries the
    /// recipient block.
    ///
    /// Returns the whole `SendMessageResponse` rather than `()` so callers can
    /// log `delivered_midturn`. Note what `Busy` means here: `send_message`
    /// handles mid-turn delivery internally, so `Busy` covers BOTH "backend
    /// cannot take a mid-turn message" AND "it can, but that turn is blocked on
    /// a pending confirmation card". The second case means even a claude/codex
    /// target can queue.
    async fn deliver_now(
        &self,
        user_id: &str,
        to: &str,
        content: String,
    ) -> Result<SendMessageResponse, DeliverAttemptError> {
        let request = SendMessageRequest {
            content,
            files: Vec::new(),
            // The delivered body is already fully composed; `@@` / `@@@`
            // resolution is a sender-side concern and must not re-run here.
            sessions: Vec::new(),
            roles: Vec::new(),
            inject_skills: Vec::new(),
            // Not hidden: the user opening the recipient conversation should
            // see this message. (cron uses hidden=true because its prompt is
            // template boilerplate; this is real content.)
            hidden: false,
        };
        match self
            .deps
            .conversation_service
            .send_message(user_id, to, request, &self.deps.task_manager)
            .await
        {
            Ok(response) => Ok(response),
            Err(error) => Err(classify_delivery_failure(error)),
        }
    }

    async fn broadcast_rate_limited(
        &self,
        user_id: &str,
        from_name: &str,
        from_conversation_id: &str,
        to: &str,
        gate: SessionRateGate,
        window_count: u32,
    ) {
        let to_name = self
            .deps
            .conversation_repo
            .get(user_id, to)
            .await
            .ok()
            .flatten()
            .map_or_else(|| to.to_owned(), |row| row.name);
        let payload = SessionMessageRateLimitedPayload {
            user_id: user_id.to_owned(),
            from_conversation_id: from_conversation_id.to_owned(),
            from_name: from_name.to_owned(),
            to_conversation_id: to.to_owned(),
            to_name,
            window_count,
            gate,
        };
        let Ok(value) = serde_json::to_value(&payload) else {
            warn!("failed to serialise sessionMessage.rateLimited payload");
            return;
        };
        self.deps
            .broadcaster
            .broadcast(WebSocketMessage::new("sessionMessage.rateLimited", value));
    }
}

#[derive(Debug)]
enum DeliverAttemptError {
    /// Try again on the next tick. Carries the rendered cause for logging only.
    Transient(String),
    /// A real answer: drop the item and `warn`.
    Hard(String),
}

/// Which `send_message` failures deserve another tick.
///
/// TRANSIENT is not just "busy". The second arm is a fixed regression: a user
/// restarting a conversation's runtime while messages were queued for it saw
/// them vanish, because `RuntimeRestarting` fell into the catch-all and the
/// drainer dropped the head. The cancel hook had deliberately KEPT that queue
/// (`TurnCancelCause::RuntimeRestart`) and was undone one tick later — silent
/// message loss, the worst failure mode this feature has, plus it made the
/// hook's cause distinction meaningless.
///
/// A restart is a ~1s window after which the conversation is IDLE — precisely
/// what a pending delivery is waiting for — so it is retried, not discarded.
/// The 10-minute TTL still bounds it: a restart that never finishes ends in an
/// expiry `warn`, not an unbounded retry loop.
///
/// Everything else is a real answer (gone, refused, archived) and must still be
/// dropped, or a bad reference would be retried until its TTL for no reason.
fn classify_delivery_failure(error: ConversationError) -> DeliverAttemptError {
    match &error {
        ConversationError::Busy { .. } | ConversationError::RuntimeRestarting { .. } => {
            DeliverAttemptError::Transient(error.to_string())
        }
        _ => DeliverAttemptError::Hard(error.to_string()),
    }
}

#[async_trait]
impl DeliverySink for SessionMessageService {
    async fn deliver(&self, item: &PendingDelivery) -> DeliveryOutcome {
        match self.deliver_now(&item.user_id, &item.to, item.message.clone()).await {
            Ok(_) => DeliveryOutcome::Delivered,
            Err(DeliverAttemptError::Transient(_)) => DeliveryOutcome::Busy,
            Err(DeliverAttemptError::Hard(reason)) => DeliveryOutcome::HardError(reason),
        }
    }
}

#[async_trait]
impl DrainGate for SessionMessageService {
    async fn is_enabled_for(&self, user_id: &str) -> bool {
        SessionMessageService::is_enabled_for(self, user_id).await
    }
}

#[cfg(test)]
#[path = "service_test.rs"]
mod service_test;
