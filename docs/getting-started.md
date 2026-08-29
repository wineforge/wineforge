# Getting started

This guide takes a recipe to either a direct Wineforge launch or a native
application package. It uses explicit paths so that each input remains visible.

## 1. Install the CLI

With a current Rust toolchain:

```sh
cargo install wineforge-cli --locked
wineforge --version
```

Prebuilt releases can instead be installed with:

```sh
cargo binstall wineforge-cli
```

On Apple Silicon, install Rosetta 2 before using an x86-64 Wine engine. Linux
launches require Bubblewrap (`bwrap`). Install Winetricks only if the selected
recipe contains a `winetricks` action.

## 2. Obtain a recipe and engine builder

Public recipes and engine build definitions are separate from the CLI:

```sh
git clone https://github.com/wineforge/wineforge-recipes.git
git clone https://github.com/wineforge/wineforge-engines.git
```

Treat the engine-builder checkout as trusted code: `prepare` may run its fixed
`scripts/build-local.sh` interface. Recipe data cannot select an arbitrary
build command.

Choose a recipe and inspect it before downloading or executing anything:

```sh
wineforge recipe validate wineforge-recipes/recipes/v1/RECIPE.toml
wineforge recipe inspect wineforge-recipes/recipes/v1/RECIPE.toml
```

## 3. Prepare an instance

From a terminal, run:

```sh
wineforge prepare wineforge-recipes/recipes/v1/RECIPE.toml \
  --profile-out ./profile.toml \
  --engine-builder ./wineforge-engines \
  --build-if-missing \
  --setup-engine-dependencies
```

`prepare` performs these operations:

1. validates the recipe and selects its variant for the current host;
2. searches the default managed engine store for a compatible version;
3. builds and installs an engine when none is present and
   `--build-if-missing` was supplied; and
4. writes a new local profile without overwriting an existing file.

Default locations are:

| Data | macOS | Linux |
| --- | --- | --- |
| Engine store | `~/Applications/Wineforge/engines` | `~/.local/share/wineforge/engines` |
| Direct-run prefix | `~/Library/Application Support/Wineforge/instances/ID/prefix` | `~/.local/share/wineforge/instances/ID/prefix` |

Interactive preparation prompts for profile details and folder mappings. A
non-interactive invocation must declare each mapping itself:

```sh
wineforge prepare recipe.toml \
  --profile-out ./profile.toml \
  --engine-builder ./wineforge-engines \
  --build-if-missing \
  --non-interactive \
  --mapping 'W=read-write=/absolute/path/to/workspace'
```

The drive letter must be one letter other than `A`, `B`, or `C`. Use
`read-only` where possible. Wineforge warns when a recipe requests a folder
class but a non-interactive run supplies no mapping; it never guesses a host
directory.

Engine builds can be long. Linux builds use Podman or Docker; macOS builds are
native and may re-execute under Rosetta. Generated engine archives and work
trees are discarded by default. Verified Wine source downloads remain in the
builder's content-addressed user cache and interrupted `.part` files resume on
the next attempt. Add `--keep-build-artifacts` only when you need generated
build output for inspection.

On macOS, `--setup-engine-dependencies` is explicit permission for the trusted
engine checkout to bootstrap Intel Homebrew in `/usr/local` and install missing
x86_64 build formulae. A separate Apple Silicon Homebrew installation in
`/opt/homebrew` is left unchanged. The official installer may request
administrator access while preparing `/usr/local`. Later builds reuse the same
packages. Without this option, missing dependencies are reported but never
installed.

## 4. Install and launch directly

The selected managed engine directory contains its `engine.toml` manifest. Use
that manifest and the engine directory with the generated profile:

```sh
wineforge recipe install recipe.toml \
  --profile ./profile.toml \
  --engine-manifest /path/to/engine/engine.toml \
  --engine-root /path/to/engine

wineforge run ./profile.toml \
  --engine-manifest /path/to/engine/engine.toml \
  --engine-root /path/to/engine
```

Recipe installation requires a fresh prefix and removes the new prefix if an
installation step or postcondition fails. `run` creates and sanitizes an absent
managed prefix automatically, but it refuses an existing unmanaged prefix.

If a declared mapping has drifted, launch fails closed. Review and apply the
plan explicitly:

```sh
wineforge plan ./profile.toml
wineforge run ./profile.toml \
  --engine-manifest /path/to/engine/engine.toml \
  --engine-root /path/to/engine \
  --apply
```

## 5. Create a native package

`prepare` can feed its generated profile and resolved engine directly to the
native-package stage.

macOS:

```sh
wineforge prepare recipe.toml \
  --profile-out ./profile.toml \
  --engine-builder ./wineforge-engines \
  --build-if-missing \
  --then-install app \
  --destination "$HOME/Applications/Wineforge/Example.app"
```

Linux:

```sh
wineforge prepare recipe.toml \
  --profile-out ./profile.toml \
  --engine-builder ./wineforge-engines \
  --build-if-missing \
  --then-install deb \
  --destination ./example_1.0.0_amd64.deb
```

When a recipe requires license acceptance, review its `license.noticeUrl` and
add `--accept-license` only if you accept those terms.

Generated packages contain a snapshot of the validated configuration, engine,
and installed prefix. They do not depend on a Wineforge instance database.

## Alternative: install a prebuilt engine archive

If you already have a content-addressed engine archive and matching manifest:

```sh
wineforge validate-engine engine.toml
wineforge install-engine engine.tar.gz engine.toml /absolute/engine/destination
```

`install-engine` verifies the archive digest before unpacking it. The engine
root accepted by other commands is either that destination or its contained
`wineforge-engine` directory.

## Removing managed data

Deletion is always a preview unless `--yes` is supplied:

```sh
wineforge engine prune --store /absolute/engine/store --id ENGINE_ID
wineforge engine prune --store /absolute/engine/store --id ENGINE_ID --yes

wineforge recipe prune-cache --sha256 SHA256
wineforge recipe prune-cache --sha256 SHA256 --yes
```

Pruning recognizes only immediate child directories with Wineforge management
markers. Unmarked data is ignored.
