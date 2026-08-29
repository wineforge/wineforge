use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};
use wineforge_core::{
    ApplicationProfile, Artifact, ArtifactSource, EngineManifest, EngineSelection, Environment,
    HostMapping, IsolationPolicy, License, MappingAccess, Platform, Sha256Digest, Translation,
    Validate,
};

use crate::recipe::{Recipe, Variant};

const ENGINE_MARKER: &str = ".wineforge-engine.json";
const ENGINE_MANIFEST: &str = "engine.toml";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum BuildRuntime {
    #[default]
    Auto,
    Docker,
    Podman,
    Native,
}

impl BuildRuntime {
    fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Docker => "docker",
            Self::Podman => "podman",
            Self::Native => "native",
        }
    }
}

pub struct PrepareRequest<'a> {
    pub recipe: &'a Recipe,
    pub profile_out: &'a Path,
    pub profile_id: Option<String>,
    pub profile_name: Option<String>,
    pub prefix: Option<PathBuf>,
    pub mappings: Vec<String>,
    pub engine_store: &'a Path,
    pub engine_builder: Option<&'a Path>,
    pub engine_version: Option<String>,
    pub build_if_missing: bool,
    pub build_runtime: BuildRuntime,
    pub keep_build_artifacts: bool,
    pub non_interactive: bool,
}

pub struct PreparedApplication {
    pub profile: ApplicationProfile,
    pub engine: EngineManifest,
    /// The managed installation directory accepted by Wineforge's engine-root interface.
    pub engine_root: PathBuf,
}

#[derive(Debug)]
enum EnginePlan {
    Installed(EngineCandidate),
    Build(BuildPlan),
}

impl EnginePlan {
    fn id(&self) -> &str {
        match self {
            Self::Installed(candidate) => &candidate.manifest.id,
            Self::Build(plan) => &plan.id,
        }
    }
}

#[derive(Debug)]
struct EngineCandidate {
    version: Version,
    manifest: EngineManifest,
    root: PathBuf,
}

