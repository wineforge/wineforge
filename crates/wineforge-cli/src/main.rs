use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io;
use std::net::SocketAddr;
use std::path::{Component, Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use flate2::read::GzDecoder;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use wineforge_core::{
    ApplicationProfile, Capability, CapabilityInventory, CapabilityProvider, CapabilityRequirement,
    CapabilitySet, CurrentMapping, EngineDistribution, EngineManifest, IsolationMode,
    MappingAccess, MappingAction, Platform, Translation, Validate, apply_mapping_plan,
    inspect_prefix, plan_mappings,
};

mod chocolatey;
mod download;
mod mcp_broker;
mod mcp_registry;
mod native_package;
mod prepare;
mod recipe;
mod recipe_executor;
mod sandbox;

#[derive(Debug, Parser)]
#[command(
    name = "wineforge",
    version,
    about = "A fail-closed Wine profile manager"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Configure a profile, ensure its engine, and optionally install a native package.
    Prepare {
        recipe: PathBuf,
        /// New TOML profile to create. Existing files are never overwritten.
        #[arg(long)]
        profile_out: PathBuf,
        #[arg(long)]
        id: Option<String>,
        #[arg(long)]
        name: Option<String>,
        /// Prefix used by stateless runs. Native packages replace it with a package-relative path.
        #[arg(long)]
        prefix: Option<PathBuf>,
        /// Host mapping as DRIVE=read-only|read-write=/absolute/path. May be repeated.
        #[arg(long = "mapping")]
        mappings: Vec<String>,
        #[arg(long)]
        engine_store: Option<PathBuf>,
        /// Trusted wineforge-engines checkout used only when a compatible engine is absent.
        #[arg(long)]
        engine_builder: Option<PathBuf>,
        /// Pin one builder version instead of selecting the newest compatible version.
        #[arg(long)]
        engine_version: Option<String>,
        /// Permit a missing engine to start a potentially long local build without prompting.
        #[arg(long)]
        build_if_missing: bool,
        /// Allow the trusted engine builder to install missing host build dependencies.
        #[arg(long, requires = "engine_builder")]
        setup_engine_dependencies: bool,
        #[arg(long, value_enum, default_value = "auto")]
        build_runtime: prepare::BuildRuntime,
        /// How installed native packages should obtain their engine.
        #[arg(long, value_enum, default_value = "auto")]
        engine_distribution: prepare::EngineDistributionArg,
        /// Retain the archive and work tree after the verified engine is installed.
        #[arg(long)]
        keep_build_artifacts: bool,
        /// Use supplied values and safe defaults without reading prompts from the terminal.
        #[arg(long)]
        non_interactive: bool,
        /// After preparation, install an app or deb package using the generated profile.
        #[arg(long, value_enum)]
        then_install: Option<native_package::NativeFormat>,
        #[arg(long, requires = "then_install")]
        destination: Option<PathBuf>,
        #[arg(long, requires = "then_install")]
        cache: Option<PathBuf>,
        #[arg(long, default_value = "winetricks", requires = "then_install")]
        winetricks_command: PathBuf,
        #[arg(long, requires = "then_install")]
        wineforge_binary: Option<PathBuf>,
        #[arg(long, requires = "then_install")]
        launcher_binary: Option<PathBuf>,
        /// Confirm acceptance after reviewing the recipe's required license notice.
        #[arg(long)]
        accept_license: bool,
    },
    /// Create and manage isolated application instances.
    App {
        #[command(subcommand)]
        command: AppCommand,
    },
    /// Manage locally installed and built engines.
    Engine {
        #[command(subcommand)]
        command: EngineCommand,
    },
    /// Validate, inspect, and install declarative application recipes.
    Recipe {
        #[command(subcommand)]
        command: RecipeCommand,
    },
    /// Run explicitly authorized, application-scoped MCP forwarding.
    Mcp {
        #[command(subcommand)]
        command: McpCommand,
    },
    /// Install a recipe as a registry-free native operating-system package.
    Install {
        recipe: PathBuf,
        #[arg(long)]
        profile: PathBuf,
        #[arg(long)]
        engine_manifest: PathBuf,
        #[arg(long)]
        engine_root: PathBuf,
        /// Native package format. Defaults to app on macOS and deb on Linux.
        #[arg(long, value_enum)]
        format: Option<native_package::NativeFormat>,
        /// Final .app directory or .deb file.
        #[arg(long)]
        destination: Option<PathBuf>,
        #[arg(long)]
        cache: Option<PathBuf>,
        #[arg(long, default_value = "winetricks")]
        winetricks_command: PathBuf,
        /// Wineforge executable embedded in the package. Defaults to this executable.
        #[arg(long)]
        wineforge_binary: Option<PathBuf>,
        /// Generic native launcher embedded in the package. Defaults to a sibling binary.
        #[arg(long)]
        launcher_binary: Option<PathBuf>,
        /// Confirm acceptance after reviewing the recipe's required license notice.
        #[arg(long)]
        accept_license: bool,
    },
    /// Validate a declarative application profile without changing anything.
    ValidateProfile { profile: PathBuf },
    /// Validate an engine manifest without downloading or executing it.
    ValidateEngine { manifest: PathBuf },
    /// Report every symlink in a Wine prefix and flag host exposure.
    Inspect { prefix: PathBuf },
    /// Verify and unpack a content-addressed engine archive.
    InstallEngine {
        archive: PathBuf,
        manifest: PathBuf,
        destination: PathBuf,
    },
    /// Print the mapping changes needed to reach a profile's desired state.
    Plan { profile: PathBuf },
    /// Apply a reviewed mapping plan, with backups and post-apply verification.
    Apply {
        profile: PathBuf,
        /// Confirm that the printed plan has been reviewed.
        #[arg(long)]
        yes: bool,
    },
    /// Verify that effective drive mappings match the profile exactly.
    Verify { profile: PathBuf },
    /// Launch Wine directly after validating the profile, engine, and mappings.
    Run {
        profile: PathBuf,
        #[arg(long)]
        engine_manifest: PathBuf,
        #[arg(long)]
        engine_root: PathBuf,
        /// Apply mapping changes before launch. Without this flag, drift fails closed.
        #[arg(long)]
        apply: bool,
        /// Trusted machine-local registry for symbolic MCP server bindings.
        #[arg(long)]
        mcp_registry: Option<PathBuf>,
    },
}

#[derive(Debug, Subcommand)]
enum McpCommand {
    /// Forward one authenticated Unix-socket connection to a native stdio server.
    #[cfg(unix)]
    ForwardUnix {
        #[arg(long)]
        socket: PathBuf,
        /// Environment variable containing the per-launch authentication token.
        #[arg(long, default_value = "WINEFORGE_MCP_TOKEN")]
        token_env: String,
        /// Absolute path to the native MCP server. No shell is used.
        #[arg(long)]
        executable: PathBuf,
        #[arg(long = "server-arg", allow_hyphen_values = true)]
        server_arguments: Vec<String>,
        #[arg(long)]
        working_directory: Option<PathBuf>,
        #[arg(long, default_value_t = 1_048_576)]
        max_message_bytes: usize,
        #[arg(long, default_value_t = 300)]
        idle_timeout_seconds: u64,
    },
    /// Forward one authenticated loopback TCP connection to a native stdio server.
    ForwardTcp {
        #[arg(long)]
        listen: SocketAddr,
        /// Environment variable containing the per-launch authentication token.
        #[arg(long, default_value = "WINEFORGE_MCP_TOKEN")]
        token_env: String,
        /// Absolute path to the native MCP server. No shell is used.
        #[arg(long)]
        executable: PathBuf,
        #[arg(long = "server-arg", allow_hyphen_values = true)]
        server_arguments: Vec<String>,
        #[arg(long)]
        working_directory: Option<PathBuf>,
        #[arg(long, default_value_t = 1_048_576)]
        max_message_bytes: usize,
        #[arg(long, default_value_t = 300)]
        idle_timeout_seconds: u64,
    },
}

#[derive(Debug, Subcommand)]
enum AppCommand {
    /// Create a fresh prefix and sanitize host integration.
    Create {
        profile: PathBuf,
        #[arg(long)]
        engine_manifest: PathBuf,
        #[arg(long)]
        engine_root: PathBuf,
    },
    /// Apply explicitly requested local Winetricks changes to a managed instance.
    Provision {
        profile: PathBuf,
        #[arg(long)]
        engine_manifest: PathBuf,
        #[arg(long)]
        engine_root: PathBuf,
        /// Winetricks verb to install. May be repeated.
        #[arg(long = "winetricks", required = true)]
        winetricks: Vec<String>,
        #[arg(long, default_value = "winetricks")]
        winetricks_command: PathBuf,
    },
    /// Package a cloned existing Wine prefix as a managed macOS application.
    Import {
        /// Existing prefix to clone. The source is never modified.
        source_prefix: PathBuf,
        #[arg(long)]
        recipe: PathBuf,
        #[arg(long)]
        profile: PathBuf,
        #[arg(long)]
        engine_manifest: PathBuf,
        #[arg(long)]
        engine_root: PathBuf,
        /// Final .app directory.
        #[arg(long)]
        destination: PathBuf,
        /// Wineforge executable embedded in the package. Defaults to this executable.
        #[arg(long)]
        wineforge_binary: Option<PathBuf>,
        /// Generic native launcher embedded in the package. Defaults to a sibling binary.
        #[arg(long)]
        launcher_binary: Option<PathBuf>,
    },
    /// Change an installed macOS app between shared and bundled engine storage.
    RelinkEngine {
        /// Existing Wineforge-managed .app bundle.
        application: PathBuf,
        /// Installed engine directory or its wineforge-engine child.
        #[arg(long)]
        engine_root: PathBuf,
        /// Engine storage policy written back to the packaged profile.
        #[arg(long, value_enum, default_value = "auto")]
        distribution: prepare::EngineDistributionArg,
        /// Wineforge executable installed into the application. Defaults to this executable.
        #[arg(long)]
        wineforge_binary: Option<PathBuf>,
        /// Generic native launcher installed into the application. Defaults to a sibling binary.
        #[arg(long)]
        launcher_binary: Option<PathBuf>,
    },
}

