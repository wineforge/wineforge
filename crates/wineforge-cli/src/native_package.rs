use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{Cursor, Write};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use flate2::Compression;
use flate2::write::GzEncoder;
use icns::{IconFamily, Image as IcnsImage, PixelFormat};
use pelite::PeFile;
use serde::Serialize;
use wineforge_core::{ApplicationProfile, EngineManifest};

use crate::recipe::{Recipe, windows_path_to_prefix};
use crate::{adopt_imported_instance, recipe_executor, resolved_engine_root, shutdown_wineserver};

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum NativeFormat {
    App,
    Deb,
}

pub struct InstallRequest<'a> {
    pub recipe: &'a Recipe,
    pub recipe_path: &'a Path,
    pub profile: &'a ApplicationProfile,
    pub engine: &'a EngineManifest,
    pub engine_root: &'a Path,
    pub destination: &'a Path,
    pub format: NativeFormat,
    pub wineforge_binary: &'a Path,
    pub launcher_binary: &'a Path,
    pub winetricks_command: &'a Path,
    pub cache: &'a Path,
    pub accept_license: bool,
}

pub struct ImportRequest<'a> {
    pub source_prefix: &'a Path,
    pub recipe: &'a Recipe,
    pub recipe_path: &'a Path,
    pub profile: &'a ApplicationProfile,
    pub engine: &'a EngineManifest,
    pub engine_root: &'a Path,
    pub destination: &'a Path,
    pub wineforge_binary: &'a Path,
    pub launcher_binary: &'a Path,
}

pub fn host_default_format() -> NativeFormat {
    if cfg!(target_os = "macos") {
        NativeFormat::App
    } else {
        NativeFormat::Deb
    }
}

pub fn default_destination(
    format: NativeFormat,
    recipe: &Recipe,
    profile: &ApplicationProfile,
) -> Result<PathBuf> {
    match format {
        NativeFormat::App => {
            let home =
                std::env::var_os("HOME").context("HOME is unavailable; pass --destination")?;
            Ok(PathBuf::from(home)
                .join("Applications/Wineforge")
                .join(format!("{}.app", safe_bundle_name(&profile.name))))
        }
        NativeFormat::Deb => Ok(std::env::current_dir()?.join(format!(
            "{}_{}_amd64.deb",
            debian_package_name(&profile.id)?,
            recipe.version
        ))),
    }
}

pub fn install(request: &InstallRequest<'_>) -> Result<()> {
    require_regular_file(request.wineforge_binary, "Wineforge executable")?;
    require_regular_file(request.launcher_binary, "Wineforge native launcher")?;
    if fs::symlink_metadata(request.destination).is_ok() {
        bail!(
            "native package destination already exists: {}",
            request.destination.display()
        );
    }
    match request.format {
        NativeFormat::App => install_macos_app(request),
        NativeFormat::Deb => build_debian_package(request),
    }
}

pub fn import_macos_app(request: &ImportRequest<'_>) -> Result<()> {
    if !cfg!(target_os = "macos") {
        bail!("macOS application bundles must be imported on macOS");
    }
    require_real_directory(request.source_prefix, "source Wine prefix")?;
    require_regular_file(request.wineforge_binary, "Wineforge executable")?;
    require_regular_file(request.launcher_binary, "Wineforge native launcher")?;
    if !request.destination.is_absolute()
        || request.destination.extension() != Some(OsStr::new("app"))
    {
        bail!("macOS application destination must be an absolute .app path");
    }
    if fs::symlink_metadata(request.destination).is_ok() {
        bail!(
            "native package destination already exists: {}",
            request.destination.display()
        );
    }
    let parent = request
        .destination
        .parent()
        .context("application destination has no parent directory")?;
    ensure_real_directory(parent)?;
    let staging = staging_path(request.destination)?;
    fs::create_dir(&staging)?;
    let result = (|| -> Result<()> {
        let contents = staging.join("Contents");
        let macos = contents.join("MacOS");
        let resources = contents.join("Resources");
        let engine_destination = contents.join("Frameworks/WineEngine");
        let prefix_destination = contents.join("WinePrefix");
        fs::create_dir_all(&macos)?;
        fs::create_dir_all(&resources)?;
        fs::create_dir_all(contents.join("Frameworks"))?;
        copy_executable(request.wineforge_binary, &macos.join("wineforge"))?;
        copy_executable(request.launcher_binary, &macos.join("wineforge-launcher"))?;
        copy_tree(
            &resolved_engine_root(request.engine, request.engine_root)?,
            &engine_destination,
        )?;
        copy_imported_prefix(request.source_prefix, &prefix_destination)?;

        let mut imported_profile = request.profile.clone();
        imported_profile.prefix = prefix_destination.clone();
        adopt_imported_instance(&imported_profile, request.engine, &engine_destination)?;

        let executable =
            windows_path_to_prefix(&imported_profile.prefix, &imported_profile.executable)?;
        write_icon_assets(
            &executable,
            &resources.join("AppIcon.png"),
            Some(&resources.join("AppIcon.icns")),
        )?;
        let mut final_profile = request.profile.clone();
        final_profile.prefix = request.destination.join("Contents/WinePrefix");
        write_toml(&resources.join("profile.toml"), &final_profile)?;
        write_toml(&resources.join("engine.toml"), request.engine)?;
        copy_regular_file(request.recipe_path, &resources.join("recipe.toml"))?;
        fs::write(
            contents.join("Info.plist"),
            macos_info_plist(request.recipe, request.profile),
        )?;
        fs::rename(&staging, request.destination).with_context(|| {
            format!(
                "failed to commit imported macOS application {}",
                request.destination.display()
            )
        })?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&staging);
    }
    result?;
    println!(
        "imported macOS application {}",
        request.destination.display()
    );
    Ok(())
}

