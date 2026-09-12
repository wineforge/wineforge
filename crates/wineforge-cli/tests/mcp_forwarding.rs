#![cfg(unix)]

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

fn connect_when_ready(path: &std::path::Path) -> UnixStream {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match UnixStream::connect(path) {
            Ok(stream) => return stream,
            Err(error) if Instant::now() < deadline => {
                let _ = error;
                thread::sleep(Duration::from_millis(20));
            }
            Err(error) => panic!("broker did not create its socket: {error}"),
        }
    }
}

#[test]
fn authenticated_connection_forwards_json_rpc_to_native_stdio() {
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("broker.sock");
    let mut broker = Command::new(env!("CARGO_BIN_EXE_wineforge"))
        .args([
            "mcp",
            "forward-unix",
            "--socket",
            socket.to_str().unwrap(),
            "--executable",
            env!("CARGO_BIN_EXE_wineforge-mcp-echo-fixture"),
            "--idle-timeout-seconds",
            "5",
        ])
        .env("WINEFORGE_MCP_TOKEN", "test-application-token")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let mut stream = connect_when_ready(&socket);
    writeln!(
        stream,
        r#"{{"protocol":"wineforge-mcp-forward/1","token":"test-application-token"}}"#
    )
    .unwrap();
    let request = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
    writeln!(stream, "{request}").unwrap();
    stream.flush().unwrap();

    let mut response = String::new();
    BufReader::new(stream.try_clone().unwrap())
        .read_line(&mut response)
        .unwrap();
    assert_eq!(response.trim_end(), request);
    drop(stream);

    let status = broker.wait().unwrap();
    if !status.success() {
        let stderr = String::from_utf8_lossy(
            &broker
                .stderr
                .take()
                .unwrap()
                .bytes()
                .flatten()
                .collect::<Vec<_>>(),
        )
        .into_owned();
        panic!("broker failed: {stderr}");
    }
    assert!(!socket.exists());
}

#[test]
fn failed_authentication_does_not_start_native_server() {
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("broker.sock");
    let marker = directory.path().join("must-not-exist");
    let mut broker = Command::new(env!("CARGO_BIN_EXE_wineforge"))
        .args([
            "mcp",
            "forward-unix",
            "--socket",
            socket.to_str().unwrap(),
            "--executable",
            "/usr/bin/touch",
            "--server-arg",
            marker.to_str().unwrap(),
        ])
        .env("WINEFORGE_MCP_TOKEN", "correct-token")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let mut stream = connect_when_ready(&socket);
    writeln!(
        stream,
        r#"{{"protocol":"wineforge-mcp-forward/1","token":"wrong-token"}}"#
    )
    .unwrap();
    drop(stream);

    assert!(!broker.wait().unwrap().success());
    assert!(!marker.exists());
}
