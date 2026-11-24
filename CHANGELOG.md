# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org/). Release process: [docs/RELEASING.md](docs/RELEASING.md).

## [0.2.0] - 2026-11-24

### Added

- `outlook-index` (preview), a separate binary (cargo feature `index`) that builds a local hybrid index of the
  mailbox: metadata, SQLite FTS5 full-text index and vectors in one SQLite file, embeddings computed locally on
  the CPU (`multilingual-e5-base` by default, `e5-small` optional; pinned revisions checked by SHA-256).
  Commands: `doctor`, `sync`, `embed`, `search` (`keyword`, `semantic`, `hybrid`), `status`, `scope`, `backup`,
  `model list|fetch`, `bench-embed`.
- Incremental refresh: `sync` reconciles new, changed, deleted, moved and copied messages and read/unread changes,
  reopens only new or changed messages, reuses vectors of unchanged chunks, and refuses a mass deletion unless
  `--allow-mass-delete`. Deleted Items and Junk E-mail excluded by default.
- Attachment text (PDF text layer, DOCX, PPTX, XLSX, TXT, CSV, HTML), deduplicated by SHA-256, searchable by
  keyword and, up to a per-attachment chunk cap, by meaning.
- Memory containment of attachment extraction: all parsing in a child process with structural caps
  (`too_complex`) and an OS memory limit adapted to the available memory (`--extract-memory-mb`,
  `memory_limit`); embedding worker defaults adapted to the available memory.
- Progress reporting on stderr (`--progress`) and live run state through `status` / `status --watch`.
- Verified index backups (`backup`, SQLite `VACUUM INTO`, integrity check on the copy).
- Privacy boundary: outputs and errors carry counts, sizes, durations and fixed error kinds only, never mail
  content or paths; private permissions for new index files on Unix; no network access except `model fetch`.
- Native Windows PowerShell scripts (Windows PowerShell 5.1 and 7, no admin): `install.ps1` (release download
  checked against `SHA256SUMS`, attestation checked when `gh` is logged in, fail closed; `-FromDir` for local
  exes; `-Uninstall`, with `-PurgeData -Yes` to also delete the index and models), `build.ps1` (source build
  without WSL or Visual Studio), `schedule.ps1` (per-user scheduled refresh). Every script download follows
  redirects over `https` only. WSL: `install.sh --release`.
- Release assets: `outlook-cli.exe`, `outlook-index.exe`, `SHA256SUMS` and GitHub artifact attestations for both
  executables; manual dry-run workflow.
- Agent onboarding: `AGENTS.md`, repo-local skills `outlook-mail-setup` and `outlook-cli` in `.agents/skills/`,
  consent gates for costly steps, `-Skill` / `--skill` to install the usage skill for Hermes, Codex or Claude Code.
  Setup adapts to the tools present: Windows PowerShell 5.1 and the built-in `curl.exe` suffice; `gh`, `git`, WSL,
  PowerShell 7 and `winget` are optional.
- README installation rewritten release-first (native PowerShell) and this changelog.

### Unchanged

- `outlook-cli`: same commands, options and help as 0.1.0; no SQLite or embedding crate in its dependency set
  (checked in CI).

### Known limits

- No OCR, no legacy `.doc`/`.xls`, no embedded messages or encrypted attachments.
- Brute-force vector search (no ANN index); each embedding worker holds its own copy of the model.
- The extractor memory limit is tested on Linux and Windows only, not on macOS or the BSDs.

## [0.1.0] - 2026-10-15

Initial `outlook-cli` (not tagged): read-only reader of Outlook `.ost`/`.pst` files while Outlook runs, with
`info`, `folders`, `list`, `get`, `attachments`, `new` and `watch`; JSON, JSONL and text output; stable exit codes.

[0.2.0]: https://github.com/dmoraine/outlook-cli/releases/tag/v0.2.0
[0.1.0]: https://github.com/dmoraine/outlook-cli/tree/b8efdb4
