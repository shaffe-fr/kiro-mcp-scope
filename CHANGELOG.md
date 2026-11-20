# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/).

## [Unreleased]
### Added
- `--sync` applies the catalog's definition to every copy of a server, with
  `--dry-run` (files and a masked diff), `--server`, `--include agents,examples`,
  `--no-worktrees` and `--force`. It covers `.kiro/settings/mcp.json` and
  `.ai/mcp/mcp.json` of projects, worktrees and Herd clones; Kiro agent specs and
  `mcp.json.example` files are compared and, with `--include`, synchronized.
- `--check`: the same comparison without writing, exit code 0 aligned, 1
  diverged, 2 unreadable; `--strict` makes warnings fail.
- Edits replace only the entry that changes, keeping indentation, key order, line
  endings and BOM; a file whose content would not change is never rewritten.
- A three-version rule for `autoApprove` / `disabledTools`: a copy carrying an
  older catalog value is refreshed, a local customization is kept and reported,
  with the tools the catalog disables that the copy leaves active. History in
  `~/.kiro/mcp-catalog.state.json`.
- Git worktrees are discovered from git's records, wherever they live, and
  attached to their project; folded under it in the matrix view and unfolded when
  they diverge or have no config.
- `--init-worktree` (and `i` in the matrix view) writes a worktree's config from
  its project.
- Backups of every write in `~/.kiro/kms-backups` (last 20 runs) and `--undo`,
  with `--undo --list`.
- Warnings: an `mcp-remote` `env` variable no argument references, a literal
  secret in a copy. `--status` and the matrix view report the copies outside the
  projects.

### Changed
- `--discover` lists worktrees under their project instead of as projects of
  their own.
- `--dry-run` and `--force` apply to the new commands as well.

## [0.1.0] - 2026-09-28

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

[Unreleased]: https://github.com/shaffe-fr/kiro-mcp-scope/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/shaffe-fr/kiro-mcp-scope/releases/tag/v0.1.0