#[derive(Debug, Subcommand)]
enum EngineCommand {
    /// Print the typed capabilities advertised by an engine manifest.
    Capabilities { manifest: PathBuf },
    /// Delete managed engine installations from a store.
    Prune {
        /// Directory whose immediate children are managed engine installations.
        #[arg(long)]
        store: PathBuf,
        /// Engine identifier to delete. May be repeated.
        #[arg(long, conflicts_with = "all")]
        id: Vec<String>,
        /// Delete every managed engine in the store.
        #[arg(long)]
        all: bool,
        /// Profiles whose selected engines must be protected from deletion.
        #[arg(long)]
        profile: Vec<PathBuf>,
        /// Apply the printed deletion plan.
        #[arg(long)]
        yes: bool,
    },
    /// Delete managed local build-artifact directories.
    PruneArtifacts {
        /// Directory whose immediate children are managed build-artifact directories.
        #[arg(long)]
        store: PathBuf,
        /// Build identifier to delete. May be repeated.
        #[arg(long, conflicts_with = "all")]
        id: Vec<String>,
        /// Delete every managed artifact directory in the store.
        #[arg(long)]
        all: bool,
        /// Apply the printed deletion plan.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Debug, Subcommand)]
enum RecipeCommand {
    /// Explain layered recipe/profile configuration and verify engine capabilities.
    Explain {
        recipe: PathBuf,
        #[arg(long)]
        profile: PathBuf,
        #[arg(long)]
        engine_manifest: PathBuf,
    },
    /// Validate recipe TOML without downloading or executing anything.
    Validate { recipe: PathBuf },
    /// Print a validated recipe's sources, actions, and postconditions.
    Inspect { recipe: PathBuf },
    /// Inspect the metadata and translatability of a local Chocolatey package.
    InspectNupkg {
        package: PathBuf,
        #[arg(long)]
        package_id: String,
        #[arg(long)]
        package_version: String,
    },
    /// Install a recipe into a fresh, isolated application prefix.
    Install {
        recipe: PathBuf,
        #[arg(long)]
        profile: PathBuf,
        #[arg(long)]
        engine_manifest: PathBuf,
        #[arg(long)]
        engine_root: PathBuf,
        #[arg(long)]
        cache: Option<PathBuf>,
        #[arg(long, default_value = "winetricks")]
        winetricks_command: PathBuf,
        /// Confirm acceptance after reviewing a recipe's required license notice.
        #[arg(long)]
        accept_license: bool,
    },
    /// Delete verified content-addressed source-cache entries.
    PruneCache {
        #[arg(long)]
        cache: Option<PathBuf>,
        /// SHA-256 cache key to delete. May be repeated.
        #[arg(long, conflicts_with = "all")]
        sha256: Vec<String>,
        /// Delete every content-addressed source in the cache.
        #[arg(long)]
        all: bool,
        /// Apply the printed deletion plan.
        #[arg(long)]
        yes: bool,
    },
}

const ENGINE_MARKER: &str = ".wineforge-engine.json";
const BUILD_ARTIFACT_MARKER: &str = ".wineforge-build-artifacts.json";
const APP_INSTANCE_MARKER: &str = "instance.json";

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ManagedEntry {
    schema_version: u32,
    kind: String,
    id: String,
    artifact_sha256: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ManualProvisions {
    schema_version: u32,
    profile_id: String,
    entries: Vec<ManualProvisionEntry>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ManualProvisionEntry {
    engine_id: String,
    winetricks: Vec<String>,
    completed_unix_seconds: u64,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Prepare {
            recipe: recipe_path,
            profile_out,
            id,
            name,
            prefix,
            mappings,
            engine_store,
            engine_builder,
            engine_version,
            build_if_missing,
            setup_engine_dependencies,
            build_runtime,
            engine_distribution,
            keep_build_artifacts,
            non_interactive,
            then_install,
            destination,
            cache,
            winetricks_command,
            wineforge_binary,
            launcher_binary,
            accept_license,
        } => {
            let recipe = recipe::read(&recipe_path)?;
            if then_install.is_some()
                && recipe.license.acceptance == recipe::LicenseAcceptance::Required
                && !accept_license
            {
                bail!(
                    "recipe license acceptance is required before preparation can continue to installation; review license.noticeUrl and pass --accept-license"
                );
            }
            let engine_store = engine_store.map_or_else(prepare::default_engine_store, Ok)?;
            if !engine_store.is_absolute()
                || engine_store
                    .components()
                    .any(|component| matches!(component, Component::ParentDir))
            {
                bail!("engine store must be an absolute path without `..`");
            }
            match fs::symlink_metadata(&engine_store) {
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    fs::create_dir_all(&engine_store).with_context(|| {
                        format!("failed to create engine store {}", engine_store.display())
                    })?;
                }
                Err(error) => return Err(error.into()),
            }
            let prepared = prepare::execute(prepare::PrepareRequest {
                recipe: &recipe,
                profile_out: &profile_out,
                profile_id: id,
                profile_name: name,
                prefix,
                mappings,
                engine_store: &engine_store,
                engine_builder: engine_builder.as_deref(),
                engine_version,
                build_if_missing,
                setup_engine_dependencies,
                build_runtime,
                engine_distribution,
                keep_build_artifacts,
                non_interactive,
            })?;
            resolve_recipe_profile(&recipe, &prepared.profile, &prepared.engine)?;
            if let Some(format) = then_install {
                install_native_package(
                    &recipe,
                    &recipe_path,
                    &prepared.profile,
                    &prepared.engine,
                    &prepared.engine_root,
                    format,
                    destination,
                    cache,
                    &winetricks_command,
                    wineforge_binary,
                    launcher_binary,
                    accept_license,
                )?;
            }
        }
        Command::App { command } => match command {
            AppCommand::Create {
                profile,
                engine_manifest,
                engine_root,
            } => create_app_instance(
                &read_profile(&profile)?,
                &read_engine(&engine_manifest)?,
                &engine_root,
            )?,
            AppCommand::Provision {
                profile,
                engine_manifest,
                engine_root,
                winetricks,
                winetricks_command,
            } => provision_app(
                &read_profile(&profile)?,
                &read_engine(&engine_manifest)?,
                &engine_root,
                &winetricks_command,
                &winetricks,
            )?,
            AppCommand::Import {
                source_prefix,
                recipe,
                profile,
                engine_manifest,
                engine_root,
                destination,
                wineforge_binary,
                launcher_binary,
            } => {
                let recipe_value = recipe::read(&recipe)?;
                let mut profile_value = read_profile(&profile)?;
                let engine = read_engine(&engine_manifest)?;
                profile_value.configuration =
                    resolve_recipe_profile(&recipe_value, &profile_value, &engine)?
                        .into_profile_configuration();
                let current_executable =
                    std::env::current_exe().context("failed to locate the Wineforge executable")?;
                let wineforge_binary =
                    wineforge_binary.unwrap_or_else(|| current_executable.clone());
                let launcher_binary = launcher_binary
                    .unwrap_or_else(|| current_executable.with_file_name("wineforge-launcher"));
                native_package::import_macos_app(&native_package::ImportRequest {
                    source_prefix: &source_prefix,
                    recipe: &recipe_value,
                    recipe_path: &recipe,
                    profile: &profile_value,
                    engine: &engine,
                    engine_root: &engine_root,
                    destination: &destination,
                    wineforge_binary: &wineforge_binary,
                    launcher_binary: &launcher_binary,
                })?;
            }
            AppCommand::RelinkEngine {
                application,
                engine_root,
                distribution,
                wineforge_binary,
                launcher_binary,
            } => {
                let current_executable =
                    std::env::current_exe().context("failed to locate the Wineforge executable")?;
                let wineforge_binary =
                    wineforge_binary.unwrap_or_else(|| current_executable.clone());
                let launcher_binary = launcher_binary
                    .unwrap_or_else(|| current_executable.with_file_name("wineforge-launcher"));
                native_package::relink_macos_app(&native_package::RelinkRequest {
                    application: &application,
                    engine_root: &engine_root,
                    distribution: EngineDistribution::from(distribution),
                    wineforge_binary: &wineforge_binary,
                    launcher_binary: &launcher_binary,
                })?;
            }
        },
        Command::Engine { command } => match command {
            EngineCommand::Capabilities { manifest } => {
                let manifest = read_engine(&manifest)?;
                println!(
                    "{}",
                    toml::to_string_pretty(&capability_inventory(&manifest)?)?
                );
            }
            EngineCommand::Prune {
                store,
                id,
                all,
                profile,
                yes,
            } => prune_engines(&store, &id, all, &profile, yes)?,
            EngineCommand::PruneArtifacts {
                store,
                id,
                all,
                yes,
            } => prune_managed_entries(
                &store,
                BUILD_ARTIFACT_MARKER,
                "build-artifacts",
                "build artifacts",
                &id,
                all,
                &BTreeSet::new(),
                yes,
            )?,
        },
        Command::Recipe { command } => match command {
            RecipeCommand::Explain {
                recipe: path,
                profile,
                engine_manifest,
            } => {
                let recipe = recipe::read(&path)?;
                let profile = read_profile(&profile)?;
                let engine = read_engine(&engine_manifest)?;
                println!(
                    "{}",
                    toml::to_string_pretty(&resolve_recipe_profile(&recipe, &profile, &engine)?)?
                );
            }
            RecipeCommand::Validate { recipe: path } => {
                let recipe = recipe::read(&path)?;
                println!("valid recipe: {} {}", recipe.id, recipe.version);
            }
            RecipeCommand::Inspect { recipe: path } => {
                let recipe = recipe::read(&path)?;
                recipe_executor::inspect(&recipe, &path)?;
            }
            RecipeCommand::InspectNupkg {
                package,
                package_id,
                package_version,
            } => {
                let translation = chocolatey::translate(&package, &package_id, &package_version)?;
                println!(
                    "package\t{}\t{}\ninstaller\t{}\t{}\t{}\narguments\t{:?}\nsuccess-exit-codes\t{:?}",
                    translation.metadata.id,
                    translation.metadata.version,
                    translation.sha256,
                    match translation.installer_type {
                        recipe::InstallerType::Exe => "exe",
                        recipe::InstallerType::Msi => "msi",
                    },
                    translation.url,
                    translation.arguments,
                    translation.success_exit_codes,
                );
            }
            RecipeCommand::Install {
                recipe: path,
                profile,
                engine_manifest,
                engine_root,
                cache,
                winetricks_command,
                accept_license,
            } => {
                let recipe = recipe::read(&path)?;
                let cache = cache.map_or_else(default_source_cache, Ok)?;
                recipe_executor::install(
                    &recipe,
                    &path,
                    &read_profile(&profile)?,
                    &read_engine(&engine_manifest)?,
                    &engine_root,
                    &winetricks_command,
                    &cache,
                    accept_license,
                )?;
            }
            RecipeCommand::PruneCache {
                cache,
                sha256,
                all,
                yes,
            } => prune_source_cache(
                &cache.map_or_else(default_source_cache, Ok)?,
                &sha256,
                all,
                yes,
            )?,
        },
        Command::Mcp { command } => match command {
            #[cfg(unix)]
            McpCommand::ForwardUnix {
                socket,
                token_env,
                executable,
                server_arguments,
                working_directory,
                max_message_bytes,
                idle_timeout_seconds,
            } => {
                let token = mcp_token(&token_env)?;
                let server = mcp_broker::NativeServer {
                    executable,
                    arguments: server_arguments,
                    working_directory,
                    removed_environment: vec![token_env],
                    permissions: vec!["*".into()],
                };
                let limits = mcp_limits(max_message_bytes, idle_timeout_seconds)?;
                mcp_broker::serve_unix_once(&socket, &token, &server, &limits)?;
            }
            McpCommand::ForwardTcp {
                listen,
                token_env,
                executable,
                server_arguments,
                working_directory,
                max_message_bytes,
                idle_timeout_seconds,
            } => {
                if listen.port() == 0 {
                    bail!("--listen must specify a nonzero port");
                }
                let token = mcp_token(&token_env)?;
                let server = mcp_broker::NativeServer {
                    executable,
                    arguments: server_arguments,
                    working_directory,
                    removed_environment: vec![token_env],
                    permissions: vec!["*".into()],
                };
                let limits = mcp_limits(max_message_bytes, idle_timeout_seconds)?;
                mcp_broker::serve_tcp_once(listen, &token, &server, &limits)?;
            }
        },
        Command::Install {
            recipe: recipe_path,
            profile: profile_path,
            engine_manifest,
            engine_root,
            format,
            destination,
            cache,
            winetricks_command,
            wineforge_binary,
            launcher_binary,
            accept_license,
        } => {
            let recipe = recipe::read(&recipe_path)?;
            let profile = read_profile(&profile_path)?;
            let engine = read_engine(&engine_manifest)?;
            let format = format.unwrap_or_else(native_package::host_default_format);
            install_native_package(
                &recipe,
                &recipe_path,
                &profile,
                &engine,
                &engine_root,
                format,
                destination,
                cache,
                &winetricks_command,
                wineforge_binary,
                launcher_binary,
                accept_license,
            )?;
        }
        Command::ValidateProfile { profile } => {
            let profile: ApplicationProfile = read_config(&profile)?;
            profile.validate().context("profile validation failed")?;
            println!("valid profile: {}", profile.id);
        }
        Command::ValidateEngine { manifest } => {
            let manifest: EngineManifest = read_config(&manifest)?;
            manifest
                .validate()
                .context("engine manifest validation failed")?;
            println!("valid engine manifest: {}", manifest.id);
        }
        Command::Inspect { prefix } => print_inspection(&prefix)?,
        Command::InstallEngine {
            archive,
            manifest,
            destination,
        } => install_engine(&archive, &read_engine(&manifest)?, &destination)?,
        Command::Plan { profile } => {
            let profile = read_profile(&profile)?;
            print_plan(&profile)?;
        }
        Command::Apply { profile, yes } => apply_profile(&read_profile(&profile)?, yes)?,
        Command::Verify { profile } => verify_profile(&read_profile(&profile)?)?,
        Command::Run {
            profile,
            engine_manifest,
            engine_root,
            apply,
            mcp_registry,
        } => run_profile(
            &read_profile(&profile)?,
            &read_engine(&engine_manifest)?,
            &engine_root,
            apply,
            mcp_registry.as_deref(),
        )?,
    }
    Ok(())
}

fn mcp_token(variable: &str) -> Result<String> {
    if variable.is_empty()
        || !variable
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
    {
        bail!("MCP token environment variable must use uppercase ASCII, digits, and underscores");
    }
    std::env::var(variable)
        .with_context(|| format!("MCP authentication token is not set in {variable}"))
}

fn mcp_limits(
    max_message_bytes: usize,
    idle_timeout_seconds: u64,
) -> Result<mcp_broker::BrokerLimits> {
    if !(256..=16 * 1024 * 1024).contains(&max_message_bytes) {
        bail!("MCP maximum message size must be between 256 bytes and 16 MiB");
    }
    if !(1..=86_400).contains(&idle_timeout_seconds) {
        bail!("MCP idle timeout must be between 1 second and 24 hours");
    }
    Ok(mcp_broker::BrokerLimits {
        max_message_bytes,
        idle_timeout: Duration::from_secs(idle_timeout_seconds),
    })
}

fn prune_source_cache(cache: &Path, digests: &[String], all: bool, confirmed: bool) -> Result<()> {
    if !all && digests.is_empty() {
        bail!("select at least one --sha256 or pass --all");
    }
    validate_prune_store(cache)?;
    for digest in digests {
        if digest.len() != 64
            || !digest
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            bail!("invalid lowercase SHA-256 cache key: {digest}");
        }
    }
    let requested = digests.iter().map(String::as_str).collect::<BTreeSet<_>>();
    let mut candidates = Vec::new();
    for entry in fs::read_dir(cache)? {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(ToOwned::to_owned) else {
            continue;
        };
        if name.len() != 64
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
            || (!all && !requested.contains(name.as_str()))
        {
            continue;
        }
        let metadata = fs::symlink_metadata(entry.path())?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            bail!("unsafe source-cache entry: {}", entry.path().display());
        }
        candidates.push((name, entry.path()));
    }
    candidates.sort_by(|left, right| left.0.cmp(&right.0));
    if !all {
        let found = candidates
            .iter()
            .map(|candidate| candidate.0.as_str())
            .collect::<BTreeSet<_>>();
        let missing = requested.difference(&found).copied().collect::<Vec<_>>();
        if !missing.is_empty() {
            bail!("no source-cache entry found for: {}", missing.join(", "));
        }
    }
    if candidates.is_empty() {
        println!("nothing to prune: no source-cache entries matched");
        return Ok(());
    }
    for (digest, path) in &candidates {
        println!("prune\tsource-cache\t{digest}\t{}", path.display());
    }
    if !confirmed {
        bail!("refusing deletion without --yes after reviewing the prune plan");
    }
    for (_, path) in candidates {
        fs::remove_file(path)?;
    }
    Ok(())
}