fn install_macos_app(request: &InstallRequest<'_>) -> Result<()> {
    if !cfg!(target_os = "macos") {
        bail!("macOS application bundles must be built on macOS");
    }
    if !request.destination.is_absolute() {
        bail!("macOS application destination must be absolute");
    }
    if request.destination.extension() != Some(OsStr::new("app")) {
        bail!("macOS application destination must end in .app");
    }
    let parent = request
        .destination
        .parent()
        .context("application destination has no parent directory")?;
    ensure_real_directory(parent)?;
    let staging = staging_path(request.destination)?;
    fs::create_dir(&staging)?;
    let result = (|| -> Result<()> {
        let contents = staging.join("Contents");
        let macos = contents.join("MacOS");
        let resources = contents.join("Resources");
        let engine_destination = contents.join("Frameworks/WineEngine");
        fs::create_dir_all(&macos)?;
        fs::create_dir_all(&resources)?;
        fs::create_dir_all(contents.join("Frameworks"))?;
        copy_executable(request.wineforge_binary, &macos.join("wineforge"))?;
        copy_executable(request.launcher_binary, &macos.join("wineforge-launcher"))?;
        copy_tree(
            &resolved_engine_root(request.engine, request.engine_root)?,
            &engine_destination,
        )?;

        let mut install_profile = request.profile.clone();
        install_profile.prefix = staging.join("Contents/WinePrefix");
        recipe_executor::install(
            request.recipe,
            request.recipe_path,
            &install_profile,
            request.engine,
            request.engine_root,
            request.winetricks_command,
            request.cache,
            request.accept_license,
        )?;
        shutdown_wineserver(&install_profile, request.engine, request.engine_root)?;

        let executable =
            windows_path_to_prefix(&install_profile.prefix, &install_profile.executable)?;
        write_icon_assets(
            &executable,
            &resources.join("AppIcon.png"),
            Some(&resources.join("AppIcon.icns")),
        )?;
        let mut final_profile = request.profile.clone();
        final_profile.prefix = request.destination.join("Contents/WinePrefix");
        write_toml(&resources.join("profile.toml"), &final_profile)?;
        write_toml(&resources.join("engine.toml"), request.engine)?;
        copy_regular_file(request.recipe_path, &resources.join("recipe.toml"))?;
        fs::write(
            contents.join("Info.plist"),
            macos_info_plist(request.recipe, request.profile),
        )?;
        fs::rename(&staging, request.destination).with_context(|| {
            format!(
                "failed to commit macOS application {}",
                request.destination.display()
            )
        })?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&staging);
    }
    result?;
    println!(
        "installed macOS application {}",
        request.destination.display()
    );
    Ok(())
}

