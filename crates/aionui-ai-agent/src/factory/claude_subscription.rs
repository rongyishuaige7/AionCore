//! Explicit local subscription selection; never a provider or protocol fallback.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use aionui_api_types::AgentMetadata;
use aionui_common::{CommandSpec, EnvVar};
use aionui_process::{ManagedProcess, ProcessError, Spawner};

use crate::error::AgentError;

#[derive(Clone, Debug)]
pub(crate) struct SubscriptionLaunch {
    home: PathBuf,
    program: PathBuf,
    account: u8,
}

impl SubscriptionLaunch {
    pub(crate) fn resolve(metadata: &AgentMetadata) -> Result<Option<Self>, AgentError> {
        if metadata.backend.as_deref() != Some("claude") {
            return Ok(None);
        }
        let command = metadata
            .command
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .or(metadata.agent_source_info.binary_name.as_deref());
        let Some(command) = command else { return Ok(None) };
        if account_for_command(command)?.is_none() {
            return Ok(None);
        }
        let home = dirs::home_dir().ok_or_else(|| AgentError::bad_request("Subscription home unavailable"))?;
        Self::resolve_for_home(command, metadata.resolved_command.as_deref(), &home)
    }

    fn resolve_for_home(command: &str, resolved: Option<&Path>, home: &Path) -> Result<Option<Self>, AgentError> {
        let Some(account) = account_for_command(command)? else {
            return Ok(None);
        };
        if !home.is_absolute() || home.to_str().is_none() {
            return Err(AgentError::bad_request(
                "Subscription home must be an absolute UTF-8 path",
            ));
        }
        let path = Path::new(command);
        let expected = home
            .join(".local/bin")
            .join(path.file_name().expect("recognized command"));
        let bare = path.components().count() == 1;
        if path != expected && !(bare && resolved == Some(expected.as_path())) {
            return Err(AgentError::bad_request(
                "Subscription launcher must be selected from home/.local/bin",
            ));
        }
        let name = if account == 1 {
            "claude-pro-danger"
        } else {
            "claude-pro2-danger"
        };
        Ok(Some(Self {
            home: home.to_path_buf(),
            program: home.join(".local/bin").join(name),
            account,
        }))
    }

    pub(crate) fn program(&self) -> &Path {
        &self.program
    }

    pub(crate) fn account(&self) -> u8 {
        self.account
    }

    pub(crate) fn environment(&self) -> Vec<EnvVar> {
        [
            ("HOME", self.home.to_string_lossy().into_owned()),
            ("PATH", "/usr/bin:/bin:/usr/sbin:/sbin".to_owned()),
            ("TERM", "dumb".to_owned()),
        ]
        .into_iter()
        .map(|(name, value)| EnvVar {
            name: name.to_owned(),
            value,
        })
        .collect()
    }

    /// Apply before registry probes as well as long-lived session spawns.
    pub(crate) fn configure_probe(&self, command: &mut aionui_runtime::Builder) {
        command
            .env_clear()
            .envs(self.environment().iter().map(|e| (&e.name, &e.value)));
    }

    /// The connection retains this spawner in its resume/wake/version recipes.
    #[cfg(test)]
    pub(crate) fn scope(&self, inner: Arc<dyn Spawner>) -> Arc<dyn Spawner> {
        self.scope_with_mcp(inner, None)
    }
    pub(crate) fn scope_with_mcp(
        &self,
        inner: Arc<dyn Spawner>,
        file: Option<super::claude_mcp::NativeMcpFile>,
    ) -> Arc<dyn Spawner> {
        Arc::new(SubscriptionSpawner {
            launch: self.clone(),
            inner,
            _mcp_file: file,
        })
    }
}

