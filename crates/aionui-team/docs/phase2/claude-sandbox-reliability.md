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

The stage-1 full `just push` gate subsequently passed: 9,410 tests passed and
52 existing tests were skipped. Commit `5a955753` was pushed to the personal fork.

## Stage 2 design: subscription launch boundary

Resolve only the two explicitly selected home-local subscription launchers.
Legacy `claude-pro-yi` / `claude-pro2-yi` selections map to their account's
`claude-pro-danger` / `claude-pro2-danger`. Never promote a restricted entry,
accept a same-named script outside that home-local directory, search PATH on
failure, or switch accounts. Other vendors and ordinary Claude installations
keep their existing behavior.

A scoped Spawner enforces the selected absolute program and replaces explicit
environment entries with HOME, a fixed system PATH and TERM. A new default-off
`CommandSpec.clear_env` instructs both process implementations to clear inherited
environment first. This foundation change is source-only (no API/DB format
change); existing command literals retain `false`. The scoped Spawner also
discards extra environment supplied by callers. It is retained by the connection
and its wake recipe, covering initial/resume/fork, version checks and idle wake.
Registry and optional-flag probes must use the same isolated launch policy.
Subscription assembly never reads cc-switch credentials or merges agent/runtime
environment. Add one production-visible lifecycle log with profile and counts,
never values, arguments, MCP payloads or credentials.

The production guard's argument whitelist currently rejects the stream/control
flags (`~/.local/share/claude-pro2-hardening/daily-policy.cjs`, bool/values sets).
The sandbox repository supplies an opt-in argument-policy adapter for review;
it is not a replacement launcher and is not deployed. It allows only the verified
partial/replay flags, summarized/omitted thinking display, stdio permission control
and additive preset text. Existing validation still controls workspace, local
ports, commands and all other arguments. MCP config, plugins, settings, API and
network options receive no new permission. Existing uncommitted first-message
preset patches are preserved, not imported into this source commit.

CLI evidence: `claude-pro2-danger --help` on 2026-11-09 documents
`--include-partial-messages`, `--replay-user-messages` and
`--append-system-prompt`. First-party `@anthropic-ai/claude-agent-sdk` 0.3.319
(`sdk.mjs`, `sdk.d.ts`, fetched from registry.npmjs.org and verified against its
published SHA-512 integrity) serializes `--permission-prompt-tool=stdio` and
`--thinking-display`; ThinkingEnabled/ThinkingAdaptive permit only summarized
or omitted display. These sources establish argument contracts, not a successful
live session or MCP readiness.

Verify with real harmless fixture processes that host variables disappear,
injected provider/runtime variables never reach a subscription process, both
account selections remain fixed through session variants and probes, and legacy
launches keep their environment behavior. Account 1 is never executed by tests.
Run affected-crate tests/Clippy and the full pre-push gate in an isolated target
directory, so builds do not replace the running backend's on-disk executable.
Sandbox validation uses task-created fixtures and listeners only.

Deployment remains a separate reviewed step. The source changes require the
matching guard integration before use; native MCP and its protected descriptor
remain stage 3. No production services, credentials, routes, manifests or
historical databases are changed by this stage.

## Stage 2 verification and delivery boundary

On 2026-11-09, the four affected crates (`aionui-common`, `aionui-process`,
`aionui-ai-agent`, `aionui-session`) passed 1,809 tests, with 15 existing ignored
tests, using `aionui-ai-agent/test-support` and the locked dependencies. Both
real process implementations were exercised with harmless `/usr/bin/env -0`
fixtures. Assertions retained only environment key names. The inherited-env
baseline uses the existing Builder, including macOS's own DYLD-variable
filtering; isolated children expose only their explicit entries.

Session assembly fixtures cover both subscription accounts and fresh, resume,
fork and optional-flag version probes. They capture the exact danger program,
white-listed environment, working directory and additive/streaming arguments.
Only `/usr/bin/printf` supplies the fake version banner; no recorded Claude
command is executed. Scoped-spawner tests also reject a program mismatch and
discard extra caller environment. Registry probe configuration is checked with
a real harmless process. Idle wake retains the same scoped spawner in
`crates/aionui-session/src/backend/claude_conn.rs`; this stage adds no live
idle-wake or model-session evidence.

The opt-in sandbox adapter is delivered separately as `6b83d5b` on
`rongyishuaige7/claude-pro-sandbox`, branch `fix/claude-native-cli-arguments`.
Its complete `node scripts/check.cjs` run passed 324 tests with zero skips,
including 66/66 required real Seatbelt cases (profile prefix `d5831a681d8b`).
The first temporary-worktree run triggered existing host-assist overlap denials;
moving the fixture repository outside system temp resolved them without
changing policy. Account 1 was never launched. The only real subscription CLI
inspection in this stage was account 2's `--help`; no model turn was sent.

