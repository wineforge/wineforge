//! Trusted, machine-local MCP server registry and launch resolution.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use wineforge_core::McpBinding;

use crate::mcp_broker::{BrokerLimits, NativeServer};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ServerRegistry {
    pub(crate) schema_version: u32,
    pub(crate) servers: BTreeMap<String, RegisteredServer>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RegisteredServer {
    pub(crate) executable: PathBuf,
    #[serde(default)]
    pub(crate) arguments: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) working_directory: Option<PathBuf>,
    #[serde(default)]
    pub(crate) permissions: Vec<String>,
    #[serde(default = "default_max_message_bytes")]
    pub(crate) max_message_bytes: usize,
    #[serde(default = "default_idle_timeout_seconds")]
    pub(crate) idle_timeout_seconds: u64,
}

fn default_max_message_bytes() -> usize {
    1024 * 1024
}
fn default_idle_timeout_seconds() -> u64 {
    300
}

#[derive(Debug)]
pub(crate) struct ResolvedServer {
    pub(crate) endpoint: String,
    pub(crate) native: NativeServer,
    pub(crate) limits: BrokerLimits,
}

pub(crate) fn read(path: &Path) -> Result<ServerRegistry> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("could not inspect MCP registry {}", path.display()))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        bail!("MCP registry must be a regular file: {}", path.display());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.mode() & 0o022 != 0 {
            bail!("MCP registry must not be writable by group or other users");
        }
    }
    let text = fs::read_to_string(path)?;
    let registry: ServerRegistry = toml::from_str(&text)
        .with_context(|| format!("invalid MCP registry {}", path.display()))?;
    validate(&registry)?;
    Ok(registry)
}

pub(crate) fn resolve(
    registry: &ServerRegistry,
    bindings: &[McpBinding],
    token_environment: &str,
) -> Result<Vec<ResolvedServer>> {
    let mut endpoints = BTreeSet::new();
    let mut result = Vec::new();
    for binding in bindings {
        if !endpoints.insert(&binding.endpoint) {
            bail!("duplicate MCP binding for endpoint {}", binding.endpoint);
        }
        let registered = registry.servers.get(&binding.server).with_context(|| {
            format!(
                "MCP server {} is not present in the trusted local registry",
                binding.server
            )
        })?;
        let allowed = registered
            .permissions
            .iter()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        for permission in &binding.permissions {
            if !allowed.contains(permission.as_str()) {
                bail!(
                    "MCP binding {} requests permission {} not allowed by server {}",
                    binding.endpoint,
                    permission,
                    binding.server
                );
            }
        }
        result.push(ResolvedServer {
            endpoint: binding.endpoint.clone(),
            native: NativeServer {
                executable: registered.executable.clone(),
                arguments: registered.arguments.clone(),
                working_directory: registered.working_directory.clone(),
                removed_environment: vec![token_environment.into()],
                permissions: binding.permissions.clone(),
            },
            limits: BrokerLimits {
                max_message_bytes: registered.max_message_bytes,
                idle_timeout: std::time::Duration::from_secs(registered.idle_timeout_seconds),
            },
        });
    }
    Ok(result)
}

fn validate(registry: &ServerRegistry) -> Result<()> {
    if registry.schema_version != 1 {
        bail!(
            "unsupported MCP registry schema version {}",
            registry.schema_version
        );
    }
    for (id, server) in &registry.servers {
        if !valid_id(id) {
            bail!("invalid MCP server id {id:?}");
        }
        if !server.executable.is_absolute() {
            bail!("MCP server {id} executable must be an absolute path");
        }
        let executable = fs::symlink_metadata(&server.executable)
            .with_context(|| format!("could not inspect MCP server {id} executable"))?;
        if !executable.is_file() || executable.file_type().is_symlink() {
            bail!("MCP server {id} executable must be a regular, non-symlink file");
        }
        if let Some(directory) = &server.working_directory {
            if !directory.is_absolute() {
                bail!("MCP server {id} working directory must be absolute");
            }
            let directory_metadata = fs::symlink_metadata(directory)
                .with_context(|| format!("could not inspect MCP server {id} working directory"))?;
            if !directory_metadata.is_dir() || directory_metadata.file_type().is_symlink() {
                bail!("MCP server {id} working directory must be a real directory");
            }
        }
        if !(256..=16 * 1024 * 1024).contains(&server.max_message_bytes) {
            bail!("MCP server {id} has an invalid message size limit");
        }
        if !(1..=86_400).contains(&server.idle_timeout_seconds) {
            bail!("MCP server {id} has an invalid idle timeout");
        }
        if server.arguments.len() > 128 || server.arguments.iter().any(|value| value.len() > 4096) {
            bail!("MCP server {id} has too many or oversized arguments");
        }
        let mut permissions = BTreeSet::new();
        for permission in &server.permissions {
            if !known_permission(permission) || !permissions.insert(permission) {
                bail!("MCP server {id} has an invalid or duplicate permission");
            }
        }
    }
    Ok(())
}

fn known_permission(value: &str) -> bool {
    matches!(
        value,
        "tools.read" | "tools.call" | "resources.read" | "prompts.read" | "completion.use"
    )
}

fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_' | b'.')
        })
}

pub(crate) fn default_path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME is unavailable; pass --mcp-registry")?;
    #[cfg(target_os = "macos")]
    return Ok(PathBuf::from(home).join("Library/Application Support/Wineforge/mcp-servers.toml"));
    #[cfg(not(target_os = "macos"))]
    Ok(PathBuf::from(home).join(".config/wineforge/mcp-servers.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> ServerRegistry {
        ServerRegistry {
            schema_version: 1,
            servers: BTreeMap::from([(
                "tools".into(),
                RegisteredServer {
                    executable: PathBuf::from("/usr/bin/example-mcp"),
                    arguments: vec!["serve".into()],
                    working_directory: Some(PathBuf::from("/tmp")),
                    permissions: vec!["tools.call".into()],
                    max_message_bytes: 4096,
                    idle_timeout_seconds: 30,
                },
            )]),
        }
    }

    #[test]
    fn resolves_symbolic_binding_without_shell_commands() {
        let result = resolve(
            &registry(),
            &[McpBinding {
                endpoint: "application-tools".into(),
                server: "tools".into(),
                permissions: vec!["tools.call".into()],
            }],
            "TOKEN",
        )
        .unwrap();
        assert_eq!(
            result[0].native.executable,
            Path::new("/usr/bin/example-mcp")
        );
        assert_eq!(result[0].native.arguments, ["serve"]);
    }

    #[test]
    fn rejects_permission_not_granted_by_registry() {
        let error = resolve(
            &registry(),
            &[McpBinding {
                endpoint: "application-tools".into(),
                server: "tools".into(),
                permissions: vec!["resources.write".into()],
            }],
            "TOKEN",
        )
        .unwrap_err();
        assert!(error.to_string().contains("not allowed"));
    }
}
