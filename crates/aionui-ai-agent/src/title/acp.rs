//! Isolated ACP title exchange. Wire types: agent-client-protocol-schema 1.5.0
//! `src/v1/{agent,client}.rs`. No client filesystem/terminal capabilities or MCP.

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    ContentBlock, Implementation, InitializeRequest, NewSessionRequest, NewSessionResponse, PromptRequest,
    SessionConfigOptionCategory, SessionConfigValueId, SessionNotification, SessionUpdate,
    SetSessionConfigOptionRequest, TextContent,
};
use aionui_common::CommandSpec;
use aionui_session::{BackendError, TITLE_INSTRUCTIONS, TitleGenerator};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::capability::cli_process::CliAgentProcess;

pub(crate) struct AcpTitleGenerator {
    pub command: CommandSpec,
    pub model: Option<String>,
}

struct TitleProcess(CliAgentProcess);
impl Drop for TitleProcess {
    fn drop(&mut self) {
        self.0.force_kill_tree();
    }
}

fn failure() -> BackendError {
    BackendError::Transport("ACP title exchange failed".into())
}

#[async_trait::async_trait]
impl TitleGenerator for AcpTitleGenerator {
    async fn generate(&self, description: &str) -> Result<String, BackendError> {
        let process = TitleProcess(
            CliAgentProcess::spawn_for_title(self.command.clone())
                .await
                .map_err(|_| failure())?,
        );
        let (stdin, stdout) = process.0.take_stdio().await.ok_or_else(failure)?;
        exchange(stdin, stdout, &self.command, self.model.as_deref(), description).await
    }
}

async fn exchange<W, R>(
    mut stdin: W,
    stdout: R,
    command: &CommandSpec,
    model: Option<&str>,
    description: &str,
) -> Result<String, BackendError>
where
    W: tokio::io::AsyncWrite + Unpin,
    R: tokio::io::AsyncRead + Unpin,
{
    let mut lines = BufReader::new(stdout).lines();
    let init = InitializeRequest::new(ProtocolVersion::LATEST).client_info(Implementation::new("aionui-title", "1"));
    send(&mut stdin, 1, "initialize", &init).await?;
    response(&mut lines, 1).await?;
    let cwd = command.cwd.as_deref().ok_or_else(failure)?;
    send(&mut stdin, 2, "session/new", &NewSessionRequest::new(cwd)).await?;
    let session_value = response(&mut lines, 2).await?;
    let session: NewSessionResponse = serde_json::from_value(session_value.clone()).map_err(|_| failure())?;
    if let Some(model) = model {
        if let Some(option) = session.config_options.as_ref().and_then(|options| {
            options
                .iter()
                .find(|o| o.category == Some(SessionConfigOptionCategory::Model) || o.id.to_string() == "model")
        }) {
            let request = SetSessionConfigOptionRequest::new(
                session.session_id.clone(),
                option.id.clone(),
                SessionConfigValueId::new(model),
            );
            send(&mut stdin, 3, "session/set_config_option", &request).await?;
            response(&mut lines, 3).await?;
        } else if session_value.get("models").is_some() {
            // Legacy contract implemented by our existing AcpProtocol::set_model.
            send(
                &mut stdin,
                3,
                "session/set_model",
                &json!({"sessionId": session.session_id, "modelId": model}),
            )
            .await?;
            response(&mut lines, 3).await?;
        } else {
            // Never silently summarize on a different, default model.
            return Err(BackendError::Transport("ACP title model selection unavailable".into()));
        }
    }
    let prompt = PromptRequest::new(
        session.session_id.clone(),
        vec![ContentBlock::Text(TextContent::new(format!(
            "{TITLE_INSTRUCTIONS}\n\n{description}"
        )))],
    );
    send(&mut stdin, 4, "session/prompt", &prompt).await?;
    let mut title = String::new();
    while let Some(line) = lines.next_line().await.map_err(|_| failure())? {
        let frame: Value = serde_json::from_str(&line).map_err(|_| failure())?;
        // Refuse every reverse request before any client-side action can run.
        if frame.get("method").is_some() && frame.get("id").is_some() {
            return Err(BackendError::Transport(
                "ACP title requested a tool or permission".into(),
            ));
        }
        if frame["id"] == 4 {
            if frame["result"]["stopReason"] == "end_turn" && !title.trim().is_empty() {
                return Ok(title);
            }
            return Err(failure());
        }
        if frame["method"] == "session/update" {
            let notification: SessionNotification =
                serde_json::from_value(frame["params"].clone()).map_err(|_| failure())?;
            if notification.session_id != session.session_id {
                continue;
            }
            match notification.update {
                SessionUpdate::AgentMessageChunk(chunk) => {
                    if let ContentBlock::Text(text) = chunk.content {
                        title.push_str(&text.text);
                    }
                    if title.len() > 4096 {
                        return Err(failure());
                    }
                }
                SessionUpdate::ToolCall(_) | SessionUpdate::ToolCallUpdate(_) => return Err(failure()),
                _ => {}
            }
        }
    }
    Err(failure())
}

