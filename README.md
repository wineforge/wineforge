# Wineforge

Wineforge is an open-source, declarative launcher and prefix manager for running
Windows applications with Wine on macOS and Linux.

The project separates four concerns:

- application profiles describe an executable, arguments, engine and explicit
  host-folder mappings;
- engine manifests identify immutable Wine runtimes and their provenance;
- the planner computes filesystem changes without mutating a prefix;
- the launcher applies and verifies the plan before starting Wine.

Wineforge applies operating-system filesystem isolation in addition to removing
implicit Wine convenience mappings. Required isolation is the default. The
macOS CLI backend currently uses Apple's deprecated `sandbox-exec` facility and
is therefore experimental; a signed App Sandbox launcher is the supported
long-term design. Linux uses Bubblewrap.

## Security defaults

- No `Z:` mapping to the host root.
- No raw-device mappings.
- Missing or empty mapping paths are disabled, never defaulted.
- Executables and arguments are arrays; profile values are never evaluated by
  a shell.
- Unknown configuration fields are rejected.
- Recipe downloads and translated Chocolatey vendor downloads are independently
  pinned with SHA-256. Chocolatey PowerShell is parsed as untrusted data and is
  never executed.
- Prefix mutations are planned and verified before launch.
- Every newly created app instance gets a dedicated fresh prefix. Host-root and
  macOS/Linux user-folder convenience links are removed before use.
- Winetricks dependencies are a validated list of verbs, never a shell command.
- Linux Wine processes see only explicitly mounted system runtime files, the
  selected engine, the private prefix, and declared host mappings. The
  experimental macOS CLI backend denies undeclared reads and writes beneath
  `/Users`, `/Applications`, `/Volumes`, and `/Network`. Read-only mappings are
  enforced by the operating-system backend.
- Launch fails closed if required isolation is unavailable.

## Workspace

```text
crates/wineforge-core  schemas, validation, inspection and planning
crates/wineforge-cli   command-line interface
```

## Development

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## CLI workflow

```sh
# Inspect and validate without mutation.
wineforge inspect /absolute/path/to/prefix
wineforge validate-profile profile.toml
wineforge validate-engine engine.toml

# Inspect a recipe or a local Chocolatey package without executing it.
wineforge recipe validate recipe.toml
wineforge recipe inspect recipe.toml
wineforge recipe inspect-nupkg package.nupkg \
  --package-id PACKAGE_ID --package-version PACKAGE_VERSION

# Install into the fresh prefix named by the profile. This refuses an existing
# prefix and removes the new prefix if any installation or postcondition fails.
wineforge recipe install recipe.toml \
  --profile profile.toml \
  --engine-manifest engine.toml \
  --engine-root /absolute/engine/destination

# Build and install a self-contained macOS application bundle. The application
# launcher resolves package-relative files and calls the unchanged `run` command.
cargo build --release --bins
wineforge install recipe.toml \
  --profile profile.toml \
  --engine-manifest engine.toml \
  --engine-root /absolute/engine/destination \
  --format app \
  --destination "$HOME/Applications/Wineforge/Example.app"

# Build a Debian package with an immutable prefix template. Its launcher creates
# a deterministic per-user prefix and then calls the same stateless `run` command.
wineforge install recipe.toml \
  --profile profile.toml \
  --engine-manifest engine.toml \
  --engine-root /absolute/engine/destination \
  --format deb \
  --destination ./wineforge-app-example_1.0.0_amd64.deb

# Preview and delete verified, content-addressed installer/package cache entries.
wineforge recipe prune-cache --sha256 SHA256
wineforge recipe prune-cache --sha256 SHA256 --yes

# Verify and install a CI-built, content-addressed engine archive.
wineforge install-engine engine.tar.gz engine.toml /absolute/engine/destination

# Normally optional: `run` performs this automatically when the prefix is absent.
wineforge app create profile.toml \
  --engine-manifest engine.toml \
  --engine-root /absolute/engine/destination

# Preview and then delete a managed engine installed beneath a store.
wineforge engine prune --store /absolute/engine/store --id ENGINE_ID
wineforge engine prune --store /absolute/engine/store --id ENGINE_ID --yes

# Delete a managed local build, including its work tree and engine archive.
wineforge engine prune-artifacts \
  --store /absolute/path/to/wineforge-engines/local-builds \
  --id BUILD_ID --yes

# Review before applying. Mutation requires an explicit confirmation flag.
wineforge plan profile.toml
wineforge apply profile.toml --yes
wineforge verify profile.toml

# Launch fails closed on mapping drift unless --apply is explicitly supplied.
# An absent prefix is initialized, sanitized, provisioned, and marked as a
# managed app instance automatically. An existing unmanaged prefix is refused.
wineforge run profile.toml \
  --engine-manifest engine.toml \
  --engine-root /absolute/engine/destination
```

`--engine-root` accepts either the destination passed to `install-engine` or
its contained `wineforge-engine` directory.

Pruning only recognizes immediate child directories containing Wineforge's
management marker; unrelated files and unmarked directories are ignored. Pass
one or more `--profile` arguments to `engine prune` to protect engines selected
by those profiles. `--all` selects every managed entry, and no deletion occurs
without `--yes`.