#[derive(Debug)]
struct BuildPlan {
    version: Version,
    id: String,
    target: String,
    builder: PathBuf,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct EngineMarker {
    schema_version: u32,
    kind: String,
    id: String,
    artifact_sha256: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BuilderManifest {
    source: BuilderSource,
    build: BuilderBuild,
}

#[derive(Debug, Deserialize)]
struct BuilderSource {
    version: String,
}

#[derive(Debug, Deserialize)]
struct BuilderBuild {
    targets: Vec<String>,
}

/// Resolve or build one engine and produce a complete, validated local profile.
/// No profile is committed until the engine is installed and verified.
pub fn execute(request: PrepareRequest<'_>) -> Result<PreparedApplication> {
    require_output_absent(request.profile_out, "profile output")?;
    require_safe_store(request.engine_store, "engine store")?;
    let variant = select_host_variant(request.recipe)?;
    let requirement = validate_engine_requirement(variant)?;
    let plan = plan_engine(
        request.engine_store,
        request.engine_builder,
        &requirement,
        request.engine_version.as_deref(),
    )?;

    let interactive = !request.non_interactive && io::stdin().is_terminal();
    let profile = configure_profile(&request, plan.id(), interactive)?;

    let candidate = match plan {
        EnginePlan::Installed(candidate) => {
            println!(
                "using installed engine {} at {}",
                candidate.manifest.id,
                candidate.root.display()
            );
            candidate
        }
        EnginePlan::Build(plan) => {
            if !(request.build_if_missing
                || interactive
                    && prompt_yes(&format!("Build missing engine {} locally?", plan.id))?)
            {
                bail!(
                    "no compatible engine is installed; pass --build-if-missing after reviewing the local build"
                );
            }
            build_and_install(
                request.engine_store,
                &plan,
                request.build_runtime,
                request.keep_build_artifacts,
            )?
        }
    };

    write_profile_atomic(request.profile_out, &profile)?;
    println!("wrote profile {}", request.profile_out.display());
    Ok(PreparedApplication {
        profile,
        engine: candidate.manifest,
        engine_root: candidate.root,
    })
}

pub fn default_engine_store() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME is unavailable; pass --engine-store")?;
    let relative = if cfg!(target_os = "macos") {
        "Applications/Wineforge/engines"
    } else {
        ".local/share/wineforge/engines"
    };
    Ok(PathBuf::from(home).join(relative))
}

fn configure_profile(
    request: &PrepareRequest<'_>,
    engine_id: &str,
    interactive: bool,
) -> Result<ApplicationProfile> {
    let default_id = request
        .profile_id
        .clone()
        .unwrap_or_else(|| request.recipe.id.clone());
    let id = if interactive {
        prompt_value("Profile ID", &default_id)?
    } else {
        default_id
    };
    let default_name = request
        .profile_name
        .clone()
        .unwrap_or_else(|| request.recipe.name.clone());
    let name = if interactive {
        prompt_value("Application name", &default_name)?
    } else {
        default_name
    };
    let default_prefix = request
        .prefix
        .clone()
        .map(Ok)
        .unwrap_or_else(|| default_prefix(&id))?;
    let prefix = if interactive {
        expand_home(&prompt_value(
            "Wine prefix",
            default_prefix.to_string_lossy().as_ref(),
        )?)?
    } else {
        default_prefix
    };

    let mut mappings = request
        .mappings
        .iter()
        .map(|mapping| parse_mapping(mapping))
        .collect::<Result<Vec<_>>>()?;
    if interactive {
        if !request.recipe.runtime_access.folders.is_empty() {
            eprintln!(
                "Recipe requests folder classes: {}",
                request.recipe.runtime_access.folders.join(", ")
            );
        }
        while prompt_yes("Add a host-folder mapping?")? {
            let drive = prompt_value("Drive letter", "W")?;
            let access = prompt_value("Access (read-only/read-write)", "read-write")?;
            let host_path = prompt_value("Absolute host folder", "")?;
            mappings.push(parse_mapping(&format!("{drive}={access}={host_path}"))?);
        }
    } else if mappings.is_empty() && !request.recipe.runtime_access.folders.is_empty() {
        eprintln!(
            "warning: recipe requests folder classes [{}], but no --mapping was supplied",
            request.recipe.runtime_access.folders.join(", ")
        );
    }

    let profile = ApplicationProfile {
        schema_version: 1,
        id,
        name,
        prefix,
        executable: request.recipe.application.executable.clone(),
        arguments: request.recipe.application.arguments.clone(),
        engines: BTreeMap::from([(
            crate::current_platform()?,
            EngineSelection {
                id: engine_id.to_owned(),
            },
        )]),
        environment: Environment::default(),
        mappings,
        isolation: IsolationPolicy::default(),
    };
    profile.validate().context("generated profile is invalid")?;
    Ok(profile)
}

fn default_prefix(id: &str) -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME is unavailable; pass --prefix")?;
    let relative = if cfg!(target_os = "macos") {
        PathBuf::from("Library/Application Support/Wineforge/instances")
    } else {
        PathBuf::from(".local/share/wineforge/instances")
    };
    Ok(PathBuf::from(home).join(relative).join(id).join("prefix"))
}

fn expand_home(value: &str) -> Result<PathBuf> {
    if value == "~" || value.starts_with("~/") {
        let home = std::env::var_os("HOME").context("HOME is unavailable")?;
        return Ok(PathBuf::from(home).join(value.trim_start_matches("~/")));
    }
    Ok(PathBuf::from(value))
}

fn parse_mapping(value: &str) -> Result<HostMapping> {
    let mut fields = value.splitn(3, '=');
    let drive = fields.next().unwrap_or_default();
    let access = fields.next().unwrap_or_default();
    let path = fields.next().unwrap_or_default();
    if drive.is_empty() || access.is_empty() || path.is_empty() {
        bail!("mapping must use DRIVE=read-only|read-write=/absolute/path");
    }
    let access = match access {
        "read-only" | "ro" => MappingAccess::ReadOnly,
        "read-write" | "rw" => MappingAccess::ReadWrite,
        _ => bail!("mapping access must be read-only or read-write"),
    };
    Ok(HostMapping {
        drive: drive.to_owned(),
        host_path: expand_home(path)?,
        access,
    })
}

