use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;

#[derive(Parser)]
#[command(about = "Forward MCP stdio to an application-scoped Wineforge broker")]
struct Arguments {
    /// Symbolic endpoint ID declared by the application recipe.
    #[arg(long)]
    endpoint: String,
    /// Runtime configuration. Defaults to WINEFORGE_MCP_CONFIG.
    #[arg(long)]
    config: Option<PathBuf>,
    /// Environment variable containing the runtime configuration path.
    #[arg(long, default_value = "WINEFORGE_MCP_CONFIG")]
    config_env: String,
}

fn main() -> Result<()> {
    let arguments = Arguments::parse();
    let config = match arguments.config {
        Some(path) => path,
        None => std::env::var_os(&arguments.config_env)
            .map(PathBuf::from)
            .with_context(|| format!("{} is not set", arguments.config_env))?,
    };
    wineforge_mcp_bridge::forward_path(
        &config,
        &arguments.endpoint,
        std::io::stdin(),
        std::io::stdout(),
    )
}
