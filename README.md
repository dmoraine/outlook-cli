# outlook-cli

CLI Rust qui lit directement le fichier de données d'Outlook (`.ost`, ou `.pst`) et en extrait les e-mails selon des
critères passés en ligne de commande. Conçu pour des agents IA qui surveillent l'arrivée de nouveaux messages :
sortie JSON, codes de retour stables, démarrage instantané, mémoire minimale.

- **Fonctionne pendant qu'Outlook tourne** : Outlook verrouille l'en-tête de l'OST (verrou de plage d'octets) ;
  le fichier est lu par projection mémoire, que ces verrous n'affectent pas. Lecture seule, jamais d'écriture.
- **Rapide** : ~50 ms pour les 10 derniers e-mails d'une Inbox de 10 000 messages dans un OST de 9 Go ; seules les
  pages utiles du fichier sont lues.
- **Léger** : binaire autonome de 1,3 Mo, 5 à 25 Mo de mémoire privée (≈ 30 Mo pour exporter 13 000 messages
  avec leur corps, la sortie étant écrite au fil de l'eau).
- **Formats** : OST Outlook 2013+ (pages 4 Ko, blocs compressés zlib), PST/OST Unicode (Outlook 2003–2010),
  encodages PST « compressible » et « high ». Le format ANSI (Outlook 97–2002) n'est pas géré.

## Installation

Le binaire est un `.exe` Windows, appelable aussi bien depuis PowerShell que depuis WSL. Il doit s'exécuter côté
Windows : depuis WSL, un programme Linux ne peut pas lire un OST ouvert par Outlook.

Prérequis de compilation (sous WSL/Linux, sans droits root) :

```bash
scripts/setup-toolchain.sh   # rustup + cible x86_64-pc-windows-gnullvm + linker llvm-mingw (~/opt)
scripts/build.sh             # tests + dist/outlook-cli.exe (Windows) et dist/outlook-cli (Linux)
scripts/install.sh           # %LOCALAPPDATA%\Programs\outlook-cli\outlook-cli.exe + lien ~/.local/bin/outlook-cli
```

PowerShell : `& "$env:LOCALAPPDATA\Programs\outlook-cli\outlook-cli.exe" info`
(ou ajouter ce dossier au `PATH` utilisateur pour taper simplement `outlook-cli`).

## Fichier lu

Par défaut : `$OUTLOOK_CLI_FILE`, sinon le `.ost` modifié le plus récemment dans
`%LOCALAPPDATA%\Microsoft\Outlook`. Sinon `-f/--file` ; les chemins WSL (`/mnt/c/...`, `/home/...`) sont acceptés.

## Commandes

```bash
outlook-cli info                         # format, taille, nombre de dossiers/messages, Inbox
outlook-cli folders                      # arborescence : id, chemin, nombre de messages, non lus
outlook-cli list -n 10                   # 10 derniers messages de l'Inbox
outlook-cli list --since 2h              # reçus depuis 2 heures
outlook-cli list --since "2026-10-15 08:00" --until 2026-10-16   # heure locale
outlook-cli list --since 2026-10-15T06:00:00Z --from martin --unread
outlook-cli list -F all --has-attachments --subject facture -n 5
outlook-cli list -F "Inbox/Projets" -F Archive --to durand
outlook-cli list -n 5 --body --body-max 2000     # avec le corps texte (tronqué)
outlook-cli list -n 5 --full                     # corps + destinataires + pièces jointes
outlook-cli get 2097444                          # message complet (id issu de list)
outlook-cli get "<ABC123@mail.example.com>"      # par Message-ID Internet
outlook-cli get 2097444 --html --headers         # + HTML brut + en-têtes de transport
outlook-cli attachments 2097444 -d ./pj          # enregistre les pièces jointes
outlook-cli new --state agent.state              # nouveaux messages depuis l'appel précédent
outlook-cli watch --interval 5                   # flux JSONL des messages au fil de leur arrivée
```

Filtres (communs à `list`, `new` et `watch`) :

| Option | Effet |
|---|---|
| `-F, --folder` | chemin (`Inbox/Projets`), nom, ou id ; répétable ; `all` = tous les dossiers de mails ; défaut : Inbox (détectée quelle que soit la langue) |
| `-s, --since` | reçu à partir de : `2026-10-15T08:00:00Z`, `2026-10-15T10:00+02:00`, `"2026-10-15 08:00"` (heure locale), `2026-10-15`, `90s`, `15m`, `2h`, `3d`, `1w`, `today`, `yesterday` |
| `--after` | reçu strictement après (pour repasser le `cursor` d'un appel précédent) |
| `-u, --until` | reçu strictement avant |
| `--from` | nom ou adresse de l'expéditeur contient (insensible à la casse) ; répétable = OU |
| `--to` | un destinataire (to/cc/bcc) contient ; répétable = OU |
| `--subject` | le sujet contient |
| `--unread` / `--read` | état de lecture |
| `--has-attachments` | avec pièces jointes |
| `--class` | préfixe de classe : `IPM.Note` (e-mails), `IPM.Schedule` (invitations)… |

`list` : `-n/--limit` (défaut 20, `0` = tout), `--oldest-first`.
Options globales : `--format json|jsonl|text`, `--pretty`, `--local-time` (heures locales au lieu d'UTC),
`--ascii` (voir PowerShell ci-dessous).

## Sortie

`list` (format `json`) :

```json
{"cursor":"2026-10-15T18:26:40.732853300Z","messages":[{
  "id":2097444, "folder":"Inbox",
  "received":"2026-10-15T18:26:40.732853300Z", "sent":"2026-10-15T18:26:38Z",
  "subject":"Réunion de projet jeudi",
  "from":{"name":"Jeanne Martin","email":"jeanne.martin@example.com"},
  "to":"Paul Durand; Marie Leroy", "cc":"Luc Bernard",
  "unread":true, "has_attachments":true, "importance":"normal", "class":"IPM.Note", "size":199079,
  "message_id":"<0F3A9C21-1B2D-4E5F-8A7B-123456789ABC@example.com>", "in_reply_to":"<1234ABCD@example.com>",
  "preview":"Bonjour à tous, veuillez trouver ci-joint l'ordre du jour…"
}],"count":1}
```

- `id` : identifiant interne du message dans le fichier (pour `get` et `attachments`). `message_id` : Message-ID
  Internet, stable, à utiliser pour dédupliquer.
- Les heures sont en UTC (RFC 3339, précision 100 ns, exactement réutilisables comme `--after`).
- Avec `--body` : `body` (texte brut ; issu du corps texte, du HTML converti ou du RTF décompressé) et
  `body_truncated` si `--body-max` a coupé. Avec `--recipients` : `recipients` `[{type,name,email}]`.
  Avec `--attachments` : `attachments` `[{index,name,size,mime_type,content_id,method,inline}]`.
- `jsonl` : un objet message par ligne, sans enveloppe. `text` : une ligne par message (`*` non lu, `@` pièce jointe).

Erreurs : un objet JSON sur stderr, `{"error":{"kind":"not_found","message":"..."}}`, et le code de retour :
`0` succès, `2` usage, `3` introuvable, `4` E/S, `5` fichier non géré, `6` données incohérentes.

## Surveillance par un agent

Trois approches, de la plus robuste à la plus simple :

1. **`new --state FICHIER`** (recommandée pour un agent qui interroge périodiquement) : renvoie les messages jamais vus
   lors des appels précédents, du plus ancien au plus récent ; l'état (quelques centaines d'octets) est mis à jour
   après écriture de la sortie. Un message est « nouveau » parce que son id n'a jamais été vu, pas à cause de sa date :
   un e-mail synchronisé en retard par Exchange n'est donc pas manqué. Le premier appel initialise l'état et ne
   renvoie rien (ou les `--initial N` plus récents).

   ```bash
   outlook-cli new --state ~/.agent/inbox.state --full --body-max 4000
   ```

2. **`watch`** : processus permanent qui écrit chaque nouveau message en JSONL sur stdout dès son arrivée
   (sondage toutes les `--interval` secondes, quasi gratuit tant que le fichier ne change pas : ~0,005 s de CPU par
   seconde). Avec `--state`, reprise sans perte après redémarrage. `--max N` pour s'arrêter après N messages.

3. **Curseur temporel** : `list --after <cursor> --oldest-first` avec le `cursor` de la réponse précédente.
   Sans état côté CLI, mais un message dont la date de réception est antérieure au curseur (synchronisation tardive)
   serait ignoré : prévoir un recouvrement (`--since`) et dédupliquer sur `message_id`.

Les filtres `--from`, `--subject`, etc. s'appliquent aussi à `new` et `watch` ; un message écarté par un filtre
est tout de même marqué comme vu.

## PowerShell

Windows PowerShell 5.1 décode la sortie des programmes natifs avec la page de code de la console (pas en UTF-8),
ce qui corrompt les accents. Utiliser `--ascii` (ou `$env:OUTLOOK_CLI_ASCII = "1"`), qui échappe les caractères non
ASCII en `\uXXXX` : le JSON reste valide et `ConvertFrom-Json` restitue le texte exact.

```powershell
$env:OUTLOOK_CLI_ASCII = "1"
$r = outlook-cli list -n 10 --unread | ConvertFrom-Json
$r.messages | Select-Object id, received, subject
```

(Alternative : `[Console]::OutputEncoding = [Text.Encoding]::UTF8`.) Inutile dans WSL ou PowerShell 7.

## outlook-index (preview)

Separate binary for a local mailbox index (cargo feature `index`, bundled SQLite with FTS5). Read-only on the OST.
Built by `scripts/build.sh` as `dist/outlook-index.exe`. Commands: `doctor`, `sync`, `search`, `status`, `scope`,
`embed`, `model`, `bench-embed`.

```powershell
outlook-index.exe doctor --pretty                # samples 200 messages
outlook-index.exe doctor --sample 1000 --pretty  # more precise, slower
```

`doctor` prints aggregate JSON only: machine (OS, arch, CPUs, total RAM bytes), SQLite/FTS5, index location status,
file kind/size, folder and message counts by class prefix (`IPM.Note`, `IPF.Note`, unknown segments → `other`)/depth/
year, full-scan time, and for `--sample` messages picked at random: open+text, open and text-extraction times (µs),
text lengths, a chunk estimate, Message-ID coverage of the scanned rows (`messages.with_message_id` /
`without_message_id`), attachment counts by extension/method and sizes, and `well_known`: for the Inbox, Deleted
Items and Junk E-mail, `found`, `method` (`entry_ids`, `entry_id_nid`, `entry_id_source_key`, `name`, `not_found`)
and the message count; Deleted Items and Junk add `entry_id` facts on the entry-id path (`present`, `length`,
`tail_nid_exists`, `tail_nid_is_folder`, `folders_with_source_key`, `source_key_match`, `source_key_in_entry_id`;
booleans and counts only). Distributions are
`{min, p50, p95, max, sum}`. Errors: counts by kind only; a fatal error prints `{"error":{"kind","message"}}` with a
fixed message. No subjects, addresses, names, folder or attachment names, body text or paths.

**Paste back**: the whole stdout JSON, plus any stderr line (`{"warning":...}` or `{"error":...}`). Read it first.

### Attachment text (diagnostic)

Used by `sync` (see *Attachments in the index*) and by this diagnostic. Supported: PDF (text layer, pages separated by a blank line; no OCR), DOCX (paragraphs), PPTX (slides in number
order), XLSX (sheet name line, one line per row, cells tab-separated; at most 20,000 rows per sheet and 200,000
cells), TXT and CSV (UTF-8, UTF-16 with BOM, else Windows-1252), HTML. The type comes from the extension, then the
magic bytes, then the MIME type. Text is whitespace-normalised and capped at 2,000,000 characters (`truncated`).
Skip reasons: `image`, `inline`, `too_large` (> 20 MB), `unsupported_type` (including archives and old `.doc`/`.xls`),
`embedded_message`, `encrypted`, `empty_text` (e.g. a scanned PDF), `failed` (malformed file, parser panic or worker
crash), `timeout`. Each file is parsed in a child process (`outlook-index extract-worker`, internal), killed after
30 s and restarted for the next file.

```powershell
outlook-index.exe doctor --attachments --pretty               # 300 attachments of random messages
outlook-index.exe doctor --attachments --sample 1000 --pretty # more precise, slower
```

It opens random messages with attachments (whole messages) until `--sample` attachments are seen and prints
aggregates only: `by_kind` (`pdf`, `docx`, ..., `image`, `other`) × outcome (`ok` or a skip reason), `by_outcome`,
`extracted` (distinct contents that gave text: chars and extraction time `{min, p50, p95, max, sum}` in µs, count
truncated), `chunks` per kind (same chunking as bodies, sample and mailbox estimate), `dedupe` (`hashed` attachments,
`distinct` SHA-256, `distinct_ratio`; the same content is extracted once), `limits`, `workers_started` (more than 1 =
restarts after a timeout or crash), and error counts by kind. No file names, text or subjects.

**Paste back**: the whole stdout JSON and any stderr line.

Index file: `--index` / `OUTLOOK_INDEX_DB`, default `%LOCALAPPDATA%\outlook-cli\index.sqlite` (Linux:
`$XDG_DATA_HOME/outlook-cli/index.sqlite`). A path under OneDrive gets a warning. `doctor` does not create it.

```powershell
outlook-index.exe sync                    # mail folders but Deleted Items and Junk; counts only on stdout
outlook-index.exe sync --include-folders deleted,junk   # index them too (stored; `none` = the default)
outlook-index.exe sync --max-time 30m     # stop after 30 min; the next sync resumes
outlook-index.exe status --pretty         # counts and size only
outlook-index.exe search "budget café" -n 5 -F Inbox --since 30d --from alice
outlook-index.exe search "budget" --format jsonl   # one hit per line
```

`sync` is a full reconciliation against the folders' contents tables: new, deleted, moved/copied messages and
read/unread changes. Only new messages are opened. A message is identified by its Internet Message-ID, else by a
fingerprint (sender, submit time, subject, size); the same message in two folders is indexed once. Nothing is done
when the file has not changed since the last complete sync. New messages are committed in batches, so `Ctrl-C` or
`--max-time` lose nothing. A sync that would delete more than max(100, 20%) of the indexed messages aborts with no
change (usage error, counts only) unless `--allow-mass-delete` is passed. One sync at a time per index (lock file
`index.sqlite.lock`). Quoted replies, forward headers and signature/disclaimer tails (French and English) are
stripped; the rest is cut into ~1750-character chunks with ~200 characters of overlap; each chunk keeps its character
range in the stripped body.

Deleted Items and Junk E-mail (with their subfolders) are not indexed by default. They are found by the store's
well-known entry ids (Deleted Items: wastebasket entry id; Junk: additional REN entry ids, by NID or source key),
else by their default localized name at the top level. `--include-folders deleted,junk` (or `deleted`, `junk`,
`none`) changes the choice; it is stored in the index and reused by later syncs (`status.include`). A changed choice
forces a full reconciliation. Messages leaving the index because of the exclusion are reported as
`excluded_removed`, not `deleted`, and do not count for the mass-deletion limit.

### Attachments in the index

`sync` then reads the attachments of new messages (and, once, of messages indexed before attachments existed: a
backfill, newest first, in batches, resumable with `--max-time`). Inline attachments, images, files over the max
size, embedded messages, links and unknown extensions are skipped from their properties, without reading the data;
the rest is read, hashed (SHA-256) and, for a content not seen before, extracted in the child process (30 s
timeout, restarted after a failure). A content is stored and chunked once, shared by every message carrying it, and
deleted with its last message. A `failed` or `timeout` extraction is retried by the next syncs, 3 attempts in
all. FTS covers the whole extracted text; only the first chunks of each content are embedded (see *Embeddings:
scope and runs*). `--no-attachments` skips the step.
Summary: `attachments_extracted`, `attachments_deduplicated`, `attachments_failed`,
`attachments_skipped_by_reason` (`inline`, `image`, `too_large`, `unsupported_type`, `excluded_type`,
`embedded_message`, `encrypted`, `empty_text`), `attachments_errors`, `attachments_remaining` (messages).

```powershell
outlook-index.exe scope --attachment-types pdf,docx,xlsx,pptx,txt,csv,html --attachment-max-mb 20   # the defaults
outlook-index.exe sync --no-attachments                            # bodies only; a later sync catches up
```

The attachment scope is stored in the index. Narrowing it keeps stored texts (`attachments.out_of_scope`);
attachments skipped as `too_large` or `excluded_type` are read again by the next sync after a change
(`requeued_messages`). `status` and `scope` print `attachments`: links, `contents` (hashed), `distinct`, `chunks`,
`by_outcome`, `skipped_by_reason`, `pending_messages`, `scope`.

`search` modes (`--mode`):

- `keyword` matches every word, accent- and case-insensitive (FTS5 `unicode61 remove_diacritics 2`), over subject
  and body, or within one attachment chunk. Query operators and quotes are plain words.
- `semantic` embeds the query locally (`query: ` prefix, model of the stored vectors) and ranks the embedded chunks
  by cosine similarity (brute force over the vectors, streamed from SQLite; no ANN index). It finds other wordings
  and the other language (a French query finds an English mail). Body and attachment vectors are scanned; a
  message ranks by its best chunk, from the body or an attachment it carries. Only embedded chunks (the embedding
  scope, the attachment chunk cap) are searched.
- `hybrid` fuses the top 200 of an any-word keyword search (every query word, OR) and the top 200 semantic hits
  by Reciprocal Rank Fusion (k = 60); each leg stays at 200 whatever `-n`, so at most their union is returned.
  Messages outside the embedding scope are still found by their words.
- Default: `hybrid` when the index holds vectors of its model, else `keyword`. `semantic`/`hybrid` with no such
  vectors run `keyword` and print `{"warning":{"kind":"keyword_fallback",...}}` on stderr; so does the default mode
  when the model files are missing (an explicit `--mode semantic|hybrid` is then a `not_found` error). `--model`
  other than the model of the stored vectors is a usage error naming the expected model.

```powershell
outlook-index.exe search "budget café" -n 5 -F Inbox --since 30d --from alice   # hybrid once embedded
outlook-index.exe search "facture impayée" --mode keyword                         # every word, exact
outlook-index.exe search "unpaid invoice" --mode semantic                         # finds "Facture impayée"
outlook-index.exe search "server outage" --mode hybrid --format jsonl
```

Filters apply to every mode: `-F` folder name, or path with its subfolders (`Inbox` matches `Inbox/Projects`, not
`Inboxes`; repeatable), `--since`/`--until`, `--from`, `--to`, `--has-attachments`. Output: `{count, mode, hits}`
(`--format jsonl`: one hit per line). One hit per message (its best chunk): `{id, message_id, folder, received,
subject, from, excerpt, source, ranks, scores}`; `source` is `"body"` or `{"kind": "attachment", "index", "name",
"type"}` with the excerpt from the attachment; `ranks`/`scores` hold `keyword` (bm25), `semantic` (cosine) and,
in hybrid, `hybrid` and `rrf`, for the legs that found the message. Hybrid hits also carry `keyword_rank` and
`semantic_rank` (`null` when that leg missed the message) and `score` (the RRF score). `id` works with `outlook-cli get` until the
message moves (then run `sync`). Excerpts: 300 characters by default, `--excerpt-max` up to 2000; a keyword excerpt
is a snippet around the words, a semantic one the start of the best chunk (without the overlap with the previous
chunk).

Latency (VPS, 4 vCPU, 3 threads, release build): a semantic or hybrid search loads the model first, which takes
~1.4 s (SHA-256 check of the files included); the query embedding then takes ~30 ms. The vector scan is ~80 ms per
50,000 vectors and ~320 ms per 200,000 (e5-small, warm page cache, linear in the number of vectors); a whole hybrid
search ~0.17 s at 50,000 and ~0.7 s at 200,000 (worst case: every chunk matches a word). Measured with the ignored
tests `semantic_bench` (synthetic index) and `real_e5_search` (30 fictitious French/English mails, cross-lingual
hits in the top 3):
`E5_SMALL_DIR=... cargo test --release --features index --lib -- --ignored --nocapture semantic_bench real_e5_search`.

### Embedding model (local, CPU)

`intfloat/multilingual-e5-base` (default for a new index, 768 dims, XLM-RoBERTa) or `multilingual-e5-small` (384
dims, BERT), run on
the CPU with candle (pure Rust). The model is not in the exe. Its files (`config.json`, `tokenizer.json`,
`model.safetensors`) are pinned to a Hugging Face revision and checked by SHA-256 before every load.

```powershell
outlook-index.exe model list --pretty                   # pinned revisions, files present in the default dirs
outlook-index.exe model fetch                           # model of the index, else e5-base (~1.1 GB); the only network access
outlook-index.exe model fetch --model e5-small          # ~490 MB
outlook-index.exe bench-embed --pretty                  # ~30 s of synthetic passages on this CPU
outlook-index.exe bench-embed --model e5-small --seconds 60 --workers 4 --threads 3 --pretty
```

`model fetch` downloads over HTTPS from huggingface.co with `curl` (shipped with Windows 10+) into `<name>.part`,
resumes an interrupted download and installs a file only when its SHA-256 matches. Manual download: put the three
files of the pinned revision (`model list`) in a directory and pass `--model-dir` (or `OUTLOOK_INDEX_MODEL_DIR`).
Default directory: `%LOCALAPPDATA%\outlook-cli\models\multilingual-e5-base` (or `-small`; Linux: under
`$XDG_DATA_HOME/outlook-cli`). Texts get the mandatory e5 prefix (`query: ` / `passage: `), are cut at 512 tokens,
embedded in length-sorted batches (`--batch`, default 8) and mean-pooled over the tokens, then L2-normalized.
`bench-embed` prints load time (hash check included), query latency and passages/s on synthetic text only (with
`--workers` above 1: aggregate passages/s, no token or query figures). **Paste back** its whole JSON for both models.

**Workers and threads.** `--workers N` (on `embed`, `sync` and `bench-embed`) embeds N batches at once; `--threads`
is the compute threads *per worker*. Defaults: workers = max(1, (logical CPUs − 2) / 3), threads = 3, so 14 CPUs
give 4 × 3 and 4 CPUs 1 × 3; two CPUs stay free. Summaries print the effective `workers` and `threads`. One worker
runs in the process. More workers run as child processes (`outlook-index embed-worker`, internal), each with its own
copy of the model: memory grows with N (VPS, e5-small: ~1.25 GB RSS per process). One process with more threads
scales badly; several small ones scale well. Owner's laptop (Intel i5, 14 logical CPUs, e5-small, ~311
tokens/passage): one process with 2 / 4 / 6 / 8 / 13 threads gave 2.39 / 2.96 / 3.71 / 3.81 / 4.48 passages/s; 4
processes × 3 threads 8.19 in total, 6 × 2 7.42; batch size 1/8/16 made no difference. On the VPS (4 vCPU) the same
model in one process with 2 workers sharing the weights reached 3.5–3.6 passages/s against 4.5 for two processes ×
2 threads, hence processes.

### Embeddings: scope and runs

Search covers every indexed mail folder; embeddings are limited by a scope (default: the whole index) stored in
the index and reused by every run.

```powershell
outlook-index.exe scope --pretty                                   # scope + counts
outlook-index.exe scope --embed-since 2025-01-01 --embed-until "2026-03-31" -F Inbox -F "Archive/2026"
outlook-index.exe scope --attachment-chunks 10                     # attachment chunks embedded per content (default 30)
outlook-index.exe scope --clear                                    # back to everything
outlook-index.exe embed --max-time 30m                             # newest first; resumable
outlook-index.exe embed --model e5-small --reembed                 # change model: drops the e5-base vectors
outlook-index.exe sync --max-time 1h                               # index, then embed in the time left
outlook-index.exe sync --no-embed                                  # index only
```

`scope`: `--embed-since` (inclusive) and `--embed-until` (exclusive) on the received time, same syntax as
`outlook-cli --since/--until`, stored as absolute times (the typed text is shown too: `90d` stays the date it meant
when set); `-F` as in `search -F`, repeatable, replaces the stored folders; options not given are kept. Output:
`chunks_in_scope`, `embedded`, `pending`, `embedded_pct`, `out_of_scope`, and `out_of_scope_embedded` (vectors are
never deleted by a scope change). Folder specs are counted, not printed. A folder spec is checked against the
indexed folders once a sync has run.

`--attachment-chunks N` (default 30, about 50,000 characters; `0` = attachments not embedded) is stored with the
scope: only chunks 0 to N-1 of each distinct attachment content are embedded, keyword search covers the whole
text. Raising it embeds the newly eligible chunks on the next run; lowering it keeps the vectors already stored. A
content is in scope when at least one in-scope message (dates, folders) carries it and its type and size are in
the attachment scope; it is embedded once, whatever the number of messages carrying it. Output
`attachment_chunks` (in `status`: `embedding.attachment_chunks`): `cap`, `vectors`, `chunks`, `in_scope`,
`eligible` (in scope, under the cap), `embedded`, `pending`, `beyond_cap` (keyword only), `beyond_cap_embedded`
(kept from a higher cap), `out_of_scope_embedded`. The other counts above are body chunks.

`embed` embeds the pending in-scope body chunks, newest message first (passage: the subject, a newline and the
chunk text), then the pending attachment chunks under the cap, contents ordered by the newest message carrying
them (passage: the file name, a newline and the chunk text); both get the `passage: ` prefix. Attachment vectors
have their own table, follow the same model rules (`--reembed` drops both kinds) and go with their content. Each batch (`--batch`, default 8) is one commit made by the parent process (the
only DB writer) as soon as a worker returns it, so with several workers commits may land slightly out of
newest-first order; `Ctrl-C` loses at most the batches in flight (one per worker) and `--max-time` starts no new batch
once reached (batches in flight finish); the next run resumes. A failing worker stores nothing of its batch: the
run stops dispatching, commits the batches still in flight and exits with the error. It runs at below-normal
priority (Windows `BELOW_NORMAL_PRIORITY_CLASS`, Linux nice 10, inherited by worker processes; `--priority normal`
to opt out) with `--workers` × `--threads`. The model defaults to the one of the index (else e5-base; also for `model fetch` and `bench-embed`); another model is refused unless
`--reembed`, which drops the old vectors. Missing model files are a `not_found` error. Output: `embedded`, `pending`
(body and attachment chunks; `attachment_chunks: {embedded, pending}` for the attachment part), `dropped`,
`complete`, `interrupted`, `passages_per_s`, `load_ms`, `elapsed_ms`, model, workers, threads, priority.

`sync` runs the same step after indexing, within what is left of `--max-time`, with the same options. Without model
files indexing still completes and the summary says `"embedding": {"skipped": "model files missing; run outlook-index
model fetch"}`. `status` adds `embedding`: model, revision, dims, vectors, the scope counts above and a scope
summary.

Time estimate: chunks to embed (`pending` + `attachment_chunks.pending` from `scope`, i.e. body chunks plus
min(chunks, cap) per distinct attachment content) ÷ passages/s (from `bench-embed` with the same `--workers`
and `--threads`) = seconds; e.g. 40,000 chunks at 8.19 passages/s (laptop above, 4 × 3, e5-small) ≈ 81 min. With
attachments, the owner's mailbox adds at most ~10,000 chunks (doctor estimate; the 2 M-character TXT counts 30, not
~190,000): ~50,000 chunks ≈ 1 h 42 min at that rate (e5-base is slower: use its `bench-embed` rate). Size: about
2 MB per 1,000 e5-small vectors (measured: 384 × 4 bytes plus row and page overhead).

