//! A separate, non-persistent Claude print call on the session's chosen model.
//! Flags are documented by `claude --help` (verified locally 2026-11-11).

use std::sync::Arc;

use aionui_common::CommandSpec;
use aionui_process::Spawner;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::{AgentIo, BackendError, ManagedProcessIo, TITLE_INSTRUCTIONS, TitleGenerator};

pub(super) struct ClaudeTitleGenerator {
    pub spawner: Arc<dyn Spawner>,
    pub command: CommandSpec,
}

#[async_trait::async_trait]
impl TitleGenerator for ClaudeTitleGenerator {
    async fn generate(&self, description: &str) -> Result<String, BackendError> {
        let process = self
            .spawner
            .spawn_quiet(self.command.clone(), &[], "aionui-session-title")
            .await
            .map_err(|_| BackendError::Transport("Claude title process could not start".into()))?;
        let io = ManagedProcessIo::new(process);
        let result = tokio::time::timeout(std::time::Duration::from_secs(60), exchange(&io, description)).await;
        io.terminate().await;
        result.map_err(|_| BackendError::Transport("Claude title request timed out".into()))?
    }
}

pub(super) fn command(config: &super::SessionConfig, model: Option<String>) -> CommandSpec {
    let mut args = vec![
        "--print".into(),
        "--input-format".into(),
        "text".into(),
        "--output-format".into(),
        "stream-json".into(),
        "--verbose".into(),
        "--no-session-persistence".into(),
        "--tools".into(),
        String::new(),
        "--strict-mcp-config".into(),
        "--system-prompt".into(),
        TITLE_INSTRUCTIONS.into(),
    ];
    if let Some(model) = model.filter(|m| !m.is_empty()) {
        args.extend(["--model".into(), model]);
    }
    CommandSpec {
        command: config.cli_program.clone().unwrap_or_else(|| "claude".into()),
        args,
        env: config.spawn_env.clone(),
        cwd: config.cwd.clone(),
    }
}

async fn exchange(io: &dyn AgentIo, description: &str) -> Result<String, BackendError> {
    let (mut stdin, stdout) = io
        .take_stdio()
        .await
        .ok_or_else(|| BackendError::Transport("Claude title stdio unavailable".into()))?;
    stdin
        .write_all(description.as_bytes())
        .await
        .map_err(|_| BackendError::Transport("Claude title write failed".into()))?;
    stdin
        .shutdown()
        .await
        .map_err(|_| BackendError::Transport("Claude title write failed".into()))?;
    drop(stdin);
    let mut lines = BufReader::new(stdout).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(frame) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if frame["type"] == "result" {
            if frame["is_error"] == true || frame["subtype"] != "success" {
                return Err(BackendError::Transport("Claude title request failed".into()));
            }
            return frame["result"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .map(str::to_owned)
                .ok_or_else(|| BackendError::Transport("Claude title was empty".into()));
        }
    }
    Err(BackendError::Transport(
        "Claude title stream closed without a result".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::FakeAgentIo;
    #[test]
    fn command_preserves_transport_and_selects_model_without_tools() {
        let config = super::super::SessionConfig {
            cwd: Some("/work".into()),
            cli_program: Some("/custom/claude".into()),
            spawn_env: vec![aionui_common::EnvVar {
                name: "ANTHROPIC_BASE_URL".into(),
                value: "https://provider.example".into(),
            }],
            ..Default::default()
        };
        let command = command(&config, Some("selected-model".into()));
        assert_eq!(command.command, std::path::PathBuf::from("/custom/claude"));
        assert_eq!(command.cwd, config.cwd);
        assert_eq!(command.env[0].value, config.spawn_env[0].value);
        assert!(command.args.windows(2).any(|a| a == ["--model", "selected-model"]));
        assert!(command.args.windows(2).any(|a| a == ["--tools", ""]));
        assert!(command.args.contains(&"--no-session-persistence".into()));
        assert!(command.args.contains(&"--strict-mcp-config".into()));
    }
    #[tokio::test]
    async fn accepts_only_complete_successful_result_and_passes_context_on_stdin() {
        let io = FakeAgentIo::never_exits(
            b"{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"Fix login\"}\n".to_vec(),
        );
        let written = io.captured_stdin();
        assert_eq!(
            exchange(&io, "User: login\nAssistant: fixed callback").await.unwrap(),
            "Fix login"
        );
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert!(String::from_utf8_lossy(&written.lock().await).contains("fixed callback"));
        let bad = FakeAgentIo::never_exits(
            b"{\"type\":\"result\",\"subtype\":\"error\",\"is_error\":true,\"result\":\"private payload\"}\n".to_vec(),
        );
        assert_eq!(
            exchange(&bad, "test").await.unwrap_err().to_string(),
            "backend transport error: Claude title request failed"
        );
    }
    #[tokio::test]
    #[ignore = "LIVE: real Claude title call; never uses claude-pro/claude-pro2"]
    async fn title_live_claude() {
        let tmp = tempfile::tempdir().unwrap();
        let runner = ClaudeTitleGenerator {
            spawner: Arc::new(aionui_process::RealSpawner::new(
                Arc::new(aionui_process::FileRegistryStore::new(tmp.path())),
                uuid::Uuid::now_v7(),
                "title-live",
            )),
            command: command(
                &super::super::SessionConfig {
                    cwd: Some(tmp.path().to_string_lossy().into()),
                    ..Default::default()
                },
                std::env::var("AION_TITLE_LIVE_MODEL").ok(),
            ),
        };
        let result = runner.generate("{\"current_title\":\"登录问题\",\"messages\":[{\"role\":\"user\",\"text\":\"修复登录跳转\"},{\"role\":\"assistant\",\"text\":\"已修复 OAuth 回调并完成回归测试\"}]}").await;
        assert!(
            result
                .as_ref()
                .is_ok_and(|title| !title.trim().is_empty() && title.chars().count() <= 80),
            "live Claude title call failed: {result:?}"
        );
        println!("Claude title live: success");
    }
}