fn select_host_variant(recipe: &Recipe) -> Result<&Variant> {
    let os = if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else {
        bail!("this host operating system is not supported");
    };
    let matches = recipe
        .variants
        .iter()
        .filter(|variant| {
            variant.platform == os && variant.host_architecture == std::env::consts::ARCH
        })
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [variant] => Ok(*variant),
        [] => bail!("recipe has no variant for {os}/{}", std::env::consts::ARCH),
        _ => bail!(
            "recipe has multiple variants for {os}/{}",
            std::env::consts::ARCH
        ),
    }
}

fn validate_engine_requirement(variant: &Variant) -> Result<VersionReq> {
    match variant.engine.family.as_str() {
        "wine" | "winecx" => {}
        family => bail!("no trusted local builder is registered for engine family {family}"),
    }
    let supported_features = match crate::current_platform()? {
        Platform::MacosX86_64 => BTreeSet::from(["win32", "win64"]),
        Platform::LinuxX86_64 => BTreeSet::from(["win64"]),
    };
    let unsupported = variant
        .engine
        .features
        .iter()
        .filter(|feature| !supported_features.contains(feature.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    if !unsupported.is_empty() {
        bail!(
            "local builder does not provide required engine features: {}",
            unsupported.join(", ")
        );
    }
    let expected_translation = match crate::current_platform()? {
        Platform::MacosX86_64 => "rosetta2",
        Platform::LinuxX86_64 => "native",
    };
    if variant.translation != expected_translation {
        bail!(
            "recipe translation {} is incompatible with this host's trusted engine builder",
            variant.translation
        );
    }
    VersionReq::parse(&variant.engine.version).with_context(|| {
        format!(
            "invalid recipe engine version range: {}",
            variant.engine.version
        )
    })
}

fn plan_engine(
    store: &Path,
    builder: Option<&Path>,
    requirement: &VersionReq,
    requested_version: Option<&str>,
) -> Result<EnginePlan> {
    let requested_version = requested_version
        .map(Version::parse)
        .transpose()
        .context("--engine-version must be semantic versioning")?;
    if let Some(candidate) = find_installed_engine(store, requirement, requested_version.as_ref())?
    {
        return Ok(EnginePlan::Installed(candidate));
    }
    let builder = builder.context(
        "no compatible engine is installed; pass --engine-builder with a trusted wineforge-engines checkout",
    )?;
    let version = select_builder_version(
        builder,
        requirement,
        requested_version.as_ref(),
        target_name(crate::current_platform()?),
    )?;
    let target = target_name(crate::current_platform()?).to_owned();
    let id = format!("crossover-{version}-{target}");
    Ok(EnginePlan::Build(BuildPlan {
        version,
        id,
        target,
        builder: builder.to_owned(),
    }))
}

fn find_installed_engine(
    store: &Path,
    requirement: &VersionReq,
    requested: Option<&Version>,
) -> Result<Option<EngineCandidate>> {
    let target = target_name(crate::current_platform()?);
    let mut candidates = Vec::new();
    for entry in fs::read_dir(store)
        .with_context(|| format!("failed to read engine store {}", store.display()))?
    {
        let entry = entry?;
        let root = entry.path();
        let metadata = fs::symlink_metadata(&root)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            continue;
        }
        let marker_path = root.join(ENGINE_MARKER);
        let Ok(marker_metadata) = fs::symlink_metadata(&marker_path) else {
            continue;
        };
        if marker_metadata.file_type().is_symlink() || !marker_metadata.is_file() {
            bail!("unsafe engine marker: {}", marker_path.display());
        }
        let marker: EngineMarker = read_json(&marker_path)?;
        if marker.schema_version != 1 || marker.kind != "engine" {
            bail!("invalid engine marker: {}", marker_path.display());
        }
        let Some(version) = parse_engine_version(&marker.id, target) else {
            continue;
        };
        if !requirement.matches(&version) || requested.is_some_and(|value| value != &version) {
            continue;
        }
        let manifest_path = root.join(ENGINE_MANIFEST);
        let manifest = if manifest_path.is_file() {
            let text = fs::read_to_string(&manifest_path)?;
            toml::from_str::<EngineManifest>(&text)
                .with_context(|| format!("invalid TOML in {}", manifest_path.display()))?
        } else {
            legacy_manifest(&marker, crate::current_platform()?)?
        };
        manifest
            .validate()
            .context("installed engine manifest is invalid")?;
        if manifest.id != marker.id
            || manifest.artifact.sha256.0 != marker.artifact_sha256.clone().unwrap_or_default()
            || manifest.platform != crate::current_platform()?
        {
            bail!(
                "installed engine marker and manifest disagree at {}",
                root.display()
            );
        }
        if manifest.translation == Translation::Rosetta2 && !crate::rosetta_available() {
            bail!("installed engine requires Rosetta 2, which was not detected");
        }
        crate::engine_wine(&manifest, &root)?;
        candidates.push(EngineCandidate {
            version,
            manifest,
            root,
        });
    }
    candidates.sort_by(|left, right| right.version.cmp(&left.version));
    Ok(candidates.into_iter().next())
}

fn legacy_manifest(marker: &EngineMarker, platform: Platform) -> Result<EngineManifest> {
    let digest = marker
        .artifact_sha256
        .clone()
        .context("legacy engine marker has no artifact digest")?;
    Ok(EngineManifest {
        schema_version: 1,
        id: marker.id.clone(),
        platform,
        host_architecture: "x86_64".into(),
        translation: match platform {
            Platform::MacosX86_64 => Translation::Rosetta2,
            Platform::LinuxX86_64 => Translation::Native,
        },
        artifact: Artifact {
            source: ArtifactSource::UserSupplied,
            sha256: Sha256Digest(digest),
        },
        wine_binary: "bin/wine".into(),
        environment: Environment(BTreeMap::from([
            ("WINEESYNC".into(), "1".into()),
            ("WINEMSYNC".into(), "1".into()),
        ])),
        license: License {
            name: "CrossOver component licences".into(),
            url: "https://www.codeweavers.com/crossover/source".parse()?,
            acceptance_required: false,
        },
    })
}

fn select_builder_version(
    builder: &Path,
    requirement: &VersionReq,
    requested: Option<&Version>,
    target: &str,
) -> Result<Version> {
    require_real_directory(builder, "engine builder checkout")?;
    let script = builder.join("scripts/build-local.sh");
    require_regular_executable(&script, "local engine builder")?;
    let manifests = builder.join("engines");
    require_real_directory(&manifests, "engine builder manifest directory")?;
    let mut versions = Vec::new();
    for entry in fs::read_dir(&manifests)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let manifest: BuilderManifest = match read_json(&path) {
            Ok(manifest) => manifest,
            Err(_) => continue,
        };
        let version = Version::parse(&manifest.source.version)
            .with_context(|| format!("invalid builder version in {}", path.display()))?;
        if manifest.build.targets.iter().any(|value| value == target)
            && requirement.matches(&version)
            && requested.is_none_or(|value| value == &version)
        {
            versions.push(version);
        }
    }
    versions.sort();
    versions.pop().with_context(|| {
        format!("trusted engine builder has no {target} version matching {requirement}")
    })
}

