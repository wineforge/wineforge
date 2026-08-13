# Contributing

Keep the public project neutral: examples and tests must use fictional software,
temporary directories and synthetic prefixes. Do not commit proprietary
installers, application-specific credentials, licences or personal paths.

All profile and manifest changes need negative tests for unknown fields and
unsafe paths. Filesystem mutation code must avoid following untrusted symlinks.

Before opening a pull request, run formatting, Clippy and the complete workspace
test suite locally.