The AionCore full pre-push gate is required after the focused commit and its
result is reported with the pushed SHA. These source and fixture results do
not make the guard adapter active in production or restore native Team MCP.

## Stage 3 design: native Team MCP

Replace the handwritten HTTP reader with the existing Axum/Hyper stack: bounded
bodies, fragmented/chunked requests, JSON-RPC notifications, protocol negotiation,
and explicit HTTP status codes. Keep the endpoint on an ephemeral IPv4 loopback
port, reject browser origins and incorrect Host headers, and stop it with its
team. Issue a separate random HTTP credential per slot; the shared TCP bridge
credential must not authorize HTTP. Validate the live slot and bind its role on
the server. Log only lifecycle IDs and method names, never auth headers or bodies.

Add an optional native HTTP config to the existing internal Team launch snapshot.
Existing stdio consumers keep their existing fields; old persisted snapshots do
not authorize a subscription fallback. Official subscription assembly uses only
the native Team HTTP server and refuses unreviewed stdio/remote servers. Write
the native MCP JSON to a private, per-launch file outside workspace grants, pass
only its path to `--mcp-config`, and retain it through the connection's wake
recipe. No bearer token appears in argv or environment; delete the owned file
when the owning session is dropped. Version probes keep the clean launch policy.

The sandbox library adds a default-off, host-configured descriptor root. It
validates exact schema, ownership, permissions, links, size and IPv4 loopback URL;
the original validator still handles all other arguments. The profile grants
read access to this one config, denies writes and sibling config reads, and
allows only the exact Team loopback port. Preserve the inference route, TLS,
proxy environment and Keychain; never add a local API inference bridge.

Verify HTTP fragmentation/chunking, handshake/notifications, malformed requests,
slot spoofing, missing/invalid/shared credentials, lead permissions and real
task/message effects against isolated fixtures. Verify descriptor lifetime,
permissions, clean spawn assembly, and both positive/negative Seatbelt file and
network controls. Run affected checks and full push gates. Live Claude readiness
requires account-2 evidence through the existing danger entry; source or fixture
success alone is not that evidence. Host candidate activation/production changes
remain separately reviewed, with no service restarts in this source stage.


### Stage 3 contracts and verification boundary

The MCP config/strict arguments are grounded in first-party
`@anthropic-ai/claude-agent-sdk@0.3.319/sdk.mjs` (the argv serialization code).
The earlier account-2 `claude-pro2-danger --help` inspection covered streaming
flags; this stage has not executed a subscription CLI. Its `sdk.d.ts`
`McpHttpServerConfig` declares `type: 'http'`, `url`, and `headers`. The official
MCP schema at
`https://github.com/modelcontextprotocol/specification/blob/main/schema/2025-06-18/schema.ts`
is the reference for request IDs, initialization, notifications and tools.
The same official specification
`docs/specification/2025-06-18/basic/transports.mdx` defines POST/202/405 and
version-header behavior. HTTP supports 2025-06-18 and 2025-03-26; an unknown initialize version receives
2025-06-18, not the older TCP transport version. GET/DELETE are unsupported (405);
notifications return 202 without a body. JSON-only request bodies are bounded
at 1 MiB. Browser Origin and mismatched Host headers are rejected.

Focused tests use real local HTTP sockets for split headers/body, chunked
100 KiB input, handshake/version negotiation, string IDs, malformed/oversized
requests and endpoint shutdown. Per-slot HTTP credentials reject shared TCP
auth, cross-slot impersonation, absent/removed slots and duplicate headers.
Lead/worker tool lists and permission checks reuse the existing dispatcher.
Task creation/listing changes the fixture task board. A live TeamSessionService
with a held execution fixture verifies lead-to-worker dispatch and worker-to-lead
reply against persisted mailbox rows and sender/recipient IDs.

Subscription assembly tests inspect the private file at spawn time, verify
path-only argv and clean environment, reject old credentials and additional
MCP servers before spawning, and verify file cleanup after assembly failure. Before writing any bearer file,
the backend also rejects roots overlapping the canonical workspace; guard-side
refusal alone would be too late to protect a token written into a project.
A shared-spawner lifetime test keeps the file while a wake-recipe clone exists
and removes it when the final owner drops. These are fixture checks; no real
subscription process is executed. They do not prove live CLI MCP connectivity.

