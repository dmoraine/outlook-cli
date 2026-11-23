# outlook-index — detailed reference

Companion of [SKILL.md](../SKILL.md) (read it first: choice of tool, answer workflow, agent rules).
Checked against `outlook-index --help` and the `--help` of each subcommand (binaries built at commit
`a2001e75a98e221558b4aaac281b8ce91bc9bea8`), then against the [README](../../../../README.md#outlook-index-preview)
and [docs/WINDOWS_TESTING.md](../../../../docs/WINDOWS_TESTING.md) at `990b9dc` (extractor limits, memory-adapted
worker defaults). "Agent" column: ✅ allowed, 🔒 only after the user's explicit consent (SKILL.md §5).

## Contents

1. [Conventions and index file](#1-conventions-and-index-file)
2. [Commands, readers and writers](#2-commands-readers-and-writers)
3. [search](#3-search)
4. [status and run tracking](#4-status-and-run-tracking)
5. [scope](#5-scope)
6. [sync](#6-sync)
7. [embed, model, bench-embed](#7-embed-model-bench-embed)
8. [backup and restore](#8-backup-and-restore)
9. [doctor](#9-doctor)
10. [Permissions](#10-permissions)
11. [Schema upgrade](#11-schema-upgrade)
12. [Errors and warnings](#12-errors-and-warnings)
13. [Appendix: accuracy corrections](#13-appendix-accuracy-corrections)

## 1. Conventions and index file

- Separate binary (preview): a single SQLite index (metadata, FTS5, vectors), embeddings computed on the CPU.
  Read-only on the OST; no network access except `model fetch`. Always the Windows `.exe` (link
  `~/.local/bin/outlook-index` under WSL).
- Global options of every command: `-f/--file` (env `OUTLOOK_CLI_FILE`), `--index` (env
  `OUTLOOK_INDEX_DB`), `--pretty`, `--local-time`, `--ascii` (env `OUTLOOK_CLI_ASCII`). No global `--format`:
  only `search` has `--format json|jsonl`. JSON on stdout; errors and warnings as JSON on stderr.
- Default index: `%LOCALAPPDATA%\outlook-cli\index.sqlite` (Linux: `$XDG_DATA_HOME/outlook-cli/index.sqlite`).
  Keep it on the Windows side (not under `~` in WSL, not under OneDrive: `onedrive` warning). From WSL, an
  environment variable only reaches the `.exe` through `WSLENV` (e.g. `WSLENV=$WSLENV:OUTLOOK_INDEX_DB/p`);
  an option (`--index /mnt/c/...`) needs nothing.
- The index can always be rebuilt from the OST; the backup only saves the hours of embedding.

## 2. Commands, readers and writers

| Command | Role | Index access | Agent |
|---|---|---|---|
| `search` | search | reader (consistent snapshot) | ✅ |
| `status [--watch [S]]` | counts, running run | reader | ✅ |
| `scope` without options | scopes + counts | reader | ✅ |
| `backup [--to P]` | checked copy into a new file | reader | ✅ |
| `model list` | known models, files present | does not open the index | ✅ |
| `model fetch` | downloads ~1.1 GB (e5-base) / ~490 MB (e5-small) | reader | 🔒 network |
| `bench-embed` | embedding throughput, synthetic text | reader | 🔒 CPU |
| `doctor` | aggregate diagnostic of the mailbox and the PC | does not open the index | 🔒 |
| `sync` | updates the index, then embeddings | **writer** | 🔒 |
| `embed` | pending embeddings | **writer** | 🔒 |
| `scope` with an option | changes the scope | **writer** | 🔒 |

One writer at a time per index (lock `index.sqlite.lock`): it waits up to 3 s, then fails (`usage`,
"another sync is running"). Readers never create, migrate or modify the index and can run during a sync.

## 3. search

`outlook-index search [OPTIONS] <QUERY>` — best chunk per message.

| Option | Default | Effect |
|---|---|---|
| `--mode keyword\|semantic\|hybrid` | `hybrid` if the index has vectors of its model, else `keyword` | see modes |
| `-n, --limit N` | 10 | maximum number of hits |
| `-F, --folder X` | every indexed folder | name, or path with its subfolders (`Inbox` covers `Inbox/Projects`, not `Inboxes`); repeatable = OR. No id and no `all` (unlike `outlook-cli`) |
| `-s, --since T` / `-u, --until T` | — | received from (inclusive) / strictly before; same syntax as `outlook-cli` |
| `--from S` | — | sender (name or address) contains; repeatable = OR |
| `--to S` | — | a **To/Cc** recipient contains (not Bcc); repeatable = OR |
| `--has-attachments` | — | messages with attachments |
| `--excerpt-max N` | 300 | excerpt length, max 2000 |
| `--format json\|jsonl` | `json` | `jsonl`: one hit per line |
| `--model e5-small\|e5-base` | model of the stored vectors | another model → `usage` |
| `--model-dir D` | `<data dir>/models/<repository>` (env `OUTLOOK_INDEX_MODEL_DIR`) | model files |

Modes:
- `keyword`: every word required (FTS5), case- and accent-insensitive, in subject + body or in one attachment
  chunk. Operators and quotes are treated as words (no exact phrase). Does not load the model. Excerpt around
  the words.
- `semantic`: query embedded locally (prefix `query: `), cosine against every embedded chunk (brute force).
  Finds other wordings and the other language; only sees embedded chunks (scope, attachment cap). Excerpt:
  start of the best chunk.
- `hybrid`: RRF fusion (k = 60) of the first 200 hits of an "any word" keyword search and the first 200
  semantic hits; messages outside the embedding scope are still found by their words.
- Fallback: `semantic`/`hybrid` without vectors → `keyword` with `{"warning":{"kind":"keyword_fallback",...}}`;
  the default mode too when the model files are missing. An explicit `--mode semantic|hybrid` without model
  files → `not_found`.
- Cost: `semantic`/`hybrid` load the model on every call (≈ 6 s for e5-base on the owner's laptop, `load_ms`
  6439; ~1.4 s for e5-small on the test VPS); the search itself takes under a second.

`json` output: `{count, mode, hits}` (`mode` = mode actually used, after fallback). Fields of a hit:
`id`, `message_id`, `folder` (path), `received`, `sent`, `subject`, `from {name, email}`, `to`, `cc`, `size`,
`class`, `has_attachments`, `read` (beware: `outlook-cli` gives `unread`), `excerpt`, `source`, `ranks`,
`scores`.
- `source`: `"body"` or `{"kind":"attachment","index":N,"name":"...","type":"pdf|docx|..."}`; `index` is the
  one of `outlook-cli attachments <id> -i <index>`.
- `ranks`/`scores`: `keyword` (bm25) and/or `semantic` (cosine) for the searches that found the message;
  in hybrid also `ranks.hybrid` and `scores.rrf`, plus the flat fields `keyword_rank`, `semantic_rank` (`null`
  if absent from that search) and `score` (RRF).
- `id` works with `outlook-cli get` as long as the message has not been moved; otherwise `get "<message_id>"`.

## 4. status and run tracking

`outlook-index status`: `messages`, `chunks`, `folders`, `locations`, `schema` (`version`, `expected`,
`upgrade_pending`), `embedding` (model, revision, dimensions, vectors, scope counts), `include`,
`attachments`, `db_bytes`, `last_sync` (`at`, `revision`, `complete`, `summary` = summary of the last sync),
`running`, `permissions_warning`. Aggregates only.

- `running`: `null` without a run; otherwise `phase`, `done`, `total`, `percent`, `rate`, `eta_s`, `elapsed_s`,
  `workers`, `model`, `pid`, `started_at`, `updated_at`, `interval_s`…; `stale: true` = process gone, the next
  `sync` resumes.
- `status --watch [SECONDS]` (default 10): one `{"running": ...}` line per tick (even with `--pretty`), stops on
  `null`, `done` or `stale`.
- `sync`/`embed`: progress on stderr, final summary only on stdout. `--progress auto|bar|jsonl|none` (default
  `auto` = `bar` on a terminal, else `jsonl`: lines `{"progress":{...}}`), `--progress-interval S` (default
  10; every phase change is reported). Phases: `scan`, `index`, `attachments`, `embed_bodies`,
  `embed_attachments`.

## 5. scope

Without options (✅): shows the scopes and counts: `chunks_in_scope`, `embedded`, `pending`, `embedded_pct`,
`out_of_scope`, `out_of_scope_embedded`, `attachment_chunks`, `attachments`. Folders are counted, not
printed. Search covers every indexed folder; only the embedding scope is limited.

With an option (🔒, writer; options left out are kept):
- `--embed-since T` (inclusive) / `--embed-until T` (exclusive): stored as absolute dates (`90d` stays the date it
  meant).
- `-F, --folder X`: as `search -F`, repeatable, replaces the stored folders.
- `--attachment-chunks N`: chunks embedded per distinct content, from the start (default 30, ~50,000 characters;
  `0` = none). Keyword search covers the whole text; raising it embeds the rest, lowering it keeps the vectors.
- `--attachment-types` (comma-separated list, replaces the previous one; values `pdf`, `docx`, `xlsx`,
  `pptx`, `txt`, `csv`, `html`), `--attachment-max-mb N` (1 to 100, default 20).
- `--clear`: resets both scopes to the defaults (everything, 30 chunks, every type, 20 MB); cannot be combined.
A scope change never deletes a vector or extracted text.

## 6. sync

🔒 Writer. Reconciles the index with every mail folder: new, modified (`refreshed`), deleted,
moved/copied, read/unread; then attachment text; then embeddings of the scope if the model files are present.
Resumable: an interrupted sync continues where it stopped. Nothing is done if the file has not changed since
the last complete sync.

| Option | Default | Effect |
|---|---|---|
| `--max-time D` | — | stop after `90s`/`30m`/`2h`, embedding included |
| `--no-embed` | — | indexing only |
| `--no-attachments` | — | skips the attachment step; a later sync catches up |
| `--include-folders deleted\|junk\|none` | `none` | include Deleted Items / Junk E-mail (stored, `status.include`) |
| `--allow-mass-delete` | — | allows deleting more than max(100, 20 %) of the indexed messages (otherwise stops with no change, `usage`) |
| `--extract-memory-mb N` | ¼ of the available memory at start, clamped to 256 MB..2 GB (1 GB if unknown) | memory limit of the extractor child process (at least 128) |
| `--model`, `--model-dir`, `--workers`, `--threads`, `--worker-timeout`, `--batch`, `--reembed`, `--priority`, `--progress`, `--progress-interval` | | as `embed` (§7) |

Indexing: common quotes and signatures removed, chunks of about 1,750 characters (overlap ~200).
Attachments: inline, images, too large, attached messages, links and unknown extensions skipped without reading;
the rest is extracted in a child process (30 s max, OS memory limit; `failed`/`timeout`/`memory_limit` retried,
3 attempts in all). The parent only checks sizes and magic bytes; every archive and PDF is parsed in the child.
Structural caps in the child (past them: `too_complex`, file skipped, sync goes on): ZIP 10,000 entries, 16 path
levels, 256 MB unpacked per part and 512 MB per file, unpacked/packed ratio 100 past 16 MB, nested archives never
opened; XML 256 nesting levels, 10,000,000 elements per part; PDF 200,000 objects, 10,000 pages. Formats: PDF
(text layer, no OCR), DOCX, PPTX, XLSX, TXT, CSV, HTML; text limited to 2,000,000 characters. Skip reasons:
`inline`, `image`, `too_large`, `unsupported_type` (archives, old `.doc`/`.xls`, Markdown…), `excluded_type`,
`embedded_message`, `encrypted`, `empty_text` (scanned PDF), `failed`, `timeout`, `too_complex`, `memory_limit`.

Summary (stdout, counts): `complete`, `new`, `refreshed`, `deleted`, `excluded_removed`, `moved`,
`flags_changed`, `chunks_added`, `failed`, `remaining`, `errors`, `totals`, `attachments_extracted`,
`attachments_deduplicated`, `attachments_failed`, `attachments_skipped_by_reason`, `attachments_errors`,
`attachments_remaining`, `embedding` (result of the step, or `{"skipped": ...}`).

## 7. embed, model, bench-embed

`embed` (🔒, writer): pending body chunks, newest message first, then attachment chunks under the cap.
Every batch is committed: `Ctrl-C` only loses the batches in flight.

| Option (`embed`, `sync`) | Default |
|---|---|
| `--max-time D` | — (`embed`: no new batch after D; batches in flight finish) |
| `--model e5-small\|e5-base` | model of the index, else `e5-base`; another model requires `--reembed` |
| `--model-dir D` | `<data dir>/models/<repository>` (env `OUTLOOK_INDEX_MODEL_DIR`) |
| `--workers N` | (logical CPUs − 2) / 3, at least 1, lowered to fit (available memory − 1 GiB) / per-worker estimate (e5-small 1.25 GiB, e5-base 2.75 GiB); > 1 = as many processes, each with its own model |
| `--threads N` | 3 (per worker) |
| `--worker-timeout D` | 180 s to load the model, 120 s per batch; past it: worker killed, `io` stop |
| `--batch N` | 8 passages per batch |
| `--reembed` | — deletes the vectors of another model |
| `--priority low\|normal` | `low` (Windows below normal, Linux nice 10) |

`embed` output: `embedded`, `pending`, `attachment_chunks`, `dropped`, `complete`, `interrupted`,
`passages_per_s`, `load_ms`, `elapsed_ms`, model, `workers`, `threads`, `priority`. Model files missing:
`not_found` for `embed`, step skipped for `sync`.

Models: `e5-base` (`intfloat/multilingual-e5-base`, 768 dimensions, default of a new index) or `e5-small`
(384). Files `config.json`, `tokenizer.json`, `model.safetensors`, pinned revision, SHA-256 checked on every
load; default folder `%LOCALAPPDATA%\outlook-cli\models\multilingual-e5-base` (or `-small`).
- `model list` (✅): pinned revisions and presence of the files (sizes only).
- `model fetch [--model M] [--model-dir D]` (🔒): HTTPS from huggingface.co with `curl`, resumes an
  interrupted download, SHA-256 check. The tool's only network access.
- `bench-embed` (🔒): `--seconds` (default 30), `--model`, `--model-dir`, `--workers`, `--threads`,
  `--worker-timeout`, `--batch` (8); synthetic text, output `passages_per_s`.
  Estimated embedding time: (`pending` + `attachment_chunks.pending` of `scope`) ÷ `passages_per_s`.

## 8. backup and restore

`outlook-index backup [--to <new file>]` (✅, reader, safe during a sync): consistent copy
(SQLite `VACUUM INTO`) into a **new** file; an existing destination is refused (`usage`). Default:
`backups/index-YYYYMMDD-HHMMSS.sqlite` next to the index. The copy is reopened read-only and checked.
Output: `destination`, `bytes`, `schema_version`, `counts` (`messages`, `chunks`, `vectors`, `attachment_chunks`,
`attachment_vectors`), `integrity` (`ok`; otherwise `inconsistent` error and copy deleted). Do not copy
`destination` (a path) into a report. Never copy `index.sqlite` (or `-wal`/`-shm`) by hand.

Restore (🔒), **every `outlook-index` stopped** (no `sync`, `embed`, `search`, `status --watch`, scheduled sync):
delete `index.sqlite-wal` and `index.sqlite-shm` if they exist, then copy the verified backup onto
`index.sqlite`:

```powershell
$d = "$env:LOCALAPPDATA\outlook-cli"
Remove-Item "$d\index.sqlite-wal", "$d\index.sqlite-shm" -ErrorAction SilentlyContinue
Copy-Item "$d\backups\index-YYYYMMDD-HHMMSS.sqlite" "$d\index.sqlite"
```

## 9. doctor

`doctor` (🔒): aggregates only (machine with `machine.defaults`: extractor memory limit `extract_memory_mb` and
default `workers` per model, SQLite/FTS5, index state with `permissions_warning`, folder and message counts, scan
time, statistics of a sample with `sample.body_chunks.mailbox_estimate`). `--sample N`: messages opened (default
200); with `--attachments`, attachments each extracted in a child process (default 300, 30 s each;
`attachments.chunks.mailbox_estimate`, `limits.memory_mb`, `limits.memory_enforced`; `--extract-memory-mb N` as for
`sync`). No mail data in the output.

## 10. Permissions

The index holds a plain-text copy of the mails. On Unix, an index folder created by the tool is 0700 and its
files 0600; an existing folder or file is never changed. If it is accessible to the group or others, `status` and
`doctor` show `permissions_warning` (fixed text; `null` otherwise). Agent: report it, do not change the
permissions. On Windows, `%LOCALAPPDATA%` is per-user; an `--index` elsewhere keeps the ACLs its owner chose.

## 11. Schema upgrade

After a tool update, `status` may show `"schema": {"version": 4, "expected": 6, "upgrade_pending":
true}` (counts of missing tables `null`); `search` and `scope` without options then fail with
`upgrade_pending` (code 3). The next writer (`sync`, 🔒) applies the upgrade in a single transaction.
An index from a newer version than the tool is refused (`format`, 5). Do not install a new version
while an `outlook-index` runs (Windows refuses to overwrite a running `.exe`).

## 12. Errors and warnings

Codes: 0 ok, 2 `usage`, 3 `not_found` / `upgrade_pending`, 4 `io`, 5 `format`, 6 `inconsistent`. On stderr:
`{"error":{"kind","message"}}`. The `message` is a fixed text per kind (plus numbers, option names, allowed
values, model names or a code location): never a path, folder or attachment name, query or mail text; an error
line can be copied as is. Option syntax errors are JSON too (`usage`). Some messages suggest
`OUTLOOK_INDEX_DEBUG=1`: **an agent never sets it** (the `debug` field holds the raw text: paths, names, possibly
mail content).

- `usage` (2): query without a word, invalid option, `--model` different from the vectors, another run in
  progress, refused mass deletion, existing `backup` destination.
- `not_found` (3): no index yet (→ `sync`, 🔒), `-F` filter with no matching folder in the index, model files
  missing with an explicit mode (→ `model fetch`, 🔒).
- `upgrade_pending` (3): see §11. `io` (4): unreadable file, worker timed out. `format` (5): mailbox, index or
  model file not supported. `inconsistent` (6): the mailbox changed during the read, retry.
- Warnings (stderr): `{"warning":{"kind":"keyword_fallback",...}}`, `{"warning":{"kind":"onedrive",...}}`.

## 13. Appendix: accuracy corrections

Found when comparing the old `SKILL.md` with the `--help` of the `a2001e7` binaries, the README and the docs:

1. `model fetch` and `bench-embed` were listed among the commands an agent may run: they need the user's
   consent (~1.1 GB network download; CPU kept busy).
2. "`search` and `scope` fail with `upgrade_pending`" → only `scope` **without options** fails; with an option
   it is a writer, which applies the upgrade.
3. File resolution order: `-f/--file` first, else `$OUTLOOK_CLI_FILE`, else the most recent `.ost`
   (the old text put `-f` last).
4. `search` output: fields `sent`, `size`, `class` added; `folder` is a path; the actual mode is in `mode` (the
   `--help` of `--format` says `{count, hits}`, the real output also has `mode`).
5. `search -F`: name or path only (no id, no `all`); `search --to` covers To/Cc, not Bcc.
6. `status --watch` takes a period (`--watch 30`); `last_sync` holds `at`, `revision`, `complete`, `summary`.
7. `scope --attachment-chunks`: default 30 (~50,000 characters); `--attachment-max-mb` default 20;
   `--attachment-types` limited to `pdf, docx, xlsx, pptx, txt, csv, html`.
8. `scripts/build.sh` also produces `dist/outlook-cli` (Linux); `outlook-cli list`: short `-b` for `--body`,
   `--body-max` default `0` (no limit); `watch -i` = `--interval`, `attachments -i` = `--index`, `-d` default `.`.
9. Removed: "preview up to ~500 characters" and "already retried 5 times" (not verifiable in `--help`/README);
   `preview` only appears without `--body`.
10. `doctor` is no longer listed as unrestricted: it walks the whole mailbox (and `--attachments` runs
    extractions); consent required.
11. Broken line at the end of section 9 (sentence "Details: [README]…" glued to the errors paragraph): replaced
    by the header links of this reference.
12. Errors: an agent does not follow the `OUTLOOK_INDEX_DEBUG=1` suggestion present in some messages.