fn capability_inventory(manifest: &EngineManifest) -> Result<CapabilityInventory> {
    let runtime = CapabilitySet(BTreeMap::from([
        ("configuration.layering".into(), Capability { version: 1 }),
        ("mcp.broker".into(), Capability { version: 1 }),
        ("mcp.bridge".into(), Capability { version: 1 }),
    ]));
    CapabilityInventory {
        engine: manifest.capabilities.clone(),
        runtime,
        composed: CapabilitySet::default(),
    }
    .compose(&manifest.composed_capabilities)
    .context("engine composed capabilities cannot be satisfied")
}

fn resolve_recipe_profile(
    recipe: &recipe::Recipe,
    profile: &ApplicationProfile,
    manifest: &EngineManifest,
) -> Result<wineforge_core::EffectiveConfiguration> {
    let effective =
        wineforge_core::resolve_configuration(&recipe.configuration, &profile.configuration)
            .context("recipe/profile configuration cannot be resolved")?;
    let mut requirements = recipe.requirements.capabilities.clone();
    requirements.extend(effective.engine_requirements());
    if !effective.mcp_endpoints.is_empty() {
        requirements.push(CapabilityRequirement {
            name: "mcp.bridge".into(),
            minimum_version: 1,
            provider: CapabilityProvider::Runtime,
        });
        requirements.push(CapabilityRequirement {
            name: "mcp.forwarding".into(),
            minimum_version: 1,
            provider: CapabilityProvider::Composed,
        });
    }
    capability_inventory(manifest)?
        .satisfy(&requirements)
        .context("engine/runtime capability requirements are not satisfied")?;
    Ok(effective)
}

fn default_source_cache() -> Result<PathBuf> {
    #[cfg(target_os = "macos")]
    let relative = Path::new("Library/Caches/wineforge/sources");
    #[cfg(not(target_os = "macos"))]
    let relative = Path::new(".cache/wineforge/sources");
    let home = std::env::var_os("HOME").context("HOME is unavailable; pass --cache explicitly")?;
    Ok(PathBuf::from(home).join(relative))
}

#[allow(clippy::too_many_arguments)]
fn install_native_package(
    recipe: &recipe::Recipe,
    recipe_path: &Path,
    profile: &ApplicationProfile,
    engine: &EngineManifest,
    engine_root: &Path,
    format: native_package::NativeFormat,
    destination: Option<PathBuf>,
    cache: Option<PathBuf>,
    winetricks_command: &Path,
    wineforge_binary: Option<PathBuf>,
    launcher_binary: Option<PathBuf>,
    accept_license: bool,
) -> Result<()> {
    let effective = resolve_recipe_profile(recipe, profile, engine)?;
    let mut materialized_profile = profile.clone();
    materialized_profile.configuration = effective.into_profile_configuration();
    let destination = destination.map_or_else(
        || native_package::default_destination(format, recipe, &materialized_profile),
        Ok,
    )?;
    let current_executable =
        std::env::current_exe().context("failed to locate the Wineforge executable")?;
    let wineforge_binary = wineforge_binary.unwrap_or_else(|| current_executable.clone());
    let launcher_binary =
        launcher_binary.unwrap_or_else(|| current_executable.with_file_name("wineforge-launcher"));
    native_package::install(&native_package::InstallRequest {
        recipe,
        recipe_path,
        profile: &materialized_profile,
        engine,
        engine_root,
        destination: &destination,
        format,
        wineforge_binary: &wineforge_binary,
        launcher_binary: &launcher_binary,
        winetricks_command,
        cache: &cache.map_or_else(default_source_cache, Ok)?,
        accept_license,
    })
}

fn read_profile(path: &Path) -> Result<ApplicationProfile> {
    let profile: ApplicationProfile = read_config(path)?;
    profile.validate().context("profile validation failed")?;
    Ok(profile)
}

fn read_engine(path: &Path) -> Result<EngineManifest> {
    let engine: EngineManifest = read_config(path)?;
    engine
        .validate()
        .context("engine manifest validation failed")?;
    Ok(engine)
}

pub(crate) fn install_engine(
    archive: &Path,
    manifest: &EngineManifest,
    destination: &Path,
) -> Result<()> {
    if destination.exists() {
        bail!("destination already exists: {}", destination.display());
    }
    let actual = sha256_file(archive)?;
    if actual != manifest.artifact.sha256.0 {
        bail!(
            "engine archive digest mismatch: expected {}, got {actual}",
            manifest.artifact.sha256.0
        );
    }
    let parent = destination
        .parent()
        .context("engine destination must have a parent directory")?;
    if !parent.is_dir() {
        bail!(
            "destination parent is not a directory: {}",
            parent.display()
        );
    }
    fs::create_dir(destination)
        .with_context(|| format!("failed to create {}", destination.display()))?;
    if let Err(error) = unpack_engine(archive, destination) {
        let _ = fs::remove_dir_all(destination);
        return Err(error);
    }
    let engine_root = destination.join("wineforge-engine");
    let wine = engine_root.join(&manifest.wine_binary);
    let canonical_root = fs::canonicalize(&engine_root).context("invalid installed engine root")?;
    let canonical_wine = fs::canonicalize(&wine).context("invalid declared Wine executable")?;
    if !canonical_wine.starts_with(&canonical_root) || !canonical_wine.is_file() {
        let _ = fs::remove_dir_all(destination);
        bail!(
            "archive did not contain the declared Wine executable: {}",
            wine.display()
        );
    }
    let marker = ManagedEntry {
        schema_version: 1,
        kind: "engine".into(),
        id: manifest.id.clone(),
        artifact_sha256: Some(manifest.artifact.sha256.0.clone()),
    };
    if let Err(error) = write_engine_manifest(&destination.join("engine.toml"), manifest)
        .and_then(|()| write_json(&destination.join(ENGINE_MARKER), &marker))
    {
        let _ = fs::remove_dir_all(destination);
        return Err(error);
    }
    println!(
        "installed engine {} at {}",
        manifest.id,
        engine_root.display()
    );
    Ok(())
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    fs::write(path, bytes).with_context(|| format!("failed to write {}", path.display()))
}

fn write_engine_manifest(path: &Path, manifest: &EngineManifest) -> Result<()> {
    let text = toml::to_string_pretty(manifest)?;
    fs::write(path, text).with_context(|| format!("failed to write {}", path.display()))
}

fn prune_engines(
    store: &Path,
    ids: &[String],
    all: bool,
    profiles: &[PathBuf],
    confirmed: bool,
) -> Result<()> {
    let mut protected = discover_installed_engine_references(store)?;
    for path in profiles {
        let profile = read_profile(path)?;
        protected.extend(
            profile
                .engines
                .values()
                .map(|selection| selection.id.clone()),
        );
    }
    prune_managed_entries(
        store,
        ENGINE_MARKER,
        "engine",
        "engine",
        ids,
        all,
        &protected,
        confirmed,
    )
}

fn discover_installed_engine_references(store: &Path) -> Result<BTreeSet<String>> {
    let mut protected = BTreeSet::new();
    let Some(application_root) = store.parent() else {
        return Ok(protected);
    };
    let metadata = match fs::symlink_metadata(application_root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(protected),
        Err(error) => return Err(error.into()),
    };
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        bail!(
            "engine store parent must be a real directory: {}",
            application_root.display()
        );
    }
    for entry in fs::read_dir(application_root)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("app") {
            continue;
        }
        let profile_path = path.join("Contents/Resources/profile.toml");
        let profile_metadata = match fs::symlink_metadata(&profile_path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        if !profile_metadata.is_file() || profile_metadata.file_type().is_symlink() {
            bail!(
                "installed profile must be a regular file: {}",
                profile_path.display()
            );
        }
        let profile = read_profile(&profile_path)?;
        protected.extend(
            profile
                .engines
                .values()
                .filter(|selection| {
                    selection.distribution != EngineDistribution::Bundled
                        && selection.root.is_some()
                })
                .map(|selection| selection.id.clone()),
        );
    }
    Ok(protected)
}

