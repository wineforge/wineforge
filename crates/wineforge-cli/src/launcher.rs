use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use wineforge_core::{ApplicationProfile, EngineDistribution, EngineManifest};

fn main() -> Result<()> {
    let executable = std::env::current_exe().context("failed to locate native launcher")?;
    let layout = PackageLayout::discover(&executable)?;
    let engine_root = resolve_engine_root(&layout)?;
    let profile = match layout.kind {
        PackageKind::MacosApp => prepare_macos_profile(&layout)?,
        PackageKind::LinuxPackage => prepare_linux_profile(&layout)?,
    };

    let mut command = wineforge_run_command(&layout, profile, engine_root);

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let error = command.exec();
        Err(error).context("failed to execute embedded Wineforge runtime")
    }
    #[cfg(not(unix))]
    {
        let status = command
            .status()
            .context("failed to execute embedded Wineforge runtime")?;
        std::process::exit(status.code().unwrap_or(1));
    }
}

fn wineforge_run_command(
    layout: &PackageLayout,
    profile: PathBuf,
    engine_root: PathBuf,
) -> Command {
    let mut command = Command::new(&layout.wineforge);
    command
        .arg("run")
        .arg(profile)
        .arg("--engine-manifest")
        .arg(&layout.engine_manifest)
        .arg("--engine-root")
        .arg(engine_root)
        .arg("--apply");
    command
}

