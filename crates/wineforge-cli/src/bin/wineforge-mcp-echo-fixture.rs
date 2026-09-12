//! Test fixture used by MCP forwarding integration tests.

use std::io::{self, BufRead, Write};

fn main() -> io::Result<()> {
    if std::env::var_os("WINEFORGE_MCP_TOKEN").is_some() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "broker token leaked to native child",
        ));
    }
    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line?;
        serde_json::from_str::<serde_json::Value>(&line).map_err(io::Error::other)?;
        writeln!(stdout, "{line}")?;
        stdout.flush()?;
    }
    Ok(())
}
