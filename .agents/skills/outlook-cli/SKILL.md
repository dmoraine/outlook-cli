---
name: outlook-cli
description: >-
  Read, search and watch the user's local Outlook mailbox (.ost/.pst file, Windows/WSL PC), read-only.
  outlook-cli lists the latest messages, filters by date, sender, recipient, subject or folder, reads a full
  message, extracts attachments and reports new e-mails (new --state, watch). outlook-index searches the whole
  indexed mailbox, across years and inside attachment text, by keywords or by meaning (local embeddings), to answer
  a question about mail content. Use it whenever a task needs to consult, search, summarise or watch the user's
  Outlook e-mail. To install or set up the index, use the outlook-mail-setup skill.
---

# outlook-cli and outlook-index

- **`outlook-cli`** reads Outlook's OST/PST directly, even while Outlook is open: JSON, ~50 ms, always as current
  as what Outlook has synchronised. It can **not send, delete or mark as read**.
- **`outlook-index`** (preview) queries a local SQLite index of the whole mailbox: keywords (FTS5) and meaning
  (embeddings computed on the CPU). It is only as current as the last `sync`. Full reference (options, outputs,
  progress, backup, permissions, upgrade): [references/outlook-index.md](references/outlook-index.md).

## 1. Check / install

```bash
outlook-cli --version && outlook-cli info     # {"kind":"ost","format":...,"inbox":{...}}
outlook-index --version && outlook-index status --pretty
```

