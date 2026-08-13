# Wineforge

Wineforge is an open-source, declarative launcher and prefix manager for running
Windows applications with Wine on macOS and Linux.

The project separates four concerns:

- application profiles describe an executable, arguments, engine and explicit
  host-folder mappings;
- engine manifests identify immutable Wine runtimes and their provenance;
- the planner computes filesystem changes without mutating a prefix;
- the launcher applies and verifies the plan before starting Wine.

Wineforge does not treat drive mappings as a security sandbox. It removes
implicit convenience mappings and makes effective access auditable. Stronger
isolation remains platform-dependent.

## Security defaults

- No `Z:` mapping to the host root.
- No raw-device mappings.
- Missing or empty mapping paths are disabled, never defaulted.
- Executables and arguments are arrays; profile values are never evaluated by
  a shell.
- Unknown configuration fields are rejected.
- Prefix mutations are planned and verified before launch.
- Every newly created app instance gets a dedicated fresh prefix. Host-root and
  macOS/Linux user-folder convenience links are removed before use.
- Winetricks dependencies are a validated list of verbs, never a shell command.

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
wineforge validate-profile profile.json
wineforge validate-engine engine.runtime.json

# Verify and install a CI-built, content-addressed engine archive.
wineforge install-engine engine.tar.gz engine.runtime.json /absolute/engine/destination

# Normally optional: `run` performs this automatically when the prefix is absent.
wineforge app create profile.json \
  --engine-manifest engine.runtime.json \
  --engine-root /absolute/engine/destination

# Preview and then delete a managed engine installed beneath a store.
wineforge engine prune --store /absolute/engine/store --id ENGINE_ID
wineforge engine prune --store /absolute/engine/store --id ENGINE_ID --yes

# Delete a managed local build, including its work tree and engine archive.
wineforge engine prune-artifacts \
  --store /absolute/path/to/wineforge-engines/local-builds \
  --id BUILD_ID --yes

# Review before applying. Mutation requires an explicit confirmation flag.
wineforge plan profile.json
wineforge apply profile.json --yes
wineforge verify profile.json

# Launch fails closed on mapping drift unless --apply is explicitly supplied.
# An absent prefix is initialized, sanitized, provisioned, and marked as a
# managed app instance automatically. An existing unmanaged prefix is refused.
wineforge run profile.json \
  --engine-manifest engine.runtime.json \
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

Profiles may declare Winetricks verbs as a typed array:

```json
"winetricks": ["corefonts", "vcrun2022"]
```

During first creation Wineforge initializes `drive_c`, removes `Z:` and all
other undeclared host-facing symlinks, replaces linked Windows user folders
with private directories, invokes Winetricks once with the declared verbs, and
then repeats sanitization before applying declared drive mappings. Every launch
audits the full prefix and refuses any host-facing symlink other than an exact
declared drive mapping.

Engine build definitions and application recipes live in separate repositories.

## Status

Wineforge is pre-release software. Use cloned or disposable prefixes until the
transactional interfaces are declared stable. The current apply operation
creates recoverable backups but does not yet expose a public rollback command,
and prefix-process detection is still a required launcher milestone.

## License

Wineforge is available under the MIT License.
