# AGENTS.md

Guide for AI agents (Hermes, Codex, Claude Code, …) working with this repository. Answer the user in their language.

## What the tools are

- `outlook-cli.exe`: read-only reader of the Outlook data file (`.ost`/`.pst`) on a Windows PC, even while Outlook
  runs. Lists, filters, reads messages, extracts attachments, reports new mail. JSON output. Cannot send, delete or
  mark as read.
- `outlook-index.exe` (preview): a local hybrid index of the whole mailbox (SQLite FTS5 + embeddings computed on
  the CPU) for keyword and semantic search across years and attachment text.
- Everything stays on the PC. The only network access: the GitHub release download (or the toolchain for a source
  build) and `outlook-index model fetch` (embedding model, ~1.1 GB from huggingface.co).
- Windows `.exe` only, from native PowerShell (5.1 or 7) or WSL; no admin rights needed. No extra tool is required:
  Windows PowerShell 5.1 and the built-in `curl.exe` (model download) suffice; `gh`, `git`, WSL, PowerShell 7,
  `winget` and Hermes only open optional paths (setup skill step 0).

## Which skill to load

Repo-local skills in [`.agents/skills/`](.agents/skills/) (Agent Skills format; Hermes loads them after
`hermes skills trust <checkout>`):

| Task | Skill |
|---|---|
| Install, first index, refresh schedule, upgrade, uninstall | [`outlook-mail-setup`](.agents/skills/outlook-mail-setup/SKILL.md) |
| Search, read, watch the user's mail once installed | [`outlook-cli`](.agents/skills/outlook-cli/SKILL.md) |

`scripts\install.ps1 -Skill hermes|codex|claude` (WSL: `scripts/install.sh --skill …`) installs the `outlook-cli`
skill for use outside this repository.

## Consent gates

Run these only after the user's explicit consent in the current conversation (or a standing instruction the user
recorded), and never while another run is active (`outlook-index status` → `running` not `null`, not `stale`):

- `outlook-index model fetch` (download), `bench-embed` (CPU), `doctor` (walks the whole mailbox);
- `sync` and `embed` (hours of CPU for a first index), `scope` with an option (scope change);
- setting up or changing a scheduled refresh (Windows scheduled task or Hermes cron job);
- restoring a backup; `install.ps1 -Uninstall -PurgeData -Yes` (deletes the index and models);
- downloading a build toolchain (`build.ps1 -InstallToolchain`).

Allowed without asking: every `outlook-cli` command, `outlook-index search`, `status`, `scope` without options,
`backup`, `model list`, `--version`, `--help`.

## Privacy

- Search results and messages contain subjects, addresses and text: quote minimal excerpts, never copy bodies,
  attachments or result lists to a third-party tool or service (API, ticket, repository, shared log) without the
  user's consent.
- Reports, logs and feedback: aggregates only (counts, sizes, durations, error kinds). No subjects, names,
  addresses, folder or file names (the OST file name is often the e-mail address), no paths.
- Never set `OUTLOOK_INDEX_DEBUG`, even when an error message suggests it.
- Do not fix `permissions_warning`, `upgrade_pending` or a refused mass deletion yourself: report them.

## Contributors

- Build: `scripts/build.sh` (WSL/Linux, `scripts/setup-toolchain.sh` once) or `scripts\build.ps1` (native Windows,
  gnullvm + llvm-mingw, no Visual Studio). About 8 GB free RAM; `CARGO_BUILD_JOBS=1` when short.
- Checks (CI, `.github/workflows/ci.yml`): `cargo fmt --check`; `cargo clippy --release --all-targets -- -D warnings`
  and the same with `--features index`; `cargo test --release` with and without `--features index`;
  `shellcheck scripts/*.sh tests/scripts/*.sh`; `bash tests/scripts/install_sh_test.sh`; on Windows
  `tests/scripts/windows.ps1 -ExeDir target/release` under both PowerShells. Tests with less RAM:
  `CARGO_PROFILE_RELEASE_LTO=off CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16`.
- Do not touch `src/ndb.rs`, `src/ltp.rs`, `src/crypt.rs`. `outlook-cli` keeps its dependency set, behaviour and
  help (no SQLite or embedding crate: CI checks `cargo tree`). No new crate without a reason; index schema
  migrations are additive only. Errors and outputs must never carry mail content or paths.
- Releases (tags, GitHub releases) are cut by the owner only: [docs/RELEASING.md](docs/RELEASING.md). Code,
  comments, commits and agent docs in English; the README is French (user-facing).