#[allow(clippy::too_many_arguments)]
fn prune_managed_entries(
    store: &Path,
    marker_name: &str,
    expected_kind: &str,
    label: &str,
    ids: &[String],
    all: bool,
    protected: &BTreeSet<String>,
    confirmed: bool,
) -> Result<()> {
    if !all && ids.is_empty() {
        bail!("select at least one --id or pass --all");
    }
    validate_prune_store(store)?;
    let requested: BTreeSet<&str> = ids.iter().map(String::as_str).collect();
    let mut candidates = Vec::new();
    for entry in fs::read_dir(store)
        .with_context(|| format!("failed to read prune store {}", store.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            continue;
        }
        let marker_path = path.join(marker_name);
        let marker_metadata = match fs::symlink_metadata(&marker_path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        if marker_metadata.file_type().is_symlink() || !marker_metadata.is_file() {
            bail!("unsafe managed marker: {}", marker_path.display());
        }
        let marker: ManagedEntry = read_json(&marker_path)?;
        if marker.schema_version != 1 || marker.kind != expected_kind || marker.id.is_empty() {
            bail!("invalid managed marker: {}", marker_path.display());
        }
        if !all && !requested.contains(marker.id.as_str()) {
            continue;
        }
        if protected.contains(&marker.id) {
            bail!(
                "refusing to prune {label} {} because a profile or installed package references it",
                marker.id
            );
        }
        candidates.push((marker.id, path));
    }
    candidates.sort_by(|left, right| left.0.cmp(&right.0));
    if !all {
        let found: BTreeSet<&str> = candidates
            .iter()
            .map(|candidate| candidate.0.as_str())
            .collect();
        let missing: Vec<_> = requested.difference(&found).copied().collect();
        if !missing.is_empty() {
            bail!("no managed {label} entry found for: {}", missing.join(", "));
        }
    }
    if candidates.is_empty() {
        println!("nothing to prune: no managed {label} entries matched");
        return Ok(());
    }
    for (id, path) in &candidates {
        println!("prune\t{label}\t{id}\t{}", path.display());
    }
    if !confirmed {
        bail!("refusing deletion without --yes after reviewing the prune plan");
    }
    for (_, path) in candidates {
        fs::remove_dir_all(&path).with_context(|| format!("failed to prune {}", path.display()))?;
    }
    Ok(())
}

fn validate_prune_store(store: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(store)
        .with_context(|| format!("invalid prune store: {}", store.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("prune store must be a real directory: {}", store.display());
    }
    let canonical = fs::canonicalize(store)?;
    if canonical.parent().is_none()
        || canonical.components().count() < 2
        || canonical
            .components()
            .any(|component| matches!(component, Component::ParentDir))
    {
        bail!("refusing unsafe prune store: {}", canonical.display());
    }
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file =
        File::open(path).with_context(|| format!("failed to read {}", path.display()))?;
    let mut hasher = Sha256::new();
    io::copy(&mut file, &mut hasher)
        .with_context(|| format!("failed to hash {}", path.display()))?;
    Ok(format!("{:x}", hasher.finalize()))
}

fn unpack_engine(archive: &Path, destination: &Path) -> Result<()> {
    let file = File::open(archive)?;
    let decoder = GzDecoder::new(file);
    let mut archive = tar::Archive::new(decoder);
    for entry in archive.entries().context("failed to read engine archive")? {
        let mut entry = entry.context("failed to read engine archive entry")?;
        let path = entry
            .path()
            .context("invalid engine archive path")?
            .into_owned();
        if path.is_absolute()
            || path
                .components()
                .any(|component| matches!(component, std::path::Component::ParentDir))
            || path.components().next().is_none_or(|component| {
                component.as_os_str() != std::ffi::OsStr::new("wineforge-engine")
            })
        {
            bail!("unsafe path in engine archive: {}", path.display());
        }
        let entry_type = entry.header().entry_type();
        if entry_type.is_block_special()
            || entry_type.is_character_special()
            || entry_type.is_fifo()
        {
            bail!("special file in engine archive: {}", path.display());
        }
        if entry_type.is_symlink() || entry_type.is_hard_link() {
            let target = entry
                .link_name()
                .context("invalid engine archive link")?
                .context("engine archive link has no target")?;
            let safe_target = if entry_type.is_symlink() {
                relative_symlink_stays_in_engine(&path, &target)
            } else {
                !target.is_absolute()
                    && !target.components().any(|component| {
                        matches!(
                            component,
                            Component::ParentDir | Component::RootDir | Component::Prefix(_)
                        )
                    })
                    && target.components().next().is_some_and(|component| {
                        component.as_os_str() == std::ffi::OsStr::new("wineforge-engine")
                    })
            };
            if !safe_target {
                bail!(
                    "unsafe link target in engine archive: {} -> {}",
                    path.display(),
                    target.display()
                );
            }
        }
        entry
            .unpack_in(destination)
            .context("failed to unpack engine archive")?;
    }
    Ok(())
}

fn relative_symlink_stays_in_engine(path: &Path, target: &Path) -> bool {
    if target.is_absolute() {
        return false;
    }
    let Some(parent) = path.parent() else {
        return false;
    };
    let mut depth = parent.components().count();
    if depth == 0 {
        return false;
    }
    for component in target.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(_) => depth += 1,
            Component::ParentDir if depth > 1 => depth -= 1,
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return false,
        }
    }
    true
}

fn mapping_plan(profile: &ApplicationProfile) -> Result<wineforge_core::MappingPlan> {
    let mut current = read_drive_mappings(&profile.prefix)?;
    for observed in &mut current {
        if let Some(desired) = profile.mappings.iter().find(|mapping| {
            mapping.normalized_drive() == Some(observed.drive)
                && mapping.host_path == observed.host_path
        }) {
            // A symlink records the target, not its access mode. The platform
            // sandbox enforces the profile's mode at every launch, so an
            // access-only profile change requires no prefix mutation.
            observed.access = desired.access;
        }
    }
    Ok(plan_mappings(profile, &current))
}

fn print_plan(profile: &ApplicationProfile) -> Result<()> {
    let plan = mapping_plan(profile)?;
    let unexpected = unexpected_host_exposures(profile)?;
    if plan.actions.is_empty() && unexpected.is_empty() {
        println!("unchanged: mappings already match the profile");
    } else {
        for action in plan.actions {
            print_action(action);
        }
        for exposure in unexpected {
            println!("remove\thost integration {exposure}");
        }
    }
    Ok(())
}

fn apply_profile(profile: &ApplicationProfile, confirmed: bool) -> Result<()> {
    let plan = mapping_plan(profile)?;
    let unexpected = unexpected_host_exposures(profile)?;
    if plan.actions.is_empty() && unexpected.is_empty() {
        println!("unchanged: mappings already match the profile");
        return Ok(());
    }
    for action in &plan.actions {
        print_action(action.clone());
    }
    for exposure in &unexpected {
        println!("remove\thost integration {exposure}");
    }
    if !confirmed {
        bail!("refusing mutation without --yes after reviewing the plan");
    }
    if !plan.actions.is_empty() {
        let receipt = apply_mapping_plan(&profile.prefix, &plan)?;
        println!(
            "changed: drives {:?}; backup: {}",
            receipt.changed_drives,
            receipt.backup_directory.display()
        );
    }
    sanitize_profile(profile)?;
    verify_host_exposure(profile)?;
    if plan.actions.is_empty() {
        println!("changed: removed undeclared host integrations");
    }
    Ok(())
}

fn verify_profile(profile: &ApplicationProfile) -> Result<()> {
    let plan = mapping_plan(profile)?;
    if !plan.actions.is_empty() {
        for action in plan.actions {
            print_action(action);
        }
        bail!("effective mappings differ from the profile");
    }
    println!("verified: effective mappings match {}", profile.id);
    Ok(())
}

fn validate_engine_selection(
    profile: &ApplicationProfile,
    engine: &EngineManifest,
) -> Result<Platform> {
    let platform = current_platform()?;
    if engine.platform != platform {
        bail!("engine platform does not match this host");
    }
    let selected = profile
        .engines
        .get(&platform)
        .context("profile has no engine for this platform")?;
    if selected.id != engine.id {
        bail!(
            "profile selects engine {}, but manifest describes {}",
            selected.id,
            engine.id
        );
    }
    if engine.translation == Translation::Rosetta2 && !rosetta_available() {
        bail!("this x86-64 engine requires Rosetta 2, which was not detected");
    }
    Ok(platform)
}

pub(crate) fn engine_wine(engine: &EngineManifest, engine_root: &Path) -> Result<PathBuf> {
    let resolved_root = resolved_engine_root(engine, engine_root)?;
    let canonical_root = fs::canonicalize(&resolved_root)
        .with_context(|| format!("invalid engine root: {}", engine_root.display()))?;
    let wine = resolved_root.join(&engine.wine_binary);
    let canonical_wine = fs::canonicalize(&wine)
        .with_context(|| format!("Wine executable does not exist: {}", wine.display()))?;
    if !canonical_wine.starts_with(&canonical_root) || !canonical_wine.is_file() {
        bail!("declared Wine executable escapes the engine root");
    }
    Ok(canonical_wine)
}

pub(crate) fn shutdown_wineserver(
    profile: &ApplicationProfile,
    engine: &EngineManifest,
    engine_root: &Path,
) -> Result<()> {
    let wine = engine_wine(engine, engine_root)?;
    let wineserver = wine
        .parent()
        .context("Wine executable has no parent directory")?
        .join("wineserver");
    let metadata = fs::symlink_metadata(&wineserver).with_context(|| {
        format!(
            "selected engine has no wineserver: {}",
            wineserver.display()
        )
    })?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        bail!("selected wineserver is not a regular file");
    }
    for argument in ["-k", "-w"] {
        let mut command = sandbox::command(profile, engine_root, &wineserver)?;
        command.arg(argument).current_dir(&profile.prefix);
        add_wine_environment(&mut command, profile, engine);
        let status = command
            .status()
            .with_context(|| format!("failed to run wineserver {argument}"))?;
        // Wine and CrossOver return 1 when `-k` finds no running server. That
        // is already the desired postcondition, so continue to the `-w`
        // barrier instead of failing a completed installation.
        if !wineserver_shutdown_status_ok(argument, &status) {
            bail!("wineserver {argument} exited with {status}");
        }
    }
    Ok(())
}

fn wineserver_shutdown_status_ok(argument: &str, status: &std::process::ExitStatus) -> bool {
    status.success() || (argument == "-k" && status.code() == Some(1))
}

pub(crate) fn resolved_engine_root(engine: &EngineManifest, engine_root: &Path) -> Result<PathBuf> {
    let direct_wine = engine_root.join(&engine.wine_binary);
    let resolved_root = if direct_wine.is_file() {
        engine_root.to_path_buf()
    } else {
        let installed_root = engine_root.join("wineforge-engine");
        if installed_root.join(&engine.wine_binary).is_file() {
            installed_root
        } else {
            engine_root.to_path_buf()
        }
    };
    let canonical_root = fs::canonicalize(&resolved_root)
        .with_context(|| format!("invalid engine root: {}", engine_root.display()))?;
    if !canonical_root.is_dir() {
        bail!(
            "engine root is not a directory: {}",
            canonical_root.display()
        );
    }
    Ok(canonical_root)
}

fn add_wine_environment(
    command: &mut ProcessCommand,
    profile: &ApplicationProfile,
    engine: &EngineManifest,
) {
    command.env("WINEPREFIX", &profile.prefix);
    if profile.isolation.mode == IsolationMode::Required {
        let state = profile.prefix.join(".wineforge");
        command
            .env("HOME", state.join("home"))
            .env("TMPDIR", state.join("tmp"));
    }
    for (key, value) in &engine.environment.0 {
        command.env(key, value);
    }
    for (key, value) in &profile.environment.0 {
        let prefix = profile.prefix.to_string_lossy();
        command.env(key, value.replace("${WINEFORGE_PREFIX}", &prefix));
    }
}

