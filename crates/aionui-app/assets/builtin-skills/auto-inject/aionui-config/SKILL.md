---
name: aionui-config
description: Configure AionUi itself with the bundled aioncore CLI. Use when asked to set up or change assistants, assistant rules, skills, MCP servers, model providers, agents, scheduled tasks, resident roles, or settings, or when the user describes someone they want working for them.
---

# AionUi Config

Configure AionUi with the bundled agent-facing CLI. Do not discover ports, do
not call raw backend paths, and do not depend on tools outside the bundled
`aioncore` binary.

## Rules

1. Use only `"$AIONUI_HELPER_BIN" config ...`.
2. Never pass, inline, export, echo, or set any `AIONUI_...` environment variable.
3. Put all command input in stdin JSON.
4. Do not use flags for business fields.
5. Use `"$AIONUI_HELPER_BIN" config capabilities` when unsure which config command or stdin fields are supported.
6. Read context before changing the current assistant.
7. Read before writing, then read back after writing.
8. Use `"assistant_id": "current"` when the user asks to change the assistant used by this conversation.
9. Use `"conversation_id": "current"` when a command accepts a conversation selector.
10. Do not show internal ids unless the user needs them for a follow-up operation.
11. Never reveal provider keys, MCP headers, environment values, or other secrets.
12. If the CLI fails, report the stable `CONFIG_...` error from stderr in normal prose and do not claim the change was made.
13. After assistant changes, explain both persistence and effect timing. Saving and read-back do not mean the current running conversation has reloaded the changed runtime behavior.

## Output

Successful commands print a JSON envelope:

```json
{
  "success": true,
  "data": {},
  "meta": {
    "schema_version": 1
  }
}
```

Failures print one stable error line to stderr. Treat stderr as authoritative.

## Capability Discovery

Ask aioncore what this version supports:

```bash
"$AIONUI_HELPER_BIN" config capabilities
```

The result is a JSON envelope whose `data.domains[].commands[]` entries list
supported command paths, input mode, expected stdin fields, selector fields,
read-back behavior, destructive behavior, context requirements, and fields
redacted from ordinary output.

## Context

Read the current user, conversation, assistant, and local runtime context:

```bash
"$AIONUI_HELPER_BIN" config context
```

If `data.assistant` is `null`, the current conversation is not backed by an
assistant. Ask the user which assistant to edit before changing assistant
rules or defaults.

## Assistant Change Timing

AionUi persists assistant configuration immediately, but running conversations
may keep the assistant snapshot created when the conversation started. Use this
timing model when reporting successful assistant changes:

- Identity fields such as name, description, avatar, and recommended prompts are
  saved immediately. If the open UI still shows old values, tell the user to
  refresh or reopen the assistant view.
- Runtime fields such as agent, default model, default permission, default
  skills, default MCPs, thought level, and rules apply to new conversations
  created from that assistant. Do not claim they change the current running
  conversation.
- Skills and MCP defaults are not retroactively injected into the current agent
  runtime. If a tool is already available in the current conversation, it can be
  used; otherwise the user should start a new conversation with the assistant.

When reporting a successful runtime-field change, say that the change was saved
and read back, then state that it will affect new conversations only.

## Assistants

List assistants:

```bash
"$AIONUI_HELPER_BIN" config assistants list
```

Inspect the current assistant:

```bash
"$AIONUI_HELPER_BIN" config assistants get <<'JSON'
{
  "assistant_id": "current",
  "locale": "en-US"
}
JSON
```

Examples use English sample text and `en-US`. For real localized assistant
content, use the user's actual locale.

Create an assistant:

```bash
"$AIONUI_HELPER_BIN" config assistants create <<'JSON'
{
  "name": "Requirements Analyst",
  "description": "Turn rough product ideas into clear PRDs",
  "agent_id": "2d23ff1c",
  "prompts": [
    "Turn this feature idea into a PRD",
    "Review this PRD and identify confusing parts for new users"
  ],
  "enabled_skills": ["aionui-config"]
}
JSON
```

Update assistant metadata or defaults:

```bash
"$AIONUI_HELPER_BIN" config assistants update <<'JSON'
{
  "assistant_id": "current",
  "locale": "en-US",
  "description": "Updated assistant description",
  "defaults": {
    "permission": {
      "mode": "fixed",
      "value": "plan"
    }
  }
}
JSON
```

For `name`, `description`, `avatar`, or recommended prompt changes, report that
the change is saved and may require refreshing or reopening the UI to see. For
`agent_id`, `defaults`, `enabled_skills`, or other runtime defaults (MCP
defaults are set via `defaults.mcps`, not `default_mcp_ids`), report that the
saved change applies to new conversations only.

Enable, disable, or reorder an assistant:

```bash
"$AIONUI_HELPER_BIN" config assistants state <<'JSON'
{
  "assistant_id": "current",
  "enabled": true,
  "sort_order": 10
}
JSON
```

## Assistant Rules

Assistant rules are the system prompt that defines assistant behavior.

Read the current assistant rule:

```bash
"$AIONUI_HELPER_BIN" config assistants rule read <<'JSON'
{
  "assistant_id": "current",
  "locale": "en-US"
}
JSON
```

Write the current assistant rule:

```bash
"$AIONUI_HELPER_BIN" config assistants rule write <<'JSON'
{
  "assistant_id": "current",
  "locale": "en-US",
  "content": "# Role\nYou are..."
}
JSON
```

For rule edits, preserve the user's existing useful instructions unless the
user explicitly asks to replace them.

After a successful rule write or delete, always tell the user that the rule was
saved and read back, but it applies only to new conversations created from this
assistant. The current conversation continues using the rule snapshot it started
with.

## Skills

List available skills:

```bash
"$AIONUI_HELPER_BIN" config skills list
```

Inspect a skill directory before importing:

```bash
"$AIONUI_HELPER_BIN" config skills info <<'JSON'
{
  "skill_path": "/absolute/path/to/skill"
}
JSON
```

Import a skill:

```bash
"$AIONUI_HELPER_BIN" config skills import <<'JSON'
{
  "skill_path": "/absolute/path/to/skill-or-parent-or-zip"
}
JSON
```

Attach skills to an assistant by updating the assistant's full skill list:

```bash
"$AIONUI_HELPER_BIN" config assistants update <<'JSON'
{
  "assistant_id": "current",
  "enabled_skills": ["aionui-config", "cron"]
}
JSON
```

Do not append blindly. Read the assistant first, merge the list locally, then
send the full intended `enabled_skills` value.

Enabled skills are assistant defaults for new conversations. Do not tell the
user that newly attached skills are available in the current conversation unless
the current runtime already exposes them.

Manage external skill paths:

```bash
"$AIONUI_HELPER_BIN" config skills external-paths list
```

```bash
"$AIONUI_HELPER_BIN" config skills external-paths add <<'JSON'
{
  "name": "Team Skills",
  "path": "/absolute/path/to/team-skills"
}
JSON
```

```bash
"$AIONUI_HELPER_BIN" config skills external-paths remove <<'JSON'
{
  "path": "/absolute/path/to/team-skills"
}
JSON
```

Enable or disable the skills market:

```bash
"$AIONUI_HELPER_BIN" config skills market enable
```

```bash
"$AIONUI_HELPER_BIN" config skills market disable
```

## MCP Servers

List MCP servers:

```bash
"$AIONUI_HELPER_BIN" config mcp servers list
```

Create an MCP server:

```bash
"$AIONUI_HELPER_BIN" config mcp servers create <<'JSON'
{
  "name": "Local Tools",
  "transport": {
    "type": "stdio",
    "command": "my-mcp-server",
    "args": [],
    "env": {}
  }
}
JSON
```

Update an MCP server:

```bash
"$AIONUI_HELPER_BIN" config mcp servers update <<'JSON'
{
  "server_id": "mcp_123",
  "description": "Updated description"
}
JSON
```

Test a server configuration:

```bash
"$AIONUI_HELPER_BIN" config mcp test-connection <<'JSON'
{
  "name": "Local Tools",
  "transport": {
    "type": "stdio",
    "command": "my-mcp-server",
    "args": []
  }
}
JSON
```

OAuth helpers:

```bash
"$AIONUI_HELPER_BIN" config mcp oauth check-status <<'JSON'
{
  "server_url": "https://mcp.example.com"
}
JSON
```

Never show MCP headers or stdio env values to the user. CLI output redacts
sensitive fields by default.

## Providers

List model providers:

```bash
"$AIONUI_HELPER_BIN" config providers list
```

Create a provider:

```bash
"$AIONUI_HELPER_BIN" config providers create <<'JSON'
{
  "name": "OpenAI",
  "platform": "openai",
  "base_url": "https://api.openai.com/v1",
  "api_key": "sk-..."
}
JSON
```

Update a provider:

```bash
"$AIONUI_HELPER_BIN" config providers update <<'JSON'
{
  "provider_id": "provider_123",
  "api_key": "sk-..."
}
JSON
```

Detect protocol, fetch models, or run a provider health check:

```bash
"$AIONUI_HELPER_BIN" config providers detect-protocol <<'JSON'
{
  "base_url": "https://api.example.com/v1",
  "api_key": "..."
}
JSON
```

```bash
"$AIONUI_HELPER_BIN" config providers models fetch <<'JSON'
{
  "provider_id": "provider_123"
}
JSON
```

```bash
"$AIONUI_HELPER_BIN" config providers health-check <<'JSON'
{
  "provider_id": "provider_123",
  "model": "gpt-4.1"
}
JSON
```

