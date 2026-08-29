# File-format reference

Wineforge separates reusable installation data from local runtime policy and
engine provenance. All human-authored configuration is TOML.

Recipes use camelCase field names because their canonical schema is shared with
the recipe catalog. Profiles and engine manifests use snake_case. Unknown fields
are rejected, which turns a misspelled security setting into an error instead of
silently ignoring it.

## Recipe

A recipe describes how to create an application prefix. Its normative version
1 definition is the
[recipe JSON Schema](https://github.com/wineforge/wineforge-recipes/blob/main/schema/v1/recipe.schema.json).
The JSON Schema is a machine-readable validation artifact; authored recipes
must be TOML.

### Top-level sections

| Field | Meaning |
| --- | --- |
| `schemaVersion` | Schema major version; currently `1` |
| `id`, `version`, `name`, `summary`, `homepage` | Stable identity and display metadata; `homepage` is optional |
| `[license]` | SPDX expression, optional notice URL, and `none` or `required` acceptance |
| `[application]` | Installed Windows executable, argument array, and optional working directory |
| `[runtimeAccess]` | Requested network intent and host-folder classes; actual paths stay in the profile |
| `[[variants]]` | Supported host, translation, engine family, semantic version range, and features |
| `[[sources]]` | SHA-256-pinned HTTPS downloads or files beneath the recipe directory |
| `[[install]]` | Ordered, typed installation actions |
| `[[verify]]` | Postconditions that must pass before installation succeeds |

Every recipe needs at least one variant and one verification postcondition.
Executable paths and process arguments are separate values and are never shell
strings.

`runtimeAccess.network` is currently catalog metadata (`none` or `outbound`);
the CLI's present isolation backends enforce filesystem access, not network
policy. Do not treat `network = "none"` as a network sandbox yet.

### Source forms

```toml
[[sources]]
id = "remote-installer"
kind = "remote"
url = "https://publisher.example/installer.exe"
sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"

[[sources]]
id = "local-installer"
kind = "repository"
path = "files/installer.exe"
sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
```

Remote URLs must use HTTPS. A repository path is relative to the recipe and may
not be absolute or contain `..`.

### Installation actions

| `action` | Important fields | Runtime status |
| --- | --- | --- |
| `run-installer` | `source`, optional `archiveMember`, `installerType`, `arguments`, `successExitCodes` | Supported |
| `chocolatey-package` | `source`, `packageId`, `packageVersion`, `mode = "translate"` | Supported static subset |
| `create-directory` | `path` | Supported |
| `extract-archive` | `source`, `destination`, optional `stripComponents` | Supported for bounded safe ZIP extraction |
| `copy-file` | `source`, `destination` | Supported; destination may begin with `%APPDATA%\\` |
| `winetricks` | `verbs` | Supported; 1–32 validated verbs |
| `set-registry-value` | registry fields | Parsed by version 1, but execution currently fails closed |

Arbitrary shell, PowerShell, and batch actions are not part of the format.
`chocolatey-package` does not run package PowerShell; it translates a restricted
static form into an ordinary installer plan. `mode = "sandboxed-script"` is not
implemented and is rejected.

### Verification postconditions

| `check` | Fields | Runtime status |
| --- | --- | --- |
| `file-exists` | `path`, optional `sha256` | Supported |
| `registry-value-equals` | registry fields and expected value | Parsed, but execution currently fails closed |

Recipe installation uses a fresh prefix. All postconditions must pass before a
receipt is committed.

## Application profile

A profile describes one local application instance. See the neutral complete
[profile example](../examples/profile.toml).

```toml
schema_version = 1
id = "sample-editor"
name = "Sample Editor"
prefix = "/absolute/path/to/sample-editor/prefix"
executable = "C:\\Program Files\\Sample Editor\\editor.exe"
arguments = []

[engines.macos-x86-64]
id = "sample-engine-macos-x86_64"

[environment]
WINEDEBUG = "-all"

[isolation]
mode = "required"

[[mappings]]
drive = "W"
host_path = "/absolute/path/to/workspace"
access = "read-write"
```

| Field | Meaning and constraints |
| --- | --- |
| `schema_version` | Must be `1` |
| `id` | Lowercase stable identifier |
| `name` | Non-empty display name |
| `prefix` | Absolute, non-root path without `..` |
| `executable` | Non-empty Windows path |
| `arguments` | Optional array passed directly to Wine |
| `[engines.PLATFORM]` | Engine ID for `macos-x86-64` or `linux-x86-64`; at least one is required |
| `[environment]` | Validated environment variables passed to Wine |
| `[[mappings]]` | One host directory exposed as a DOS drive |
| `[isolation]` | `required` by default; `disabled` is a diagnostic escape hatch |

A mapping drive is one ASCII letter; `A`, `B`, and `C` are reserved. Host paths
must be absolute, may not be the filesystem root, and may not use `..`. Access
is `read-only` or `read-write`. Read-only mappings require operating-system
isolation, and mappings with conflicting access may not overlap.

Profiles deliberately have no installation actions or Winetricks list. Put
reproducible dependencies in the recipe; use `wineforge app provision` only for
a local diagnostic adjustment.

## Engine manifest

An engine manifest identifies one immutable runtime. It is metadata; the engine
root is the separate directory containing the actual Wine files. See the
complete [engine example](../examples/engine.toml).

```toml
schema_version = 1
id = "sample-engine-macos-x86_64"
platform = "macos-x86-64"
host_architecture = "x86_64"
translation = "rosetta2"
wine_binary = "bin/wine"

[artifact]
sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"

[artifact.source]
kind = "user-supplied"

[license]
name = "Example License"
url = "https://example.invalid/license"
acceptance_required = false
```

| Field | Meaning and constraints |
| --- | --- |
| `schema_version` | Must be `1` |
| `id` | Lowercase stable identifier matched by the profile |
| `platform` | `macos-x86-64` or `linux-x86-64` |
| `host_architecture` | Currently `x86_64` |
| `translation` | `native` or `rosetta2`; Rosetta 2 is valid only for macOS x86-64 engines |
| `wine_binary` | Non-empty path relative to the engine root, without `..` |
| `[artifact]` | Exactly 64 lowercase hexadecimal characters of SHA-256 |
| `[artifact.source]` | Artifact provenance |
| `[environment]` | Engine-required environment variables |
| `[license]` | License name, URL, and whether explicit acceptance is required |

Artifact source forms are:

```toml
[artifact.source]
kind = "direct-download"
url = "https://downloads.example/engine.tar.gz"
```

```toml
[artifact.source]
kind = "user-supplied"
```

```toml
[artifact.source]
kind = "build-from-source"
source_url = "https://source.example/project.git"
revision = "immutable-revision"
```

The manifest digest must describe the exact archive accepted by
`wineforge install-engine`. It proves integrity and reproducibility, not absence
of malware or vulnerabilities.

## Generated files

Wineforge also writes machine state such as installation receipts, SBOMs,
attestations, and management markers. These generated artifacts may use JSON;
they are not hand-authored schemas. A native package embeds snapshots of its
recipe, profile, manifest, engine, and installed prefix so its launcher can call
the stateless `wineforge run` interface.

Profiles and engine manifests may still be read from JSON for compatibility,
but new human-authored configuration and all generated examples should use
TOML.
