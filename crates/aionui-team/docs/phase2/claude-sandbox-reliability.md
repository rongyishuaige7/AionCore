# Official Claude Code sandbox integration

## Contract

The two subscription profiles must execute the existing `claude-pro-danger` and
`claude-pro2-danger` entrypoints. Keep `env -i`, the original residential routing,
official CLI TLS, separate configuration directories, and native macOS Keychain
authentication. Do not inject provider credentials or `ANTHROPIC_BASE_URL`, add an
ACP/API bridge, inherit the host environment, or silently change accounts.

Tests must never launch `claude-pro` or `claude-pro-danger`. If a real account is
needed, use only `claude-pro2-danger`. Ordinary regression tests use recorded
non-sensitive error text, fake processes, and isolated databases/listeners.

## Staged changes

1. Stop unsafe delivery retries and preserve unread messages. Classify the
   observed Claude Code session-limit error as non-retryable. Carry an explicit
   automatic-retry decision from conversation execution into Team's port. Pause
   unsafe/non-retryable failed deliveries atomically; retry exhaustion must not
   acknowledge messages. Retain the existing bounded retry for safe failures.
2. Introduce explicit official-subscription launch profiles and remove provider
   environment injection for these profiles. Restore only verified streaming
   arguments. Keep the existing first-message preset workaround.
3. Integrate native HTTP MCP through a protected, launch-scoped descriptor and
   an exact loopback network exception. Fix HTTP framing/handshake and bind local
   authorization to the caller slot. Tool readiness needs real CLI evidence.
4. Persist account cooldown/delivery state, converge runtime recovery ownership,
   and prevent persistence gaps before WebSocket fanout. Add UI recovery and the
   scoped configuration/model/artifact fixes from the approved plan.

Each implementation stage gets its own checks and commit. Existing uncommitted
work is not part of these commits. Source changes do not authorize production
sandbox deployment, account-1 test launches, or historical database repair.

## Stage 1 scope and verification

Observed diagnostic text (2026-11-08 backend log):
`You've hit your session limit · resets 11am (America/New_York)`.
The original log is not copied; it can contain sensitive payloads.

- Classifier regression: the observed message retains its reset detail and is
  non-retryable; ordinary transient rate limiting remains retryable.
- Conversation policy: output, tool side effects, previous replay, or an unknown
  event gap prohibit automatic delivery replay.
- Team regression: a non-retryable outcome pauses after one attempt, preserves
  unread mail, and background notifications cannot restart it.
- Team regression: explicitly safe failures can retry and acknowledge only on
  successful completion; exhausting retries retains unread mail and notifies
  the lead that the member is paused.
- Coordinator regression: stale results cannot pause a newer batch, and explicit
  user intervention can resume retained messages.
- Run format, affected-crate tests, and affected-crate Clippy before committing.

Use existing slot-work events for the paused UI state. Add only low-volume,
structured retry-decision/pause diagnostics with IDs, counts, and booleans; no
prompts, tool payloads, error payloads, credentials, or environment values.

Stage 1 does not implement reset-time parsing, durable account cooldown, native
MCP, production deployment, or restoration of previously acknowledged messages.
Those remain explicit later-stage work, not verified behavior.

## Stage 1 evidence and limits

The existing native-session result translation calls the shared classifier for
non-cancelled error text (`crates/aionui-ai-agent/src/session_agent.rs`,
`translate_event`, `SessionEvent::TurnResult`). The new classification flows
through the conversation outcome and the Team adapter; Team must receive an
explicit safe-retry decision before retrying a started turn. An event gap makes
that decision unsafe even when all surviving events look harmless.

Verification on 2026-11-08:

- `cargo fmt --all -- --check`: passed.
- Affected-crate Clippy, including `--all-targets --locked -- -D warnings`:
  passed for `aionui-ai-agent`, `aionui-conversation`, `aionui-team`, and
  `aionui-app`; the last conversation/team edits were checked again.
- Final `cargo test -p aionui-conversation -p aionui-team --locked --quiet`:
  1362 passed and six existing ignored tests. This includes 485 conversation
  unit tests, the five new service-level retry regressions, and delivery recovery
  that verifies retained message IDs are delivered exactly once after resuming.
- Application regression targets passed in the affected-crate run; after the
  last lifecycle guard change, the application library's 51 tests passed again.
- AI-agent integration targets with `test-support`: passed; existing ignored
  tests were left ignored, without enabling live tests.
- The initial affected-crate run stopped at the AI-agent library with 1012
  passed and one failed test. The failing test is
  `registry::registry_tests::builtin_non_codex_with_broken_wrapper_is_not_installed`
  (`crates/aionui-ai-agent/src/registry_tests.rs:301`): it expects `Offline` and
  receives `Unchecked`. The same test fails identically in a clean, detached
  checkout of the original `8a5f805d322b535fb7dbc92a948e0820d1218394` HEAD.
  Its assertions were not relaxed, and this result is not a passing gate.

The final checks include cancellation during task construction: an early build
failure cannot authorize replay while the conversation is cancelling, deleting,
or shutting down. Recovery keeps the existing priority order: new foreground
user work can run before retained background mail, in separate batches.

These are source/fixture checks. They do not prove native MCP readiness,
residential egress, production sandbox behavior, or an account's live state.
The delivery pause remains in memory until the later durability stage; restarting
the backend can allow retained mail to be retried. Broadcast gaps now block
replay, but this stage does not repair lost text or change incomplete-turn UI.
