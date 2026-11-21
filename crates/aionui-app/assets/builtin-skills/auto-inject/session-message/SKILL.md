---
name: session-message
description: Cross-session messaging - deliver a message to another one of this user's conversations, and reply to one you received.
---

# Cross-Session Message Skill

Deliver a message to another one of this user's conversations with the bundled
agent-facing CLI. Delivering is exactly like the user opening that conversation
and pressing send: the recipient starts a turn and decides for itself whether
to reply.

## Rules

1. Use this when the user selected a target conversation with `@@`, selected a
   resident role with `@@@`, or explicitly asked you to communicate with another
   conversation or resident role (or when you run as a resident role and need a
   colleague's domain or are replying to an incoming `reply_to`).
2. `to` must be a conversation id (`id` from `[[AION_SESSIONS]]`,
   `thread_conversation_id` from `[[AION_ROLES]]`, or `reply_to` from
   `[[AION_SESSION_MESSAGE]]`). Names are not addresses, and there is no
   broadcast — one `send-message` call reaches exactly one conversation.
3. Never pass, inline, export, echo, or set any `AIONUI_...` environment variable.
4. Where your MCP tools include `session_send_message` / `session_list` (or
   `role_send` / `role_peers` for a resident role), prefer calling that MCP tool
   directly — it reaches the backend even when your shell runs in an offline
   sandbox. Otherwise call `"$AIONUI_HELPER_BIN" session ...` directly and pass
   payloads through stdin heredocs. Do not write payload JSON files to disk.
5. If the current conversation belongs to a team, do NOT use this skill. Use
   `team send-message` instead.
6. On `rate_limited` or `hop_limit_reached`, STOP delivering and tell the user.
   Do not retry. Never send courtesy acknowledgements ("thanks", "got it") that
   spend another turn without new substance.
7. Word results precisely. `queued` means "delivered; the other side is busy and
   will see it when it frees up" — it does NOT mean "they received it" or "they
   read it". Never claim a message was read.
8. If the CLI fails, report the failure from stderr/stdout in normal prose. Do
   not claim the message was delivered.

## Reading targets from the user's message

When the user typed `@@`, their message carries a block like:

```
[[AION_SESSIONS]]
To deliver to the conversations below, use the session-message skill (address by conversation id); if it is unavailable, run `"$AIONUI_HELPER_BIN" session capabilities` for the delivery contract.
重构-鉴权模块	conv_019f…	workspace: same
文档站改版	conv_01a0…	workspace: /Users/x/docs (differs from yours)
[[/AION_SESSIONS]]
```

The first line is a hint that this skill is how to deliver, with a fallback to
`session capabilities` when the skill is unavailable. Every following line is a
target: `name`, tab, `id`, tab, `workspace:`. Use the **id**.

When the user typed `@@@` to mention a resident role, their message carries:

```
[[AION_ROLES]]
To deliver to the resident roles below, use `role send` (`to_role` = role_id) when you run as a resident role, or use the session-message skill / `session_send_message` (`to` = thread_conversation_id); if unavailable, run `"$AIONUI_HELPER_BIN" session capabilities` for the delivery contract.
安全官	sec-officer	conv_01b2…	workspace: /Users/x/role-sec (differs from yours)
[[/AION_ROLES]]
```

Each target line has four tab-separated fields: `role_name`, `role_id`,
`thread_conversation_id`, and `workspace:`. When calling `session send-message`
(or `session_send_message`), pass **`thread_conversation_id`** as `to`. When
you run as a resident role calling `role send` (or `role_send`), pass
**`role_id`** as `to_role`.

## Delivering a message

```bash
"$AIONUI_HELPER_BIN" session send-message <<'JSON'
{
  "to": "conv_019f…",
  "message": "接口定完了吗？"
}
JSON
```

## Replying to a message you received

A delivered message arrives with this block at the top:

```
[[AION_SESSION_MESSAGE]]
from: 重构-鉴权模块	conv_019f…
workspace: same
reply_to: conv_019f…	(reply: session send-message, to=reply_to)
If the session-message skill is unavailable, run `"$AIONUI_HELPER_BIN" session capabilities` for the full delivery contract.
[[/AION_SESSION_MESSAGE]]
```

Reply by sending to `reply_to` with the same command:

```bash
"$AIONUI_HELPER_BIN" session send-message <<'JSON'
{
  "to": "conv_019f…",
  "message": "定完了，已经推到 main。"
}
JSON
```

Replying is optional. Decide for yourself whether a reply is useful — there is
no synchronous wait on the other side.

## Finding a target the user only described in prose

```bash
"$AIONUI_HELPER_BIN" session list
```

Optional stdin filters: `q` (name filter), `project_id`, `limit`, `cursor`.

## Cross-workspace rule

When `workspace` is not `same`, the other conversation runs in a different
directory:

- Do NOT use relative paths — they resolve against the recipient's workspace and
  will silently read a different file, or none.
- Do NOT assume the recipient can read your files. Cross-directory access may be
  blocked by its sandbox or permissions.
- To share file content, put the content itself into `message`.

## Getting more detail about a target conversation

For a target's workspace path, whether it is currently running a turn, or
stuck/waiting hints:

```bash
"$AIONUI_HELPER_BIN" diagnose conversations get <<'JSON'
{ "conversation_id": "conv_019f…" }
JSON
```

## Exact schemas

For enum values, error-code meanings, and full field tables:

```bash
"$AIONUI_HELPER_BIN" session capabilities
```
