use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result, bail};
use regex::Regex;
use roxmltree::Document;
use zip::ZipArchive;

use crate::recipe::InstallerType;

const MAX_NUSPEC_BYTES: u64 = 1024 * 1024;
const MAX_SCRIPT_BYTES: u64 = 2 * 1024 * 1024;
const MAX_ZIP_ENTRIES: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageMetadata {
    pub id: String,
    pub version: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Translation {
    pub metadata: PackageMetadata,
    pub url: String,
    pub sha256: String,
    pub installer_type: InstallerType,
    pub arguments: Vec<String>,
    pub success_exit_codes: Vec<i32>,
}

pub fn translate(path: &Path, expected_id: &str, expected_version: &str) -> Result<Translation> {
    let (metadata, script) = read_package(path)?;
    if !metadata.id.eq_ignore_ascii_case(expected_id) || metadata.version != expected_version {
        bail!(
            "Chocolatey package identity mismatch: expected {expected_id} {expected_version}, found {} {}",
            metadata.id,
            metadata.version
        );
    }
    let tables = parse_hashtables(&script)?;
    let invocation = Regex::new(r"(?im)^\s*Install-ChocolateyPackage\s+@(\w+)\s*(?:#.*)?$")
        .expect("static regex");
    let calls = invocation.captures_iter(&script).collect::<Vec<_>>();
    if calls.len() != 1 {
        bail!(
            "translation requires exactly one direct Install-ChocolateyPackage @hashtable invocation"
        );
    }
    let table_name = calls[0]
        .get(1)
        .expect("capture")
        .as_str()
        .to_ascii_lowercase();
    let table = tables.get(&table_name).with_context(|| {
        format!("invoked Chocolatey hashtable ${table_name} was not a static literal")
    })?;

    let url = string_value(
        table.get("url64bit").or_else(|| table.get("url")),
        "url/url64bit",
    )?;
    let digest = string_value(
        table.get("checksum64").or_else(|| table.get("checksum")),
        "checksum/checksum64",
    )?
    .to_ascii_lowercase();
    let checksum_type = string_value(
        table
            .get("checksumtype64")
            .or_else(|| table.get("checksumtype")),
        "checksumType/checksumType64",
    )?;
    if !checksum_type.eq_ignore_ascii_case("sha256") {
        bail!("only SHA-256 Chocolatey installer checksums are accepted");
    }
    if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("Chocolatey installer checksum is not a SHA-256 digest");
    }
    let parsed_url = reqwest::Url::parse(&url).context("invalid Chocolatey installer URL")?;
    if parsed_url.scheme() != "https"
        || parsed_url.host_str().is_none()
        || !parsed_url.username().is_empty()
        || parsed_url.password().is_some()
    {
        bail!("Chocolatey installer URL must use HTTPS without credentials");
    }
    let file_type = string_value(table.get("filetype"), "fileType")?;
    let installer_type = match file_type.to_ascii_lowercase().as_str() {
        "exe" => InstallerType::Exe,
        "msi" => InstallerType::Msi,
        other => bail!("unsupported Chocolatey fileType {other}"),
    };
    let arguments = match table.get("silentargs") {
        None => Vec::new(),
        Some(Value::String(value)) => windows_command_line_split(value)?,
        Some(Value::InterpolatedString(value)) => {
            let expanded = expand_safe_silent_args(value, &metadata)?;
            windows_command_line_split(&expanded)?
        }
        Some(_) => bail!("Chocolatey silentArgs must be a static string"),
    };
    let success_exit_codes = match table.get("validexitcodes") {
        None => vec![0],
        Some(Value::Integers(values)) if !values.is_empty() => values.clone(),
        Some(_) => bail!("Chocolatey validExitCodes must be a static integer array"),
    };
    if success_exit_codes
        .iter()
        .any(|value| !(0..=65535).contains(value))
    {
        bail!("Chocolatey validExitCodes contains an out-of-range value");
    }

    Ok(Translation {
        metadata,
        url,
        sha256: digest,
        installer_type,
        arguments,
        success_exit_codes,
    })
}