fn build_and_install(
    store: &Path,
    plan: &BuildPlan,
    runtime: BuildRuntime,
    keep_build_artifacts: bool,
) -> Result<EngineCandidate> {
    let build_store = store.join(".builds");
    if fs::symlink_metadata(&build_store).is_err() {
        fs::create_dir(&build_store)?;
    }
    require_real_directory(&build_store, "engine build store")?;
    println!("building engine {} with {}", plan.id, runtime.as_str());
    let status = Command::new(plan.builder.join("scripts/build-local.sh"))
        .arg(plan.version.to_string())
        .arg(&plan.target)
        .arg("--runtime")
        .arg(runtime.as_str())
        .arg("--store")
        .arg(&build_store)
        .status()
        .context("failed to start the local engine builder")?;
    if !status.success() {
        bail!("local engine build failed with {status}");
    }

    let build_root = build_store.join(&plan.id);
    let dist = build_root.join("dist");
    let archive = dist.join(format!(
        "wineforge-engine-{}-{}.tar.gz",
        plan.version, plan.target
    ));
    let runtime_manifest = archive.with_extension("gz.runtime.json");
    let manifest: EngineManifest = read_json(&runtime_manifest)?;
    manifest
        .validate()
        .context("built engine manifest is invalid")?;
    if manifest.id != plan.id || manifest.platform != crate::current_platform()? {
        bail!("built engine manifest does not match the selected build plan");
    }

    let destination = store.join(&manifest.id);
    crate::install_engine(&archive, &manifest, &destination)?;
    crate::engine_wine(&manifest, &destination)?;
    if !keep_build_artifacts {
        remove_verified_build(&build_root, &plan.id)?;
        println!("discarded temporary engine build {}", build_root.display());
    } else {
        println!("kept engine build artifacts at {}", build_root.display());
    }
    Ok(EngineCandidate {
        version: plan.version.clone(),
        manifest,
        root: destination,
    })
}