fn build_debian_package(request: &InstallRequest<'_>) -> Result<()> {
    if !cfg!(target_os = "linux") {
        bail!("Debian packages must be built on Linux");
    }
    if request.destination.extension() != Some(OsStr::new("deb")) {
        bail!("Debian package destination must end in .deb");
    }
    let parent = request
        .destination
        .parent()
        .context("Debian package destination has no parent directory")?;
    ensure_real_directory(parent)?;
    let staging = staging_path(request.destination)?;
    fs::create_dir(&staging)?;
    let result = (|| -> Result<()> {
        let package_name = debian_package_name(&request.profile.id)?;
        let app_root_relative = PathBuf::from("opt/wineforge/apps").join(&request.profile.id);
        let app_root = staging.join(&app_root_relative);
        let bin = app_root.join("bin");
        let resources = app_root.join("resources");
        let engine_destination = app_root.join("engine");
        let prefix_template = app_root.join("prefix-template");
        fs::create_dir_all(&bin)?;
        fs::create_dir_all(&resources)?;
        copy_executable(request.wineforge_binary, &bin.join("wineforge"))?;
        copy_executable(request.launcher_binary, &bin.join("wineforge-launcher"))?;
        copy_tree(
            &resolved_engine_root(request.engine, request.engine_root)?,
            &engine_destination,
        )?;

        let mut install_profile = request.profile.clone();
        install_profile.prefix = prefix_template.clone();
        recipe_executor::install(
            request.recipe,
            request.recipe_path,
            &install_profile,
            request.engine,
            request.engine_root,
            request.winetricks_command,
            request.cache,
            request.accept_license,
        )?;
        shutdown_wineserver(&install_profile, request.engine, request.engine_root)?;
        let executable =
            windows_path_to_prefix(&install_profile.prefix, &install_profile.executable)?;
        write_icon_assets(&executable, &resources.join("AppIcon.png"), None)?;
        write_toml(&resources.join("profile.toml"), &install_profile)?;
        write_toml(&resources.join("engine.toml"), request.engine)?;
        copy_regular_file(request.recipe_path, &resources.join("recipe.toml"))?;

        let desktop_relative =
            PathBuf::from("usr/share/applications").join(format!("{package_name}.desktop"));
        let desktop = staging.join(&desktop_relative);
        fs::create_dir_all(desktop.parent().context("desktop file has no parent")?)?;
        let installed_root = Path::new("/opt/wineforge/apps").join(&request.profile.id);
        fs::write(
            &desktop,
            linux_desktop_entry(request.profile, &installed_root),
        )?;

        let control = debian_control(
            &package_name,
            &request.recipe.version,
            request.profile,
            tree_size_kib(&staging)?,
        )?;
        let control_tar = tar_gzip_single("control", control.as_bytes(), 0o644)?;
        let data_tar = tar_gzip_tree(&staging)?;
        write_deb(request.destination, &control_tar, &data_tar)?;
        Ok(())
    })();
    let _ = fs::remove_dir_all(&staging);
    result?;
    println!("built Debian package {}", request.destination.display());
    Ok(())
}

fn staging_path(destination: &Path) -> Result<PathBuf> {
    let parent = destination.parent().context("destination has no parent")?;
    let filename = destination
        .file_name()
        .and_then(OsStr::to_str)
        .context("destination filename is not UTF-8")?;
    let staging = parent.join(format!(
        ".{filename}.wineforge-{}.staging",
        std::process::id()
    ));
    if fs::symlink_metadata(&staging).is_ok() {
        bail!(
            "native package staging path already exists: {}",
            staging.display()
        );
    }
    Ok(staging)
}

fn macos_info_plist(recipe: &Recipe, profile: &ApplicationProfile) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
           <key>CFBundleDevelopmentRegion</key><string>en</string>\n\
           <key>CFBundleDisplayName</key><string>{}</string>\n\
           <key>CFBundleExecutable</key><string>wineforge-launcher</string>\n\
           <key>CFBundleIconFile</key><string>AppIcon</string>\n\
           <key>CFBundleIdentifier</key><string>{}</string>\n\
           <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>\n\
           <key>CFBundleName</key><string>{}</string>\n\
           <key>CFBundlePackageType</key><string>APPL</string>\n\
           <key>CFBundleShortVersionString</key><string>{}</string>\n\
           <key>CFBundleVersion</key><string>{}</string>\n\
           <key>LSMinimumSystemVersion</key><string>13.0</string>\n\
           <key>NSHighResolutionCapable</key><true/>\n\
         </dict>\n\
         </plist>\n",
        xml_escape(&profile.name),
        xml_escape(&recipe.id),
        xml_escape(&profile.name),
        xml_escape(&recipe.version),
        macos_bundle_version(&recipe.version)
    )
}

fn linux_desktop_entry(profile: &ApplicationProfile, installed_root: &Path) -> String {
    format!(
        "[Desktop Entry]\nType=Application\nName={}\nExec={}/bin/wineforge-launcher\nIcon={}/resources/AppIcon.png\nTerminal=false\nCategories=Utility;\nStartupNotify=true\n",
        desktop_escape(&profile.name),
        installed_root.display(),
        installed_root.display()
    )
}

fn debian_package_name(id: &str) -> Result<String> {
    let name = format!("wineforge-app-{}", id.replace('.', "-"));
    if name.bytes().all(|byte| {
        byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'+' | b'-' | b'.')
    }) {
        Ok(name)
    } else {
        bail!("profile id cannot be represented as a Debian package name")
    }
}