Never reveal provider keys. Do not repeat secret values from the user's input.

## Settings

Read backend settings:

```bash
"$AIONUI_HELPER_BIN" config settings get
```

Patch backend settings:

```bash
"$AIONUI_HELPER_BIN" config settings patch <<'JSON'
{
  "language": "en-US",
  "notification_enabled": true
}
JSON
```

Supported patch fields: `language`, `notification_enabled`, `cron_notification_enabled`,
`command_queue_enabled`, `save_upload_to_workspace`. Unknown fields are silently ignored.

Read or update client preferences:

```bash
"$AIONUI_HELPER_BIN" config settings client get
```

```bash
"$AIONUI_HELPER_BIN" config settings client put <<'JSON'
{
  "ui.zoomFactor": 1.2
}
JSON
```

Client preferences are a free-form key-value map. Pass `null` to remove a key. Ask the
user or read back first to discover keys in use — there is no fixed schema.

## Agents

List available agents:

```bash
"$AIONUI_HELPER_BIN" config agents list
```

Enable or disable an agent:

```bash
"$AIONUI_HELPER_BIN" config agents enable <<'JSON'
{
  "agent_id": "codex",
  "enabled": true
}
JSON
```

Read or set per-agent overrides:

```bash
"$AIONUI_HELPER_BIN" config agents overrides get <<'JSON'
{
  "agent_id": "codex"
}
JSON
```

```bash
"$AIONUI_HELPER_BIN" config agents overrides set <<'JSON'
{
  "agent_id": "codex",
  "command_override": "/absolute/path/to/codex"
}
JSON
```

Create, update, delete, or try-connect a custom agent:

```bash
"$AIONUI_HELPER_BIN" config agents custom create <<'JSON'
{
  "name": "Custom Agent",
  "command": "/absolute/path/to/agent-cli"
}
JSON
```

```bash
"$AIONUI_HELPER_BIN" config agents custom update <<'JSON'
{
  "agent_id": "custom_agent_123",
  "name": "Custom Agent",
  "command": "/absolute/path/to/agent-cli"
}
JSON
```

Test whether a custom agent binary is reachable (does not persist anything):

```bash
"$AIONUI_HELPER_BIN" config agents custom try-connect <<'JSON'
{
  "command": "/absolute/path/to/agent-cli"
}
JSON
```

Do not reveal agent env values or secret override values.

## Scheduled Tasks

For tasks tied to the current conversation, use the cron current commands.

List current conversation tasks:

```bash
"$AIONUI_HELPER_BIN" config cron current list
```

Create a task:

```bash
"$AIONUI_HELPER_BIN" config cron current create <<'JSON'
{
  "name": "Daily Summary",
  "schedule": "0 18 * * MON-FRI",
  "schedule_description": "Weekdays at 6:00 PM",
  "message": "Review the conversation context and produce a concise end-of-day summary."
}
JSON
```

Update a task:

```bash
"$AIONUI_HELPER_BIN" config cron current update <<'JSON'
{
  "job_id": "cron_123",
  "name": "Daily Summary",
  "schedule": "0 18 * * MON-FRI",
  "schedule_description": "Weekdays at 6:00 PM",
  "message": "Review the conversation context and produce a concise end-of-day summary."
}
JSON
```

After a successful create or update, explain the task name and schedule in
normal user-facing language. Do not show `cron_...` ids unless needed.

For global cron job administration, use `config cron jobs`.

List all cron jobs:

```bash
"$AIONUI_HELPER_BIN" config cron jobs list
```

Create a cron job:

```bash
"$AIONUI_HELPER_BIN" config cron jobs create <<'JSON'
{
  "name": "Weekly Report",
  "schedule": { "kind": "cron", "expr": "0 9 * * MON", "tz": "Asia/Shanghai" },
  "message": "Produce the weekly report.",
  "conversation_id": "current",
  "created_by": "user"
}
JSON
```

The `schedule` field is a tagged object, not a flat string:
- `{ "kind": "cron", "expr": "<cron-expr>", "tz": "<IANA-tz>" }` — recurring cron schedule
- `{ "kind": "every", "every_ms": <milliseconds> }` — fixed interval
- `{ "kind": "at", "at_ms": <epoch-ms> }` — one-shot at a specific time

`conversation_id` and `created_by` are required. `message` carries the task text.
Use `"conversation_id": "current"` to attach the job to the current conversation.

Update, run, or manage a cron job skill:

Note: `cron jobs` uses the tagged `schedule` object (same shape as create). This
is different from `cron current`, where `schedule` is a flat cron string.

```bash
"$AIONUI_HELPER_BIN" config cron jobs update <<'JSON'
{
  "job_id": "cron_123",
  "name": "Weekly Report",
  "schedule": { "kind": "cron", "expr": "0 10 * * MON" }
}
JSON
```

