//! Factory branch for Antigravity (`agy` CLI) sessions.
//!
//! Antigravity is a direct-CLI backend that does NOT speak ACP, so it does not
//! go through `factory::acp` (nor through `session_agent::build_session_instance`,
//! which carries claude/codex-private assembly). It assembles here and hands the
//! opened backend to the generic `SessionAgentTask`.

use std::sync::Arc;

use crate::agent_task::AgentInstance;
use crate::error::AgentError;
use crate::factory::AgentFactoryDeps;
use crate::factory::context::FactoryContext;
use crate::session_context::AntigravitySessionBuildContext;

/// The mode id that means "run without asking" for this agent.
///
/// A SENTINEL, not one of agy's modes: agy has no full-auto mode (all three of
/// its modes behave identically on permissions — only
/// `--dangerously-skip-permissions` decides). Callers that mean "unattended"
/// — a teammate via `session_mode_for_backend`, a scheduled run via
/// `fallback_full_auto_mode` — arrive here carrying this value, the same way
/// every other agent expresses full auto. A user can also pick it deliberately
/// from the mode list.
///
/// Deriving this from where the session came from instead (team? cron?) would
/// need a new branch for every future caller; this needs none.
fn yolo_mode_id(catalog_yolo_id: Option<&str>) -> &str {
    catalog_yolo_id
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .unwrap_or("yolo")
}

/// Whether this session runs without a human to answer permission prompts.
pub(crate) fn is_full_auto(session_mode: Option<&str>, yolo_id: &str) -> bool {
    session_mode.is_some_and(|m| m.eq_ignore_ascii_case(yolo_id))
}

/// Install the approval hook in `workspace` and return the environment that lets
/// this conversation's agy reach the callback.
///
/// Done for EVERY mode, full auto included. `hooks.json` belongs to the workspace
/// while the callback coordinates belong to the conversation, and several
/// conversations can share one workspace. Treating the file as this
/// conversation's own — install it for approval modes, delete it for full auto,
/// and give a full-auto session no coordinates — let neighbours break each other:
///
/// - an approval-mode conversation installed the hook, and a full-auto one in the
///   same directory then ran it with no callback address, which fails closed:
///   every tool denied with "aionui hook is not configured";
/// - a full-auto conversation deleted the hook, and the approval-mode one beside
///   it stopped being asked about anything and ran unattended.
///
/// Full auto is decided where the mode is known per conversation instead: the
/// session backend approves a full-auto session's callbacks on the spot.
fn register_hook(
    workspace: &std::path::Path,
    hook_binary: &std::path::Path,
    base_url: &str,
    tokens: &crate::antigravity_hook::HookTokenRegistry,
    conversation_id: &str,
    full_auto: bool,
) -> Result<Vec<(String, String)>, AgentError> {
    if let Err(e) = crate::antigravity_hook::write_hooks_json(workspace, hook_binary) {
        if !full_auto {
            // Without the hook this session would run with agy's gate wide open
            // and no per-call approval, so the failure must be loud.
            return Err(AgentError::internal(format!(
                "antigravity: could not register the permission hook in {}: {e}",
                workspace.display()
            )));
        }
        // Full auto loses nothing: with no hook its tools run unasked, which is
        // what the mode means, and a hook already there can still call back.
        tracing::warn!(
            conversation_id = %conversation_id,
            error = %e,
            "antigravity: full auto — could not write the approval hook; continuing without it"
        );
    }
    if full_auto {
        tracing::info!(
            conversation_id = %conversation_id,
            "antigravity: full-auto mode — tool calls are approved without a prompt"
        );
    }
    // Minting here also invalidates any token left over from a prior build of
    // this conversation, so a stale hook cannot answer for the new session.
    let token = tokens.issue(conversation_id);
    Ok(vec![
        (
            aionui_api_types::AntigravityHookConfig::ENV_BASE_URL.to_owned(),
            base_url.to_owned(),
        ),
        (aionui_api_types::AntigravityHookConfig::ENV_TOKEN.to_owned(), token),
        (
            aionui_api_types::AntigravityHookConfig::ENV_CONVERSATION_ID.to_owned(),
            conversation_id.to_owned(),
        ),
    ])
}