pub(crate) fn adopt_imported_instance(
    profile: &ApplicationProfile,
    engine: &EngineManifest,
    engine_root: &Path,
) -> Result<()> {
    validate_engine_selection(profile, engine)?;
    sanitize_prefix(&profile.prefix)?;
    prepare_private_runtime(profile)?;
    apply_profile(profile, true)?;
    verify_host_exposure(profile)?;

    let wine = engine_wine(engine, engine_root)?;
    let mut wineboot = sandbox::command(profile, engine_root, &wine)?;
    wineboot.args(["wineboot", "--update"]);
    wineboot.current_dir(&profile.prefix);
    add_wine_environment(&mut wineboot, profile, engine);
    let status = wineboot
        .status()
        .context("failed to update imported Wine prefix")?;
    if !status.success() {
        bail!("imported Wine prefix update exited with {status}");
    }

    sanitize_prefix(&profile.prefix)?;
    apply_profile(profile, true)?;
    verify_host_exposure(profile)?;
    write_json(
        &profile.prefix.join(".wineforge").join(APP_INSTANCE_MARKER),
        &ManagedEntry {
            schema_version: 1,
            kind: "app-instance".into(),
            id: profile.id.clone(),
            artifact_sha256: None,
        },
    )?;
    shutdown_wineserver(profile, engine, engine_root)
}

fn prepare_private_runtime(profile: &ApplicationProfile) -> Result<()> {
    if profile.isolation.mode == IsolationMode::Disabled {
        return Ok(());
    }
    let state = profile.prefix.join(".wineforge");
    fs::create_dir_all(state.join("home"))
        .with_context(|| format!("failed to create private HOME beneath {}", state.display()))?;
    fs::create_dir_all(state.join("tmp")).with_context(|| {
        format!(
            "failed to create private TMPDIR beneath {}",
            state.display()
        )
    })?;
    Ok(())
}

fn add_winetricks_engine_environment(command: &mut ProcessCommand, wine: &Path) -> Result<()> {
    let engine_bin = wine
        .parent()
        .context("Wine executable must have a parent directory")?;
    command.env("WINE", wine).env("WINE64", wine);
    let wineserver = engine_bin.join("wineserver");
    if wineserver.is_file() {
        command.env("WINESERVER", wineserver);
    }
    let mut paths = vec![engine_bin.to_path_buf()];
    if let Some(existing) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&existing));
    }
    command.env(
        "PATH",
        std::env::join_paths(paths).context("failed to construct Winetricks PATH")?,
    );
    Ok(())
}

fn create_app_instance(
    profile: &ApplicationProfile,
    engine: &EngineManifest,
    engine_root: &Path,
) -> Result<()> {
    validate_engine_selection(profile, engine)?;
    let wine = engine_wine(engine, engine_root)?;
    if fs::symlink_metadata(&profile.prefix).is_ok() {
        bail!(
            "app instance already exists; refusing overwrite: {}",
            profile.prefix.display()
        );
    }
    let parent = profile
        .prefix
        .parent()
        .context("prefix must have a parent directory")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("failed to create instance parent: {}", parent.display()))?;
    let parent_metadata = fs::symlink_metadata(parent)
        .with_context(|| format!("invalid prefix parent: {}", parent.display()))?;
    if !parent_metadata.is_dir() || parent_metadata.file_type().is_symlink() {
        bail!(
            "prefix parent must be a real directory: {}",
            parent.display()
        );
    }

    fs::create_dir(&profile.prefix)
        .with_context(|| format!("failed to create prefix: {}", profile.prefix.display()))?;

    let result = (|| -> Result<()> {
        prepare_private_runtime(profile)?;
        let mut wineboot = sandbox::command(profile, engine_root, &wine)?;
        wineboot.args(["wineboot", "--init"]);
        wineboot.current_dir(&profile.prefix);
        add_wine_environment(&mut wineboot, profile, engine);
        let status = wineboot
            .status()
            .context("failed to start Wine prefix initialization")?;
        if !status.success() {
            bail!("Wine prefix initialization exited with {status}");
        }
        sanitize_prefix(&profile.prefix)?;

        apply_profile(profile, true)?;
        verify_host_exposure(profile)?;
        write_json(
            &profile.prefix.join(".wineforge").join(APP_INSTANCE_MARKER),
            &ManagedEntry {
                schema_version: 1,
                kind: "app-instance".into(),
                id: profile.id.clone(),
                artifact_sha256: None,
            },
        )?;
        println!("created managed app instance {}", profile.id);
        Ok(())
    })();

    if result.is_err()
        && fs::symlink_metadata(&profile.prefix)
            .is_ok_and(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
    {
        fs::remove_dir_all(&profile.prefix).with_context(|| {
            format!(
                "creation failed and the incomplete prefix could not be removed: {}",
                profile.prefix.display()
            )
        })?;
    }
    result
}

fn validate_winetricks_verbs(verbs: &[String]) -> Result<()> {
    let mut unique = BTreeSet::new();
    for verb in verbs {
        if !recipe::is_safe_winetricks_verb(verb) {
            bail!("invalid Winetricks verb {verb:?}; options and commands are not accepted");
        }
        if !unique.insert(verb) {
            bail!("duplicate Winetricks verb {verb}");
        }
    }
    Ok(())
}

fn provision_app(
    profile: &ApplicationProfile,
    engine: &EngineManifest,
    engine_root: &Path,
    winetricks_command: &Path,
    verbs: &[String],
) -> Result<()> {
    validate_winetricks_verbs(verbs)?;
    validate_engine_selection(profile, engine)?;
    verify_managed_instance(profile)?;
    verify_profile(profile).context("provisioning failed closed because mapping state drifted")?;
    verify_host_exposure(profile)?;
    prepare_private_runtime(profile)?;
    let wine = engine_wine(engine, engine_root)?;
    let mut command = sandbox::command(profile, engine_root, winetricks_command)?;
    command.arg("-q").args(verbs);
    command.current_dir(profile.prefix.join("drive_c"));
    add_winetricks_engine_environment(&mut command, &wine)?;
    add_wine_environment(&mut command, profile, engine);
    let status = command.status().with_context(|| {
        format!(
            "failed to start Winetricks executable {}",
            winetricks_command.display()
        )
    })?;
    // Wine and Winetricks may recreate host-facing convenience links even when
    // provisioning fails, so sanitization and exposure verification are unconditional.
    sanitize_profile(profile)?;
    verify_host_exposure(profile)?;
    if !status.success() {
        bail!("Winetricks exited with {status}");
    }

    let receipt_path = profile
        .prefix
        .join(".wineforge")
        .join("manual-provisions.json");
    let mut receipt = if receipt_path.exists() {
        let metadata = fs::symlink_metadata(&receipt_path)?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            bail!(
                "unsafe manual-provision receipt: {}",
                receipt_path.display()
            );
        }
        let receipt: ManualProvisions = read_json(&receipt_path)?;
        if receipt.schema_version != 1 || receipt.profile_id != profile.id {
            bail!(
                "manual-provision receipt does not match profile {}",
                profile.id
            );
        }
        receipt
    } else {
        ManualProvisions {
            schema_version: 1,
            profile_id: profile.id.clone(),
            entries: Vec::new(),
        }
    };
    receipt.entries.push(ManualProvisionEntry {
        engine_id: engine.id.clone(),
        winetricks: verbs.to_vec(),
        completed_unix_seconds: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("system clock is before the Unix epoch")?
            .as_secs(),
    });
    write_json(&receipt_path, &receipt)?;
    println!(
        "manually provisioned {} with Winetricks verbs: {}",
        profile.id,
        verbs.join(", ")
    );
    Ok(())
}

fn sanitize_prefix(prefix: &Path) -> Result<()> {
    sanitize_prefix_with_allowed_mappings(prefix, &BTreeMap::new())
}

pub(crate) fn sanitize_profile(profile: &ApplicationProfile) -> Result<()> {
    let allowed_mappings = profile
        .mappings
        .iter()
        .filter_map(|mapping| {
            mapping
                .normalized_drive()
                .map(|drive| (drive, &mapping.host_path))
        })
        .map(|(drive, host_path)| {
            let canonical_host = fs::canonicalize(host_path)
                .with_context(|| format!("invalid mapped host path: {}", host_path.display()))?;
            Ok((
                profile
                    .prefix
                    .join("dosdevices")
                    .join(format!("{}:", drive.to_ascii_lowercase())),
                canonical_host,
            ))
        })
        .collect::<Result<BTreeMap<_, _>>>()?;
    sanitize_prefix_with_allowed_mappings(&profile.prefix, &allowed_mappings)
}

fn sanitize_prefix_with_allowed_mappings(
    prefix: &Path,
    allowed_mappings: &BTreeMap<PathBuf, PathBuf>,
) -> Result<()> {
    let users = prefix.join("drive_c/users");
    let inspection = inspect_prefix(prefix)?;
    for finding in inspection.symlinks.iter().filter(|item| {
        item.escapes_prefix
            && allowed_mappings
                .get(&item.path)
                .is_none_or(|target| target != &item.resolved_target)
    }) {
        fs::remove_file(&finding.path).with_context(|| {
            format!(
                "failed to remove host integration {}",
                finding.path.display()
            )
        })?;
        if finding.path.starts_with(&users) {
            fs::create_dir(&finding.path).with_context(|| {
                format!(
                    "failed to replace host user-folder link with private directory {}",
                    finding.path.display()
                )
            })?;
        }
    }
    let remaining = inspect_prefix(prefix)?
        .symlinks
        .into_iter()
        .filter(|item| {
            item.escapes_prefix
                && allowed_mappings
                    .get(&item.path)
                    .is_none_or(|target| target != &item.resolved_target)
        })
        .map(|item| item.path)
        .collect::<Vec<_>>();
    if !remaining.is_empty() {
        bail!("prefix sanitization left host exposure: {remaining:?}");
    }
    Ok(())
}

fn verify_managed_instance(profile: &ApplicationProfile) -> Result<()> {
    let marker_path = profile.prefix.join(".wineforge").join(APP_INSTANCE_MARKER);
    let metadata = fs::symlink_metadata(&marker_path).with_context(|| {
        format!(
            "prefix is not a managed app instance: {}",
            marker_path.display()
        )
    })?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        bail!("unsafe app-instance marker: {}", marker_path.display());
    }
    let marker: ManagedEntry = read_json(&marker_path)?;
    if marker.schema_version != 1 || marker.kind != "app-instance" || marker.id != profile.id {
        bail!("app-instance marker does not match profile {}", profile.id);
    }
    Ok(())
}

fn verify_host_exposure(profile: &ApplicationProfile) -> Result<()> {
    let unexpected = unexpected_host_exposures(profile)?;
    if !unexpected.is_empty() {
        bail!(
            "launch refused because the prefix exposes undeclared host paths: {}",
            unexpected.join(", ")
        );
    }
    Ok(())
}

fn unexpected_host_exposures(profile: &ApplicationProfile) -> Result<Vec<String>> {
    let allowed = profile
        .mappings
        .iter()
        .filter_map(|mapping| mapping.normalized_drive())
        .map(|drive| {
            profile
                .prefix
                .join("dosdevices")
                .join(format!("{}:", drive.to_ascii_lowercase()))
        })
        .collect::<BTreeSet<_>>();
    Ok(inspect_prefix(&profile.prefix)?
        .symlinks
        .into_iter()
        .filter(|item| item.escapes_prefix && !allowed.contains(&item.path))
        .map(|item| format!("{} -> {}", item.path.display(), item.target.display()))
        .collect::<Vec<_>>())
}

