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

# Review before applying. Mutation requires an explicit confirmation flag.
wineforge plan profile.json
wineforge apply profile.json --yes
wineforge verify profile.json

# Launch fails closed on mapping drift unless --apply is explicitly supplied.
wineforge run profile.json \
  --engine-manifest engine.runtime.json \
  --engine-root /absolute/engine/destination/wineforge-engine
```

Profiles never contain shell command strings. Wineforge passes the executable
and each argument directly to the selected Wine process.

Engine build definitions and application recipes live in separate repositories.

## Status

Wineforge is pre-release software. Use cloned or disposable prefixes until the
transactional interfaces are declared stable. The current apply operation
creates recoverable backups but does not yet expose a public rollback command,
and prefix-process detection is still a required launcher milestone.

## License

Wineforge is available under the MIT License.