fn safe_bundle_name(name: &str) -> String {
    let normalized: String = name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, ' ' | '-' | '_' | '.') {
                character
            } else {
                '-'
            }
        })
        .collect();
    let normalized = normalized.trim_matches([' ', '.']);
    if normalized.is_empty() {
        "Wineforge App".to_owned()
    } else {
        normalized.to_owned()
    }
}

fn macos_bundle_version(version: &str) -> String {
    let mut components = version
        .split(|character: char| !character.is_ascii_digit())
        .filter(|component| !component.is_empty())
        .take(3)
        .map(|component| component.parse::<u64>().unwrap_or(0).to_string())
        .collect::<Vec<_>>();
    if components.is_empty() {
        components.push("1".into());
    }
    components.join(".")
}

fn debian_control(
    package_name: &str,
    version: &str,
    profile: &ApplicationProfile,
    installed_size: u64,
) -> Result<String> {
    if version.is_empty()
        || !version.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'+' | b'~' | b'-' | b':')
        })
    {
        bail!("recipe version cannot be represented as a Debian version");
    }
    Ok(format!(
        "Package: {package_name}\nVersion: {version}\nSection: utils\nPriority: optional\nArchitecture: amd64\nInstalled-Size: {installed_size}\nMaintainer: Wineforge <noreply@wineforge.local>\nDescription: {} managed by Wineforge\n",
        control_escape(&profile.name)
    ))
}

fn write_icon_assets(executable: &Path, png: &Path, icns: Option<&Path>) -> Result<()> {
    let images = extract_pe_icons(executable).unwrap_or_default();
    let mut images = if images.is_empty() {
        vec![fallback_icon(256)]
    } else {
        images
    };
    if images
        .iter()
        .map(ico::IconImage::width)
        .max()
        .is_some_and(|width| width < 256)
    {
        let largest = images
            .iter()
            .max_by_key(|image| image.width() * image.height())
            .context("icon image selection unexpectedly failed")?;
        images.push(resize_icon_nearest(largest, 256));
    }
    let largest = images
        .iter()
        .max_by_key(|image| image.width() * image.height())
        .context("icon image selection unexpectedly failed")?;
    largest.write_png(File::create(png)?)?;
    if let Some(path) = icns {
        let mut family = IconFamily::new();
        let mut sizes = BTreeSet::new();
        for image in &images {
            if image.width() != image.height() || !sizes.insert(image.width()) {
                continue;
            }
            let converted = IcnsImage::from_data(
                PixelFormat::RGBA,
                image.width(),
                image.height(),
                image.rgba_data().to_vec(),
            )?;
            let _ = family.add_icon(&converted);
        }
        if family.is_empty() {
            let image = fallback_icon(256);
            let converted = IcnsImage::from_data(
                PixelFormat::RGBA,
                image.width(),
                image.height(),
                image.rgba_data().to_vec(),
            )?;
            family.add_icon(&converted)?;
        }
        family.write(File::create(path)?)?;
    }
    Ok(())
}

fn extract_pe_icons(executable: &Path) -> Result<Vec<ico::IconImage>> {
    const MAX_EXECUTABLE_BYTES: u64 = 1024 * 1024 * 1024;
    let metadata = fs::symlink_metadata(executable)?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.len() > MAX_EXECUTABLE_BYTES
    {
        bail!("installed executable is not a bounded regular file");
    }
    let bytes = fs::read(executable)?;
    let pe = PeFile::from_bytes(&bytes).context("installed executable is not a valid PE file")?;
    let resources = pe.resources().context("PE file has no resources")?;
    let mut best = Vec::new();
    let mut best_area = 0_u32;
    for result in resources.icons() {
        let (_, group) = result.context("invalid PE icon group")?;
        let mut ico_bytes = Vec::new();
        group.write(&mut ico_bytes)?;
        let directory = ico::IconDir::read(Cursor::new(ico_bytes))?;
        let mut decoded = Vec::new();
        for entry in directory.entries() {
            if let Ok(image) = entry.decode() {
                decoded.push(image);
            }
        }
        let area = decoded
            .iter()
            .map(|image| image.width() * image.height())
            .max()
            .unwrap_or(0);
        if area > best_area {
            best_area = area;
            best = decoded;
        }
    }
    Ok(best)
}