fn run_profile(
    profile: &ApplicationProfile,
    engine: &EngineManifest,
    engine_root: &Path,
    apply: bool,
    mcp_registry_path: Option<&Path>,
) -> Result<()> {
    validate_engine_selection(profile, engine)?;
    if fs::symlink_metadata(&profile.prefix).is_err() {
        create_app_instance(profile, engine, engine_root)?;
    }
    verify_managed_instance(profile)?;
    if apply {
        apply_profile(profile, true)?;
    } else {
        verify_profile(profile).context("launch failed closed because mapping state drifted")?;
    }
    verify_host_exposure(profile)?;
    prepare_private_runtime(profile)?;
    validate_profile_runtime_capabilities(profile, engine)?;
    let wine = engine_wine(engine, engine_root)?;
    let mut mcp = start_profile_mcp(profile, mcp_registry_path)?;
    #[cfg(unix)]
    let runtime_paths = mcp
        .as_ref()
        .map(|runtime| vec![runtime.directory.as_path()])
        .unwrap_or_default();
    #[cfg(not(unix))]
    let runtime_paths = Vec::<&Path>::new();
    let mut command =
        sandbox::command_with_runtime_paths(profile, engine_root, &wine, &runtime_paths)?;
    command.arg(&profile.executable).args(&profile.arguments);
    command.current_dir(profile.prefix.join("drive_c"));
    add_wine_environment(&mut command, profile, engine);
    add_resolved_runtime_environment(&mut command, profile)?;
    #[cfg(unix)]
    if let Some(runtime) = &mcp {
        let bridge = install_windows_mcp_bridge(profile)?;
        command.env("WINEFORGE_MCP_CONFIG", &runtime.windows_config_path);
        command.env(
            "WINEFORGE_MCP_BRIDGE",
            r"C:\.wineforge\bin\wineforge-mcp-bridge.exe",
        );
        debug_assert_eq!(
            bridge,
            profile
                .prefix
                .join("drive_c/.wineforge/bin/wineforge-mcp-bridge.exe")
        );
    }
    let launch_result = command
        .status()
        .with_context(|| format!("failed to launch {}", wine.display()));
    #[cfg(unix)]
    let shutdown_result = match mcp.take() {
        Some(runtime) => runtime.shutdown(),
        None => Ok(()),
    };
    let status = launch_result?;
    #[cfg(unix)]
    shutdown_result?;
    if !status.success() {
        bail!("Wine process exited with {status}");
    }
    Ok(())
}

#[cfg(unix)]
fn install_windows_mcp_bridge(profile: &ApplicationProfile) -> Result<PathBuf> {
    let current = std::env::current_exe().context("could not locate Wineforge executable")?;
    let source = current
        .parent()
        .context("Wineforge executable has no parent directory")?
        .join("wineforge-mcp-bridge.exe");
    let metadata = fs::symlink_metadata(&source).with_context(|| {
        format!(
            "MCP bindings require the Windows bridge beside Wineforge: {}",
            source.display()
        )
    })?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        bail!("Windows MCP bridge must be a regular, non-symlink file");
    }
    let destination = profile
        .prefix
        .join("drive_c/.wineforge/bin/wineforge-mcp-bridge.exe");
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = destination.with_extension(format!("exe.tmp-{}", std::process::id()));
    fs::copy(&source, &temporary).with_context(|| {
        format!(
            "could not install Windows MCP bridge into {}",
            destination.display()
        )
    })?;
    fs::rename(&temporary, &destination).with_context(|| {
        format!(
            "could not commit Windows MCP bridge into {}",
            destination.display()
        )
    })?;
    Ok(destination)
}

fn add_resolved_runtime_environment(
    command: &mut ProcessCommand,
    profile: &ApplicationProfile,
) -> Result<()> {
    let effective = wineforge_core::resolve_configuration(
        &wineforge_core::RecipeConfiguration::default(),
        &profile.configuration,
    )
    .context("self-contained profile configuration cannot be resolved")?;
    if effective.keyboard_preset == Some(wineforge_core::KeyboardPreset::MacNative) {
        command
            .env("WINEFORGE_INPUT_LEFT_COMMAND_IS_CTRL", "true")
            .env("WINEFORGE_INPUT_RIGHT_COMMAND_IS_CTRL", "true")
            .env("WINEFORGE_INPUT_LEFT_OPTION_IS_ALT", "true")
            .env("WINEFORGE_INPUT_RIGHT_OPTION_IS_ALT", "true");
    }
    if let Some(precise) = effective.scrolling_settings.precise {
        command.env(
            "WINEFORGE_INPUT_PRECISE_SCROLLING",
            if precise { "true" } else { "false" },
        );
    }
    if effective.macos_window_isolation == Some(wineforge_core::MacosWindowIsolation::Strict) {
        command.env("WINEFORGE_STRICT_WINDOW_ISOLATION", "true");
    }
    Ok(())
}

#[cfg(unix)]
#[derive(Deserialize, Serialize)]
struct McpRuntimeConfiguration {
    schema_version: u32,
    endpoints: Vec<McpRuntimeEndpoint>,
}

#[cfg(unix)]
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
struct McpRuntimeEndpoint {
    id: String,
    transport: String,
    address: String,
    token: String,
    max_message_bytes: usize,
    idle_timeout_seconds: u64,
}

#[cfg(unix)]
struct ProfileMcpRuntime {
    directory: PathBuf,
    windows_config_path: String,
    brokers: Vec<mcp_broker::RunningTcpBroker>,
}

#[cfg(unix)]
impl Drop for ProfileMcpRuntime {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}

#[cfg(unix)]
impl ProfileMcpRuntime {
    fn shutdown(mut self) -> Result<()> {
        let mut first_error = None;
        while let Some(broker) = self.brokers.pop() {
            if let Err(error) = broker.shutdown() {
                first_error.get_or_insert(error);
            }
        }
        if self.directory.exists() {
            fs::remove_dir_all(&self.directory)?;
        }
        if let Some(error) = first_error {
            return Err(error).context("MCP broker shutdown failed");
        }
        Ok(())
    }
}

#[cfg(unix)]
fn start_profile_mcp(
    profile: &ApplicationProfile,
    registry_path: Option<&Path>,
) -> Result<Option<ProfileMcpRuntime>> {
    start_profile_mcp_at(
        profile,
        registry_path,
        &profile.prefix.join("drive_c/.wineforge-runtime"),
    )
}

#[cfg(unix)]
fn start_profile_mcp_at(
    profile: &ApplicationProfile,
    registry_path: Option<&Path>,
    ipc_root: &Path,
) -> Result<Option<ProfileMcpRuntime>> {
    let nonce = secure_mcp_token()?;
    let directory = ipc_root.join(format!("wf-mcp-{}-{}", std::process::id(), &nonce[..8]));
    start_profile_mcp_in(profile, registry_path, directory)
}

#[cfg(unix)]
fn start_profile_mcp_in(
    profile: &ApplicationProfile,
    registry_path: Option<&Path>,
    directory: PathBuf,
) -> Result<Option<ProfileMcpRuntime>> {
    let bindings = &profile.configuration.mcp.bindings;
    if bindings.is_empty() {
        return Ok(None);
    }
    for binding in bindings {
        let endpoint = profile
            .configuration
            .mcp
            .endpoints
            .iter()
            .find(|endpoint| endpoint.id == binding.endpoint)
            .with_context(|| {
                format!(
                    "MCP binding {} has no materialized endpoint",
                    binding.endpoint
                )
            })?;
        if endpoint.transport != wineforge_core::McpTransport::Stdio {
            bail!(
                "automatic MCP forwarding currently supports only stdio endpoints; {} uses {:?}",
                endpoint.id,
                endpoint.transport
            );
        }
    }
    let registry_path = match registry_path {
        Some(path) => path.to_path_buf(),
        None => mcp_registry::default_path()?,
    };
    let registry = mcp_registry::read(&registry_path)?;
    let resolved = mcp_registry::resolve(&registry, bindings, "WINEFORGE_MCP_TOKEN")?;
    fs::create_dir_all(&directory)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
    }

    let result = (|| -> Result<ProfileMcpRuntime> {
        let mut brokers = Vec::new();
        let mut endpoints = Vec::new();
        for server in resolved {
            let token = secure_mcp_token()?;
            let limits = server.limits.clone();
            let broker =
                mcp_broker::RunningTcpBroker::start(token.clone(), server.native, server.limits)?;
            endpoints.push(McpRuntimeEndpoint {
                id: server.endpoint,
                transport: "tcp-loopback".into(),
                address: broker.address().to_string(),
                token,
                max_message_bytes: limits.max_message_bytes,
                idle_timeout_seconds: limits.idle_timeout.as_secs(),
            });
            brokers.push(broker);
        }
        let config_path = directory.join("runtime.toml");
        fs::write(
            &config_path,
            toml::to_string(&McpRuntimeConfiguration {
                schema_version: 1,
                endpoints,
            })?,
        )?;
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&config_path, fs::Permissions::from_mode(0o600))?;
        let runtime_name = directory
            .file_name()
            .and_then(|name| name.to_str())
            .context("runtime directory name is not valid UTF-8")?;
        Ok(ProfileMcpRuntime {
            directory: directory.clone(),
            windows_config_path: format!(r"C:\.wineforge-runtime\{runtime_name}\runtime.toml"),
            brokers,
        })
    })();
    if result.is_err() && directory.exists() {
        let _ = fs::remove_dir_all(&directory);
    }
    result.map(Some)
}

fn validate_profile_runtime_capabilities(
    profile: &ApplicationProfile,
    engine: &EngineManifest,
) -> Result<()> {
    let effective = wineforge_core::resolve_configuration(
        &wineforge_core::RecipeConfiguration::default(),
        &profile.configuration,
    )
    .context("self-contained profile configuration cannot be resolved")?;
    let mut requirements = effective.engine_requirements();
    if !effective.mcp_endpoints.is_empty() {
        requirements.push(CapabilityRequirement {
            name: "mcp.bridge".into(),
            minimum_version: 1,
            provider: CapabilityProvider::Runtime,
        });
        requirements.push(CapabilityRequirement {
            name: "mcp.forwarding".into(),
            minimum_version: 1,
            provider: CapabilityProvider::Composed,
        });
    }
    capability_inventory(engine)?
        .satisfy(&requirements)
        .context("profile runtime capabilities are not satisfied")
}

#[cfg(not(unix))]
fn start_profile_mcp(
    profile: &ApplicationProfile,
    _registry_path: Option<&Path>,
) -> Result<Option<()>> {
    if !profile.configuration.mcp.bindings.is_empty() {
        bail!("automatic MCP forwarding currently requires a Unix host");
    }
    Ok(None)
}

#[cfg(unix)]
fn secure_mcp_token() -> Result<String> {
    use std::io::Read;
    let mut bytes = [0_u8; 32];
    File::open("/dev/urandom")
        .context("could not open the operating-system random source")?
        .read_exact(&mut bytes)
        .context("could not read the operating-system random source")?;
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut token = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        token.push(char::from(HEX[usize::from(byte >> 4)]));
        token.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    Ok(token)
}

#[cfg(target_os = "macos")]
fn current_platform() -> Result<Platform> {
    Ok(Platform::MacosX86_64)
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn current_platform() -> Result<Platform> {
    Ok(Platform::LinuxX86_64)
}

#[cfg(not(any(target_os = "macos", all(target_os = "linux", target_arch = "x86_64"))))]
fn current_platform() -> Result<Platform> {
    bail!("this host platform is not currently supported")
}

pub(crate) fn rosetta_available() -> bool {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    return Path::new("/Library/Apple/usr/share/rosetta/rosetta").exists();
    #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
    true
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("invalid JSON in {}", path.display()))
}

fn read_config<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("toml") => {
            let text = fs::read_to_string(path)
                .with_context(|| format!("failed to read {} as UTF-8", path.display()))?;
            toml::from_str(&text).with_context(|| format!("invalid TOML in {}", path.display()))
        }
        Some("json") => read_json(path),
        _ => bail!(
            "configuration must use a .toml extension (preferred) or .json for compatibility: {}",
            path.display()
        ),
    }
}

fn print_inspection(prefix: &Path) -> Result<()> {
    let inspection = inspect_prefix(prefix)?;
    if inspection.symlinks.is_empty() {
        println!("no symlinks found");
        return Ok(());
    }
    for finding in inspection.symlinks {
        let exposure = if finding.escapes_prefix {
            "HOST-EXPOSURE"
        } else {
            "internal"
        };
        let existence = if finding.target_exists {
            "exists"
        } else {
            "missing"
        };
        println!(
            "{exposure}\t{existence}\t{} -> {}",
            finding.path.display(),
            finding.target.display()
        );
    }
    Ok(())
}

