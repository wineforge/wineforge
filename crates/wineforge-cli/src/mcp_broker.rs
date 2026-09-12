//! Application-scoped MCP forwarding between a Wine client and native stdio.
//!
//! The broker intentionally supports one authenticated connection and one
//! explicitly configured child. It never invokes a shell or observes other
//! processes.

use std::fs;
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::Value;

const AUTH_PROTOCOL: &str = "wineforge-mcp-forward/1";

#[derive(Clone, Debug)]
pub(crate) struct BrokerLimits {
    pub(crate) max_message_bytes: usize,
    pub(crate) idle_timeout: Duration,
}

impl Default for BrokerLimits {
    fn default() -> Self {
        Self {
            max_message_bytes: 1024 * 1024,
            idle_timeout: Duration::from_secs(300),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct NativeServer {
    pub(crate) executable: PathBuf,
    pub(crate) arguments: Vec<String>,
    pub(crate) working_directory: Option<PathBuf>,
    pub(crate) removed_environment: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Authentication {
    protocol: String,
    token: String,
}

pub(crate) fn serve_tcp_once(
    address: SocketAddr,
    token: &str,
    server: &NativeServer,
    limits: &BrokerLimits,
) -> Result<SocketAddr> {
    if !address.ip().is_loopback() {
        bail!("MCP forwarding may only bind a loopback address");
    }
    let listener = TcpListener::bind(address)
        .with_context(|| format!("could not bind MCP broker to {address}"))?;
    let bound = listener.local_addr()?;
    let (stream, peer) = listener.accept().context("could not accept MCP client")?;
    if !peer.ip().is_loopback() {
        bail!("refused a non-loopback MCP client");
    }
    configure_tcp(&stream, limits.idle_timeout)?;
    forward(stream, token, server, limits)?;
    Ok(bound)
}

#[cfg(unix)]
pub(crate) fn serve_unix_once(
    socket_path: &Path,
    token: &str,
    server: &NativeServer,
    limits: &BrokerLimits,
) -> Result<()> {
    if socket_path.exists() {
        bail!(
            "refusing to replace existing socket {}",
            socket_path.display()
        );
    }
    let parent = socket_path
        .parent()
        .context("Unix socket path must have a parent directory")?;
    let metadata = fs::metadata(parent)
        .with_context(|| format!("could not inspect socket directory {}", parent.display()))?;
    if !metadata.is_dir() {
        bail!("socket parent is not a directory: {}", parent.display());
    }

    let listener = UnixListener::bind(socket_path)
        .with_context(|| format!("could not bind MCP socket {}", socket_path.display()))?;
    fs::set_permissions(socket_path, fs::Permissions::from_mode(0o600))?;
    let _cleanup = SocketCleanup(socket_path.to_path_buf());
    let (stream, _) = listener.accept().context("could not accept MCP client")?;
    stream.set_read_timeout(Some(limits.idle_timeout))?;
    stream.set_write_timeout(Some(limits.idle_timeout))?;
    forward(stream, token, server, limits)
}

#[cfg(unix)]
struct SocketCleanup(PathBuf);

#[cfg(unix)]
impl Drop for SocketCleanup {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn configure_tcp(stream: &TcpStream, timeout: Duration) -> io::Result<()> {
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))
}

fn spawn_native(server: &NativeServer) -> Result<Child> {
    if !server.executable.is_absolute() {
        bail!("native MCP executable must be an absolute path");
    }
    let mut command = Command::new(&server.executable);
    command
        .args(&server.arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    for variable in &server.removed_environment {
        command.env_remove(variable);
    }
    if let Some(directory) = &server.working_directory {
        if !directory.is_absolute() {
            bail!("native MCP working directory must be an absolute path");
        }
        command.current_dir(directory);
    }
    command.spawn().with_context(|| {
        format!(
            "could not start native MCP server {}",
            server.executable.display()
        )
    })
}

fn forward<S>(stream: S, token: &str, server: &NativeServer, limits: &BrokerLimits) -> Result<()>
where
    S: Read + Write + Send + TryCloneStream + 'static,
{
    if token.is_empty() {
        bail!("MCP authentication token must not be empty");
    }
    let reader_stream = stream.try_clone_stream()?;
    let mut reader = BufReader::new(reader_stream);
    authenticate(&mut reader, token, limits.max_message_bytes)?;

    let mut child = spawn_native(server)?;
    let child_stdin = child.stdin.take().context("native MCP stdin unavailable")?;
    let child_stdout = child
        .stdout
        .take()
        .context("native MCP stdout unavailable")?;
    let max = limits.max_message_bytes;

    let to_client = thread::spawn(move || -> Result<()> {
        let mut source = BufReader::new(child_stdout);
        let mut destination = BufWriter::new(stream);
        while let Some(message) = read_json_line(&mut source, max)? {
            destination.write_all(&message)?;
            destination.flush()?;
        }
        Ok(())
    });

    let to_child = (|| -> Result<()> {
        let mut destination = BufWriter::new(child_stdin);
        while let Some(message) = read_json_line(&mut reader, max)? {
            destination.write_all(&message)?;
            destination.flush()?;
        }
        Ok(())
    })();

    // The native server is scoped to this single application connection.
    let _ = child.kill();
    let _ = child.wait();
    let from_child = to_client
        .join()
        .map_err(|_| anyhow::anyhow!("native MCP output forwarding thread panicked"))?;
    to_child.and(from_child)
}

fn authenticate<R: BufRead>(reader: &mut R, expected: &str, max: usize) -> Result<()> {
    let line =
        read_bounded_line(reader, max)?.context("MCP client disconnected before authentication")?;
    let authentication: Authentication =
        serde_json::from_slice(&line).context("invalid MCP authentication message")?;
    if authentication.protocol != AUTH_PROTOCOL || !tokens_equal(&authentication.token, expected) {
        bail!("MCP client authentication failed");
    }
    Ok(())
}

fn tokens_equal(left: &str, right: &str) -> bool {
    let left = left.as_bytes();
    let right = right.as_bytes();
    let mut difference = left.len() ^ right.len();
    let length = left.len().max(right.len());
    for index in 0..length {
        difference |= usize::from(*left.get(index).unwrap_or(&0) ^ *right.get(index).unwrap_or(&0));
    }
    difference == 0
}

fn read_json_line<R: BufRead>(reader: &mut R, max: usize) -> Result<Option<Vec<u8>>> {
    let Some(line) = read_bounded_line(reader, max)? else {
        return Ok(None);
    };
    let value: Value = serde_json::from_slice(&line).context("invalid JSON-RPC message")?;
    if !value.is_object() {
        bail!("JSON-RPC message must be an object");
    }
    Ok(Some(line))
}

fn read_bounded_line<R: BufRead>(reader: &mut R, max: usize) -> Result<Option<Vec<u8>>> {
    if max == 0 {
        bail!("maximum MCP message size must be greater than zero");
    }
    let mut line = Vec::new();
    let read = reader
        .take((max + 1) as u64)
        .read_until(b'\n', &mut line)
        .context("could not read MCP message")?;
    if read == 0 {
        return Ok(None);
    }
    if line.len() > max {
        bail!("MCP message exceeds the configured size limit");
    }
    if !line.ends_with(b"\n") {
        bail!("MCP transport requires newline-delimited messages");
    }
    Ok(Some(line))
}

trait TryCloneStream: Sized {
    fn try_clone_stream(&self) -> io::Result<Self>;
}

impl TryCloneStream for TcpStream {
    fn try_clone_stream(&self) -> io::Result<Self> {
        self.try_clone()
    }
}

#[cfg(unix)]
impl TryCloneStream for UnixStream {
    fn try_clone_stream(&self) -> io::Result<Self> {
        self.try_clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_compared_without_early_exit() {
        assert!(tokens_equal("secret", "secret"));
        assert!(!tokens_equal("secret", "secreu"));
        assert!(!tokens_equal("secret", "secret-long"));
    }

    #[test]
    fn bounded_reader_rejects_oversized_messages() {
        let mut reader = BufReader::new(&b"12345\n"[..]);
        let error = read_bounded_line(&mut reader, 5).unwrap_err();
        assert!(error.to_string().contains("size limit"));
    }

    #[test]
    fn json_reader_requires_an_object() {
        let mut reader = BufReader::new(&b"[]\n"[..]);
        let error = read_json_line(&mut reader, 128).unwrap_err();
        assert!(error.to_string().contains("must be an object"));
    }

    #[test]
    fn authentication_rejects_unknown_fields() {
        let input = br#"{"protocol":"wineforge-mcp-forward/1","token":"x","extra":true}\n"#;
        let mut reader = BufReader::new(&input[..]);
        assert!(authenticate(&mut reader, "x", 1024).is_err());
    }

    #[test]
    fn tcp_rejects_non_loopback_binding() {
        let server = NativeServer {
            executable: PathBuf::from("/not/started"),
            arguments: vec![],
            working_directory: None,
            removed_environment: vec![],
        };
        let error = serve_tcp_once(
            SocketAddr::new(std::net::IpAddr::from([0, 0, 0, 0]), 0),
            "secret",
            &server,
            &BrokerLimits::default(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("loopback"));
    }
}
