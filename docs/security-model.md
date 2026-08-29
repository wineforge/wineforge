# Security model

Wineforge treats recipes, profiles, engines, installers, Windows applications,
and existing prefixes as potentially hostile input. Its goal is to make access
explicit and fail closed when the requested policy cannot be enforced. It is
not a malware scanner and does not prove that pinned software is benign.

## Filesystem boundaries

When Wineforge creates or imports a prefix, it:

- removes Wine's default `Z:` mapping to the host filesystem root;
- removes undeclared DOS-device and drive mappings;
- replaces linked Windows user folders with private directories;
- adds only the drive mappings declared in the profile; and
- audits the complete prefix for unexpected host-facing symlinks.

Every launch verifies the effective mappings. Drift is an error unless the user
explicitly requests application of the reviewed plan.

This symlink policy alone does not block Wine's direct Unix-path access. With
the default `isolation.mode = "required"`, Wineforge also confines Wine and its
child processes through an operating-system backend.

## Platform isolation

Linux uses Bubblewrap to create an empty mount namespace containing only the
system runtime files required by Wine, the selected engine, the private prefix,
and declared mappings.

The current macOS CLI backend uses the deprecated `sandbox-exec` utility. It
denies undeclared access beneath conventional user-data and external-volume
roots including `/Users`, `/Applications`, `/Volumes`, and `/Network`, but it is
experimental and is not a system-wide default-deny sandbox. A signed App
Sandbox launcher is the intended long-term backend.

These backends currently enforce filesystem policy, not the recipe's declared
`runtimeAccess.network` value. Use separate host network controls when network
confinement is required.

Launch fails when required isolation is unavailable. `mode = "disabled"` is an
explicit diagnostic escape hatch and cannot enforce read-only mappings. Do not
disable isolation for untrusted Windows software.

## Configuration and process execution

- Unknown configuration fields are rejected.
- Executables and arguments are separate arrays and are passed directly to a
  process; profile or recipe values are never evaluated by a shell.
- Winetricks accepts only a bounded list of validated verb names.
- Recipes contain typed installation actions rather than arbitrary scripts.
- New installations require a fresh prefix and are removed after a failed step
  or postcondition.

## Content integrity

Remote recipe sources and engine archives are pinned by SHA-256. A digest proves
that the bytes match the reviewed artifact. It does not prove that the artifact
is free of malicious behavior, compromised dependencies, or vulnerabilities.

Prefer engines built from reviewed source and reproducible build definitions.
Retain manifests, source revisions, build logs, checksums, SBOMs, and
attestations so independent builds can be compared. Code signing and
notarization establish publisher identity and integrity after signing; they do
not replace source review or sandboxing.

## Chocolatey translation

Wineforge does not launch Chocolatey, PowerShell, or a .NET runtime when using a
`chocolatey-package` recipe step. It opens the pinned `.nupkg` as a bounded ZIP,
checks its nuspec identity, and parses a deliberately restricted static
hashtable from `tools/chocolateyInstall.ps1`.

Only one directly expressed `Install-ChocolateyPackage` invocation with a
static x64 HTTPS URL, SHA-256 checksum, EXE/MSI type, silent arguments, and exit
codes is translated. Dynamic URLs, unknown interpolation, indirect calls,
non-SHA-256 checksums, and multiple installer calls are rejected. The nested
vendor installer is downloaded and hash-verified independently.

## Native packages and imported prefixes

Generated packages currently embed their engines and installed prefixes. This
improves independence from a shared engine store but means the package contains
all state captured during installation.

An imported prefix may include registry data, application documents,
credentials, tokens, machine identifiers, or license state. Treat imported
bundles as private user data and inspect them before backup or transfer. Do not
redistribute them merely because the recipe or engine definition is public.

## Reporting a vulnerability

Follow [SECURITY.md](../SECURITY.md). Do not open a public issue containing an
unpatched vulnerability, sensitive prefix data, or credentials.