fn fallback_icon(size: u32) -> ico::IconImage {
    let mut rgba = vec![0_u8; (size * size * 4) as usize];
    for y in 0..size {
        for x in 0..size {
            let offset = ((y * size + x) * 4) as usize;
            rgba[offset] = 36;
            rgba[offset + 1] = 42;
            rgba[offset + 2] = 56;
            rgba[offset + 3] = 255;
            let stroke = size / 16;
            let left = x.abs_diff(size / 4) <= stroke && y > size / 4 && y < 3 * size / 4;
            let right = x.abs_diff(3 * size / 4) <= stroke && y > size / 4 && y < 3 * size / 4;
            let diagonal = x.abs_diff(y) <= stroke || x.abs_diff(size - 1 - y) <= stroke;
            if left || right || (diagonal && y > size / 2) {
                rgba[offset] = 216;
                rgba[offset + 1] = 76;
                rgba[offset + 2] = 83;
            }
        }
    }
    ico::IconImage::from_rgba_data(size, size, rgba)
}

fn resize_icon_nearest(source: &ico::IconImage, size: u32) -> ico::IconImage {
    let mut rgba = vec![0_u8; (size * size * 4) as usize];
    for y in 0..size {
        for x in 0..size {
            let source_x = x * source.width() / size;
            let source_y = y * source.height() / size;
            let source_offset = ((source_y * source.width() + source_x) * 4) as usize;
            let destination_offset = ((y * size + x) * 4) as usize;
            rgba[destination_offset..destination_offset + 4]
                .copy_from_slice(&source.rgba_data()[source_offset..source_offset + 4]);
        }
    }
    ico::IconImage::from_rgba_data(size, size, rgba)
}

fn write_toml<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let text = toml::to_string_pretty(value)?;
    fs::write(path, text).with_context(|| format!("failed to write {}", path.display()))
}

fn copy_executable(source: &Path, destination: &Path) -> Result<()> {
    copy_regular_file(source, destination)?;
    let permissions = fs::metadata(source)?.permissions();
    fs::set_permissions(destination, permissions)?;
    Ok(())
}

fn copy_regular_file(source: &Path, destination: &Path) -> Result<()> {
    require_regular_file(source, "package input file")?;
    fs::copy(source, destination).with_context(|| {
        format!(
            "failed to copy {} to {}",
            source.display(),
            destination.display()
        )
    })?;
    Ok(())
}

fn copy_tree(source: &Path, destination: &Path) -> Result<()> {
    require_real_directory(source, "engine root")?;
    if fs::symlink_metadata(destination).is_ok() {
        bail!("tree destination already exists: {}", destination.display());
    }
    fs::create_dir(destination)?;
    let result = copy_tree_contents(source, source, destination);
    if result.is_err() {
        let _ = fs::remove_dir_all(destination);
    }
    result
}

fn copy_imported_prefix(source: &Path, destination: &Path) -> Result<()> {
    require_real_directory(source, "source Wine prefix")?;
    if fs::symlink_metadata(destination).is_ok() {
        bail!(
            "prefix destination already exists: {}",
            destination.display()
        );
    }
    fs::create_dir(destination)?;
    let result = copy_imported_prefix_contents(source, source, destination);
    if result.is_err() {
        let _ = fs::remove_dir_all(destination);
    }
    result
}

fn copy_imported_prefix_contents(root: &Path, source: &Path, destination: &Path) -> Result<()> {
    let mut entries = fs::read_dir(source)?.collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        let metadata = fs::symlink_metadata(&source_path)?;
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            fs::create_dir(&destination_path)?;
            fs::set_permissions(&destination_path, metadata.permissions())?;
            copy_imported_prefix_contents(root, &source_path, &destination_path)?;
        } else if metadata.is_file() && !metadata.file_type().is_symlink() {
            fs::copy(&source_path, &destination_path)?;
            fs::set_permissions(&destination_path, metadata.permissions())?;
        } else if metadata.file_type().is_symlink() {
            let target = fs::read_link(&source_path)?;
            if relative_link_stays_in_root(root, &source_path, &target) {
                #[cfg(unix)]
                std::os::unix::fs::symlink(target, destination_path)?;
                #[cfg(not(unix))]
                bail!("prefix links are unsupported on this platform");
            } else if source_path.starts_with(root.join("drive_c/users")) {
                fs::create_dir(&destination_path)?;
            }
        } else {
            bail!(
                "source prefix contains a special file: {}",
                source_path.display()
            );
        }
    }
    Ok(())
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
            fs::set_permissions(&destination_path, metadata.permissions())?;
            copy_tree_contents(root, &source_path, &destination_path)?;
        } else if metadata.is_file() && !metadata.file_type().is_symlink() {
            fs::copy(&source_path, &destination_path)?;
            fs::set_permissions(&destination_path, metadata.permissions())?;
        } else if metadata.file_type().is_symlink() {
            let target = fs::read_link(&source_path)?;
            if !relative_link_stays_in_root(root, &source_path, &target) {
                bail!(
                    "engine contains an escaping link: {} -> {}",
                    source_path.display(),
                    target.display()
                );
            }
            #[cfg(unix)]
            std::os::unix::fs::symlink(target, destination_path)?;
            #[cfg(not(unix))]
            bail!("engine links are unsupported on this platform");
        } else {
            bail!("engine contains a special file: {}", source_path.display());
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

fn tree_size_kib(root: &Path) -> Result<u64> {
    fn visit(path: &Path, total: &mut u64) -> Result<()> {
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let metadata = fs::symlink_metadata(entry.path())?;
            if metadata.is_dir() && !metadata.file_type().is_symlink() {
                visit(&entry.path(), total)?;
            } else if metadata.is_file() && !metadata.file_type().is_symlink() {
                *total = total.saturating_add(metadata.len());
            }
        }
        Ok(())
    }
    let mut bytes = 0;
    visit(root, &mut bytes)?;
    Ok(bytes.div_ceil(1024))
}