fn read_package(path: &Path) -> Result<(PackageMetadata, String)> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect nupkg {}", path.display()))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        bail!("nupkg must be a regular file: {}", path.display());
    }
    let file = File::open(path)?;
    let mut archive = ZipArchive::new(file).context("invalid nupkg ZIP container")?;
    if archive.len() > MAX_ZIP_ENTRIES {
        bail!("nupkg contains too many ZIP entries");
    }
    let mut nuspec = None;
    let mut script = None;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        let name = entry.name().replace('\\', "/");
        let lower = name.to_ascii_lowercase();
        if lower.ends_with(".nuspec") && !lower.contains('/') {
            if nuspec.is_some() {
                bail!("nupkg contains multiple root nuspec files");
            }
            nuspec = Some(read_limited(&mut entry, MAX_NUSPEC_BYTES, "nuspec")?);
        } else if lower == "tools/chocolateyinstall.ps1" {
            if script.is_some() {
                bail!("nupkg contains duplicate chocolateyInstall.ps1 files");
            }
            script = Some(read_limited(
                &mut entry,
                MAX_SCRIPT_BYTES,
                "Chocolatey script",
            )?);
        }
    }
    let nuspec = String::from_utf8(nuspec.context("nupkg has no root nuspec")?)
        .context("nuspec is not UTF-8")?;
    let script = decode_script(script.context("nupkg has no tools/chocolateyInstall.ps1")?)?;
    Ok((parse_nuspec(&nuspec)?, script))
}

fn read_limited<R: Read>(reader: &mut R, limit: u64, label: &str) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let copied = reader.take(limit + 1).read_to_end(&mut bytes)?;
    if copied as u64 > limit {
        bail!("{label} exceeds its size limit");
    }
    Ok(bytes)
}

fn decode_script(bytes: Vec<u8>) -> Result<String> {
    if bytes.starts_with(&[0xff, 0xfe]) {
        let body = &bytes[2..];
        if body.len() % 2 != 0 {
            bail!("invalid UTF-16LE Chocolatey script");
        }
        let units = body
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]));
        return String::from_utf16(&units.collect::<Vec<_>>())
            .context("invalid UTF-16LE Chocolatey script");
    }
    String::from_utf8(bytes).context("Chocolatey script is not UTF-8")
}