```bash
"$AIONUI_HELPER_BIN" config cron jobs run <<'JSON'
{
  "job_id": "cron_123"
}
JSON
```

```bash
"$AIONUI_HELPER_BIN" config cron jobs skill save <<'JSON'
{
  "job_id": "cron_123",
  "content": "# Skill\nTask-specific instructions."
}
JSON
```

## Resident Roles

A resident role is an assistant that works for the user on its own: it runs on
a schedule, keeps its own memory, reports what it found, and can join teams.
Who it is lives in the role's **charter**: what it owns, what it delivers and
where, what it never does, when it stays silent. The assistant underneath is
only a shell: name, description, avatar and agent.

### Hiring a role

When the user describes someone they want working for them ("I need someone
who checks X every morning and tells me what changed"), hire a role. Do not
send the user to forms.

1. Work out the role, asking only what you cannot infer:
   - what it owns and what it must never do;
   - which agent runs it (`config agents list`);
   - when it works and what each run should do;
   - whether a scheduled run asks the user before acting
     (`asks_when_scheduled`): on when runs change things beyond the role's own
     records (files, messages, pushes), off for roles that only read and report;
   - whether it reviews other members' work in teams (`verifies`);
   - team tools it must not call (`denied_tools`).
2. Show the user **one hiring sheet** in plain language: name, agent, charter,
   each scheduled run (when, and its instruction), the switches, and the
   starting memory. Create nothing until the user agrees. When they change
   something, revise the sheet and show it again.
3. Create the assistant shell with `config assistants create`: `name`, a short
   `description`, `agent_id`. Write no assistant rule; the charter goes on the
   role.
4. Make it a role with `config roles create`, `role_id` being the new
   assistant's id.
5. Schedule its runs with `config cron jobs create`. Each run then starts a
   fresh conversation as the role, carrying its charter and memory.
6. Read back with `config roles get` and `config cron jobs list`, then tell the
   user in plain words what now exists and when it first runs.

Make it a role:

```bash
"$AIONUI_HELPER_BIN" config roles create <<'JSON'
{
  "role_id": "<assistant id>",
  "charter": "You watch the nightly build of the shop app. Each morning you read last night's results and report failures with the failing step and the most likely cause. You never change code or settings. When everything passed, you stay silent.",
  "denied_tools": [],
  "verifies": false,
  "asks_when_scheduled": false,
  "memory": "- The build log lives in /absolute/path/to/logs"
}
JSON
```

Schedule one of its runs:

```bash
"$AIONUI_HELPER_BIN" config cron jobs create <<'JSON'
{
  "name": "Morning build check",
  "schedule": { "kind": "cron", "expr": "0 8 * * *", "tz": "Asia/Shanghai" },
  "message": "Check last night's build and report as your charter says.",
  "conversation_id": "",
  "created_by": "agent",
  "execution_mode": "new_conversation",
  "agent_config": { "name": "<assistant name>", "assistant_id": "<assistant id>" }
}
JSON
```

Writing the charter:

- Address the role as "you". Be concrete: what it owns, what it delivers and
  where, what it never does, and when it has nothing to say.
- Keep it under 4000 characters.
- Seed `memory` only with facts the user gave (paths, accounts, preferences),
  never with instructions; those belong in the charter. The role adds its own
  experience to its memory as it works.

`denied_tools` takes team tool names: `team_members`, `team_read_messages`,
`team_send_message`, `team_interrupt_agent`, `team_task_create`,
`team_task_update`, `team_task_list`, `team_remember`, `team_progress`,
`team_list_assistants`, `team_describe_assistant`, `team_spawn_agent`,
`team_rename_agent`, `team_clear_agent_context`, `team_shutdown_agent`.

### Looking after roles

List roles in brief, or read one with its memory, progress and reports:

```bash
"$AIONUI_HELPER_BIN" config roles list
```

```bash
"$AIONUI_HELPER_BIN" config roles get <<'JSON'
{ "role_id": "<assistant id>" }
JSON
```

Change a role: `update` changes only the fields you name and keeps the rest.

```bash
"$AIONUI_HELPER_BIN" config roles update <<'JSON'
{ "role_id": "<assistant id>", "asks_when_scheduled": true }
JSON
```

A role's memory is its own. `update` refuses `memory`, and `create` seeds it
only for a new role: a role dismissed before keeps what it had learned.

Dismiss a role with `config roles delete`. Its assistant and memory stay. Its
scheduled runs stay too: delete them with `config cron jobs delete` when the
user wants the role gone.

## Safety

Configuration changes affect the user's live app. Keep changes narrow, show
what changed in plain language, and avoid exposing raw JSON unless the user
asks for implementation detail.