Always the **Windows `.exe`**, even from WSL (`~/.local/bin/outlook-cli` and `outlook-index` are links to
`%LOCALAPPDATA%\Programs\outlook-cli\`): a Linux binary cannot read an OST that Outlook has open (`io` error, 4).
PowerShell: `& "$env:LOCALAPPDATA\Programs\outlook-cli\outlook-cli.exe" --version` (`scripts\install.ps1` also adds
that folder to the user PATH). Missing? Install with the **outlook-mail-setup** skill of the repository
(`https://github.com/dmoraine/outlook-cli`, `.agents/skills/outlook-mail-setup/`): verified GitHub release
(`scripts\install.ps1`, `scripts/install.sh --release`) or source build (`scripts\build.ps1`, `scripts/build.sh`).

File read: `-f/--file`, else `$OUTLOOK_CLI_FILE`, else the most recently modified `.ost` in
`%LOCALAPPDATA%\Microsoft\Outlook` (WSL paths `/mnt/c/...` accepted). Options common to both tools:
`--pretty`, `--local-time` (local times instead of UTC), `--ascii` (`$env:OUTLOOK_CLI_ASCII="1"`, required before
`ConvertFrom-Json` in Windows PowerShell 5.1).

## 2. Which tool?

| Need | Command |
|---|---|
| Latest messages, new ones, one folder, exact header filters | `outlook-cli list` / `new` |
| A topic across the whole mailbox, over years, including attachments | `outlook-index search "<query>"` |
| Unknown wording, vague memory, other language than the mail | `outlook-index search "<query>" --mode semantic` |
| Read the message found | `outlook-cli get <id>` (or `get "<message_id>"` if the id changed) |
| Get an attachment | `outlook-cli attachments <id> -i <index> -d <dir>` |
| Messages received after the last `sync` | `outlook-cli list -F all --since <last_sync.at>` |

## 3. outlook-cli: the essentials

```bash
outlook-cli list -n 10                                  # 10 latest of the Inbox (default -n 20; 0 = no limit)
outlook-cli list -F all --subject invoice --from dupont --since 2026-01-01 -n 20
outlook-cli get 2097444 --body-max 4000                 # body + recipients + attachments
outlook-cli attachments 2097444 -i 1 -d ./att           # -i repeatable; default: all but inline, into .
outlook-cli folders --format text                       # folder paths, ids and counts
```

- Filters (`list`, `new`, `watch`): `-F/--folder` (path `Inbox/Projects`, name or id; repeatable; `all` = every
  mail folder; default Inbox, found whatever the language), `-s/--since` (inclusive), `-u/--until` (exclusive),
  `--after` (exclusive, cursor), `--from`, `--to` (to/cc/bcc), `--subject` (case-insensitive substrings;
  `--from`/`--to` repeatable = OR), `--unread`, `--read`, `--has-attachments`, `--class IPM.Note`.
  Dates: `2026-10-15T08:00:00Z`, `2026-10-15T10:00+02:00`, `"2026-10-15 08:00"` and `2026-10-15` (local time),
  `90s` `15m` `2h` `3d` `1w`, `today`, `yesterday`. `list` only: `-n/--limit`, `--oldest-first`.
- Content: by default headers + `preview` (Outlook's preview), no body. `-b/--body` with `--body-max 2000`
  to `4000` (default `0` = no limit), `--recipients`, `--attachments`, `--full` (all three). `get`: `--body-max`,
  `--no-body`, `--html`, `--headers`. `--format json|jsonl|text` (default `json`).
- `list`/`new` output: `{"cursor","messages":[...],"count"}`; fields `id`, `folder`, `received`, `sent`,
  `subject`, `from{name,email}`, `to`, `cc`, `unread`, `has_attachments`, `importance`, `class`, `size`,
  `message_id`, `in_reply_to`, `preview`; optionally `body`/`body_truncated`, `recipients[{type,name,email}]`,
  `attachments[{index,name,size,mime_type,content_id,method,inline}]`. Times in UTC. `id` can change when the
  message is moved: **deduplicate on `message_id`**.
- Watching: `outlook-cli new --state ~/.agent/inbox.state --format jsonl --full --body-max 4000` (first call:
  nothing, or `--initial N`; then only never-seen messages; `--window` default `7d`; **one state file per
  consumer**). Long-running process: `outlook-cli watch -i 5 --state ~/.agent/watch.state` (JSONL; `--max N`).
- Pitfalls: `attachments` skips inline images without `--include-inline`, does not overwrite (` (1)` suffix) and
  does not extract an attached e-mail (`method: "embedded_message"`); quote values with spaces or starting with
  `-`; the body of meeting invitations is often empty.

## 4. Answering a question about the mailbox

1. **Freshness first**: `outlook-index status` → `last_sync.at` (date of the last sync), `running` (sync in
   progress? `null` otherwise), `schema.upgrade_pending` (if `true`, `search` fails: tell the user).
   For anything newer than `last_sync.at`: `outlook-cli list -F all --since <last_sync.at>` / `new`.
2. **Mode**:
   - `--mode keyword` for exact terms (name, reference, number, code, phrase): every word required,
     case- and accent-insensitive, instant, no model. No exact-phrase search: quotes and operators are ordinary
     words.
   - `hybrid` (default when the index has vectors) for a normal question.
   - `--mode semantic` for a paraphrase, a vague memory, another language than the mail.
3. **Query**: a short phrase or the keywords (`"maintenance contract renewal"`), not a full polite question.
   Nothing? 2 to 3 rewordings (synonyms, other language, `keyword` ↔ `semantic`), then narrow with
   `--since`/`--until`, `--from`, `--to`, `-F`, `--has-attachments` rather than adding words.
4. **Read**: take the first 3 to 5 hits; `outlook-cli get <id> --body-max 4000` (if `not_found`:
   `outlook-cli get "<message_id>"`, the message was moved). If `source.kind` is `attachment`:
   `outlook-cli attachments <id> -i <source.index> -d <dir>`. Deduplicate on `message_id`.
5. **Answer**: cite each source (date, sender, subject, and attachment name if relevant); separate what the
   mails say from what you infer; say clearly when nothing was found.

Fictitious example (`hybrid`, fields `sent`, `to`, `cc`, `size`, `class`, `scores` omitted):

```json
{"count":2,"mode":"hybrid","hits":[
 {"id":2101870,"message_id":"<k1-0042@example.org>","folder":"Inbox/Suppliers","received":"2025-03-04T09:12:00Z",
  "subject":"Maintenance contract renewal","from":{"name":"Claire Fontaine","email":"c.fontaine@example.org"},
  "has_attachments":true,"read":true,"excerpt":"… the contract is renewed for 12 months from 1 April …",
  "source":"body","ranks":{"hybrid":1,"keyword":2,"semantic":1},"keyword_rank":2,"semantic_rank":1,"score":0.032522},
 {"id":2101874,"message_id":"<k1-0043@example.org>","folder":"Inbox/Suppliers","received":"2025-03-05T14:30:00Z",
  "subject":"FW: signed amendment","from":{"name":"Marc Petit","email":"m.petit@example.org"},
  "has_attachments":true,"read":true,"excerpt":"Article 3 – Term: twelve months, renewed by tacit agreement",
  "source":{"kind":"attachment","index":1,"name":"amendment-2025.pdf","type":"pdf"},
  "ranks":{"hybrid":2,"semantic":2},"keyword_rank":null,"semantic_rank":2,"score":0.016129}]}
```

**"Not found" does not mean "does not exist"**. Blind spots to mention:
- messages received **after `last_sync.at`** (→ `outlook-cli list --since`);
- **Deleted Items and Junk E-mail** excluded from the index by default (→ `outlook-cli folders`, then
  `outlook-cli list -F "<path>" --subject ...`);
- **embedding scope** (dates/folders of `outlook-index scope`): out of scope, only `keyword` mode finds
  (`hybrid` too, through its words);
- attachments: only their **first 30 chunks** (~50,000 characters, default) are embedded; `keyword` covers the
  whole text;
- extraction **unsupported, failed or capped**: no OCR (scanned PDF, images), no old `.doc`/`.xls`, no archive
  content (zip), no attached e-mail, encrypted or too large files, files past a structural cap (`too_complex`) or
  the extractor's memory limit (`memory_limit`);
- messages Outlook **has not synchronised into the OST** (time-limited Exchange cache, Outlook offline).

**Cost**: each `semantic`/`hybrid` search loads the model (≈ 6 s for e5-base on the owner's laptop, `load_ms`
6439). Start with `keyword` when the exact terms are known; no long search loops; never run `sync` to "refresh"
without the user's consent (§5).

## 5. Rules for an agent

- **Allowed**: all of `outlook-cli` (read-only); `outlook-index search`, `status` (including `--watch`),
  `scope` **without options**, `backup` (writes a new file, safe), `model list`. All can run during a sync.
- **Only after the user's explicit consent in this conversation** (or a standing instruction the user recorded):
  `sync`, `embed`, `scope` with an option (scope change), restoring a backup, `model fetch` (downloads ~1.1 GB
  from the network), `bench-embed` (keeps the CPU busy), `doctor` (diagnostic that walks the whole mailbox),
  setting up or changing a scheduled refresh (Windows scheduled task or Hermes cron job). Never start one of them
  while another run is active (`status` → `running` not `null` and not `stale`, or a scheduled sync running).
- Do not fix `permissions_warning`, `upgrade_pending` or a refused mass deletion yourself: report it.
- **Privacy**: hits contain subjects, addresses and excerpts. Quote minimal excerpts; never copy a whole body or
  attachment, or search results, to a third-party tool or service (external API, ticket, repository, shared log)
  without the user's consent. `doctor`, `status`, `scope`, `backup` and `sync` summaries contain aggregates only.
- **Never set `OUTLOOK_INDEX_DEBUG`**, even if an error message suggests it: it adds a `debug` field with paths,
  names and possibly mail text.

## 6. Errors

Exit code, and on stderr `{"error":{"kind":"...","message":"..."}}` (warnings:
`{"warning":{"kind":...}}`, e.g. `keyword_fallback`, `onedrive`).

| Code | kind | Action |
|---|---|---|
| 0 | — | success (zero hits is not an error) |
| 2 | usage | fix the arguments (option syntax errors: plain text for `outlook-cli`, JSON for `outlook-index`) |
| 2 | usage (index) | also: query without a word, another run in progress, refused mass deletion |
| 3 | not_found | id, Message-ID or folder does not exist: check with `folders` / `list` |
| 3 | not_found (index) | no index yet, `-F` unknown to the index, model missing with an explicit `--mode`: report |
| 3 | upgrade_pending | index from an older version: a `sync`, with the user's consent |
| 4 | io | file not found or locked: use the `.exe`, check `-f` / `OUTLOOK_CLI_FILE` |
| 5 | format | unsupported file (ANSI PST Outlook 97–2002, not a PST/OST); index newer than the tool |
| 6 | inconsistent | Outlook was writing during the read: retry a few seconds later |