fn parse_nuspec(xml: &str) -> Result<PackageMetadata> {
    let document = Document::parse(xml).context("invalid nuspec XML")?;
    let metadata = document
        .descendants()
        .find(|node| node.has_tag_name("metadata"))
        .context("nuspec has no metadata element")?;
    let field = |name: &str| {
        metadata
            .children()
            .find(|node| node.is_element() && node.tag_name().name() == name)
            .and_then(|node| node.text())
            .map(str::trim)
            .filter(|value| !value.is_empty())
    };
    Ok(PackageMetadata {
        id: field("id").context("nuspec metadata has no id")?.to_owned(),
        version: field("version")
            .context("nuspec metadata has no version")?
            .to_owned(),
        description: field("description").map(ToOwned::to_owned),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Value {
    String(String),
    InterpolatedString(String),
    Integers(Vec<i32>),
}

fn parse_hashtables(script: &str) -> Result<BTreeMap<String, BTreeMap<String, Value>>> {
    let table_pattern = Regex::new(r"(?is)\$(\w+)\s*=\s*@\{(.*?)\}").expect("static regex");
    let assignment_pattern =
        Regex::new(r"^\s*([A-Za-z][A-Za-z0-9]*)\s*=\s*(.*?)\s*;?\s*$").expect("static regex");
    let mut tables = BTreeMap::new();
    for captures in table_pattern.captures_iter(script) {
        let name = captures
            .get(1)
            .expect("capture")
            .as_str()
            .to_ascii_lowercase();
        if tables.contains_key(&name) {
            bail!("Chocolatey script assigns hashtable ${name} more than once");
        }
        let mut values = BTreeMap::new();
        for raw_line in captures.get(2).expect("capture").as_str().lines() {
            let line = strip_comment(raw_line).trim();
            if line.is_empty() {
                continue;
            }
            let assignment = assignment_pattern.captures(line).with_context(|| {
                format!("unsupported dynamic Chocolatey hashtable line: {line}")
            })?;
            let key = assignment
                .get(1)
                .expect("capture")
                .as_str()
                .to_ascii_lowercase();
            if values
                .insert(
                    key.clone(),
                    parse_literal(assignment.get(2).expect("capture").as_str())?,
                )
                .is_some()
            {
                bail!("duplicate Chocolatey hashtable key {key}");
            }
        }
        tables.insert(name, values);
    }
    Ok(tables)
}

fn strip_comment(line: &str) -> &str {
    let mut single = false;
    let mut double = false;
    let bytes = line.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'\'' if !double => single = !single,
            b'"' if !single => double = !double,
            b'#' if !single && !double => return &line[..index],
            _ => {}
        }
        index += 1;
    }
    line
}

fn parse_literal(value: &str) -> Result<Value> {
    let value = value.trim().trim_end_matches(';').trim();
    if value.len() >= 2 && value.starts_with('\'') && value.ends_with('\'') {
        return Ok(Value::String(value[1..value.len() - 1].replace("''", "'")));
    }
    if value.len() >= 2 && value.starts_with('"') && value.ends_with('"') {
        let content = &value[1..value.len() - 1];
        if content.contains('$') || content.contains('`') {
            return Ok(Value::InterpolatedString(content.to_owned()));
        }
        return Ok(Value::String(content.replace("\"\"", "\"")));
    }
    if value.starts_with("@(") && value.ends_with(')') {
        let body = &value[2..value.len() - 1];
        let integers = body
            .split(',')
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .map(|part| {
                part.parse::<i32>()
                    .context("non-integer Chocolatey array value")
            })
            .collect::<Result<Vec<_>>>()?;
        return Ok(Value::Integers(integers));
    }
    bail!("unsupported dynamic Chocolatey value: {value}")
}

fn expand_safe_silent_args(value: &str, metadata: &PackageMetadata) -> Result<String> {
    let replacements = [
        (r"(?i)\$\(\$env:TEMP\)|\$env:TEMP", "C:\\windows\\temp"),
        (
            r"(?i)\$\(\$env:ChocolateyPackageName\)|\$env:ChocolateyPackageName",
            metadata.id.as_str(),
        ),
        (
            r"(?i)\$\(\$env:ChocolateyPackageVersion\)|\$env:ChocolateyPackageVersion",
            metadata.version.as_str(),
        ),
    ];
    let mut expanded = value.to_owned();
    for (pattern, replacement) in replacements {
        expanded = Regex::new(pattern)
            .expect("static regex")
            .replace_all(&expanded, regex::NoExpand(replacement))
            .into_owned();
    }
    expanded = expanded.replace("`\"", "\"").replace("``", "`");
    if expanded.contains('$') || expanded.contains('`') {
        bail!("Chocolatey silentArgs uses unsupported PowerShell interpolation");
    }
    Ok(expanded)
}

fn string_value(value: Option<&Value>, label: &str) -> Result<String> {
    match value {
        Some(Value::String(value)) if !value.is_empty() => Ok(value.clone()),
        _ => bail!("Chocolatey {label} must be a non-empty static string"),
    }
}

fn windows_command_line_split(input: &str) -> Result<Vec<String>> {
    let chars = input.chars().collect::<Vec<_>>();
    let mut arguments = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut index = 0;
    while index < chars.len() {
        if chars[index].is_whitespace() && !quoted {
            if !current.is_empty() {
                arguments.push(std::mem::take(&mut current));
            }
            index += 1;
            continue;
        }
        if chars[index] == '\\' {
            let start = index;
            while index < chars.len() && chars[index] == '\\' {
                index += 1;
            }
            let count = index - start;
            if index < chars.len() && chars[index] == '"' {
                current.extend(std::iter::repeat_n('\\', count / 2));
                if count % 2 == 0 {
                    quoted = !quoted;
                } else {
                    current.push('"');
                }
                index += 1;
            } else {
                current.extend(std::iter::repeat_n('\\', count));
            }
            continue;
        }
        if chars[index] == '"' {
            quoted = !quoted;
            index += 1;
            continue;
        }
        current.push(chars[index]);
        index += 1;
    }
    if quoted {
        bail!("Chocolatey silentArgs contains an unterminated quote");
    }
    if !current.is_empty() {
        arguments.push(current);
    }
    Ok(arguments)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    fn package(script: &str) -> tempfile::NamedTempFile {
        let file = tempfile::NamedTempFile::new().unwrap();
        let mut archive = zip::ZipWriter::new(file.reopen().unwrap());
        archive
            .start_file("sample.nuspec", SimpleFileOptions::default())
            .unwrap();
        archive
            .write_all(
                br#"<?xml version="1.0"?><package><metadata><id>sample</id><version>1.2.3</version><description>fixture</description></metadata></package>"#,
            )
            .unwrap();
        archive
            .start_file("tools/chocolateyInstall.ps1", SimpleFileOptions::default())
            .unwrap();
        archive.write_all(script.as_bytes()).unwrap();
        archive.finish().unwrap();
        file
    }

    #[test]
    fn translates_static_package_arguments() {
        let script = r#"
$packageArgs = @{
  packageName = 'sample'
  fileType = 'exe'
  url64bit = 'https://vendor.invalid/setup.exe'
  checksum64 = 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'
  checksumType64 = 'sha256'
  silentArgs = '/S /D="C:\Program Files\Sample"'
  validExitCodes = @(0, 3010)
}
Install-ChocolateyPackage @packageArgs
"#;
        let tables = parse_hashtables(script).unwrap();
        let table = tables.get("packageargs").unwrap();
        assert_eq!(
            string_value(table.get("filetype"), "fileType").unwrap(),
            "exe"
        );
        assert_eq!(
            windows_command_line_split(match table.get("silentargs").unwrap() {
                Value::String(value) => value,
                _ => unreachable!(),
            })
            .unwrap(),
            vec!["/S", "/D=C:\\Program Files\\Sample"]
        );
    }

    #[test]
    fn rejects_interpolation() {
        assert!(matches!(
            parse_literal("\"https://$host/setup.exe\"").unwrap(),
            Value::InterpolatedString(_)
        ));
        assert!(
            string_value(
                Some(&parse_literal("\"https://$host/setup.exe\"").unwrap()),
                "url"
            )
            .is_err()
        );
    }

    #[test]
    fn expands_only_known_silent_argument_variables() {
        let metadata = PackageMetadata {
            id: "sample".into(),
            version: "1.2.3".into(),
            description: None,
        };
        let expanded = expand_safe_silent_args(
            r#"/quiet /l*v `"$($env:TEMP)\$($env:ChocolateyPackageName).log`""#,
            &metadata,
        )
        .unwrap();
        assert_eq!(expanded, r#"/quiet /l*v "C:\windows\temp\sample.log""#);
        assert!(expand_safe_silent_args("/D=$env:APPDATA", &metadata).is_err());
    }

    #[test]
    fn translates_a_nupkg_without_executing_its_script() {
        let file = package(
            r#"$args = @{
fileType = 'exe'
url64bit = 'https://vendor.invalid/setup.exe'
checksum64 = 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'
checksumType64 = 'sha256'
silentArgs = '/S'
validExitCodes = @(0, 3010)
}
Install-ChocolateyPackage @args"#,
        );
        let translated = translate(file.path(), "sample", "1.2.3").unwrap();
        assert_eq!(translated.url, "https://vendor.invalid/setup.exe");
        assert_eq!(translated.arguments, ["/S"]);
        assert_eq!(translated.success_exit_codes, [0, 3010]);
    }

    #[test]
    fn rejects_dynamic_package_values() {
        let file = package(
            r#"$args = @{
fileType = 'exe'
url64bit = $url
checksum64 = 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'
checksumType64 = 'sha256'
}
Install-ChocolateyPackage @args"#,
        );
        assert!(translate(file.path(), "sample", "1.2.3").is_err());
    }

    #[test]
    fn verifies_package_identity() {
        let file = package(
            r#"$args = @{
fileType = 'exe'
url64bit = 'https://vendor.invalid/setup.exe'
checksum64 = 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'
checksumType64 = 'sha256'
}
Install-ChocolateyPackage @args"#,
        );
        assert!(translate(file.path(), "different", "1.2.3").is_err());
    }
}