fn prepare_macos_profile(layout: &PackageLayout) -> Result<PathBuf> {
    let text = fs::read_to_string(&layout.profile_template).context("failed to read profile")?;
    let mut profile: ApplicationProfile = toml::from_str(&text).context("invalid profile TOML")?;
    let prefix = layout.root.join("WinePrefix");
    require_real_directory(&prefix, "embedded Wine prefix")?;
    profile.prefix = prefix.clone();
    let profile_path = prefix.join(".wineforge/native-profile.toml");
    write_toml_atomic(&profile_path, &profile)?;
    Ok(profile_path)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PackageKind {
    MacosApp,
    LinuxPackage,
}

#[derive(Debug)]
struct PackageLayout {
    kind: PackageKind,
    root: PathBuf,
    wineforge: PathBuf,
    profile_template: PathBuf,
    engine_manifest: PathBuf,
    embedded_engine_root: PathBuf,
}

impl PackageLayout {
    fn discover(executable: &Path) -> Result<Self> {
        let binary_directory = executable
            .parent()
            .context("native launcher has no parent directory")?;
        let root = binary_directory
            .parent()
            .context("native launcher has no package root")?
            .to_path_buf();
        let macos_resources = root.join("Resources");
        let (kind, resources, embedded_engine_root) = if macos_resources.is_dir() {
            (
                PackageKind::MacosApp,
                macos_resources,
                root.join("Frameworks/WineEngine"),
            )
        } else {
            (
                PackageKind::LinuxPackage,
                root.join("resources"),
                root.join("engine"),
            )
        };
        let layout = Self {
            kind,
            root,
            wineforge: binary_directory.join("wineforge"),
            profile_template: resources.join("profile.toml"),
            engine_manifest: resources.join("engine.toml"),
            embedded_engine_root,
        };
        for (label, path) in [
            ("embedded Wineforge runtime", &layout.wineforge),
            ("profile snapshot", &layout.profile_template),
            ("engine manifest snapshot", &layout.engine_manifest),
        ] {
            require_regular_file(path, label)?;
        }
        Ok(layout)
    }
}

fn resolve_engine_root(layout: &PackageLayout) -> Result<PathBuf> {
    let profile_text = fs::read_to_string(&layout.profile_template)
        .context("failed to read profile for engine resolution")?;
    let profile: ApplicationProfile =
        toml::from_str(&profile_text).context("invalid profile TOML")?;
    let manifest_text =
        fs::read_to_string(&layout.engine_manifest).context("failed to read engine manifest")?;
    let engine: EngineManifest =
        toml::from_str(&manifest_text).context("invalid engine manifest TOML")?;
    let selection = profile.engines.get(&engine.platform).with_context(|| {
        format!(
            "profile does not select an engine for {:?}",
            engine.platform
        )
    })?;
    if selection.id != engine.id {
        anyhow::bail!(
            "profile selects engine {} but package manifest describes {}",
            selection.id,
            engine.id
        );
    }

    let shared = || -> Result<PathBuf> {
        let root = selection
            .root
            .as_ref()
            .context("shared engine selection has no resolved root")?;
        require_real_directory(root, "shared engine")?;
        fs::canonicalize(root).context("failed to resolve shared engine root")
    };
    match selection.distribution {
        EngineDistribution::Shared => shared(),
        EngineDistribution::Bundled => {
            require_real_directory(&layout.embedded_engine_root, "embedded engine")?;
            Ok(layout.embedded_engine_root.clone())
        }
        EngineDistribution::Auto => match shared() {
            Ok(root) => Ok(root),
            Err(shared_error) => {
                if layout.embedded_engine_root.is_dir() {
                    require_real_directory(&layout.embedded_engine_root, "embedded engine")?;
                    Ok(layout.embedded_engine_root.clone())
                } else {
                    Err(shared_error).context("automatic engine resolution failed")
                }
            }
        },
    }
}

fn prepare_linux_profile(layout: &PackageLayout) -> Result<PathBuf> {
    let text = fs::read_to_string(&layout.profile_template).context("failed to read profile")?;
    let mut profile: ApplicationProfile = toml::from_str(&text).context("invalid profile TOML")?;
    let data = linux_data_home()?;
    let instance = data.join("wineforge/instances").join(&profile.id);
    ensure_real_directory(&instance)?;
    let prefix = instance.join("prefix");
    if !prefix.exists() {
        let staging = instance.join(format!(".prefix.{}.staging", std::process::id()));
        if fs::symlink_metadata(&staging).is_ok() {
            bail!("prefix staging path already exists: {}", staging.display());
        }
        copy_tree(&layout.root.join("prefix-template"), &staging)?;
        match fs::rename(&staging, &prefix) {
            Ok(()) => {}
            Err(error) if prefix.is_dir() => {
                fs::remove_dir_all(&staging)?;
                let _ = error;
            }
            Err(error) => {
                let _ = fs::remove_dir_all(&staging);
                return Err(error).context("failed to commit per-user Wine prefix");
            }
        }
    }
    require_real_directory(&prefix, "per-user Wine prefix")?;
    profile.prefix = prefix;
    let profile_path = instance.join("profile.toml");
    write_toml_atomic(&profile_path, &profile)?;
    Ok(profile_path)
}

fn linux_data_home() -> Result<PathBuf> {
    let path = if let Some(value) = std::env::var_os("XDG_DATA_HOME") {
        PathBuf::from(value)
    } else {
        let home = std::env::var_os("HOME").context("HOME is unavailable")?;
        PathBuf::from(home).join(".local/share")
    };
    if !path.is_absolute() {
        bail!("Linux application data directory must be absolute");
    }
    ensure_real_directory(&path)?;
    Ok(path)
}

fn write_toml_atomic(path: &Path, value: &ApplicationProfile) -> Result<()> {
    let parent = path.parent().context("profile has no parent directory")?;
    ensure_real_directory(parent)?;
    let temporary = parent.join(format!(".profile.{}.toml.part", std::process::id()));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)
        .context("failed to create profile staging file")?;
    let result = (|| -> Result<()> {
        file.write_all(toml::to_string_pretty(value)?.as_bytes())?;
        file.sync_all()?;
        Ok(())
    })();
    drop(file);
    if let Err(error) = result {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    if path.exists() {
        require_regular_file(path, "existing generated profile")?;
    }
    fs::rename(&temporary, path).context("failed to commit generated profile")
}

fn copy_tree(source: &Path, destination: &Path) -> Result<()> {
    require_real_directory(source, "prefix template")?;
    if fs::symlink_metadata(destination).is_ok() {
        bail!("copy destination already exists: {}", destination.display());
    }
    fs::create_dir(destination)?;
    let result = copy_tree_contents(source, source, destination);
    if result.is_err() {
        let _ = fs::remove_dir_all(destination);
    }
    result
}

fn copy_tree_contents(root: &Path, source: &Path, destination: &Path) -> Result<()> {
    let mut entries = fs::read_dir(source)?.collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        let metadata = fs::symlink_metadata(&source_path)?;
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            fs::create_dir(&destination_path)?;
            copy_tree_contents(root, &source_path, &destination_path)?;
        } else if metadata.is_file() && !metadata.file_type().is_symlink() {
            fs::copy(&source_path, &destination_path)?;
            fs::set_permissions(&destination_path, metadata.permissions())?;
        } else if metadata.file_type().is_symlink() {
            let target = fs::read_link(&source_path)?;
            if !relative_link_stays_in_root(root, &source_path, &target) {
                bail!(
                    "prefix template contains an escaping link: {} -> {}",
                    source_path.display(),
                    target.display()
                );
            }
            #[cfg(unix)]
            std::os::unix::fs::symlink(target, destination_path)?;
            #[cfg(not(unix))]
            bail!("prefix links are unsupported on this platform");
        } else {
            bail!(
                "prefix template contains a special file: {}",
                source_path.display()
            );
        }
    }
    Ok(())
}