pub(super) async fn build(
    deps: Arc<AgentFactoryDeps>,
    build_context: AntigravitySessionBuildContext,
    ctx: FactoryContext,
) -> Result<AgentInstance, AgentError> {
    let mut config = build_context.config;

    // Trust the catalog row over a client-supplied backend label, mirroring the
    // ACP factory: the frontend collapses row-scoped rows to a shared slot
    // string that downstream consumers would misread.
    let meta = crate::factory::acp::resolve_catalog_metadata(&deps.agent_registry, &config, &ctx.user_id).await?;
    if config.agent_id.is_some() || config.backend.is_none() {
        config.backend.clone_from(&meta.backend);
    }

    // Register AionUi as agy's PreToolUse gate for THIS workspace, and mint the
    // token that authenticates the hook's callback. Without this the session
    // still runs, but with agy's gate wide open and no per-call approval — so a
    // failure here must be loud.
    // Resolve the mode the same way the session itself will (snapshot over
    // create-time seed, aliases normalized). Reading `config.session_mode`
    // directly saw only the value the conversation was created with, so a
    // runtime switch to full auto — and the mode a scheduled run resolves —
    // never reached this decision.
    let resolved_mode =
        crate::session_agent::resolved_session_mode(&config, build_context.session_snapshot.as_ref(), &meta);
    let full_auto = is_full_auto(resolved_mode.as_deref(), yolo_mode_id(meta.yolo_id.as_deref()));
    let hook_env = match deps.antigravity_hook_base_url.as_deref() {
        Some(base_url) => register_hook(
            std::path::Path::new(&ctx.workspace),
            deps.backend_binary_path.as_path(),
            base_url,
            &deps.antigravity_hook_tokens,
            &ctx.conversation_id,
            full_auto,
        )?,
        None => {
            if full_auto {
                // This host can answer no hook at all, so one left behind in the
                // workspace would deny every tool of a session that asked for none.
                crate::antigravity_hook::remove_hooks_json(std::path::Path::new(&ctx.workspace));
                deps.antigravity_hook_tokens.revoke(&ctx.conversation_id);
            } else {
                tracing::warn!(
                    conversation_id = %ctx.conversation_id,
                    "antigravity: no hook callback address configured — tools will run WITHOUT per-call approval"
                );
            }
            Vec::new()
        }
    };

    // Handed to the backend in every mode: it puts the file back before each
    // turn, since anything sharing the workspace can delete it in between.
    let permission_hook_body = deps
        .antigravity_hook_base_url
        .as_deref()
        .map(|_| crate::antigravity_hook::hooks_json_body(deps.backend_binary_path.as_path()));

    let mut runtime_env = ctx.runtime_env.clone();
    runtime_env.extend(hook_env);

    let mut delivery = crate::factory::resolve_skill_delivery(
        deps.as_ref(),
        &ctx.user_id,
        &ctx.conversation_id,
        &config.skills,
        &meta,
    )
    .await;
    // agy has no prompt pipeline, so compose the rules block here and let the
    // backend prepend it to the first `-p` invocation.
    delivery.injected_prefix = crate::factory::compose_injected_prefix_for(
        deps.as_ref(),
        &ctx.user_id,
        config.preset_context.as_deref(),
        &config.skills,
        &delivery.plan.mode,
    )
    .await;

    let instance = crate::session_agent::build_antigravity_instance(
        crate::session_agent::SessionBuildInputs {
            conversation_id: ctx.conversation_id.clone(),
            user_id: ctx.user_id.clone(),
            workspace: ctx.workspace.clone(),
            config: &config,
            metadata: &meta,
            skill_delivery: delivery,
            session_snapshot: build_context.session_snapshot.as_ref(),
            backend_session_id: build_context.session_id.clone(),
            mcp_server_repo: deps.mcp_server_repo.as_ref(),
            runtime_env: &runtime_env,
            broadcaster: deps.broadcaster.clone(),
            // Keyed by the resolved catalog row so discovered models/modes
            // refresh the `/api/agents` picker.
            catalog_writeback: Some((meta.id.clone(), deps.agent_registry.catalog_sender())),
            // Persists the resume anchor + observed mode/model from the pump.
            acp_session_repo: Some(deps.acp_agent_service.repo()),
            prompt_dump_dir: None,
            permission_hook_body,
        },
        deps.session_spawner.clone(),
    )
    .await?;

    tracing::info!(
        conversation_id = %ctx.conversation_id,
        "antigravity: routing conversation through the direct-CLI SessionAgentTask"
    );
    Ok(instance)
}

#[cfg(test)]
mod tests {
    use super::{is_full_auto, register_hook, yolo_mode_id};
    use crate::antigravity_hook::HookTokenRegistry;
    use aionui_api_types::AntigravityHookConfig;
    use std::path::Path;

    const BASE_URL: &str = "http://127.0.0.1:54810";

