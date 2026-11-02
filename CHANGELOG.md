# Changelog

## 0.1.0

First release of `kondor`, a rewrite of the kondo TUI.

- Parallel scanner that reads every folder once and streams results as they're measured.
- Sizes count real disk usage and count hard-linked files (e.g. pnpm) once.
- Mixed projects (say Rust + Node in one folder) are cleaned for every kind they contain.
- Table view with size bars, sort by size, age or name, filter, details and help popups.
- Confirmation before every delete; `--dry-run` to try it safely.
- Mouse support: scroll without losing your selection, click to select or mark, double-click for details, click headers to sort.
- `--older` age filter, `--ignore`, `--all`, `--same-fs`, `--follow-links`, shell completions.
