use aionui_api_types::{AgentErrorCode, AgentErrorOwnership};
use aionui_common::{AgentKillReason, AgentType};
use tracing::info;

use crate::runtime_state::RuntimeLifecycleState;
use crate::stream_relay::RelayOutcome;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionRecoverySignal {
    CompactFailed,
    ResumeFailed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnRecoveryDecision {
    None,
    AutoReplayOnce {
        reason: AgentKillReason,
        safe_to_auto_replay: bool,
        session_recovery_signal: Option<SessionRecoverySignal>,
    },
}

pub struct TurnRecoveryPolicy;

impl TurnRecoveryPolicy {
    /// Authorize a Team delivery retry only when this turn has not spent its
    /// automatic replay and no visible output or side effect can be duplicated.
    pub(crate) fn allow_delivery_retry(
        outcome: &RelayOutcome,
        lifecycle: RuntimeLifecycleState,
        already_replayed: bool,
    ) -> bool {
        lifecycle == RuntimeLifecycleState::Active
            && outcome.terminal.is_error()
            && outcome.terminal.retryable() == Some(true)
            && outcome.attempt.safe_to_auto_replay()
            && !already_replayed
    }

    pub fn decide(
        agent_type: AgentType,
        backend: Option<&str>,
        outcome: &RelayOutcome,
        lifecycle: RuntimeLifecycleState,
        already_replayed: bool,
    ) -> TurnRecoveryDecision {
        let error_code = outcome.terminal.code();
        let retryable = outcome.terminal.retryable();
        let safe_to_auto_replay = outcome.attempt.safe_to_auto_replay();
        let session_recovery_signal = classify_session_recovery_signal(outcome);

        let decision = if lifecycle == RuntimeLifecycleState::Active
            && agent_type == AgentType::Acp
            && outcome.terminal.is_error()
            && retryable == Some(true)
            && error_code != Some(AgentErrorCode::UserLlmProviderModelNotFound)
            && (session_recovery_signal.is_some() || !is_provider_turn_error(error_code, outcome))
            && safe_to_auto_replay
            && !already_replayed
        {
            TurnRecoveryDecision::AutoReplayOnce {
                reason: AgentKillReason::AgentErrorRecovery,
                safe_to_auto_replay,
                session_recovery_signal: session_recovery_signal.clone(),
            }
        } else {
            TurnRecoveryDecision::None
        };

        info!(
            ?agent_type,
            backend = backend.unwrap_or("unknown"),
            error_code = ?error_code,
            retryable = ?retryable,
            lifecycle = ?lifecycle,
            already_replayed,
            safe_to_auto_replay,
            session_recovery_signal = ?session_recovery_signal,
            saw_visible_output = outcome.attempt.saw_visible_output,
            saw_tool_or_side_effect = outcome.attempt.saw_tool_or_side_effect,
            persisted_assistant_output = outcome.attempt.persisted_assistant_output,
            saw_event_gap = outcome.attempt.saw_event_gap,
            decision = ?decision,
            "conversation turn recovery decision"
        );

        decision
    }
}

fn is_provider_turn_error(error_code: Option<AgentErrorCode>, outcome: &RelayOutcome) -> bool {
    if matches!(
        outcome
            .attempt
            .terminal_error
            .as_ref()
            .and_then(|error| error.ownership),
        Some(AgentErrorOwnership::UserLlmProvider)
    ) {
        return true;
    }

    matches!(
        error_code,
        Some(
            AgentErrorCode::UserLlmProviderAuthFailed
                | AgentErrorCode::UserLlmProviderAwsSsoExpired
                | AgentErrorCode::UserLlmProviderPermissionDenied
                | AgentErrorCode::UserLlmProviderBillingRequired
                | AgentErrorCode::UserLlmProviderConfigError
                | AgentErrorCode::UserLlmProviderModelNotFound
                | AgentErrorCode::UserLlmProviderUnsupportedModel
                | AgentErrorCode::UserLlmProviderEndpointNotFound
                | AgentErrorCode::UserLlmProviderInvalidRequest
                | AgentErrorCode::UserLlmProviderInvalidToolSchema
                | AgentErrorCode::UserLlmProviderContextTooLarge
                | AgentErrorCode::UserLlmProviderRateLimited
                | AgentErrorCode::UserLlmProviderTimeout
                | AgentErrorCode::UserLlmProviderNetworkError
                | AgentErrorCode::UserLlmProviderEmptyResponse
                | AgentErrorCode::UserLlmProviderGatewayError
        )
    )
}

fn classify_session_recovery_signal(outcome: &RelayOutcome) -> Option<SessionRecoverySignal> {
    let data = outcome.attempt.terminal_error.as_ref()?;
    let haystack = format!("{}\n{}", data.message, data.detail.as_deref().unwrap_or_default()).to_ascii_lowercase();

    if haystack.contains("/responses/compact") || haystack.contains("remote compact failed") {
        return Some(SessionRecoverySignal::CompactFailed);
    }
    if haystack.contains("session/load") || haystack.contains("resume") {
        return Some(SessionRecoverySignal::ResumeFailed);
    }
    None
}

#[cfg(test)]
mod tests {
    use aionui_api_types::{AgentErrorCode, AgentErrorOwnership, AgentStreamErrorData};
    use aionui_common::{AgentKillReason, AgentType};

    use super::*;
    use crate::runtime_state::RuntimeLifecycleState;
    use crate::stream_relay::{RelayOutcome, RelayTerminal, TurnAttemptSummary};

    fn retryable_clean_error() -> RelayOutcome {
        RelayOutcome {
            terminal: RelayTerminal::Error {
                code: Some(AgentErrorCode::UnknownUpstreamError),
                retryable: Some(true),
            },
            attempt: TurnAttemptSummary::default(),
            ..RelayOutcome::default()
        }
    }

    #[test]
    fn outer_delivery_retry_requires_explicit_permission_and_an_unused_replay() {
        let mut outcome = retryable_clean_error();
        assert!(TurnRecoveryPolicy::allow_delivery_retry(
            &outcome,
            RuntimeLifecycleState::Active,
            false,
        ));
        assert!(!TurnRecoveryPolicy::allow_delivery_retry(
            &outcome,
            RuntimeLifecycleState::Active,
            true,
        ));
        for lifecycle in [
            RuntimeLifecycleState::Deleting,
            RuntimeLifecycleState::Cancelling,
            RuntimeLifecycleState::ShuttingDown,
        ] {
            assert!(!TurnRecoveryPolicy::allow_delivery_retry(&outcome, lifecycle, false));
        }
        outcome.terminal = RelayTerminal::Error {
            code: Some(AgentErrorCode::UserLlmProviderRateLimited),
            retryable: Some(false),
        };
        assert!(!TurnRecoveryPolicy::allow_delivery_retry(
            &outcome,
            RuntimeLifecycleState::Active,
            false,
        ));
    }

    #[test]
    fn outer_delivery_retry_rejects_output_side_effects_and_missing_events() {
        for attempt in [
            TurnAttemptSummary {
                saw_visible_output: true,
                ..Default::default()
            },
            TurnAttemptSummary {
                saw_tool_or_side_effect: true,
                ..Default::default()
            },
            TurnAttemptSummary {
                persisted_assistant_output: true,
                ..Default::default()
            },
            TurnAttemptSummary {
                saw_event_gap: true,
                ..Default::default()
            },
        ] {
            let outcome = RelayOutcome {
                attempt,
                ..retryable_clean_error()
            };
            assert!(!TurnRecoveryPolicy::allow_delivery_retry(
                &outcome,
                RuntimeLifecycleState::Active,
                false,
            ));
        }
    }

    #[test]
    fn a_gap_in_an_earlier_continuation_keeps_the_turn_unsafe() {
        let mut summary = TurnAttemptSummary::default();
        summary.merge(&TurnAttemptSummary {
            saw_event_gap: true,
            ..Default::default()
        });
        summary.merge(&TurnAttemptSummary::default());
        assert!(!summary.safe_to_auto_replay());
    }

    #[test]
    fn retryable_clean_acp_error_auto_replays_once() {
        let outcome = retryable_clean_error();

        let decision = TurnRecoveryPolicy::decide(
            AgentType::Acp,
            Some("codex"),
            &outcome,
            RuntimeLifecycleState::Active,
            false,
        );

        assert_eq!(
            decision,
            TurnRecoveryDecision::AutoReplayOnce {
                reason: AgentKillReason::AgentErrorRecovery,
                safe_to_auto_replay: true,
                session_recovery_signal: None,
            }
        );
    }

    #[test]
    fn compact_error_records_session_recovery_signal_without_fresh_session() {
        let mut outcome = retryable_clean_error();
        outcome.attempt.terminal_error = Some(AgentStreamErrorData::classified(
            "The model provider could not be reached",
            AgentErrorCode::UserLlmProviderNetworkError,
            AgentErrorOwnership::UserLlmProvider,
            Some("remote compact failed: error sending request for url (https://chatgpt.com/backend-api/codex/responses/compact)".into()),
            true,
            false,
            None,
        ));

        let decision = TurnRecoveryPolicy::decide(
            AgentType::Acp,
            Some("codex"),
            &outcome,
            RuntimeLifecycleState::Active,
            false,
        );

        assert_eq!(
            decision,
            TurnRecoveryDecision::AutoReplayOnce {
                reason: AgentKillReason::AgentErrorRecovery,
                safe_to_auto_replay: true,
                session_recovery_signal: Some(SessionRecoverySignal::CompactFailed),
            }
        );
    }

    #[test]
    fn provider_turn_network_error_does_not_auto_replay() {
        let mut outcome = retryable_clean_error();
        outcome.terminal = RelayTerminal::Error {
            code: Some(AgentErrorCode::UserLlmProviderNetworkError),
            retryable: Some(true),
        };
        outcome.attempt.terminal_error = Some(AgentStreamErrorData::classified(
            "The model provider could not be reached",
            AgentErrorCode::UserLlmProviderNetworkError,
            AgentErrorOwnership::UserLlmProvider,
            Some(
                "Agent internal error (code -32603): reqwest error stream: error sending request for url (https://cli-chat-proxy.grok.com/v1/responses)"
                    .into(),
            ),
            true,
            false,
            None,
        ));

        let decision = TurnRecoveryPolicy::decide(
            AgentType::Acp,
            Some("grok"),
            &outcome,
            RuntimeLifecycleState::Active,
            false,
        );

        assert_eq!(decision, TurnRecoveryDecision::None);
    }

    #[test]
    fn session_load_error_records_resume_failed_signal() {
        let mut outcome = retryable_clean_error();
        outcome.attempt.terminal_error = Some(AgentStreamErrorData::classified(
            "The Agent session could not be resumed",
            AgentErrorCode::UserAgentSessionNotFound,
            AgentErrorOwnership::UserAgent,
            Some("session/load failed while resuming previous ACP session".into()),
            true,
            false,
            None,
        ));

        let decision = TurnRecoveryPolicy::decide(
            AgentType::Acp,
            Some("codex"),
            &outcome,
            RuntimeLifecycleState::Active,
            false,
        );

        assert_eq!(
            decision,
            TurnRecoveryDecision::AutoReplayOnce {
                reason: AgentKillReason::AgentErrorRecovery,
                safe_to_auto_replay: true,
                session_recovery_signal: Some(SessionRecoverySignal::ResumeFailed),
            }
        );
    }

    #[test]
    fn already_replayed_error_does_not_replay_again() {
        let outcome = retryable_clean_error();

        let decision = TurnRecoveryPolicy::decide(
            AgentType::Acp,
            Some("codex"),
            &outcome,
            RuntimeLifecycleState::Active,
            true,
        );

        assert_eq!(decision, TurnRecoveryDecision::None);
    }

    #[test]
    fn visible_output_blocks_auto_replay() {
        let mut outcome = retryable_clean_error();
        outcome.attempt.saw_visible_output = true;

        let decision = TurnRecoveryPolicy::decide(
            AgentType::Acp,
            Some("codex"),
            &outcome,
            RuntimeLifecycleState::Active,
            false,
        );

        assert_eq!(decision, TurnRecoveryDecision::None);
    }

    #[test]
    fn tool_side_effect_blocks_auto_replay() {
        let mut outcome = retryable_clean_error();
        outcome.attempt.saw_tool_or_side_effect = true;

        let decision = TurnRecoveryPolicy::decide(
            AgentType::Acp,
            Some("codex"),
            &outcome,
            RuntimeLifecycleState::Active,
            false,
        );

        assert_eq!(decision, TurnRecoveryDecision::None);
    }

    #[test]
    fn non_retryable_error_does_not_auto_replay() {
        let outcome = RelayOutcome {
            terminal: RelayTerminal::Error {
                code: Some(AgentErrorCode::UserLlmProviderAuthFailed),
                retryable: Some(false),
            },
            attempt: TurnAttemptSummary::default(),
            ..RelayOutcome::default()
        };

        let decision = TurnRecoveryPolicy::decide(
            AgentType::Acp,
            Some("codex"),
            &outcome,
            RuntimeLifecycleState::Active,
            false,
        );

        assert_eq!(decision, TurnRecoveryDecision::None);
    }

    #[test]
    fn model_not_found_does_not_auto_replay_even_if_retryable() {
        let outcome = RelayOutcome {
            terminal: RelayTerminal::Error {
                code: Some(AgentErrorCode::UserLlmProviderModelNotFound),
                retryable: Some(true),
            },
            attempt: TurnAttemptSummary::default(),
            ..RelayOutcome::default()
        };

        let decision = TurnRecoveryPolicy::decide(
            AgentType::Acp,
            Some("codex"),
            &outcome,
            RuntimeLifecycleState::Active,
            false,
        );

        assert_eq!(decision, TurnRecoveryDecision::None);
    }

    #[test]
    fn non_active_lifecycle_does_not_auto_replay() {
        let outcome = retryable_clean_error();

        let decision = TurnRecoveryPolicy::decide(
            AgentType::Acp,
            Some("codex"),
            &outcome,
            RuntimeLifecycleState::Deleting,
            false,
        );

        assert_eq!(decision, TurnRecoveryDecision::None);
    }

    #[test]
    fn non_acp_agent_does_not_auto_replay() {
        let outcome = retryable_clean_error();

        let decision =
            TurnRecoveryPolicy::decide(AgentType::Aionrs, None, &outcome, RuntimeLifecycleState::Active, false);

        assert_eq!(decision, TurnRecoveryDecision::None);
    }
}