fn relative_link_stays_in_root(root: &Path, path: &Path, target: &Path) -> bool {
    if target.is_absolute() {
        return false;
    }
    let Ok(parent) = path.parent().unwrap_or(path).strip_prefix(root) else {
        return false;
    };
    let mut depth = parent.components().count();
    for component in target.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(_) => depth += 1,
            Component::ParentDir if depth > 0 => depth -= 1,
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return false,
        }
    }
    true
}

fn ensure_real_directory(path: &Path) -> Result<()> {
    if let Ok(metadata) = fs::symlink_metadata(path) {
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            return Ok(());
        }
        bail!("path is not a real directory: {}", path.display());
    }
    let parent = path.parent().context("directory has no parent")?;
    ensure_real_directory(parent)?;
    fs::create_dir(path)?;
    Ok(())
}

fn require_real_directory(path: &Path, label: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("missing {label}: {}", path.display()))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        bail!("{label} is not a real directory: {}", path.display());
    }
    Ok(())
}

fn require_regular_file(path: &Path, label: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("missing {label}: {}", path.display()))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        bail!("{label} is not a regular file: {}", path.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn rejects_links_that_escape_a_copied_prefix() {
        let root = Path::new("/tmp/prefix");
        assert!(relative_link_stays_in_root(
            root,
            &root.join("dosdevices/c:"),
            Path::new("../drive_c")
        ));
        assert!(!relative_link_stays_in_root(
            root,
            &root.join("dosdevices/z:"),
            Path::new("../../../../")
        ));
    }

    #[test]
    fn resolves_shared_engine_and_auto_falls_back_to_embedded() {
        let temp = tempdir().unwrap();
        let resources = temp.path().join("Resources");
        let shared = temp.path().join("shared-engine");
        let embedded = temp.path().join("Frameworks/WineEngine");
        fs::create_dir_all(&resources).unwrap();
        fs::create_dir(&shared).unwrap();
        fs::create_dir_all(&embedded).unwrap();
        fs::write(
            resources.join("engine.toml"),
            r#"schema_version = 1
id = "sample-engine-macos-x86_64"
platform = "macos-x86-64"
host_architecture = "x86_64"
wine_binary = "bin/wine"
[artifact]
sha256 = "0000000000000000000000000000000000000000000000000000000000000000"
[artifact.source]
kind = "user-supplied"
[license]
name = "Test"
url = "https://example.invalid/license"
"#,
        )
        .unwrap();
        let write_profile = |distribution: &str, root: Option<&Path>| {
            let root = root
                .map(|path| format!("root = {:?}\n", path))
                .unwrap_or_default();
            fs::write(
                resources.join("profile.toml"),
                format!(
                    "schema_version = 1\nid = \"sample\"\nname = \"Sample\"\nprefix = \"/tmp/sample-prefix\"\nexecutable = \"C:\\\\sample.exe\"\n[engines.macos-x86-64]\nid = \"sample-engine-macos-x86_64\"\ndistribution = \"{distribution}\"\n{root}"
                ),
            )
            .unwrap();
        };
        let layout = PackageLayout {
            kind: PackageKind::MacosApp,
            root: temp.path().to_path_buf(),
            wineforge: temp.path().join("wineforge"),
            profile_template: resources.join("profile.toml"),
            engine_manifest: resources.join("engine.toml"),
            embedded_engine_root: embedded.clone(),
        };

        write_profile("shared", Some(&shared));
        assert_eq!(
            resolve_engine_root(&layout).unwrap(),
            fs::canonicalize(&shared).unwrap()
        );
        write_profile("auto", None);
        assert_eq!(resolve_engine_root(&layout).unwrap(), embedded);
    }

    #[test]
    fn native_launcher_reconciles_profile_before_every_run() {
        let layout = PackageLayout {
            kind: PackageKind::MacosApp,
            root: PathBuf::from("/package"),
            wineforge: PathBuf::from("/package/wineforge"),
            profile_template: PathBuf::from("/package/profile.toml"),
            engine_manifest: PathBuf::from("/package/engine.toml"),
            embedded_engine_root: PathBuf::from("/package/engine"),
        };
        let command = wineforge_run_command(
            &layout,
            PathBuf::from("/instance/profile.toml"),
            PathBuf::from("/shared/engine"),
        );
        let arguments = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert_eq!(
            arguments,
            [
                "run",
                "/instance/profile.toml",
                "--engine-manifest",
                "/package/engine.toml",
                "--engine-root",
                "/shared/engine",
                "--apply",
            ]
        );
    }
}
