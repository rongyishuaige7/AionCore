use std::sync::Arc;

use aion_providers::{LlmProvider, create_provider};
use aion_types::llm::{LlmEvent, LlmRequest};
use aion_types::message::{ContentBlock, Message, Role, StopReason};
use aionui_session::{BackendError, TITLE_INSTRUCTIONS, TitleGenerator};

/// Reuses the fully resolved transport, including per-model compatibility.
pub(crate) struct ProviderTitleGenerator {
    provider: Arc<dyn LlmProvider>,
    model: String,
}

impl ProviderTitleGenerator {
    pub(crate) fn new(config: &aion_config::config::Config) -> Self {
        Self {
            provider: create_provider(config),
            model: config.model.clone(),
        }
    }
}

#[async_trait::async_trait]
impl TitleGenerator for ProviderTitleGenerator {
    async fn generate(&self, description: &str) -> Result<String, BackendError> {
        let request = LlmRequest {
            model: self.model.clone(),
            system: TITLE_INSTRUCTIONS.into(),
            messages: vec![Message::new(
                Role::User,
                vec![ContentBlock::Text {
                    text: description.into(),
                }],
            )],
            tools: Vec::new(),
            max_tokens: Some(256),
            thinking: None,
            reasoning_effort: None,
        };
        let mut events = self.provider.stream(&request).await.map_err(|error| {
            let category = match error {
                aion_providers::ProviderError::Api { status, .. } => format!("API HTTP {status}"),
                aion_providers::ProviderError::RateLimited { .. } => "rate limited".into(),
                aion_providers::ProviderError::Http(error) => {
                    format!("HTTP {}", error.status().map(|s| s.as_u16()).unwrap_or_default())
                }
                aion_providers::ProviderError::Parse(_) => "response parse".into(),
                aion_providers::ProviderError::PromptTooLong(_) => "input size".into(),
                aion_providers::ProviderError::Connection(_) => "connection".into(),
            };
            BackendError::Transport(format!("Title provider request failed ({category})"))
        })?;
        let mut title = String::new();
        while let Some(event) = events.recv().await {
            match event {
                LlmEvent::TextDelta(text) => {
                    title.push_str(&text);
                    if title.len() > 4096 {
                        return Err(BackendError::Transport("Title provider output exceeded limit".into()));
                    }
                }
                LlmEvent::Error(message) => {
                    // ProviderError's Display prefixes are defined by aion-providers
                    // src/error.rs. Extract only a numeric HTTP status, never its body.
                    let status = message
                        .strip_prefix("API error ")
                        .and_then(|tail| tail.split(':').next())
                        .and_then(|value| value.parse::<u16>().ok());
                    return Err(BackendError::Transport(match status {
                        Some(status) => format!("Title provider stream failed (HTTP {status})"),
                        None => "Title provider did not return a title".into(),
                    }));
                }
                LlmEvent::ToolUse { .. } => {
                    return Err(BackendError::Transport("Title provider did not return a title".into()));
                }
                LlmEvent::Done {
                    stop_reason: StopReason::EndTurn,
                    ..
                } if !title.trim().is_empty() => return Ok(title),
                LlmEvent::Done { .. } => break,
                _ => {}
            }
        }
        Err(BackendError::Transport(
            "Title provider stream ended without a complete title".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[tokio::test]
    #[ignore = "LIVE: provider title call using the regular Claude provider configuration"]
    async fn title_live_provider() {
        use aion_config::config::{CliArgs, Config};
        let env = crate::cc_switch::read_claude_provider_env();
        let api_key = env
            .get("ANTHROPIC_AUTH_TOKEN")
            .or_else(|| env.get("ANTHROPIC_API_KEY"))
            .cloned()
            .expect("regular Claude provider credential unavailable");
        let model = env
            .get("ANTHROPIC_MODEL")
            .cloned()
            .expect("regular Claude provider model unavailable");
        let tmp = tempfile::tempdir().unwrap();
        let args = CliArgs {
            provider: Some("anthropic".into()),
            api_key: Some(api_key),
            base_url: env.get("ANTHROPIC_BASE_URL").cloned(),
            model: Some(model),
            max_tokens: None,
            thinking: None,
            thinking_budget: None,
            max_turns: None,
            max_tool_call_malformed_turns: None,
            max_tool_call_failure_turns: None,
            system_prompt: None,
            profile: None,
            auto_approve: false,
            project_dir: Some(tmp.path().into()),
        };
        let config = Config::resolve(&args).unwrap_or_else(|_| panic!("provider title config could not resolve"));
        let runner = ProviderTitleGenerator::new(&config);
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(75),
            runner.generate("User: 修复登录跳转\nAssistant: OAuth 回调已修复并通过回归测试"),
        )
        .await;
        if let Ok(Err(error)) = &result {
            panic!("provider title live failed: {error}");
        }
        assert!(
            matches!(result, Ok(Ok(ref title)) if !title.trim().is_empty() && title.chars().count() <= 80),
            "provider title live failed"
        );
        println!("Provider title live: success");
    }
    struct Provider {
        seen: Mutex<Vec<LlmRequest>>,
        failed: bool,
    }
    #[async_trait::async_trait]
    impl LlmProvider for Provider {
        async fn stream(
            &self,
            request: &LlmRequest,
        ) -> Result<tokio::sync::mpsc::Receiver<LlmEvent>, aion_providers::ProviderError> {
            self.seen.lock().unwrap().push(request.clone());
            let (tx, rx) = tokio::sync::mpsc::channel(4);
            tx.send(LlmEvent::TextDelta("登录验证".into())).await.unwrap();
            if self.failed {
                tx.send(LlmEvent::Error("private payload".into())).await.unwrap();
            } else {
                tx.send(LlmEvent::Done {
                    stop_reason: StopReason::EndTurn,
                    usage: Default::default(),
                })
                .await
                .unwrap();
            }
            Ok(rx)
        }
    }
    #[tokio::test]
    async fn provider_title_uses_selected_model_without_tools_or_history() {
        let provider = Arc::new(Provider {
            seen: Mutex::default(),
            failed: false,
        });
        let runner = ProviderTitleGenerator {
            provider: provider.clone(),
            model: "selected-model".into(),
        };
        assert_eq!(
            runner
                .generate("User: fix login\nAssistant: callback fixed")
                .await
                .unwrap(),
            "登录验证"
        );
        let seen = provider.seen.lock().unwrap();
        assert_eq!(seen[0].model, "selected-model");
        assert!(seen[0].tools.is_empty());
        assert_eq!(seen[0].messages.len(), 1);
        assert_eq!(seen[0].max_tokens, Some(256));
    }
    #[tokio::test]
    async fn partial_text_is_discarded_when_provider_errors() {
        let runner = ProviderTitleGenerator {
            provider: Arc::new(Provider {
                seen: Mutex::default(),
                failed: true,
            }),
            model: "selected-model".into(),
        };
        assert_eq!(
            runner.generate("test").await.unwrap_err().to_string(),
            "backend transport error: Title provider did not return a title"
        );
    }
}
