# Security policy

## Reporting

Do not open a public issue for a suspected vulnerability. Use GitHub private
vulnerability reporting when it is enabled for this repository.

Include the affected version, profile fragment with secrets removed, prefix
layout, expected behavior and a minimal reproduction.

## Threat model

Wineforge treats profiles, recipes, downloaded engines and Windows executables
as potentially hostile input. Its mapping manager prevents accidental host
exposure and unsafe prefix mutation. Required platform isolation additionally
restricts direct Unix-path access by Wine and its children. On macOS the current
CLI backend uses deprecated `sandbox-exec` and remains experimental pending a
signed App Sandbox launcher; it protects conventional user-data and external
volume roots, including installed application bundles under `/Applications`,
but it is not a system-wide default-deny sandbox. On Linux the backend requires
Bubblewrap and constructs an empty mount namespace. Do not disable isolation
for untrusted Windows software.

Engine manifests and release artifacts should be content-addressed. A matching
digest establishes artifact integrity, not that the artifact is free of
vulnerabilities or malicious behavior.

Chocolatey compatibility does not execute package PowerShell. Wineforge opens a
pinned `.nupkg` as a bounded ZIP container, parses its nuspec and a deliberately
restricted static hashtable, and constructs a typed installer plan. Both the
package and nested vendor installer require matching SHA-256 digests. These
checks establish reviewed-byte identity; they do not prove that either artifact
is benign. Unsupported or dynamic package behavior is rejected rather than
delegated to a PowerShell runtime.
