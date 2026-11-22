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
Built by `scripts/build.sh` as `dist/outlook-index.exe`. Commands: `doctor`, `sync`, `search`, `status`.

```powershell
outlook-index.exe doctor --pretty                # samples 200 messages
outlook-index.exe doctor --sample 1000 --pretty  # more precise, slower
```

`doctor` prints aggregate JSON only: machine (OS, arch, CPUs, total RAM bytes), SQLite/FTS5, index location status,
file kind/size, folder and message counts by class prefix (`IPM.Note`, `IPF.Note`, unknown segments → `other`)/depth/
year, full-scan time, and for `--sample` messages picked at random: open+text, open and text-extraction times (µs),
text lengths, a chunk estimate, Message-ID coverage of the scanned rows (`messages.with_message_id` /
`without_message_id`), attachment counts by extension/method and sizes. Distributions are
`{min, p50, p95, max, sum}`. Errors: counts by kind only; a fatal error prints `{"error":{"kind","message"}}` with a
fixed message. No subjects, addresses, names, folder or attachment names, body text or paths.

**Paste back**: the whole stdout JSON, plus any stderr line (`{"warning":...}` or `{"error":...}`). Read it first.

Index file: `--index` / `OUTLOOK_INDEX_DB`, default `%LOCALAPPDATA%\outlook-cli\index.sqlite` (Linux:
`$XDG_DATA_HOME/outlook-cli/index.sqlite`). A path under OneDrive gets a warning. `doctor` does not create it.

```powershell
outlook-index.exe sync                    # every mail folder; counts only on stdout
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

`search` (keyword only for now; `--mode semantic|hybrid` is a usage error until embeddings land) matches every word,
accent- and case-insensitive (FTS5 `unicode61 remove_diacritics 2`), over subject and body. Query operators and
quotes are plain words. Filters: `-F` folder name, or path with its subfolders (`Inbox` matches `Inbox/Projects`,
not `Inboxes`; repeatable), `--since`/`--until`, `--from`, `--to`. One hit per message (its best chunk): `{id,
message_id, folder, received, subject, from, excerpt, source, ranks, scores}`; `id` works with `outlook-cli get` until
the message moves (then run `sync`). Excerpts: 300 characters by default, `--excerpt-max` up to 2000.

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