fn tar_gzip_single(path: &str, bytes: &[u8], mode: u32) -> Result<Vec<u8>> {
    let encoder = GzEncoder::new(Vec::new(), Compression::best());
    let mut archive = tar::Builder::new(encoder);
    let mut header = tar::Header::new_gnu();
    header.set_size(bytes.len() as u64);
    header.set_mode(mode);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_cksum();
    archive.append_data(&mut header, path, bytes)?;
    let encoder = archive.into_inner()?;
    Ok(encoder.finish()?)
}

fn tar_gzip_tree(root: &Path) -> Result<Vec<u8>> {
    let encoder = GzEncoder::new(Vec::new(), Compression::best());
    let mut archive = tar::Builder::new(encoder);
    append_tree(&mut archive, root, root)?;
    let encoder = archive.into_inner()?;
    Ok(encoder.finish()?)
}

fn append_tree<W: Write>(
    archive: &mut tar::Builder<W>,
    root: &Path,
    directory: &Path,
) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut entries = fs::read_dir(directory)?.collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let relative = path.strip_prefix(root)?;
        let metadata = fs::symlink_metadata(&path)?;
        let mut header = tar::Header::new_gnu();
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        header.set_mode(metadata.permissions().mode() & 0o7777);
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            header.set_entry_type(tar::EntryType::Directory);
            header.set_size(0);
            header.set_cksum();
            archive.append_data(&mut header, relative, std::io::empty())?;
            append_tree(archive, root, &path)?;
        } else if metadata.is_file() && !metadata.file_type().is_symlink() {
            header.set_entry_type(tar::EntryType::Regular);
            header.set_size(metadata.len());
            header.set_cksum();
            archive.append_data(&mut header, relative, File::open(&path)?)?;
        } else if metadata.file_type().is_symlink() {
            let target = fs::read_link(&path)?;
            if !relative_link_stays_in_root(root, &path, &target) {
                bail!("package data contains an escaping symbolic link");
            }
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_size(0);
            header.set_link_name(target)?;
            header.set_cksum();
            archive.append_data(&mut header, relative, std::io::empty())?;
        } else {
            bail!("package data contains a special file: {}", path.display());
        }
    }
    Ok(())
}

fn write_deb(destination: &Path, control_tar: &[u8], data_tar: &[u8]) -> Result<()> {
    let temporary = destination.with_extension(format!("deb.{}.part", std::process::id()));
    let mut output = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)?;
    let result = (|| -> Result<()> {
        output.write_all(b"!<arch>\n")?;
        write_ar_member(&mut output, "debian-binary", b"2.0\n", 0o100644)?;
        write_ar_member(&mut output, "control.tar.gz", control_tar, 0o100644)?;
        write_ar_member(&mut output, "data.tar.gz", data_tar, 0o100644)?;
        output.sync_all()?;
        Ok(())
    })();
    drop(output);
    if let Err(error) = result {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    fs::rename(&temporary, destination).context("failed to commit Debian package")
}

fn write_ar_member<W: Write>(writer: &mut W, name: &str, bytes: &[u8], mode: u32) -> Result<()> {
    if name.len() > 15 {
        bail!("ar member name is too long");
    }
    let header = format!(
        "{:<16}{:<12}{:<6}{:<6}{:<8o}{:<10}`\n",
        format!("{name}/"),
        0,
        0,
        0,
        mode,
        bytes.len()
    );
    if header.len() != 60 {
        bail!("invalid ar member header");
    }
    writer.write_all(header.as_bytes())?;
    writer.write_all(bytes)?;
    if bytes.len() % 2 != 0 {
        writer.write_all(b"\n")?;
    }
    Ok(())
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn desktop_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('\n', "\\n")
}