### Progress of long runs

`sync` and `embed` report progress on stderr; stdout still gets only the final JSON summary.
`--progress auto|bar|jsonl|none` (default `auto`: `bar` when stderr is a terminal, else `jsonl`, e.g. a pipe, an
agent or a log file). Reports at each phase change, at most every `--progress-interval` seconds (default 10) in
between, and a final `done`. Phases in order: `scan` (folders), `index` (new messages), `attachments` (messages
with attachments to read), `embed_bodies` (pending body chunks in scope), `embed_attachments` (pending attachment
chunks under the cap); totals are taken at phase start.

```text
{"progress":{"phase":"embed_bodies","done":12345,"total":30112,"percent":40.9,"rate":3.9,"eta_s":4556,"elapsed_s":5210,"workers":4,"model":"e5-base"}}
embed_bodies       41 % 12,345/30,112  3.9/s  ETA 1h15m
```

`jsonl`: one whole line per event, so lines stay parseable among `{"warning":...}` lines. `rate` is items/s over
the last minute of the phase, `eta_s` derives from it (`null` until known), `elapsed_s` counts from the start of the
run; `workers` and `model` are `null` without embedding. `bar`: one self-overwriting line per phase, plain ASCII
(Windows Terminal, PowerShell, WSL). Counts and durations only: no names, subjects or text.

