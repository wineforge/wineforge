use std::fs;
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::path::Path;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

const AUTH_PROTOCOL: &str = "wineforge-mcp-forward/1";
const MAX_CONFIG_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeConfiguration {
    schema_version: u32,
    endpoints: Vec<RuntimeEndpoint>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuntimeEndpoint {
    id: String,
    transport: String,
    address: String,
    token: String,
    max_message_bytes: usize,
    idle_timeout_seconds: u64,
}

pub fn forward_path<R, W>(config_path: &Path, endpoint_id: &str, input: R, output: W) -> Result<()>
where
    R: Read + Send + 'static,
    W: Write,
{
    let metadata = fs::metadata(config_path).with_context(|| {
        format!(
            "could not inspect runtime configuration {}",
            config_path.display()
        )
    })?;
    if !metadata.is_file() || metadata.len() > MAX_CONFIG_BYTES {
        bail!(
            "runtime configuration must be a regular file no larger than {MAX_CONFIG_BYTES} bytes"
        );
    }
    let contents = fs::read_to_string(config_path).with_context(|| {
        format!(
            "could not read runtime configuration {}",
            config_path.display()
        )
    })?;
    let config: RuntimeConfiguration = toml::from_str(&contents)
        .with_context(|| format!("invalid runtime configuration {}", config_path.display()))?;
    forward(config, endpoint_id, input, output)
}

pub fn forward<R, W>(
    config: RuntimeConfiguration,
    endpoint_id: &str,
    input: R,
    output: W,
) -> Result<()>
where
    R: Read + Send + 'static,
    W: Write,
{
    if config.schema_version != 1 {
        bail!(
            "unsupported runtime configuration schema version {}",
            config.schema_version
        );
    }
    let endpoint = config
        .endpoints
        .into_iter()
        .find(|endpoint| endpoint.id == endpoint_id)
        .with_context(|| format!("runtime configuration has no endpoint {endpoint_id:?}"))?;
    if endpoint.transport != "tcp-loopback" {
        bail!(
            "endpoint {} does not use the supported tcp-loopback transport",
            endpoint.id
        );
    }
    let address: SocketAddr = endpoint
        .address
        .parse()
        .context("invalid MCP broker address")?;
    if !address.ip().is_loopback() {
        bail!("MCP bridge refuses a non-loopback broker address");
    }
    if !(256..=16 * 1024 * 1024).contains(&endpoint.max_message_bytes) {
        bail!("invalid MCP message size limit");
    }
    if !(1..=86_400).contains(&endpoint.idle_timeout_seconds) {
        bail!("invalid MCP idle timeout");
    }
    let timeout = Duration::from_secs(endpoint.idle_timeout_seconds);
    let stream = TcpStream::connect_timeout(&address, timeout)
        .with_context(|| format!("could not connect to application MCP broker at {address}"))?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;

    let mut network_writer = BufWriter::new(stream.try_clone()?);
    writeln!(
        network_writer,
        "{{\"protocol\":\"{AUTH_PROTOCOL}\",\"token\":\"{}\"}}",
        endpoint.token
    )?;
    network_writer.flush()?;

    let max_message_bytes = endpoint.max_message_bytes;
    let upload = thread::spawn(move || -> Result<()> {
        let mut reader = BufReader::new(input);
        loop {
            let Some(message) = read_bounded_line(&mut reader, max_message_bytes)? else {
                break;
            };
            network_writer.write_all(&message)?;
            network_writer.flush()?;
        }
        network_writer
            .get_ref()
            .shutdown(std::net::Shutdown::Write)?;
        Ok(())
    });

    let mut network_reader = BufReader::new(stream);
    let mut output = BufWriter::new(output);
    loop {
        let Some(message) = read_bounded_line(&mut network_reader, max_message_bytes)? else {
            break;
        };
        output.write_all(&message)?;
        output.flush()?;
    }
    upload
        .join()
        .map_err(|_| anyhow::anyhow!("MCP stdin forwarding worker panicked"))??;
    Ok(())
}

fn read_bounded_line<R: BufRead>(reader: &mut R, maximum: usize) -> Result<Option<Vec<u8>>> {
    let mut result = Vec::new();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            if result.is_empty() {
                return Ok(None);
            }
            bail!("MCP input ended before a newline");
        }
        let take = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |index| index + 1);
        if result.len().saturating_add(take) > maximum {
            bail!("MCP message exceeds the configured size limit");
        }
        result.extend_from_slice(&available[..take]);
        reader.consume(take);
        if result.last() == Some(&b'\n') {
            return Ok(Some(result));
        }
    }
}

pub fn localhost(port: u16) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), port)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    #[test]
    fn authenticates_and_forwards_bounded_json_lines() {
        let listener = TcpListener::bind(localhost(0)).unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut auth = String::new();
            reader.read_line(&mut auth).unwrap();
            assert!(auth.contains("application-token"));
            let mut request = String::new();
            reader.read_line(&mut request).unwrap();
            let mut writer = BufWriter::new(stream);
            writer.write_all(request.as_bytes()).unwrap();
            writer.flush().unwrap();
        });
        let output = Arc::new(Mutex::new(Vec::new()));
        let sink = SharedWriter(Arc::clone(&output));
        forward(
            RuntimeConfiguration {
                schema_version: 1,
                endpoints: vec![RuntimeEndpoint {
                    id: "tools".into(),
                    transport: "tcp-loopback".into(),
                    address: address.to_string(),
                    token: "application-token".into(),
                    max_message_bytes: 4096,
                    idle_timeout_seconds: 5,
                }],
            },
            "tools",
            io::Cursor::new(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\"}\n".to_vec()),
            sink,
        )
        .unwrap();
        server.join().unwrap();
        assert!(
            String::from_utf8(output.lock().unwrap().clone())
                .unwrap()
                .contains("tools/list")
        );
    }

    struct SharedWriter(Arc<Mutex<Vec<u8>>>);
    impl Write for SharedWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn refuses_non_loopback_and_oversized_messages() {
        let invalid = RuntimeConfiguration {
            schema_version: 1,
            endpoints: vec![RuntimeEndpoint {
                id: "tools".into(),
                transport: "tcp-loopback".into(),
                address: "192.0.2.1:1234".into(),
                token: "x".into(),
                max_message_bytes: 4096,
                idle_timeout_seconds: 5,
            }],
        };
        assert!(
            forward(invalid, "tools", io::empty(), io::sink())
                .unwrap_err()
                .to_string()
                .contains("non-loopback")
        );
        assert!(read_bounded_line(&mut BufReader::new(io::Cursor::new(b"12345\n")), 4).is_err());
    }
}