fn remove_verified_build(path: &Path, expected_id: &str) -> Result<()> {
    let marker_path = path.join(".wineforge-build-artifacts.json");
    let path_metadata = fs::symlink_metadata(path)?;
    let marker_metadata = fs::symlink_metadata(&marker_path)?;
    if path_metadata.file_type().is_symlink()
        || !path_metadata.is_dir()
        || marker_metadata.file_type().is_symlink()
        || !marker_metadata.is_file()
    {
        bail!(
            "refusing to remove unsafe build directory {}",
            path.display()
        );
    }
    let marker: EngineMarker = read_json(&marker_path)?;
    if marker.schema_version != 1 || marker.kind != "build-artifacts" || marker.id != expected_id {
        bail!(
            "refusing to remove unverified build directory {}",
            path.display()
        );
    }
    fs::remove_dir_all(path)
        .with_context(|| format!("failed to discard build directory {}", path.display()))
}

fn parse_engine_version(id: &str, target: &str) -> Option<Version> {
    let body = id.strip_prefix("crossover-")?;
    let version = body.strip_suffix(&format!("-{target}"))?;
    Version::parse(version).ok()
}

fn target_name(platform: Platform) -> &'static str {
    match platform {
        Platform::MacosX86_64 => "macos-x86_64",
        Platform::LinuxX86_64 => "linux-x86_64",
    }
}

fn prompt_value(label: &str, default: &str) -> Result<String> {
    if default.is_empty() {
        eprint!("{label}: ");
    } else {
        eprint!("{label} [{default}]: ");
    }
    io::stderr().flush()?;
    let mut value = String::new();
    io::stdin().read_line(&mut value)?;
    let value = value.trim();
    if value.is_empty() {
        if default.is_empty() {
            bail!("{label} must not be empty");
        }
        Ok(default.to_owned())
    } else {
        Ok(value.to_owned())
    }
}

fn prompt_yes(label: &str) -> Result<bool> {
    eprint!("{label} [y/N]: ");
    io::stderr().flush()?;
    let mut value = String::new();
    io::stdin().read_line(&mut value)?;
    Ok(matches!(value.trim(), "y" | "Y" | "yes" | "YES"))
}