async fn send<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    id: u64,
    method: &str,
    params: &impl serde::Serialize,
) -> Result<(), BackendError> {
    let mut bytes = serde_json::to_vec(&json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params}))
        .map_err(|_| failure())?;
    bytes.push(b'\n');
    writer.write_all(&bytes).await.map_err(|_| failure())?;
    writer.flush().await.map_err(|_| failure())
}

async fn response<R: tokio::io::AsyncBufRead + Unpin>(
    lines: &mut tokio::io::Lines<R>,
    id: u64,
) -> Result<Value, BackendError> {
    while let Some(line) = lines.next_line().await.map_err(|_| failure())? {
        let frame: Value = serde_json::from_str(&line).map_err(|_| failure())?;
        if frame.get("method").is_some() && frame.get("id").is_some() {
            return Err(failure());
        }
        if frame["id"] == id {
            return frame.get("result").cloned().ok_or_else(failure);
        }
    }
    Err(failure())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "LIVE: installed OpenCode ACP title call"]
    async fn title_live_acp_opencode() {
        let tmp = tempfile::tempdir().unwrap();
        // `opencode acp --help` (2026-11-11) verifies this entrypoint.
        let runner = AcpTitleGenerator {
            command: CommandSpec {
                command: "opencode".into(),
                args: vec!["acp".into()],
                env: vec![],
                cwd: Some(tmp.path().to_string_lossy().into()),
            },
            model: std::env::var("AION_TITLE_LIVE_MODEL").ok(),
        };
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(75),
            runner.generate("User: 修复登录跳转\nAssistant: OAuth 回调已修复并通过回归测试"),
        )
        .await;
        assert!(
            matches!(result, Ok(Ok(ref title)) if !title.trim().is_empty() && title.chars().count() <= 80),
            "OpenCode ACP title live failed"
        );
        println!("ACP title live: success");
    }
    fn command() -> CommandSpec {
        CommandSpec {
            command: "agent".into(),
            args: vec![],
            env: vec![],
            cwd: Some("/tmp".into()),
        }
    }
    fn frames(tail: &str) -> String {
        format!(
            "{{\"id\":1,\"result\":{{}}}}\n{{\"id\":2,\"result\":{{\"sessionId\":\"s\",\"configOptions\":[{{\"id\":\"model\",\"name\":\"Model\",\"category\":\"model\",\"type\":\"select\",\"currentValue\":\"default\",\"options\":[{{\"value\":\"chosen-model\",\"name\":\"Chosen\"}}]}}]}}}}\n{{\"id\":3,\"result\":{{}}}}\n{tail}"
        )
    }
    #[tokio::test]
    async fn title_exchange_selects_model_and_disables_client_tools() {
        let frames = frames(concat!(
            "{\"method\":\"session/update\",\"params\":{\"sessionId\":\"s\",\"update\":{\"sessionUpdate\":\"agent_message_chunk\",\"content\":{\"type\":\"text\",\"text\":\"登录修复\"}}}}\n",
            "{\"id\":4,\"result\":{\"stopReason\":\"end_turn\"}}\n"
        ));
        let mut written = Vec::new();
        let title = exchange(
            &mut written,
            frames.as_bytes(),
            &command(),
            Some("chosen-model"),
            "User: fix login\nAssistant: fixed callback",
        )
        .await
        .unwrap();
        assert_eq!(title, "登录修复");
        let requests: Vec<Value> = String::from_utf8(written)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_ne!(requests[0]["params"]["clientCapabilities"]["terminal"], true);
        assert_eq!(requests[1]["params"]["mcpServers"], json!([]));
        assert_eq!(requests[2]["params"]["value"], "chosen-model");
        assert!(
            requests[3]["params"]["prompt"][0]["text"]
                .as_str()
                .unwrap()
                .contains("fixed callback")
        );
    }
    #[tokio::test]
    async fn title_exchange_refuses_permission_and_partial_answer() {
        for tail in [
            "{\"id\":99,\"method\":\"session/request_permission\",\"params\":{}}\n",
            "{\"id\":4,\"result\":{\"stopReason\":\"cancelled\"}}\n",
            "{\"id\":4,\"error\":{\"message\":\"provider secret\"}}\n",
        ] {
            let frames = frames(tail);
            let error = exchange(
                &mut Vec::new(),
                frames.as_bytes(),
                &command(),
                Some("chosen-model"),
                "test",
            )
            .await
            .unwrap_err();
            assert!(!error.to_string().contains("provider secret"));
        }
    }
}
