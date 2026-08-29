# Porting a Windows application

Porting is the process of turning a known-good Windows installation procedure
into a reproducible Wineforge recipe and a machine-local profile. Start with the
smallest possible installation and add compatibility changes only when a test
shows they are necessary.

## Collect the facts first

Before writing TOML, determine:

- the publisher's official installer URL or a legally obtained local installer;
- the exact application version and SHA-256 digest;
- whether the installer is an EXE, MSI, or a file inside a ZIP;
- the publisher-documented unattended-install arguments and successful exit
  codes;
- the installed Windows executable and working-directory paths;
- whether the application is 32-bit, 64-bit, or mixed;
- any required fonts, Visual C++ runtimes, .NET versions, or other Winetricks
  verbs;
- the smallest host-folder access the application needs; and
- at least one file that proves installation completed.

Calculate the installer digest locally:

```sh
# macOS
shasum -a 256 installer.exe

# Linux
sha256sum installer.exe
```

A digest pins bytes; it does not establish that those bytes are trustworthy.
Obtain installers from the publisher and review their licenses.

## Start with a minimal recipe

This fictional example uses a file stored beside the recipe. Repository sources
are useful for private recipes and test fixtures; their paths must stay beneath
the recipe directory.

```toml
schemaVersion = 1
id = "com.example.sample-editor"
version = "1.0.0"
name = "Sample Editor"
summary = "Installs the Sample Editor desktop application."
homepage = "https://example.invalid/sample-editor"

[license]
spdx = "LicenseRef-Example-EULA"
noticeUrl = "https://example.invalid/sample-editor/license"
acceptance = "required"

[application]
executable = "C:\\Program Files\\Sample Editor\\editor.exe"
arguments = []
workingDirectory = "C:\\Program Files\\Sample Editor"

[runtimeAccess]
network = "none"
folders = ["documents"]

[[variants]]
platform = "macos"
hostArchitecture = "aarch64"
translation = "rosetta2"

[variants.engine]
family = "winecx"
version = ">=24,<26"
features = ["win64"]

[[variants]]
platform = "linux"
hostArchitecture = "x86_64"
translation = "native"

[variants.engine]
family = "wine"
version = ">=10,<11"
features = ["win64"]

[[sources]]
id = "installer"
kind = "repository"
path = "installer.exe"
sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"

[[install]]
action = "run-installer"
source = "installer"
installerType = "exe"
arguments = ["/quiet", "/norestart"]
successExitCodes = [0]

[[verify]]
check = "file-exists"
path = "C:\\Program Files\\Sample Editor\\editor.exe"
```

For an MSI, use `installerType = "msi"`; Wineforge invokes it through
`msiexec /i`. When the installer is inside a hash-pinned ZIP, add an exact safe
member path such as `archiveMember = "setup/installer.exe"` to the
`run-installer` step.

Remote sources use HTTPS and the same digest:

```toml
[[sources]]
id = "installer"
kind = "remote"
url = "https://publisher.example/downloads/installer.exe"
sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
```

## Validate before executing

Validation and inspection do not download or launch the installer:

```sh
wineforge recipe validate recipe.toml
wineforge recipe inspect recipe.toml
```

Then use `wineforge prepare` as described in
[Getting started](getting-started.md). Always test installation in a fresh
prefix. Version 1 does not upgrade or transactionally modify an existing
instance.

## Add dependencies deliberately

If testing shows that a standard Wine component is missing, make it a typed
recipe step so every installation receives the same dependency:

```toml
[[install]]
action = "winetricks"
verbs = ["corefonts", "vcrun2022"]
```

Winetricks verbs do not belong in a profile. For a one-off diagnostic change to
an existing local instance, use `wineforge app provision`; if it fixes the
application, move the required verb into the recipe before sharing it.

```sh
wineforge app provision profile.toml \
  --engine-manifest engine.toml \
  --engine-root /absolute/path/to/engine \
  --winetricks corefonts
```

## Keep host access local

`runtimeAccess.folders` records the classes of access an application expects;
it does not choose a user's directory. The local profile grants actual access:

```toml
[[mappings]]
drive = "W"
host_path = "/absolute/path/to/workspace"
access = "read-write"
```

Use a dedicated folder and read-only access whenever possible. Do not map the
filesystem root. Wineforge removes Wine's default `Z:` drive and linked Desktop,
Documents, and similar convenience folders rather than guessing what should be
visible.

`runtimeAccess.network` records intent but the current CLI does not enforce
network isolation. Use an operating-system firewall or an isolated environment
when a test must prohibit network access.

## Chocolatey `.nupkg` files

Many Chocolatey packages contain metadata and a small PowerShell script that
downloads the publisher's installer instead of redistributing it. Wineforge can
use this model without installing Chocolatey or executing PowerShell:

```toml
[[sources]]
id = "package"
kind = "remote"
url = "https://community.chocolatey.org/api/v2/package/example/1.2.3"
sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"

[[install]]
action = "chocolatey-package"
source = "package"
packageId = "example"
packageVersion = "1.2.3"
mode = "translate"
```

Wineforge verifies the `.nupkg`, checks its ID and version, parses a deliberately
restricted static `Install-ChocolateyPackage` hashtable, and independently
verifies the downloaded vendor installer. Dynamic PowerShell behavior is
rejected. Use this command to test whether a local package is translatable:

```sh
wineforge recipe inspect-nupkg package.nupkg \
  --package-id example \
  --package-version 1.2.3
```

## Diagnosing compatibility problems

Change one variable at a time and recreate the prefix between installation
tests. Useful checks include:

1. Confirm the recipe's executable path and any launcher arguments. A Windows
   shortcut may include required switches that are not obvious from the target
   executable alone.
2. Check the application's bitness against the selected engine features.
3. Compare the publisher's silent-install documentation with the declared
   argument array; arguments are case-sensitive for some installers.
4. Test required Winetricks components individually, then record only the ones
   that proved necessary.
5. Inspect host exposure and mappings with `wineforge inspect PREFIX` and
   `wineforge verify profile.toml`.
6. Treat distorted fonts, incorrect scaling, and rough controls separately from
   installation failures. They may depend on fonts, DPI settings, the graphics
   backend, or engine-specific patches.
7. Preserve logs and the exact engine ID when reporting a failure. Do not share
   a prefix blindly: registry files may contain credentials or license state.

For an already installed application, `wineforge app import` can clone an
existing prefix into a managed macOS `.app` without rerunning installation. The
source is not modified, but imported registry and application state remains
private user data and is not suitable for redistribution.

## Contributing a public recipe

The [wineforge-recipes repository](https://github.com/wineforge/wineforge-recipes)
contains the canonical schema and public examples. A public recipe should:

- use public publisher material and neutral names;
- pin every input with SHA-256;
- include accurate licensing metadata;
- request the minimum runtime access;
- avoid credentials, private paths, license state, and proprietary installer
  bytes; and
- pass the catalog validator and source-integrity checks.

Keep user-specific profiles and compatibility findings outside the public
catalog.