fn write_profile_atomic(path: &Path, profile: &ApplicationProfile) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    require_real_directory(parent, "profile output directory")?;
    let temporary = parent.join(format!(".profile.{}.toml.part", std::process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .with_context(|| format!("failed to create {}", temporary.display()))?;
    let result = (|| -> Result<()> {
        file.write_all(toml::to_string_pretty(profile)?.as_bytes())?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn require_output_absent(path: &Path, label: &str) -> Result<()> {
    if path.extension().and_then(|value| value.to_str()) != Some("toml") {
        bail!("{label} must use a .toml extension: {}", path.display());
    }
    if fs::symlink_metadata(path).is_ok() {
        bail!("{label} already exists: {}", path.display());
    }
    Ok(())
}

fn require_real_directory(path: &Path, label: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("{label} does not exist: {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("{label} must be a real directory: {}", path.display());
    }
    Ok(())
}

fn require_safe_store(path: &Path, label: &str) -> Result<()> {
    require_real_directory(path, label)?;
    if !path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        bail!(
            "{label} must be an absolute path without `..`: {}",
            path.display()
        );
    }
    let canonical = fs::canonicalize(path)?;
    if canonical.parent().is_none() || canonical.components().count() < 2 {
        bail!("refusing unsafe {label}: {}", canonical.display());
    }
    Ok(())
}

fn require_regular_executable(path: &Path, label: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("{label} does not exist: {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("{label} must be a regular file: {}", path.display());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            bail!("{label} is not executable: {}", path.display());
        }
    }
    Ok(())
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("invalid JSON in {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recipe::{
        Application, License as RecipeLicense, LicenseAcceptance, Postcondition, RuntimeAccess,
        VariantEngine,
    };
    use tempfile::tempdir;

    fn recipe() -> Recipe {
        Recipe {
            schema_version: 1,
            id: "org.example.editor".into(),
            version: "1.0.0".into(),
            name: "Example Editor".into(),
            summary: "A test recipe".into(),
            homepage: None,
            license: RecipeLicense {
                spdx: "MIT".into(),
                notice_url: None,
                acceptance: LicenseAcceptance::None,
            },
            application: Application {
                executable: r"C:\Program Files\Example\editor.exe".into(),
                arguments: vec!["--safe".into()],
                working_directory: None,
            },
            variants: vec![Variant {
                platform: if cfg!(target_os = "macos") {
                    "macos".into()
                } else {
                    "linux".into()
                },
                host_architecture: std::env::consts::ARCH.into(),
                translation: if cfg!(target_os = "macos") {
                    "rosetta2".into()
                } else {
                    "native".into()
                },
                engine: VariantEngine {
                    family: if cfg!(target_os = "macos") {
                        "winecx".into()
                    } else {
                        "wine".into()
                    },
                    version: ">=24,<26".into(),
                    features: vec!["win64".into()],
                },
            }],
            runtime_access: RuntimeAccess {
                network: "none".into(),
                folders: Vec::new(),
            },
            sources: Vec::new(),
            install: Vec::new(),
            verify: vec![Postcondition::FileExists {
                path: r"C:\Program Files\Example\editor.exe".into(),
                sha256: None,
            }],
        }
    }

    fn engine_manifest(id: &str, digest: &str) -> EngineManifest {
        let platform = crate::current_platform().unwrap();
        EngineManifest {
            schema_version: 1,
            id: id.into(),
            platform,
            host_architecture: "x86_64".into(),
            translation: match platform {
                Platform::MacosX86_64 => Translation::Rosetta2,
                Platform::LinuxX86_64 => Translation::Native,
            },
            artifact: Artifact {
                source: ArtifactSource::UserSupplied,
                sha256: Sha256Digest(digest.into()),
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
    fn mapping_parser_keeps_path_and_access_explicit() {
        let mapping = parse_mapping("R=read-only=/srv/reference=files").unwrap();
        assert_eq!(mapping.drive, "R");
        assert_eq!(mapping.access, MappingAccess::ReadOnly);
        assert_eq!(mapping.host_path, Path::new("/srv/reference=files"));
        assert!(parse_mapping("R=/srv/reference").is_err());
    }

    #[test]
    fn engine_identity_requires_the_full_managed_target() {
        assert_eq!(
            parse_engine_version("crossover-25.1.1-macos-x86_64", "macos-x86_64"),
            Some(Version::new(25, 1, 1))
        );
        assert!(parse_engine_version("untrusted-25.1.1-macos-x86_64", "macos-x86_64").is_none());
    }

    #[test]
    fn builder_selection_uses_highest_compatible_version() {
        let temp = tempdir().unwrap();
        let engines = temp.path().join("engines");
        let scripts = temp.path().join("scripts");
        fs::create_dir(&engines).unwrap();
        fs::create_dir(&scripts).unwrap();
        let builder = scripts.join("build-local.sh");
        fs::write(&builder, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&builder, fs::Permissions::from_mode(0o755)).unwrap();
        }
        for version in ["24.0.7", "25.1.1", "26.0.0"] {
            fs::write(
                engines.join(format!("crossover-{version}.json")),
                format!(
                    "{{\"source\":{{\"version\":\"{version}\"}},\"build\":{{\"targets\":[\"macos-x86_64\"]}}}}"
                ),
            )
            .unwrap();
        }

        let selected = select_builder_version(
            temp.path(),
            &VersionReq::parse(">=24,<26").unwrap(),
            None,
            "macos-x86_64",
        )
        .unwrap();

        assert_eq!(selected, Version::new(25, 1, 1));
    }

    #[test]
    fn cleanup_refuses_an_unmarked_directory() {
        let temp = tempdir().unwrap();
        assert!(remove_verified_build(temp.path(), "example-build").is_err());
        assert!(temp.path().exists());
    }

    #[test]
    fn cleanup_removes_only_the_verified_build() {
        let temp = tempdir().unwrap();
        let build = temp.path().join("example-build");
        fs::create_dir(&build).unwrap();
        fs::write(
            build.join(".wineforge-build-artifacts.json"),
            serde_json::to_vec(&EngineMarker {
                schema_version: 1,
                kind: "build-artifacts".into(),
                id: "example-build".into(),
                artifact_sha256: Some("0".repeat(64)),
            })
            .unwrap(),
        )
        .unwrap();

        remove_verified_build(&build, "example-build").unwrap();

        assert!(!build.exists());
        assert!(temp.path().exists());
    }

    #[test]
    fn profile_configuration_is_derived_from_recipe_and_local_answers() {
        let temp = tempdir().unwrap();
        let recipe = recipe();
        let request = PrepareRequest {
            recipe: &recipe,
            profile_out: &temp.path().join("profile.toml"),
            profile_id: Some("example-local".into()),
            profile_name: None,
            prefix: Some(temp.path().join("prefix")),
            mappings: vec![format!(
                "W=read-write={}",
                temp.path().join("workspace").display()
            )],
            engine_store: temp.path(),
            engine_builder: None,
            engine_version: None,
            build_if_missing: false,
            build_runtime: BuildRuntime::Auto,
            keep_build_artifacts: false,
            non_interactive: true,
        };

        let profile = configure_profile(&request, "example-engine", false).unwrap();

        assert_eq!(profile.id, "example-local");
        assert_eq!(profile.name, recipe.name);
        assert_eq!(profile.executable, recipe.application.executable);
        assert_eq!(profile.arguments, recipe.application.arguments);
        assert_eq!(profile.mappings.len(), 1);
        assert_eq!(
            profile.engines[&crate::current_platform().unwrap()].id,
            "example-engine"
        );
    }

    #[test]
    fn resolver_prefers_a_valid_installed_engine() {
        let temp = tempdir().unwrap();
        let target = target_name(crate::current_platform().unwrap());
        let id = format!("crossover-24.0.7-{target}");
        let root = temp.path().join("managed-engine");
        let runtime = root.join("wineforge-engine/bin");
        fs::create_dir_all(&runtime).unwrap();
        fs::write(runtime.join("wine"), b"synthetic").unwrap();
        let digest = "a".repeat(64);
        fs::write(
            root.join(ENGINE_MARKER),
            serde_json::to_vec(&EngineMarker {
                schema_version: 1,
                kind: "engine".into(),
                id: id.clone(),
                artifact_sha256: Some(digest.clone()),
            })
            .unwrap(),
        )
        .unwrap();
        fs::write(
            root.join(ENGINE_MANIFEST),
            toml::to_string_pretty(&engine_manifest(&id, &digest)).unwrap(),
        )
        .unwrap();

        let selected =
            find_installed_engine(temp.path(), &VersionReq::parse(">=24,<25").unwrap(), None)
                .unwrap()
                .unwrap();

        assert_eq!(selected.manifest.id, id);
        assert_eq!(selected.version, Version::new(24, 0, 7));
        assert_eq!(selected.root, root);
    }
}
