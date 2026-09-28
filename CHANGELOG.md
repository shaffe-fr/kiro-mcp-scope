# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- Project view and projects × servers matrix view, with keyboard and mouse.
- Divergence detection on the whole entry except `disabled`, reported field by
  field and never silently overwritten.
- Stable, idempotent writes that preserve unknown entries and top-level keys.
- Refusal to write a literal secret into a project `mcp.json`, based on generic
  rules rather than a list of providers.
- `--list`, `--status`, `--discover`, `--activate`, `--deactivate`.
- `--migrate` from the global `mcp.json`, moving secrets into
  `KMS__<SERVER>__<KEY>` environment variables, with `--dry-run` and
  `--no-env`; `--rollback` back to the global model.

[Unreleased]: https://github.com/shaffe-fr/kiro-mcp-scope/commits/main
