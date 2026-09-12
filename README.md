# Wineforge

Wineforge turns a declarative recipe into an isolated Wine application for
macOS or Linux. It manages one prefix per application, uses a pinned Wine
engine, removes Wine's implicit access to the host filesystem, and can package
the result as a macOS `.app` or Debian `.deb`.

Wineforge is useful when you want a repeatable answer to four questions:

1. Which installer bytes and setup steps does this application need?
2. Which Wine engine should run it?
3. Which host folders may it access?
4. How can it be installed and launched like a native application?

> [!WARNING]
> Wineforge is pre-release software. Start with cloned or disposable prefixes.
> The current macOS command-line sandbox uses Apple's deprecated
> `sandbox-exec` facility and remains experimental; Linux uses Bubblewrap.

## Install

Install from crates.io (builds locally):

```sh
cargo install wineforge-cli --locked
```

Or install a matching prebuilt release with cargo-binstall:

```sh
cargo binstall wineforge-cli
```

Both commands install `wineforge` and the `wineforge-launcher` companion.
Prebuilt archives are available for Apple Silicon and Intel macOS and x86-64
Linux. You also need:

- a compatible Wineforge engine, installed locally or built from a trusted
  checkout of [wineforge-engines];
- Rosetta 2 when using an x86-64 engine on Apple Silicon;
- Bubblewrap on Linux; and
- Winetricks only when a recipe contains a `winetricks` step.

## Quick start

The guided `prepare` command validates a recipe, creates a local profile, finds
a compatible installed engine, and can build one locally when it is missing:

```sh
wineforge recipe inspect recipe.toml

wineforge prepare recipe.toml \
  --profile-out profile.toml \
  --engine-builder /absolute/path/to/wineforge-engines \
  --build-if-missing \
  --setup-engine-dependencies
```

`prepare` is interactive by default. It shows the recipe's requested folder
classes and lets you add explicit drive mappings. For a scripted run, add
`--non-interactive` and repeat this option for each allowed folder:

```sh
--mapping 'W=read-write=/absolute/path/to/workspace'
```

On macOS, add the following options to build and install a relocatable app in
the same operation:

```sh
--then-install app \
--destination "$HOME/Applications/Wineforge/Example.app"
```

On Linux, use `--then-install deb --destination ./example_1.0.0_amd64.deb`.
Successful local engine builds discard their generated engine archive and work
tree unless `--keep-build-artifacts` is supplied. The verified Wine source
download remains in the engine builder's user cache so later attempts do not
download it again.

On macOS, `--setup-engine-dependencies` authorizes the trusted engine builder
to install Intel Homebrew in `/usr/local` when absent and install its missing
x86_64 formulae. It does not modify Apple Silicon Homebrew in `/opt/homebrew`.
The official Homebrew installer may request administrator access. Omit the
option when host package installation must remain diagnostic-only.

See the [getting-started guide](docs/getting-started.md) for the complete flow,
including direct stateless launches and manually installed engines.

## The files Wineforge uses

| Item | Purpose | Portable? |
| --- | --- | --- |
| Recipe (`*.toml`) | Installer sources, hashes, typed installation steps, application entry point, engine requirements, and post-install checks | Usually; public recipes belong in [wineforge-recipes] |
| Profile (`*.toml`) | One local instance: prefix path, selected engine ID, environment, allowed host folders, and isolation policy | Usually machine-specific |
| Engine manifest (`*.toml`) | Engine identity, platform, architecture, artifact digest, Wine binary, provenance, and license | Yes, when its artifact is reproducible |
| Engine root (directory) | The unpacked Wine runtime referenced by the manifest | No; this is runtime data, not configuration |

Recipes intentionally use **camelCase** keys. Profiles and engine manifests use
**snake_case** keys. Unknown fields are rejected in all three formats. The
[file-format reference](docs/file-formats.md) contains annotated examples and
field tables.

The [runtime-capabilities guide](docs/capabilities.md) covers versioned engine
features, layered input policy, strict macOS window isolation, and MCP endpoint
negotiation.

## Porting an application

A typical port starts with a legally obtained installer, its SHA-256 digest,
the application's installed executable path, and its unattended-install flags.
Add the smallest possible recipe, install into a fresh prefix, then introduce
dependencies and host access one at a time.

Do not put Winetricks verbs in the profile: reproducible dependencies are typed
recipe steps. The profile is reserved for local runtime policy, such as mapping
one workspace as `W:`. Wineforge never interprets a recipe field as a shell
command.

The [application-porting guide](docs/porting-applications.md) walks through the
process, troubleshooting, Chocolatey `.nupkg` translation, and the criteria for
contributing a public recipe.

## Security model

Wineforge defaults to required operating-system isolation and fails closed when
it cannot apply it. It removes the default `Z:` drive and host-facing Wine user
folder links, accepts only explicitly declared drive mappings, and audits the
complete prefix before launch. Downloads, engine archives, and translated
Chocolatey vendor installers are pinned by SHA-256.

A matching hash proves byte identity, not that an engine or installer is safe.
Read [security model](docs/security-model.md) before using untrusted Windows
software. Report vulnerabilities according to [SECURITY.md](SECURITY.md).

## Useful commands

```sh
# Validate configuration without changing the system.
wineforge recipe validate recipe.toml
wineforge validate-profile profile.toml
wineforge validate-engine engine.toml

# Show and apply a profile's drive-mapping plan.
wineforge plan profile.toml
wineforge apply profile.toml --yes
wineforge verify profile.toml

# Launch directly; mapping drift fails unless --apply is explicit.
wineforge run profile.toml \
  --engine-manifest engine.toml \
  --engine-root /absolute/path/to/engine

# Inspect a prefix for host-facing symlinks.
wineforge inspect /absolute/path/to/prefix

# Preview a managed-engine deletion; add --yes to perform it.
wineforge engine prune --store /absolute/path/to/engines --id ENGINE_ID
```

Run `wineforge --help` or `wineforge COMMAND --help` for the complete command
reference.

## Native packages

`wineforge run` remains stateless: it receives a profile, engine manifest, and
engine root on every launch. Generated native packages snapshot those inputs
and call the same command through a generic launcher; Wineforge does not
maintain a global application registry.

- A macOS `.app` contains its writable prefix and extracts an icon from the
  installed PE executable without executing it.
- A Debian package installs an immutable prefix template under `/opt/wineforge`.
  Its first launch creates per-user state under `$XDG_DATA_HOME/wineforge` or
  `$HOME/.local/share/wineforge`.

Native packages use a shared, verified engine by default. Profiles may request
`distribution = "bundled"` for a self-contained portable package. Engine
pruning discovers installed shared references and refuses unsafe deletion.
Generated packages are unsigned; signing, notarization, RPM output, and
document-type registration are future work.

## Project repositories

- [wineforge] — Rust CLI and core library (this repository)
- [wineforge-recipes] — public, reviewable application recipes and the canonical
  recipe JSON Schema
- [wineforge-engines] — reproducible engine build definitions and workflows

## Development

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

See [CONTRIBUTING.md](CONTRIBUTING.md). Release maintainers should use the
[release guide](docs/maintainer-releases.md).

Wineforge is available under the [MIT License](LICENSE).

[wineforge]: https://github.com/wineforge/wineforge
[wineforge-recipes]: https://github.com/wineforge/wineforge-recipes
[wineforge-engines]: https://github.com/wineforge/wineforge-engines