The same record (plus `pid`, `started_at`, `updated_at`, `interval_s`) is stored in the index (meta `sync_progress`)
inside the run's batch transactions, so another process can read it:

```powershell
outlook-index.exe status            # "running": null, or {phase, done, total, percent, rate, eta_s, elapsed_s, pid, updated_at, stale}
outlook-index.exe status --watch    # {"running": ...} every 10 s (--watch 30: every 30 s) until the run ends
```

`status` reads while a sync runs (WAL, no wait). `running` is `null` when no run was recorded or the last one
finished; `stale: true` means the run's process is gone (no process holds the index lock) and the record is older
than twice its interval: a crashed or killed run, resumed by the next `sync`. `status --watch` prints one line per
tick (jsonl, even with `--pretty`) and stops after a line whose record is `null`, `done` or stale. **Agents**: read
`running` from `status` (or poll `status --watch`), or parse the `{"progress":...}` lines of the run's stderr.

Parity with sentence-transformers (`scripts/e5_reference.py` → `tests/fixtures/e5_reference.json`):
`E5_SMALL_DIR=... E5_BASE_DIR=... cargo test --release --features index --test embed_parity -- --ignored --nocapture`.

## Fonctionnement

- `src/ndb.rs` : couche NDB de [MS-PST] — en-tête, B-trees des nœuds (NBT) et des blocs (BBT), blocs de données
  (décompression zlib du format 4K, décodage PST), arbres de données et de sous-nœuds.