Profiles never contain shell command strings. Wineforge passes the executable
and each argument directly to the selected Wine process.

Profiles and engine manifests use TOML. JSON remains accepted for compatibility;
generated receipts, SBOMs and attestations remain JSON. Neutral examples are in
[`examples/profile.toml`](examples/profile.toml) and
[`examples/engine.toml`](examples/engine.toml).

Application dependencies belong in recipes as typed installation actions:

```toml
[[install]]
action = "winetricks"
verbs = ["corefonts", "vcrun2022"]
```

For an explicitly local, non-reproducible adjustment to an existing managed
instance, use a manual provisioning command instead of changing the profile:

```sh
wineforge app provision profile.toml \
  --engine-manifest engine.toml \
  --engine-root /absolute/engine/destination \
  --winetricks corefonts \
  --winetricks vcrun2022
```

Manual provisioning validates verbs, runs under the selected platform sandbox,
re-sanitizes the prefix even after failure, and records the change beneath the
instance's `.wineforge` state directory. Profiles deliberately reject a
`winetricks` field.

During first creation Wineforge initializes `drive_c`, removes `Z:` and all
other undeclared host-facing symlinks, replaces linked Windows user folders
with private directories, and applies declared drive mappings. Every launch
audits the full prefix and refuses any host-facing symlink other than an exact
declared drive mapping. With `isolation.mode = "required"` (the default), the
launcher also confines direct Unix-path access such as Wine's `\\?\unix\`
namespace. `mode = "disabled"` is an explicit diagnostic escape hatch and
cannot be combined with read-only mappings.

Engine build definitions and application recipes live in separate repositories.

## Chocolatey package translation

Wineforge can consume a pinned `.nupkg` through a recipe
`chocolatey-package` action in `mode = "translate"`. It verifies the package,
checks nuspec ID and version, reads `tools/chocolateyInstall.ps1`, and accepts
one direct `Install-ChocolateyPackage @hashtable` call. Static x64 HTTPS URL,
SHA-256 checksum, EXE/MSI type, silent arguments, and exit codes become the
same native plan used by `run-installer`.

No PowerShell, Chocolatey client, or .NET runtime is launched. Dynamic URLs,
unknown interpolation, non-SHA-256 checksums, indirect calls, and multiple
installer calls fail closed. A small allowlist expands inert silent-argument
values for the private Windows temporary path and package identity. The vendor
installer is downloaded and hashed separately, staged beneath `C:`, executed
under the selected platform filesystem sandbox, and removed afterward.

Version 1 executes `run-installer`, translated `chocolatey-package`,
`create-directory`, `extract-archive`, and `winetricks` actions plus
`file-exists` postconditions. ZIP archives are hash-verified before extraction;
absolute paths, traversal, links, special files, duplicate destinations, and
unsafe overlays fail closed. Other schema actions are parsed but currently fail
closed at execution. Recipe installation currently requires a fresh prefix;
upgrading or transactionally modifying an existing app instance is not yet
supported.

A `run-installer` step can set `archiveMember` to run one exact installer file
from a hash-verified ZIP source. Wineforge extracts only that member into its
private staging directory and removes it after installation.

`copy-file` verifies its declared source and atomically installs a new file. Its
destination may use `%APPDATA%\\...`; Wineforge resolves that token to the sole
real private user profile in the fresh prefix and rejects ambiguous profiles,
links, traversal, and existing destinations.

## Native packages without an instance registry

`wineforge run` remains an explicit, stateless execution primitive. Native
packages embed the validated profile, engine manifest, recipe, Wineforge CLI,
generic launcher, content-addressed engine, application icon, and installed
prefix. The launcher discovers those paths relative to itself and invokes
`wineforge run PROFILE --engine-manifest MANIFEST --engine-root ENGINE`; no
Wineforge instance database is created.

On macOS, `wineforge install --format app` atomically installs a relocatable
`.app` bundle. Its writable prefix lives under `Contents/WinePrefix`, and its
icon is extracted from the installed PE executable without executing it. On
Linux, `--format deb` emits a native `amd64` package beneath `/opt/wineforge`.
Because package-owned files are immutable, the launcher copies its verified
prefix template to `$XDG_DATA_HOME/wineforge/instances/ID` (or
`$HOME/.local/share/wineforge/instances/ID`) on first launch. Removing the
package leaves that user data intact, matching normal Linux package behavior.

Packages currently embed their engines for independent removal and portability.
This deliberately trades disk space for a package that cannot break when a
shared engine is pruned. Generated packages are local and unsigned; platform
signing, notarization, RPM output, and document-type registration remain future
work.

## Status

Wineforge is pre-release software. Use cloned or disposable prefixes until the
transactional interfaces are declared stable. The macOS CLI isolation backend
depends on a deprecated operating-system utility and is not a substitute for
the planned signed App Sandbox launcher. The current apply operation creates
recoverable backups but does not yet expose a public rollback command, and
prefix-process detection is still a required launcher milestone.

## License

Wineforge is available under the MIT License.
