//! Isolated agy title call. Flags verified with local `agy --help` (2026-11-11).
use std::sync::Arc;

use aionui_common::CommandSpec;
use aionui_process::Spawner;
use tokio::io::{AsyncBufReadExt, BufReader};

use super::wire::{AgyEvent, AgyStepType, parse_line};
use crate::{AgentIo, BackendError, ManagedProcessIo, TITLE_INSTRUCTIONS, TitleGenerator};

pub(super) struct AgyTitleGenerator {
    pub spawner: Arc<dyn Spawner>,
    pub command: CommandSpec,
    pub model: Option<String>,
}

#[async_trait::async_trait]
impl TitleGenerator for AgyTitleGenerator {
    async fn generate(&self, description: &str) -> Result<String, BackendError> {
        let mut command = self.command.clone();
        command.args = vec![
            "-p".into(),
            format!("{TITLE_INSTRUCTIONS}\n\n{description}"),
            "--output-format".into(),
            "stream-json".into(),
            "--mode".into(),
            "plan".into(),
            "--sandbox".into(),
            "--disable-slash-commands".into(),
            "--new-project".into(),
            "--print-timeout".into(),
            "60s".into(),
        ];
        if let Some(model) = &self.model {
            command.args.extend(["--model".into(), model.clone()]);
        }
        let process = self
            .spawner
            .spawn_quiet(command, &[], "aionui-session-title")
            .await
            .map_err(|_| failure())?;
        let io = ManagedProcessIo::new(process);
        let result =
            tokio::time::timeout(std::time::Duration::from_secs(60), exchange(&io, self.model.as_deref())).await;
        io.terminate().await;
        result.map_err(|_| failure())?
    }
}

fn failure() -> BackendError {
    BackendError::Transport("Antigravity title exchange failed".into())
}

async fn exchange(io: &dyn AgentIo, model: Option<&str>) -> Result<String, BackendError> {
    let (_stdin, stdout) = io.take_stdio().await.ok_or_else(failure)?;
    let mut lines = BufReader::new(stdout).lines();
    while let Some(line) = lines.next_line().await.map_err(|_| failure())? {
        match parse_line(&line) {
            Some(AgyEvent::Init(init)) if model.is_some() && init.model.as_deref() != model => return Err(failure()),
            Some(AgyEvent::StepUpdate(step))
                if matches!(
                    step.step_type,
                    AgyStepType::Tool | AgyStepType::Subagent | AgyStepType::ErrorMessage
                ) =>
            {
                return Err(failure());
            }
            Some(AgyEvent::Result(result)) => {
                return if result.status == "SUCCESS" && !result.response.trim().is_empty() {
                    Ok(result.response)
                } else {
                    Err(failure())
                };
            }
            _ => {}
        }
    }
    Err(failure())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn accepts_success_and_refuses_tool_steps_or_model_fallback() {
        let good = crate::testing::FakeAgentIo::never_exits(
            b"{\"event\":\"result\",\"result\":{\"status\":\"SUCCESS\",\"response\":\"Login fixed\"}}\n".to_vec(),
        );
        assert_eq!(exchange(&good, None).await.unwrap(), "Login fixed");
        for frame in [
            "{\"event\":\"init\",\"init\":{\"model\":\"different-model\"}}\n",
            "{\"event\":\"step_update\",\"step_update\":{\"step_index\":1,\"state\":\"ACTIVE\",\"step_type\":\"tool\"}}\n",
        ] {
            let bad = crate::testing::FakeAgentIo::never_exits(frame.as_bytes().to_vec());
            assert!(exchange(&bad, Some("selected-model")).await.is_err());
        }
    }
    #[tokio::test]
    #[ignore = "LIVE: real Antigravity title call"]
    async fn title_live_antigravity() {
        let tmp = tempfile::tempdir().unwrap();
        let runner = AgyTitleGenerator {
            spawner: Arc::new(aionui_process::RealSpawner::new(
                Arc::new(aionui_process::FileRegistryStore::new(tmp.path())),
                uuid::Uuid::now_v7(),
                "title-live",
            )),
            command: CommandSpec {
                command: "agy".into(),
                args: vec![],
                env: vec![],
                cwd: Some(tmp.path().to_string_lossy().into()),
            },
            model: std::env::var("AION_TITLE_LIVE_MODEL").ok(),
        };
        let result = runner.generate("{\"current_title\":\"登录问题\",\"messages\":[{\"role\":\"user\",\"text\":\"修复登录跳转\"},{\"role\":\"assistant\",\"text\":\"已修复 OAuth 回调并完成回归测试\"}]}").await;
        assert!(
            result
                .as_ref()
                .is_ok_and(|title| !title.trim().is_empty() && title.chars().count() <= 80),
            "live Antigravity title call failed: {result:?}"
        );
        println!("Antigravity title live: success");
    }
}
