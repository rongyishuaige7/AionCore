//! Private native MCP config lifetime, owned by a subscription session.

use crate::error::AgentError;
use aionui_api_types::TeamMcpStdioConfig;
use std::path::{Path, PathBuf};

pub(crate) struct NativeMcpFile {
    directory: PathBuf,
    file: PathBuf,
}

impl NativeMcpFile {
    pub(crate) fn create(root: &Path, config: &TeamMcpStdioConfig, workspace: &Path) -> Result<Self, AgentError> {
        let native = config.native_http.as_ref().ok_or_else(|| {
            AgentError::bad_request(
                "Subscription Team MCP needs fresh native HTTP credentials; rebuild the Team runtime",
            )
        })?;
        if native.port < 1024
            || native.token.len() < 32
            || native.token.len() > 256
            || !native
                .token
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"+/=_-".contains(&c))
            || config.slot_id.is_empty()
            || config.slot_id.len() > 128
            || !config
                .slot_id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
        {
            return Err(AgentError::bad_request("Invalid native Team MCP connection"));
        }
        Self::write(
            root,
            workspace,
            &serde_json::json!({"mcpServers":{"aionui-team":{
                "type":"http", "url":format!("http://127.0.0.1:{}/mcp", native.port),
                "headers":{"Authorization":format!("Bearer {}",native.token),"X-Slot-Id":config.slot_id}
            }}}),
        )
    }

    #[cfg(unix)]
    fn write(root: &Path, workspace: &Path, value: &serde_json::Value) -> Result<Self, AgentError> {
        use std::io::Write;
        use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
        let io_error = |_| AgentError::internal("Unable to prepare private native MCP configuration");
        if !root.is_absolute() {
            return Err(AgentError::bad_request("Native MCP root must be absolute"));
        }
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(root)
            .map_err(io_error)?;
        let metadata = std::fs::symlink_metadata(root).map_err(io_error)?;
        // Never chmod an existing unsafe directory, follow a root symlink, or
        // reuse a file. Only the backend creates these private launch files.
        if !metadata.is_dir()
            || metadata.file_type().is_symlink()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o077 != 0
        {
            return Err(AgentError::bad_request(
                "Native MCP root must be an owned private directory",
            ));
        }
        let root = root.canonicalize().map_err(io_error)?;
        let workspace = workspace.canonicalize().map_err(io_error)?;
        if root.starts_with(&workspace) || workspace.starts_with(&root) {
            return Err(AgentError::bad_request("Native MCP root must be outside the workspace"));
        }
        let directory = root.join(format!("launch-{}", uuid::Uuid::new_v4()));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&directory)
            .map_err(io_error)?;
        let owned = Self {
            file: directory.join("mcp.json"),
            directory,
        };
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&owned.file)
            .map_err(io_error)?;
        file.write_all(
            &serde_json::to_vec(value).map_err(|_| AgentError::internal("Native MCP serialization failed"))?,
        )
        .map_err(io_error)?;
        Ok(owned)
    }

    #[cfg(not(unix))]
    fn write(_root: &Path, _workspace: &Path, _value: &serde_json::Value) -> Result<Self, AgentError> {
        Err(AgentError::bad_request(
            "Native subscription MCP requires the reviewed macOS sandbox",
        ))
    }

    pub(crate) fn path(&self) -> &Path {
        &self.file
    }
}

impl Drop for NativeMcpFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.file);
        let _ = std::fs::remove_dir(&self.directory);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    fn config() -> TeamMcpStdioConfig {
        TeamMcpStdioConfig {
            native_http: Some(aionui_api_types::TeamMcpHttpConfig {
                port: 55001,
                token: "fixture".repeat(10),
            }),
            team_id: "team-1".into(),
            port: 1,
            token: "legacy-must-not-leak".into(),
            slot_id: "slot-1".into(),
            binary_path: "/untrusted/bridge".into(),
        }
    }

    #[test]
    fn private_native_config_has_no_stdio_and_is_removed_with_its_owner() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("native-mcp");
        let workspace = tempfile::tempdir().unwrap();
        let file = NativeMcpFile::create(&root, &config(), workspace.path()).unwrap();
        let path = file.path().to_owned();
        assert_eq!(std::fs::metadata(&root).unwrap().mode() & 0o777, 0o700);
        assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        let value: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(value["mcpServers"]["aionui-team"]["type"], "http");
        assert_eq!(value["mcpServers"]["aionui-team"]["headers"]["X-Slot-Id"], "slot-1");
        assert!(!std::fs::read_to_string(&path).unwrap().contains("legacy-must-not-leak"));
        drop(file);
        assert!(!path.exists());
    }

    #[test]
    fn legacy_credentials_and_unsafe_roots_never_authorize_native_mcp() {
        let temp = tempfile::tempdir().unwrap();
        let mut legacy = config();
        legacy.native_http = None;
        assert!(
            matches!(NativeMcpFile::create(temp.path(), &legacy, temp.path()), Err(AgentError::BadRequest(message)) if message.contains("fresh native HTTP"))
        );
        let root = temp.path().join("unsafe");
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            matches!(NativeMcpFile::create(&root, &config(), temp.path()), Err(AgentError::BadRequest(message)) if message.contains("owned private"))
        );
        let link = temp.path().join("link");
        std::os::unix::fs::symlink(temp.path(), &link).unwrap();
        assert!(NativeMcpFile::create(&link, &config(), temp.path()).is_err());
        let overlap = temp.path().join("overlap");
        assert!(
            matches!(NativeMcpFile::create(&overlap, &config(), temp.path()), Err(AgentError::BadRequest(message)) if message.contains("outside the workspace"))
        );
        assert_eq!(std::fs::read_dir(&overlap).unwrap().count(), 0);
    }
}
