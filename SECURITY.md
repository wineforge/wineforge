# Security policy

## Reporting

Do not open a public issue for a suspected vulnerability. Use GitHub private
vulnerability reporting when it is enabled for this repository.

Include the affected version, profile fragment with secrets removed, prefix
layout, expected behavior and a minimal reproduction.

## Threat model

Wineforge treats profiles, recipes, downloaded engines and Windows executables
as potentially hostile input. Its mapping manager is intended to prevent
accidental host exposure and unsafe prefix mutation. It is not, by itself, an
operating-system sandbox for Wine or Windows software.

Engine manifests and release artifacts should be content-addressed. A matching
digest establishes artifact integrity, not that the artifact is free of
vulnerabilities or malicious behavior.