The sandbox's new exact IPv4 rule uses the macOS-supported
`(remote tcp4 "localhost:<port>")`: the literal `127.0.0.1` form failed profile
compilation and was corrected without widening to IPv6 or other ports. Both
access profiles verify the selected config read, sibling read/write refusal,
exact IPv4 connection, same-port IPv6 refusal and no MCP grant by default.
The legacy auth/metadata overlay keeps the entire MCP root closed.

Full source gates and personal-fork commit IDs are reported with delivery.
No production launcher, service, credential, route, environment or historical
database is changed. Production guard activation needs separate host review;
real initialized/tools/list and agent tool execution through the existing
account-2 danger entry remain unverified. In particular, do not assume how
the deployed official CLI handles loopback MCP with its existing proxy env,
and do not silently change NO_PROXY or add a bridge to make a fixture pass.


Stage 3 focused verification on 2026-11-09: `cargo test -p aionui-api-types
-p aionui-ai-agent -p aionui-team --features aionui-ai-agent/test-support
--locked` passed 2,396 tests with 18 existing ignored tests. All-target affected
Clippy with `--locked -- -D warnings`, format and whitespace checks passed.
The launch/MCP fixtures share one test to avoid competing for the same global
version-cache key; the cold-probe assertions remain intact. HTTP shutdown
verification waits through a racing TCP reset until connections are refused.
No real subscription process was launched during these checks.

## Host acceptance: fixed launcher argument compatibility

The first account-2 live Team fixture stopped before MCP initialization. The
actual session argument builder adds `--allow-dangerously-skip-permissions`
(`crates/aionui-session/src/backend/claude_conn.rs`, `build_claude_init_args`).
Read-only validation against the installed account-2 policy rejected that argv
shape with `ARGUMENT_NOT_ALLOWED`; removing only this flag passed validation.
The existing `/Users/rongyi/.local/bin/claude-pro2-danger` already supplies
`--dangerously-skip-permissions`. This is an argument compatibility failure;
it is not evidence of a subscription quota or authentication failure.

Session assembly now explicitly records that the selected fixed subscription
launcher manages bypass. The Claude builder then omits only the redundant
runtime unlock flag, leaving the requested permission mode and all other
arguments intact. Ordinary CLI launches keep their previous flag. The config
is retained by the wake recipe, and fresh/resume/fork assembly fixtures assert
the omission for both fixed accounts without executing either CLI. No guard,
entry script, environment, routing or credentials change for this correction.

Existing launch lifecycle and redacted failure logs remain sufficient; no
prompt, argument values, error payload or credential logging is added. Verify
the affected session/AI-agent tests and Clippy, the full pre-push gate, then
repeat the account-2 live Team fixture. Live and UI deployment results remain
separate from source verification.

Focused correction checks on 2026-11-09: the affected `aionui-session` and
`aionui-ai-agent` tests (with `test-support`, locked/offline dependencies) passed
1,679 tests with 15 existing ignored tests. All-target Clippy with `-D warnings`,
format and whitespace checks passed. No account-1 CLI was executed. The full
pre-push gate and repeated live Team result are recorded separately.


## Native HTTP tool-schema correction after host acceptance

The account-2 host guard's exact-port exclusion passed a zero-model official CLI
control gate: selected-port initialize/tools/list succeeded; a redirect to a
different loopback port retained broker HTTP 403 with zero target requests
(verified: task capture `native-mcp-control-exact-port.log`, 2026-11-09). Formal
installed-entry checks passed 21/21, including original residential TLS and a
real subscribed call. The subsequent Team run initialized/listed native MCP but
completed its lead turn with tool searches only, without automatic dispatch;
that run is not passing Team acceptance.

Source trace located a separate HTTP wire mismatch: the shared TCP dispatcher
returns internal `TeamToolDescriptor` fields, including `input_schema`, while
the official MCP 2025-06-18 `Tool` schema requires `inputSchema` (verified:
`https://github.com/modelcontextprotocol/specification/blob/main/schema/2025-06-18/schema.ts`,
Tool interface; `src/mcp/server.rs`, `handle_tools_list`; API descriptor in
`aionui-api-types/src/team_tools.rs`). The existing TCP adapter contract must
remain unchanged. Project descriptors at the native HTTP boundary using the
existing MCP JSON projection, exposing only name, description and inputSchema.
Keep caller-role filtering, bearer checks and execution authorization intact.

The new real-socket regression failed on the original seven-field internal
wire object, then passed after the projection. It checks every lead/worker tool,
required message arguments and lead-only visibility. All 773 Team tests passed
with six existing ignored cases. Existing native initialization/list diagnostics
are sufficient for success; add a payload-free error only for an impossible
internal descriptor contract failure. No proxy/guard change or service restart
is needed for this correction. Full gate/push and renewed real Team results
are recorded separately when complete.