    fn value<'a>(env: &'a [(String, String)], name: &str) -> Option<&'a str> {
        env.iter().find(|(key, _)| key == name).map(|(_, v)| v.as_str())
    }

    #[test]
    fn a_full_auto_session_can_reach_the_callback_too() {
        // The hook in a workspace runs for every agy started there. A full-auto
        // session spawned without these three could only be denied by it.
        let dir = tempfile::tempdir().unwrap();
        let tokens = HookTokenRegistry::new();

        let env = register_hook(dir.path(), Path::new("/opt/aioncore"), BASE_URL, &tokens, "auto", true).unwrap();

        assert_eq!(value(&env, AntigravityHookConfig::ENV_BASE_URL), Some(BASE_URL));
        assert_eq!(value(&env, AntigravityHookConfig::ENV_CONVERSATION_ID), Some("auto"));
        let token = value(&env, AntigravityHookConfig::ENV_TOKEN).expect("a token is issued");
        assert!(tokens.verify("auto", token));
    }

    #[test]
    fn conversations_sharing_a_workspace_do_not_take_the_hook_from_each_other() {
        // Whichever is built last, the approval-mode one keeps its gate and both
        // can authenticate their callbacks.
        for auto_first in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let tokens = HookTokenRegistry::new();
            let build = |conversation: &str, full_auto: bool| {
                register_hook(
                    dir.path(),
                    Path::new("/opt/aioncore"),
                    BASE_URL,
                    &tokens,
                    conversation,
                    full_auto,
                )
                .unwrap()
            };

            let (auto, asking) = if auto_first {
                let auto = build("auto", true);
                (auto, build("asking", false))
            } else {
                let asking = build("asking", false);
                (build("auto", true), asking)
            };

            assert!(
                dir.path().join(".agents/hooks.json").exists(),
                "auto_first={auto_first}"
            );
            assert!(tokens.verify("auto", value(&auto, AntigravityHookConfig::ENV_TOKEN).unwrap()));
            assert!(tokens.verify("asking", value(&asking, AntigravityHookConfig::ENV_TOKEN).unwrap()));
        }
    }

    #[test]
    fn only_an_approval_session_fails_when_the_hook_cannot_be_written() {
        // A plain file where the workspace directory should be: nothing can be
        // created under it.
        let dir = tempfile::tempdir().unwrap();
        let blocked = dir.path().join("not-a-directory");
        std::fs::write(&blocked, "").unwrap();
        let tokens = HookTokenRegistry::new();

        // Unattended anyway, so it still starts — and still gets its coordinates,
        // for a hook some other conversation may have installed.
        let env = register_hook(&blocked, Path::new("/opt/aioncore"), BASE_URL, &tokens, "auto", true).unwrap();
        assert!(value(&env, AntigravityHookConfig::ENV_TOKEN).is_some());

        // Asked to gate every tool and unable to: starting would be the lie.
        assert!(register_hook(&blocked, Path::new("/opt/aioncore"), BASE_URL, &tokens, "asking", false).is_err());
    }

    #[test]
    fn the_catalog_sentinel_turns_off_the_approval_hook() {
        // How a teammate and a scheduled run both arrive: their full-auto mode id.
        assert!(is_full_auto(Some("yolo"), yolo_mode_id(Some("yolo"))));
    }

    #[test]
    fn agys_real_modes_keep_their_approval_prompts() {
        // Measured on 1.1.9: none of these is full auto — `accept-edits` only
        // auto-approves file edits, `plan` is research-only, and all three are
        // refused the "command" permission without the flag.
        for mode in ["default", "accept-edits", "plan"] {
            assert!(!is_full_auto(Some(mode), yolo_mode_id(Some("yolo"))), "mode={mode}");
        }
        assert!(!is_full_auto(None, yolo_mode_id(Some("yolo"))));
    }

    #[test]
    fn a_catalog_without_a_yolo_id_still_recognises_the_sentinel() {
        // An older row (or a hand-edited one) must not silently lose full auto.
        assert!(is_full_auto(Some("yolo"), yolo_mode_id(None)));
        assert!(!is_full_auto(Some("default"), yolo_mode_id(None)));
    }

    #[test]
    fn a_catalog_may_rename_the_sentinel() {
        // The id is data, not a constant: honour whatever the row declares.
        assert!(is_full_auto(Some("full-access"), yolo_mode_id(Some("full-access"))));
        assert!(!is_full_auto(Some("yolo"), yolo_mode_id(Some("full-access"))));
    }
}