- `src/ltp.rs` : couche LTP — heap-on-node (blocs chargés à la demande), BTree-on-heap, property contexts (PC) et
  table contexts (TC).
- `src/store.rs` : couche messagerie — dossiers, messages, destinataires, pièces jointes.
- `src/body.rs` : corps — HTML → texte, décompression RTF (LZFu), désencapsulation du HTML contenu dans le RTF.
- `src/main.rs` : CLI.

Une requête `list` lit la table des contenus du dossier (une ligne par message : date, drapeaux, sujet…), filtre et
trie sur ces colonnes, puis n'ouvre que les messages retenus. Outlook écrit dans le fichier pendant la lecture : toute
incohérence détectée (trailers de pages et de blocs vérifiés, bornes contrôlées partout) déclenche une nouvelle
lecture sur une projection fraîche du fichier.

Particularités du format OST 2013+ (wVer 36), non documentées dans [MS-PST] et établies sur fichiers réels :
pages de 4096 octets (méta à 0xFD8, trailer à 0xFE8), blocs alignés sur 512 octets avec trailer de 24 octets,
entrées BBT avec taille décompressée, blocs jusqu'à 64 Ko compressés en zlib, et HID avec index d'allocation sur
14 bits (bloc à partir du bit 19, au lieu de 11/16).
