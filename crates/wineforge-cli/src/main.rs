use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io;
use std::path::{Component, Path, PathBuf};
use std::process::Command as ProcessCommand;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use flate2::read::GzDecoder;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use wineforge_core::{
    ApplicationProfile, CurrentMapping, EngineManifest, MappingAccess, MappingAction, Platform,
    Translation, Validate, apply_mapping_plan, inspect_prefix, plan_mappings,
};

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
    /// Manage locally installed and built engines.
    Engine {
        #[command(subcommand)]
        command: EngineCommand,
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
    },
}

#[derive(Debug, Subcommand)]
enum EngineCommand {
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

const ENGINE_MARKER: &str = ".wineforge-engine.json";
const BUILD_ARTIFACT_MARKER: &str = ".wineforge-build-artifacts.json";

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ManagedEntry {
    schema_version: u32,
    kind: String,
    id: String,
    artifact_sha256: Option<String>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Engine { command } => match command {
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
        Command::ValidateProfile { profile } => {
            let profile: ApplicationProfile = read_json(&profile)?;
            profile.validate().context("profile validation failed")?;
            println!("valid profile: {}", profile.id);
        }
        Command::ValidateEngine { manifest } => {
            let manifest: EngineManifest = read_json(&manifest)?;
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
        } => run_profile(
            &read_profile(&profile)?,
            &read_engine(&engine_manifest)?,
            &engine_root,
            apply,
        )?,
    }
    Ok(())
}

fn read_profile(path: &Path) -> Result<ApplicationProfile> {
    let profile: ApplicationProfile = read_json(path)?;
    profile.validate().context("profile validation failed")?;
    Ok(profile)
}

fn read_engine(path: &Path) -> Result<EngineManifest> {
    let engine: EngineManifest = read_json(path)?;
    engine
        .validate()
        .context("engine manifest validation failed")?;
    Ok(engine)
}

fn install_engine(archive: &Path, manifest: &EngineManifest, destination: &Path) -> Result<()> {
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
    if let Err(error) = write_json(&destination.join(ENGINE_MARKER), &marker) {
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

fn prune_engines(
    store: &Path,
    ids: &[String],
    all: bool,
    profiles: &[PathBuf],
    confirmed: bool,
) -> Result<()> {
    let mut protected = BTreeSet::new();
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
                "refusing to prune {label} {} because a supplied profile references it",
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
            if target.is_absolute()
                || target
                    .components()
                    .any(|component| matches!(component, std::path::Component::ParentDir))
            {
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

fn mapping_plan(profile: &ApplicationProfile) -> Result<wineforge_core::MappingPlan> {
    Ok(plan_mappings(
        profile,
        &read_drive_mappings(&profile.prefix)?,
    ))
}

fn print_plan(profile: &ApplicationProfile) -> Result<()> {
    let plan = mapping_plan(profile)?;
    if plan.actions.is_empty() {
        println!("unchanged: mappings already match the profile");
    } else {
        for action in plan.actions {
            print_action(action);
        }
    }
    Ok(())
}

fn apply_profile(profile: &ApplicationProfile, confirmed: bool) -> Result<()> {
    let plan = mapping_plan(profile)?;
    if plan.actions.is_empty() {
        println!("unchanged: mappings already match the profile");
        return Ok(());
    }
    for action in &plan.actions {
        print_action(action.clone());
    }
    if !confirmed {
        bail!("refusing mutation without --yes after reviewing the plan");
    }
    let receipt = apply_mapping_plan(&profile.prefix, &plan)?;
    println!(
        "changed: drives {:?}; backup: {}",
        receipt.changed_drives,
        receipt.backup_directory.display()
    );
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

fn run_profile(
    profile: &ApplicationProfile,
    engine: &EngineManifest,
    engine_root: &Path,
    apply: bool,
) -> Result<()> {
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
    if apply {
        apply_profile(profile, true)?;
    } else {
        verify_profile(profile).context("launch failed closed because mapping state drifted")?;
    }
    let wine = engine_root.join(&engine.wine_binary);
    if !wine.is_file() {
        bail!("Wine executable does not exist: {}", wine.display());
    }
    let mut command = ProcessCommand::new(&wine);
    command.arg(&profile.executable).args(&profile.arguments);
    command.env("WINEPREFIX", &profile.prefix);
    for (key, value) in &engine.environment.0 {
        command.env(key, value);
    }
    for (key, value) in &profile.environment.0 {
        command.env(key, value);
    }
    let status = command
        .status()
        .with_context(|| format!("failed to launch {}", wine.display()))?;
    if !status.success() {
        bail!("Wine process exited with {status}");
    }
    Ok(())
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

fn rosetta_available() -> bool {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    return Path::new("/Library/Apple/usr/share/rosetta/rosetta").exists();
    #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
    true
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("invalid JSON in {}", path.display()))
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
    use tempfile::tempdir;
    use wineforge_core::{Artifact, ArtifactSource, Environment, License, Sha256Digest};

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
        }
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
}