fn read_drive_mappings(prefix: &Path) -> Result<Vec<CurrentMapping>> {
    let dosdevices = prefix.join("dosdevices");
    if !dosdevices.is_dir() {
        bail!(
            "dosdevices directory does not exist: {}",
            dosdevices.display()
        );
    }
    let mut mappings = Vec::new();
    for entry in fs::read_dir(&dosdevices)
        .with_context(|| format!("failed to read {}", dosdevices.display()))?
    {
        let entry = entry?;
        if !entry.file_type()?.is_symlink() {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let bytes = name.as_bytes();
        if bytes.len() != 2 || bytes[1] != b':' || !bytes[0].is_ascii_alphabetic() {
            continue;
        }
        let target = fs::read_link(entry.path())?;
        let host_path = if target.is_absolute() {
            target
        } else {
            dosdevices.join(target)
        };
        mappings.push(CurrentMapping {
            drive: char::from(bytes[0]).to_ascii_uppercase(),
            host_path,
            access: MappingAccess::ReadWrite,
        });
    }
    Ok(mappings)
}

fn print_action(action: MappingAction) {
    match action {
        MappingAction::Create {
            drive,
            host_path,
            access,
        } => println!("create\t{drive}: -> {}\t{access:?}", host_path.display()),
        MappingAction::Replace {
            drive,
            old_host_path,
            host_path,
            access,
        } => println!(
            "replace\t{drive}: {} -> {}\t{access:?}",
            old_host_path.display(),
            host_path.display()
        ),
        MappingAction::Remove {
            drive,
            old_host_path,
        } => {
            println!("remove\t{drive}: -> {}", old_host_path.display())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::ffi::OsStr;
    use tempfile::tempdir;
    use wineforge_core::{Artifact, ArtifactSource, Environment, License, Sha256Digest};

    fn profile(prefix: &Path) -> ApplicationProfile {
        ApplicationProfile {
            schema_version: 1,
            id: "example-app".into(),
            name: "Example App".into(),
            prefix: prefix.into(),
            executable: r"C:\Program Files\Example\example.exe".into(),
            arguments: Vec::new(),
            engines: std::collections::BTreeMap::from([(
                Platform::MacosX86_64,
                wineforge_core::EngineSelection {
                    id: "example-engine-macos-x86_64".into(),
                    distribution: Default::default(),
                    root: None,
                },
            )]),
            environment: Environment::default(),
            mappings: Vec::new(),
            isolation: wineforge_core::IsolationPolicy::default(),
            configuration: Default::default(),
        }
    }

    fn manifest(digest: String) -> EngineManifest {
        EngineManifest {
            schema_version: 1,
            id: "example-engine-macos-x86_64".into(),
            platform: Platform::MacosX86_64,
            host_architecture: "x86_64".into(),
            translation: Translation::Native,
            artifact: Artifact {
                source: ArtifactSource::UserSupplied,
                sha256: Sha256Digest(digest),
            },
            wine_binary: "bin/wine".into(),
            environment: Environment::default(),
            license: License {
                name: "Example License".into(),
                url: "https://example.invalid/license".parse().unwrap(),
                acceptance_required: false,
            },
            capabilities: Default::default(),
            composed_capabilities: Default::default(),
        }
    }

    #[test]
    fn winetricks_uses_the_selected_engine_tools() {
        let temp = tempdir().unwrap();
        let bin = temp.path().join("bin");
        fs::create_dir(&bin).unwrap();
        let wine = bin.join("wine");
        let wineserver = bin.join("wineserver");
        fs::write(&wine, b"").unwrap();
        fs::write(&wineserver, b"").unwrap();
        let mut command = ProcessCommand::new("winetricks");

        add_winetricks_engine_environment(&mut command, &wine).unwrap();

        let environment = command
            .get_envs()
            .map(|(key, value)| (key.to_owned(), value.unwrap().to_owned()))
            .collect::<std::collections::BTreeMap<_, _>>();
        assert_eq!(
            environment.get(OsStr::new("WINE")),
            Some(&wine.into_os_string())
        );
        assert_eq!(
            environment.get(OsStr::new("WINESERVER")),
            Some(&wineserver.into_os_string())
        );
        assert!(
            std::env::split_paths(environment.get(OsStr::new("PATH")).unwrap())
                .next()
                .is_some_and(|path| path == bin)
        );
    }

    #[test]
    fn resolved_profile_sets_supported_engine_environment() {
        let temp = tempdir().unwrap();
        let mut value = profile(&temp.path().join("prefix"));
        value.configuration.keyboard.preset = Some(wineforge_core::KeyboardPreset::MacNative);
        value.configuration.scrolling.settings.precise = Some(true);
        value.configuration.windowing.macos.isolation =
            Some(wineforge_core::MacosWindowIsolation::Strict);
        let mut command = ProcessCommand::new("ignored");
        add_resolved_runtime_environment(&mut command, &value).unwrap();
        let environment = command
            .get_envs()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.unwrap().to_string_lossy().into_owned(),
                )
            })
            .collect::<BTreeMap<_, _>>();
        assert_eq!(environment["WINEFORGE_INPUT_LEFT_COMMAND_IS_CTRL"], "true");
        assert_eq!(environment["WINEFORGE_INPUT_PRECISE_SCROLLING"], "true");
        assert_eq!(environment["WINEFORGE_STRICT_WINDOW_ISOLATION"], "true");
    }

    #[test]
    fn wineserver_shutdown_accepts_only_the_no_server_kill_status() {
        let success = ProcessCommand::new("sh")
            .args(["-c", "exit 0"])
            .status()
            .unwrap();
        let no_server = ProcessCommand::new("sh")
            .args(["-c", "exit 1"])
            .status()
            .unwrap();
        let other_failure = ProcessCommand::new("sh")
            .args(["-c", "exit 2"])
            .status()
            .unwrap();

        assert!(wineserver_shutdown_status_ok("-k", &success));
        assert!(wineserver_shutdown_status_ok("-k", &no_server));
        assert!(!wineserver_shutdown_status_ok("-w", &no_server));
        assert!(!wineserver_shutdown_status_ok("-k", &other_failure));
    }

    #[test]
    fn manual_winetricks_rejects_options_and_duplicates() {
        assert!(
            validate_winetricks_verbs(&[
                "corefonts".into(),
                "vcrun2022".into(),
                "fontsmooth=rgb".into(),
            ])
            .is_ok()
        );
        assert!(validate_winetricks_verbs(&["--force".into()]).is_err());
        assert!(validate_winetricks_verbs(&["fontsmooth=".into()]).is_err());
        assert!(validate_winetricks_verbs(&["fontsmooth=rgb=extra".into()]).is_err());
        assert!(validate_winetricks_verbs(&["corefonts".into(), "corefonts".into()]).is_err());
        assert!(validate_winetricks_verbs(&["CoreFonts".into()]).is_err());
    }

    #[test]
    fn configuration_prefers_toml_and_keeps_json_compatibility() {
        let temp = tempdir().unwrap();
        let value = profile(&temp.path().join("prefix"));
        let toml_path = temp.path().join("profile.toml");
        fs::write(&toml_path, toml::to_string_pretty(&value).unwrap()).unwrap();
        assert_eq!(
            read_config::<ApplicationProfile>(&toml_path).unwrap(),
            value
        );

        let json_path = temp.path().join("profile.json");
        write_json(&json_path, &value).unwrap();
        assert_eq!(
            read_config::<ApplicationProfile>(&json_path).unwrap(),
            value
        );

        let ambiguous = temp.path().join("profile.conf");
        fs::write(&ambiguous, "schema_version = 1").unwrap();
        assert!(read_config::<ApplicationProfile>(&ambiguous).is_err());
    }

    #[test]
    fn prepare_second_stage_is_explicit_and_typed() {
        let cli = Cli::try_parse_from([
            "wineforge",
            "prepare",
            "recipe.toml",
            "--profile-out",
            "profile.toml",
            "--then-install",
            "app",
            "--destination",
            "/tmp/Example.app",
            "--non-interactive",
        ])
        .unwrap();
        let Command::Prepare {
            then_install,
            destination,
            ..
        } = cli.command
        else {
            panic!("prepare command was not parsed");
        };
        assert_eq!(then_install, Some(native_package::NativeFormat::App));
        assert_eq!(destination, Some(PathBuf::from("/tmp/Example.app")));

        assert!(
            Cli::try_parse_from([
                "wineforge",
                "prepare",
                "recipe.toml",
                "--profile-out",
                "profile.toml",
                "--destination",
                "/tmp/Example.app",
            ])
            .is_err()
        );
    }

    #[test]
    fn prepare_engine_dependency_setup_requires_a_builder() {
        assert!(
            Cli::try_parse_from([
                "wineforge",
                "prepare",
                "recipe.toml",
                "--profile-out",
                "profile.toml",
                "--setup-engine-dependencies",
            ])
            .is_err()
        );

        let cli = Cli::try_parse_from([
            "wineforge",
            "prepare",
            "recipe.toml",
            "--profile-out",
            "profile.toml",
            "--engine-builder",
            "/trusted/wineforge-engines",
            "--setup-engine-dependencies",
        ])
        .unwrap();
        let Command::Prepare {
            setup_engine_dependencies,
            ..
        } = cli.command
        else {
            panic!("prepare command was not parsed");
        };
        assert!(setup_engine_dependencies);
    }

    #[test]
    fn engine_root_accepts_an_install_destination() {
        let temp = tempdir().unwrap();
        let installed_root = temp.path().join("wineforge-engine");
        fs::create_dir(&installed_root).unwrap();
        let wine = installed_root.join("bin/wine");
        fs::create_dir(wine.parent().unwrap()).unwrap();
        fs::write(&wine, b"").unwrap();

        assert_eq!(
            engine_wine(&manifest("0".repeat(64)), temp.path()).unwrap(),
            wine.canonicalize().unwrap()
        );
    }

    #[test]
    fn engine_install_verifies_digest_and_declared_binary() {
        let temp = tempdir().unwrap();
        let archive_path = temp.path().join("engine.tar.gz");
        let file = File::create(&archive_path).unwrap();
        let encoder = GzEncoder::new(file, Compression::default());
        let mut archive = tar::Builder::new(encoder);
        let payload = b"synthetic wine executable";
        let mut header = tar::Header::new_gnu();
        header.set_size(payload.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        archive
            .append_data(&mut header, "wineforge-engine/bin/wine", &payload[..])
            .unwrap();
        archive.into_inner().unwrap().finish().unwrap();

        let digest = sha256_file(&archive_path).unwrap();
        let destination = temp.path().join("installed");
        install_engine(&archive_path, &manifest(digest), &destination).unwrap();
        assert!(destination.join("wineforge-engine/bin/wine").is_file());
        assert_eq!(
            read_config::<EngineManifest>(&destination.join("engine.toml"))
                .unwrap()
                .id,
            "example-engine-macos-x86_64"
        );
        let marker: ManagedEntry = read_json(&destination.join(ENGINE_MARKER)).unwrap();
        assert_eq!(marker.kind, "engine");
        assert_eq!(marker.id, "example-engine-macos-x86_64");
    }

    #[test]
    fn engine_install_fails_before_mutation_on_digest_mismatch() {
        let temp = tempdir().unwrap();
        let archive_path = temp.path().join("engine.tar.gz");
        fs::write(&archive_path, b"not an engine").unwrap();
        let destination = temp.path().join("installed");
        assert!(install_engine(&archive_path, &manifest("0".repeat(64)), &destination).is_err());
        assert!(!destination.exists());
    }

    #[test]
    fn engine_archive_allows_internal_parent_symlink() {
        assert!(relative_symlink_stays_in_engine(
            Path::new("wineforge-engine/Framework.framework/Versions/A/Resources"),
            Path::new("../../../share/resources")
        ));
    }

    #[test]
    fn engine_archive_rejects_escaping_parent_symlink() {
        assert!(!relative_symlink_stays_in_engine(
            Path::new("wineforge-engine/bin/wine"),
            Path::new("../../outside")
        ));
        assert!(!relative_symlink_stays_in_engine(
            Path::new("wineforge-engine/bin/wine"),
            Path::new("/tmp/outside")
        ));
    }

    #[cfg(unix)]
    #[test]
    fn sanitization_replaces_host_user_links_and_removes_root_drive() {
        use std::os::unix::fs::symlink;

        let temp = tempdir().unwrap();
        let prefix = temp.path().join("prefix");
        let user = prefix.join("drive_c/users/example");
        fs::create_dir_all(&user).unwrap();
        fs::create_dir(prefix.join("dosdevices")).unwrap();
        symlink(temp.path(), user.join("Documents")).unwrap();
        symlink("/", prefix.join("dosdevices/z:")).unwrap();

        sanitize_prefix(&prefix).unwrap();

        assert!(user.join("Documents").is_dir());
        assert!(!user.join("Documents").is_symlink());
        assert!(!prefix.join("dosdevices/z:").exists());
        assert!(
            inspect_prefix(&prefix)
                .unwrap()
                .symlinks
                .iter()
                .all(|finding| !finding.escapes_prefix)
        );
    }

    #[cfg(unix)]
    #[test]
    fn profile_sanitization_preserves_only_the_exact_declared_mapping() {
        use std::os::unix::fs::symlink;

        let temp = tempdir().unwrap();
        let prefix = temp.path().join("prefix");
        let allowed = temp.path().join("allowed");
        let other = temp.path().join("other");
        fs::create_dir_all(prefix.join("dosdevices")).unwrap();
        fs::create_dir(&allowed).unwrap();
        fs::create_dir(&other).unwrap();
        let mapping = prefix.join("dosdevices/s:");
        symlink(&allowed, &mapping).unwrap();
        symlink("/", prefix.join("dosdevices/z:")).unwrap();
        let mut value = profile(&prefix);
        value.mappings.push(wineforge_core::HostMapping {
            drive: "S".into(),
            host_path: allowed,
            access: MappingAccess::ReadWrite,
        });

        sanitize_profile(&value).unwrap();

        assert_eq!(
            fs::read_link(&mapping).unwrap(),
            value.mappings[0].host_path
        );
        assert!(!prefix.join("dosdevices/z:").exists());

        fs::remove_file(&mapping).unwrap();
        symlink(other, &mapping).unwrap();
        sanitize_profile(&value).unwrap();
        assert!(!mapping.exists());
    }

    #[cfg(unix)]
    #[test]
    fn launch_audit_allows_only_declared_host_mapping() {
        use std::os::unix::fs::symlink;

        let temp = tempdir().unwrap();
        let prefix = temp.path().join("prefix");
        let allowed = temp.path().join("allowed");
        fs::create_dir_all(prefix.join("dosdevices")).unwrap();
        fs::create_dir(&allowed).unwrap();
        symlink(&allowed, prefix.join("dosdevices/s:")).unwrap();
        let mut value = profile(&prefix);
        value.mappings.push(wineforge_core::HostMapping {
            drive: "S".into(),
            host_path: allowed,
            access: MappingAccess::ReadWrite,
        });
        verify_host_exposure(&value).unwrap();

        symlink("/", prefix.join("dosdevices/z:")).unwrap();
        assert!(verify_host_exposure(&value).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn access_only_mapping_changes_do_not_mutate_the_prefix() {
        use std::os::unix::fs::symlink;

        let temp = tempdir().unwrap();
        let prefix = temp.path().join("prefix");
        let mapped = temp.path().join("mapped");
        fs::create_dir_all(prefix.join("dosdevices")).unwrap();
        fs::create_dir(&mapped).unwrap();
        symlink(&mapped, prefix.join("dosdevices/r:")).unwrap();
        let mut value = profile(&prefix);
        value.mappings.push(wineforge_core::HostMapping {
            drive: "R".into(),
            host_path: mapped,
            access: MappingAccess::ReadOnly,
        });

        assert!(mapping_plan(&value).unwrap().actions.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn apply_removes_undeclared_host_link_when_drive_plan_is_unchanged() {
        use std::os::unix::fs::symlink;

        let temp = tempdir().unwrap();
        let prefix = temp.path().join("prefix");
        let outside = temp.path().join("outside");
        fs::create_dir_all(prefix.join("dosdevices")).unwrap();
        fs::create_dir(&outside).unwrap();
        let device_alias = prefix.join("dosdevices/d::");
        symlink(&outside, &device_alias).unwrap();
        let value = profile(&prefix);

        assert!(mapping_plan(&value).unwrap().actions.is_empty());
        apply_profile(&value, true).unwrap();

        assert!(!device_alias.exists());
        verify_host_exposure(&value).unwrap();
    }

    #[test]
    fn prune_removes_only_selected_managed_engine() {
        let temp = tempdir().unwrap();
        let store = temp.path().join("engines");
        fs::create_dir(&store).unwrap();
        for id in ["engine-one", "engine-two"] {
            let directory = store.join(id);
            fs::create_dir(&directory).unwrap();
            write_json(
                &directory.join(ENGINE_MARKER),
                &ManagedEntry {
                    schema_version: 1,
                    kind: "engine".into(),
                    id: id.into(),
                    artifact_sha256: Some("0".repeat(64)),
                },
            )
            .unwrap();
        }
        let unowned = store.join("unowned");
        fs::create_dir(&unowned).unwrap();

        prune_engines(&store, &["engine-one".into()], false, &[], true).unwrap();

        assert!(!store.join("engine-one").exists());
        assert!(store.join("engine-two").exists());
        assert!(unowned.exists());
    }

    #[test]
    fn prune_plan_requires_confirmation() {
        let temp = tempdir().unwrap();
        let store = temp.path().join("artifacts");
        let directory = store.join("build-one");
        fs::create_dir_all(&directory).unwrap();
        write_json(
            &directory.join(BUILD_ARTIFACT_MARKER),
            &ManagedEntry {
                schema_version: 1,
                kind: "build-artifacts".into(),
                id: "build-one".into(),
                artifact_sha256: None,
            },
        )
        .unwrap();

        let result = prune_managed_entries(
            &store,
            BUILD_ARTIFACT_MARKER,
            "build-artifacts",
            "build artifacts",
            &[],
            true,
            &BTreeSet::new(),
            false,
        );
        assert!(result.is_err());
        assert!(directory.exists());
    }

    #[test]
    fn prune_refuses_protected_engine() {
        let temp = tempdir().unwrap();
        let store = temp.path().join("engines");
        let directory = store.join("engine-one");
        fs::create_dir_all(&directory).unwrap();
        write_json(
            &directory.join(ENGINE_MARKER),
            &ManagedEntry {
                schema_version: 1,
                kind: "engine".into(),
                id: "engine-one".into(),
                artifact_sha256: Some("0".repeat(64)),
            },
        )
        .unwrap();

        let protected = BTreeSet::from(["engine-one".into()]);
        let result = prune_managed_entries(
            &store,
            ENGINE_MARKER,
            "engine",
            "engine",
            &[],
            true,
            &protected,
            true,
        );
        assert!(result.is_err());
        assert!(directory.exists());
    }

    #[test]
    fn prune_discovers_shared_engine_references_in_installed_apps() {
        let temp = tempdir().unwrap();
        let store = temp.path().join("engines");
        let directory = store.join("example-engine-macos-x86_64");
        fs::create_dir_all(&directory).unwrap();
        write_json(
            &directory.join(ENGINE_MARKER),
            &ManagedEntry {
                schema_version: 1,
                kind: "engine".into(),
                id: "example-engine-macos-x86_64".into(),
                artifact_sha256: Some("0".repeat(64)),
            },
        )
        .unwrap();
        let resources = temp.path().join("Example.app/Contents/Resources");
        fs::create_dir_all(&resources).unwrap();
        let mut installed = profile(&temp.path().join("prefix"));
        let selection = installed.engines.get_mut(&Platform::MacosX86_64).unwrap();
        selection.distribution = EngineDistribution::Shared;
        selection.root = Some(directory.join("wineforge-engine"));
        fs::write(
            resources.join("profile.toml"),
            toml::to_string_pretty(&installed).unwrap(),
        )
        .unwrap();

        let result = prune_engines(&store, &[], true, &[], true);
        assert!(result.is_err());
        assert!(directory.exists());
    }

    #[cfg(unix)]
    #[test]
    fn profile_binding_starts_authenticated_broker_and_cleans_up() {
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpStream;
        use std::os::unix::fs::PermissionsExt;

        let temp = tempdir().unwrap();
        let prefix = temp.path().join("prefix");
        fs::create_dir(&prefix).unwrap();
        let registry_path = temp.path().join("mcp-servers.toml");
        let registry = mcp_registry::ServerRegistry {
            schema_version: 1,
            servers: BTreeMap::from([(
                "echo".into(),
                mcp_registry::RegisteredServer {
                    executable: PathBuf::from("/bin/cat"),
                    arguments: vec![],
                    working_directory: None,
                    permissions: vec!["tools.read".into()],
                    max_message_bytes: 4096,
                    idle_timeout_seconds: 5,
                },
            )]),
        };
        fs::write(&registry_path, toml::to_string(&registry).unwrap()).unwrap();
        fs::set_permissions(&registry_path, fs::Permissions::from_mode(0o600)).unwrap();
        let mut value = profile(&prefix);
        value
            .configuration
            .mcp
            .endpoints
            .push(wineforge_core::McpEndpoint {
                id: "app-tools".into(),
                transport: wineforge_core::McpTransport::Stdio,
            });
        value
            .configuration
            .mcp
            .bindings
            .push(wineforge_core::McpBinding {
                endpoint: "app-tools".into(),
                server: "echo".into(),
                permissions: vec!["tools.read".into()],
            });

        let runtime = start_profile_mcp_in(&value, Some(&registry_path), temp.path().join("m"))
            .unwrap()
            .unwrap();
        let config: McpRuntimeConfiguration =
            read_config(&runtime.directory.join("runtime.toml")).unwrap();
        assert_eq!(config.endpoints.len(), 1);
        let endpoint = &config.endpoints[0];
        assert_eq!(endpoint.transport, "tcp-loopback");
        let mut stream = TcpStream::connect(&endpoint.address).unwrap();
        writeln!(
            stream,
            r#"{{"protocol":"wineforge-mcp-forward/1","token":"{}"}}"#,
            endpoint.token
        )
        .unwrap();
        let request = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
        writeln!(stream, "{request}").unwrap();
        stream.flush().unwrap();
        let mut response = String::new();
        BufReader::new(stream.try_clone().unwrap())
            .read_line(&mut response)
            .unwrap();
        if response.is_empty() {
            drop(stream);
            panic!("broker ended before response: {:?}", runtime.shutdown());
        }
        assert_eq!(response.trim_end(), request);
        drop(stream);
        let directory = runtime.directory.clone();
        runtime.shutdown().unwrap();
        assert!(!directory.exists());
    }

    #[test]
    fn source_cache_prune_removes_only_content_addressed_files() {
        let temp = tempdir().unwrap();
        let cache = temp.path().join("sources");
        fs::create_dir(&cache).unwrap();
        let selected = "a".repeat(64);
        let retained = "b".repeat(64);
        fs::write(cache.join(&selected), b"selected").unwrap();
        fs::write(cache.join(&retained), b"retained").unwrap();
        fs::write(cache.join("unmanaged-note"), b"unmanaged").unwrap();

        prune_source_cache(&cache, std::slice::from_ref(&selected), false, true).unwrap();

        assert!(!cache.join(selected).exists());
        assert!(cache.join(retained).exists());
        assert!(cache.join("unmanaged-note").exists());
    }
}