fn control_escape(value: &str) -> String {
    value.replace(['\r', '\n'], " ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recipe::{
        Application, InstallStep, License, LicenseAcceptance, Postcondition, RuntimeAccess, Source,
        Variant, VariantEngine,
    };
    use std::collections::BTreeMap;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;
    use wineforge_core::{
        Artifact, ArtifactSource, EngineSelection, Environment, IsolationMode, IsolationPolicy,
        Platform, Sha256Digest, Translation,
    };
    use zip::write::SimpleFileOptions;

    #[test]
    fn ar_member_has_a_valid_fixed_width_header() {
        let mut bytes = Vec::new();
        write_ar_member(&mut bytes, "debian-binary", b"2.0\n", 0o100644).unwrap();
        assert_eq!(&bytes[58..60], b"`\n");
        assert_eq!(bytes.len(), 64);
    }

    #[test]
    fn package_names_and_metadata_are_escaped() {
        assert_eq!(
            debian_package_name("org.example.application").unwrap(),
            "wineforge-app-org-example-application"
        );
        assert_eq!(xml_escape("A&B<\""), "A&amp;B&lt;&quot;");
        assert_eq!(desktop_escape("Line\\Name\nNext"), "Line\\\\Name\\nNext");
        assert_eq!(macos_bundle_version("7.8-p1"), "7.8.1");
    }

    #[test]
    fn internal_links_are_allowed_but_escaping_links_are_rejected() {
        let root = Path::new("/tmp/engine");
        assert!(relative_link_stays_in_root(
            root,
            &root.join("lib/link"),
            Path::new("../bin/tool")
        ));
        assert!(!relative_link_stays_in_root(
            root,
            &root.join("link"),
            Path::new("../outside")
        ));
    }

    #[cfg(unix)]
    #[test]
    fn imported_prefix_copy_preserves_internal_links_and_drops_host_links() {
        use std::os::unix::fs::symlink;

        let temp = tempdir().unwrap();
        let source = temp.path().join("source");
        let destination = temp.path().join("destination");
        let user = source.join("drive_c/users/example");
        fs::create_dir_all(&user).unwrap();
        fs::create_dir(source.join("dosdevices")).unwrap();
        fs::write(source.join("system.reg"), b"registry").unwrap();
        symlink("../drive_c", source.join("dosdevices/c:")).unwrap();
        symlink("/", source.join("dosdevices/z:")).unwrap();
        symlink(temp.path(), user.join("Documents")).unwrap();

        copy_imported_prefix(&source, &destination).unwrap();

        assert_eq!(
            fs::read_link(destination.join("dosdevices/c:")).unwrap(),
            PathBuf::from("../drive_c")
        );
        assert!(!destination.join("dosdevices/z:").exists());
        assert!(destination.join("drive_c/users/example/Documents").is_dir());
        assert_eq!(
            fs::read(destination.join("system.reg")).unwrap(),
            b"registry"
        );
    }

    #[test]
    fn builds_the_hosts_registry_free_native_package() {
        let temp = tempdir().unwrap();
        let engine_root = temp.path().join("engine");
        fs::create_dir_all(engine_root.join("bin")).unwrap();
        let wine = engine_root.join("bin/wine");
        fs::write(
            &wine,
            "#!/bin/sh\nmkdir -p \"$WINEPREFIX/.wineforge\" \"$WINEPREFIX/drive_c\" \"$WINEPREFIX/dosdevices\"\nln -s ../drive_c \"$WINEPREFIX/dosdevices/c:\" 2>/dev/null || true\n",
        )
        .unwrap();
        fs::set_permissions(&wine, fs::Permissions::from_mode(0o755)).unwrap();
        let wineserver = engine_root.join("bin/wineserver");
        fs::write(&wineserver, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&wineserver, fs::Permissions::from_mode(0o755)).unwrap();
        let runtime = temp.path().join("wineforge");
        let launcher = temp.path().join("wineforge-launcher");
        fs::write(&runtime, b"runtime").unwrap();
        fs::write(&launcher, b"launcher").unwrap();
        fs::set_permissions(&runtime, fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(&launcher, fs::Permissions::from_mode(0o755)).unwrap();

        let source_path = temp.path().join("application.zip");
        let source_file = File::create(&source_path).unwrap();
        let mut zip = zip::ZipWriter::new(source_file);
        zip.start_file("app.exe", SimpleFileOptions::default())
            .unwrap();
        zip.write_all(b"MZ-not-a-complete-pe").unwrap();
        zip.finish().unwrap();
        let recipe_path = temp.path().join("recipe.toml");
        let platform = if cfg!(target_os = "macos") {
            Platform::MacosX86_64
        } else {
            Platform::LinuxX86_64
        };
        let platform_name = if cfg!(target_os = "macos") {
            "macos"
        } else {
            "linux"
        };
        let engine = EngineManifest {
            schema_version: 1,
            id: "test-engine".into(),
            platform,
            host_architecture: std::env::consts::ARCH.into(),
            translation: Translation::Native,
            artifact: Artifact {
                source: ArtifactSource::UserSupplied,
                sha256: Sha256Digest("0".repeat(64)),
            },
            wine_binary: PathBuf::from("bin/wine"),
            environment: Environment::default(),
            license: wineforge_core::License {
                name: "Test".into(),
                url: "https://example.invalid/license".parse().unwrap(),
                acceptance_required: false,
            },
        };
        let recipe = Recipe {
            schema_version: 1,
            id: "org.example.packaged-app".into(),
            version: "1.0.0".into(),
            name: "Packaged App".into(),
            summary: "Test package".into(),
            homepage: None,
            license: License {
                spdx: "MIT".into(),
                notice_url: None,
                acceptance: LicenseAcceptance::None,
            },
            application: Application {
                executable: "C:\\Payload\\app.exe".into(),
                arguments: Vec::new(),
                working_directory: None,
            },
            variants: vec![Variant {
                platform: platform_name.into(),
                host_architecture: std::env::consts::ARCH.into(),
                translation: "native".into(),
                engine: VariantEngine {
                    family: "test".into(),
                    version: "1".into(),
                    features: Vec::new(),
                },
            }],
            runtime_access: RuntimeAccess {
                network: "none".into(),
                folders: Vec::new(),
            },
            sources: vec![Source::Repository {
                id: "application".into(),
                path: PathBuf::from("application.zip"),
                sha256: crate::sha256_file(&source_path).unwrap(),
            }],
            install: vec![
                InstallStep::CreateDirectory {
                    path: "C:\\Payload".into(),
                },
                InstallStep::ExtractArchive {
                    source: "application".into(),
                    destination: "C:\\Payload".into(),
                    strip_components: 0,
                },
            ],
            verify: vec![Postcondition::FileExists {
                path: "C:\\Payload\\app.exe".into(),
                sha256: None,
            }],
        };
        fs::write(&recipe_path, toml::to_string_pretty(&recipe).unwrap()).unwrap();
        let profile = ApplicationProfile {
            schema_version: 1,
            id: "org.example.packaged-app".into(),
            name: "Packaged App".into(),
            prefix: temp.path().join("unused-prefix"),
            executable: "C:\\Payload\\app.exe".into(),
            arguments: Vec::new(),
            engines: BTreeMap::from([(
                platform,
                EngineSelection {
                    id: engine.id.clone(),
                },
            )]),
            environment: Environment::default(),
            mappings: Vec::new(),
            isolation: IsolationPolicy {
                mode: IsolationMode::Disabled,
            },
        };
        let cache = temp.path().join("cache");

        #[cfg(target_os = "macos")]
        {
            let app = temp.path().join("Packaged App.app");
            install(&InstallRequest {
                recipe: &recipe,
                recipe_path: &recipe_path,
                profile: &profile,
                engine: &engine,
                engine_root: &engine_root,
                destination: &app,
                format: NativeFormat::App,
                wineforge_binary: &runtime,
                launcher_binary: &launcher,
                winetricks_command: Path::new("winetricks"),
                cache: &cache,
                accept_license: false,
            })
            .unwrap();
            assert!(app.join("Contents/Info.plist").is_file());
            assert!(app.join("Contents/Resources/AppIcon.icns").is_file());
            assert!(
                app.join("Contents/Frameworks/WineEngine/bin/wine")
                    .is_file()
            );
            let bundled: ApplicationProfile = toml::from_str(
                &fs::read_to_string(app.join("Contents/Resources/profile.toml")).unwrap(),
            )
            .unwrap();
            assert_eq!(bundled.prefix, app.join("Contents/WinePrefix"));
        }

        #[cfg(target_os = "linux")]
        {
            let deb = temp.path().join("packaged-app.deb");
            install(&InstallRequest {
                recipe: &recipe,
                recipe_path: &recipe_path,
                profile: &profile,
                engine: &engine,
                engine_root: &engine_root,
                destination: &deb,
                format: NativeFormat::Deb,
                wineforge_binary: &runtime,
                launcher_binary: &launcher,
                winetricks_command: Path::new("winetricks"),
                cache: &cache,
                accept_license: false,
            })
            .unwrap();
            let bytes = fs::read(deb).unwrap();
            assert_eq!(&bytes[..8], b"!<arch>\n");
            assert!(bytes.windows(14).any(|window| window == b"control.tar.gz"));
            assert!(bytes.windows(11).any(|window| window == b"data.tar.gz"));
        }
    }
}