fn account_for_command(command: &str) -> Result<Option<u8>, AgentError> {
    match Path::new(command).file_name().and_then(|s| s.to_str()) {
        Some("claude-pro-danger" | "claude-pro-yi") => Ok(Some(1)),
        Some("claude-pro2-danger" | "claude-pro2-yi") => Ok(Some(2)),
        Some("claude-pro" | "claude-pro2") => Err(AgentError::bad_request(
            "Subscription sessions require an explicitly selected danger entrypoint",
        )),
        _ => Ok(None),
    }
}

struct SubscriptionSpawner {
    _mcp_file: Option<super::claude_mcp::NativeMcpFile>,
    launch: SubscriptionLaunch,
    inner: Arc<dyn Spawner>,
}

#[async_trait::async_trait]
impl Spawner for SubscriptionSpawner {
    async fn spawn(
        &self,
        mut spec: CommandSpec,
        _extra_env: &[(String, String)],
        owner: &str,
    ) -> Result<Arc<ManagedProcess>, ProcessError> {
        if spec.command != self.launch.program {
            return Err(ProcessError::internal(
                "Subscription launcher mismatch; fallback refused",
            ));
        }
        spec.clear_env = true;
        spec.env = self.launch.environment();
        self.inner.spawn(spec, &[], owner).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selected_aliases_remain_bound_to_their_account() {
        let home = Path::new("/Users/fixture");
        for (name, target, account) in [
            ("claude-pro-yi", "claude-pro-danger", 1),
            ("claude-pro-danger", "claude-pro-danger", 1),
            ("claude-pro2-yi", "claude-pro2-danger", 2),
            ("claude-pro2-danger", "claude-pro2-danger", 2),
        ] {
            let path = home.join(".local/bin").join(name);
            let launch = SubscriptionLaunch::resolve_for_home(path.to_str().unwrap(), None, home)
                .unwrap()
                .unwrap();
            assert_eq!(launch.program(), home.join(".local/bin").join(target));
            assert_eq!(launch.account(), account);
            let bare = SubscriptionLaunch::resolve_for_home(name, Some(&path), home)
                .unwrap()
                .unwrap();
            assert_eq!(bare.program(), launch.program());
        }
    }

    #[test]
    fn never_promotes_restricted_entries_or_trusts_a_same_named_path() {
        let home = Path::new("/Users/fixture");
        for name in [
            "claude-pro",
            "claude-pro2",
            "/tmp/claude-pro2-danger",
            "claude-pro-danger",
        ] {
            let error = SubscriptionLaunch::resolve_for_home(name, None, home).unwrap_err();
            assert!(matches!(error, AgentError::BadRequest(_)));
        }
        assert!(
            SubscriptionLaunch::resolve_for_home("/custom/claude", None, home)
                .unwrap()
                .is_none()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn registry_probe_configuration_clears_inherited_environment() {
        let home = Path::new("/Users/fixture");
        let launch = SubscriptionLaunch::resolve_for_home("/Users/fixture/.local/bin/claude-pro2-danger", None, home)
            .unwrap()
            .unwrap();
        let mut command = aionui_runtime::Builder::clean_cli("/usr/bin/env");
        command.env("ANTHROPIC_BASE_URL", "fixture-invalid");
        launch.configure_probe(&mut command);
        command.arg("-0");
        let output = command.output().await.unwrap();
        assert!(output.status.success());
        let keys = output
            .stdout
            .split(|byte| *byte == 0)
            .filter_map(|entry| entry.iter().position(|byte| *byte == b'=').map(|at| &entry[..at]))
            .map(|key| String::from_utf8_lossy(key).into_owned())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            keys,
            std::collections::BTreeSet::from(["HOME".into(), "PATH".into(), "TERM".into()])
        );
    }

    #[derive(Default)]
    struct Recorder(std::sync::Mutex<Vec<SpawnRecord>>);

    type SpawnRecord = (CommandSpec, Vec<(String, String)>, String);

    #[cfg(unix)]
    #[test]
    fn native_mcp_file_survives_connection_recipe_clones_until_last_owner_drops() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let file = super::super::claude_mcp::NativeMcpFile::create(
            &temp.path().join("native-mcp"),
            &aionui_api_types::TeamMcpStdioConfig {
                team_id: "team-fixture".into(),
                port: 55000,
                token: "legacy-fixture".into(),
                slot_id: "lead-1".into(),
                binary_path: "/unreviewed/bridge".into(),
                native_http: Some(aionui_api_types::TeamMcpHttpConfig {
                    port: 55001,
                    token: "fixture-only-".repeat(5),
                }),
            },
            workspace.path(),
        )
        .unwrap();
        let path = file.path().to_owned();
        let home = Path::new("/Users/fixture");
        let launch = SubscriptionLaunch::resolve_for_home("/Users/fixture/.local/bin/claude-pro2-danger", None, home)
            .unwrap()
            .unwrap();
        let scoped = launch.scope_with_mcp(Arc::new(Recorder::default()), Some(file));
        let wake_recipe = scoped.clone();
        drop(scoped);
        assert!(path.exists());
        drop(wake_recipe);
        assert!(!path.exists());
        assert!(!path.parent().unwrap().exists());
    }

    #[async_trait::async_trait]
    impl Spawner for Recorder {
        async fn spawn(
            &self,
            spec: CommandSpec,
            extra: &[(String, String)],
            owner: &str,
        ) -> Result<Arc<ManagedProcess>, ProcessError> {
            self.0.lock().unwrap().push((spec, extra.to_vec(), owner.to_owned()));
            Err(ProcessError::internal("fixture stops before execution"))
        }
    }

    #[tokio::test]
    async fn every_spawn_discards_provider_runtime_and_extra_environment() {
        let home = Path::new("/Users/fixture");
        for name in ["claude-pro-danger", "claude-pro2-danger"] {
            let program = home.join(".local/bin").join(name);
            let launch = SubscriptionLaunch::resolve_for_home(program.to_str().unwrap(), None, home)
                .unwrap()
                .unwrap();
            let recorder = Arc::new(Recorder::default());
            let scoped = launch.scope(recorder.clone());
            for args in [
                vec!["--version"],
                vec!["--session-id", "fresh"],
                vec!["--resume", "resumed"],
            ] {
                let args = args.into_iter().map(str::to_owned).collect::<Vec<_>>();
                let spec = CommandSpec {
                    command: program.clone(),
                    args: args.clone(),
                    env: vec![
                        EnvVar {
                            name: "ANTHROPIC_BASE_URL".into(),
                            value: "fixture-invalid".into(),
                        },
                        EnvVar {
                            name: "AIONUI_RUNTIME_TOKEN".into(),
                            value: "fixture-only".into(),
                        },
                    ],
                    cwd: Some("/fixture/workspace".into()),
                    ..Default::default()
                };
                assert!(
                    scoped
                        .spawn(
                            spec,
                            &[("HTTPS_PROXY".into(), "fixture-invalid".into())],
                            "fixture-owner"
                        )
                        .await
                        .is_err()
                );
                let records = recorder.0.lock().unwrap();
                let (actual, extra, owner) = records.last().unwrap();
                assert!(actual.clear_env);
                assert_eq!(actual.env, launch.environment());
                assert_eq!(actual.command, program);
                assert_eq!(actual.args, args);
                assert_eq!(actual.cwd.as_deref(), Some("/fixture/workspace"));
                assert!(extra.is_empty());
                assert_eq!(owner, "fixture-owner");
            }
            let before = recorder.0.lock().unwrap().len();
            let wrong = CommandSpec {
                command: "claude".into(),
                ..Default::default()
            };
            let error = scoped.spawn(wrong, &[], "fixture-owner").await.err().unwrap();
            assert!(error.to_string().contains("fallback refused"));
            assert_eq!(recorder.0.lock().unwrap().len(), before);
        }
    }
}
