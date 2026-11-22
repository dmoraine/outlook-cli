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
scripts/build.sh             # tests + dist/outlook-cli.exe, dist/outlook-index.exe (Windows) et dist/outlook-cli (Linux)
scripts/install.sh           # %LOCALAPPDATA%\Programs\outlook-cli\ (les deux .exe) + liens ~/.local/bin/outlook-cli, outlook-index
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

Binaire séparé qui construit un index local de la boîte mail (feature cargo `index`) : métadonnées, index plein
texte FTS5 et vecteurs dans un seul fichier SQLite, embeddings calculés localement sur le CPU. Lecture seule sur
l'OST, aucun accès réseau hors `model fetch`. Construit par `scripts/build.sh` (`dist/outlook-index.exe`), installé
par `scripts/install.sh` à côté de `outlook-cli.exe` (lien `~/.local/bin/outlook-index`). Procédure de test complète
pour le poste Windows : [docs/WINDOWS_TESTING.md](docs/WINDOWS_TESTING.md).

```bash
outlook-index doctor --pretty                    # diagnostic agrégé (aucun contenu)
outlook-index model fetch                        # modèle e5-base (~1,1 Go), seul accès réseau
outlook-index sync                               # indexe, extrait les pièces jointes, puis calcule les embeddings
outlook-index status --pretty                    # compteurs, taille, run en cours
outlook-index search "facture impayée" -n 5      # hybride dès que des vecteurs existent
```

Commandes : `doctor`, `sync`, `embed`, `search`, `status`, `scope`, `backup`, `model list|fetch`, `bench-embed`. Options
globales comme `outlook-cli` : `-f/--file`, `--pretty`, `--local-time`, `--ascii`, plus `--index`. Mêmes
conventions : JSON sur stdout, erreurs `{"error":{"kind","message"}}` sur stderr, mêmes codes de retour.

Erreurs sans contenu : le `message` d'une erreur est un texte fixe par type, complété au plus de nombres, de noms
d'options, de valeurs autorisées, de noms de modèles ou d'un emplacement dans le code (un plantage donne
`inconsistent`, `internal error at <fichier>.rs:<ligne>`). Jamais de chemin (fichier de la boîte, index, modèles),
de nom de dossier ou de pièce jointe, de texte de requête ou de filtre, ni de texte venant de la boîte, de `curl` ou
d'un processus fils : une ligne `{"error":...}` peut être recopiée telle quelle. Les erreurs de syntaxe des options
sont aussi en JSON (`usage`, code 2) et ne reprennent pas la valeur saisie. Diagnostic local uniquement :
`OUTLOOK_INDEX_DEBUG=1` ajoute à l'erreur un champ `debug` (stderr) avec le texte brut, qui peut contenir des
chemins, des noms et du contenu de mails ; ne jamais le transmettre.

Fichier d'index : `--index` / `OUTLOOK_INDEX_DB`, par défaut `%LOCALAPPDATA%\outlook-cli\index.sqlite` (Linux :
`$XDG_DATA_HOME/outlook-cli/index.sqlite`). Un chemin sous OneDrive provoque un avertissement
`{"warning":{"kind":"onedrive",...}}` sur stderr. L'index se reconstruit toujours depuis l'OST : le supprimer
suffit à revenir en arrière.

Permissions : l'index contient une copie en clair des mails. Sous Unix, un dossier d'index créé par
`outlook-index` est en 0700 et la base (avec `index.sqlite-wal`, `index.sqlite-shm`, `index.sqlite.lock` et les
sauvegardes) en 0600, quel que soit l'umask. Un dossier ou un fichier existant n'est jamais modifié (ni le dossier
parent) : s'il est accessible au groupe ou aux autres, `status` et `doctor` (section `index`) affichent
`permissions_warning` (texte fixe, sans chemin ; `null` sinon) ; corriger avec `chmod 700` sur le dossier et
`chmod 600` sur les fichiers. Sous Windows, `%LOCALAPPDATA%` est propre à l'utilisateur ; un `--index` ailleurs
(dossier partagé, autre disque) relève des ACL choisies par son propriétaire.

Lecture et écriture de l'index :

- **Écrivains** : `sync`, `embed` et `scope` avec une option. Un seul à la fois par index (verrou
  `index.sqlite.lock`) : un écrivain attend jusqu'à 3 s que le verrou se libère (un `status` le sonde un
  instant), puis échoue (`usage`, « another sync is running »). Il prend le verrou avant toute mise à jour du
  schéma, qu'il applique dans une seule transaction.
- **Lecteurs** : `search`, `status`, `scope` sans option, `backup`, `model fetch` et `bench-embed` ouvrent l'index en
  lecture seule (jamais créé, migré ni modifié) et peuvent tourner pendant un sync ; `doctor` et `model list`
  ne l'ouvrent pas. `search` lit un instantané cohérent de l'index (modèle chargé avant).
- **Index d'une version plus ancienne** : `status` fonctionne et indique `schema` :
  `{"version": 4, "expected": 6, "upgrade_pending": true}` (compteurs dont les tables manquent à `null`) ;
  `search` et `scope` sans option échouent avec `upgrade_pending` (code 3) : lancer `sync` (ou un autre
  écrivain), qui met le schéma à jour. Un index d'une version plus récente est refusé (`format`, code 5).

### backup

```bash
outlook-index backup                             # backups/index-AAAAMMJJ-HHMMSS.sqlite à côté de l'index
outlook-index backup --to D:\sauvegardes\index-avant-test.sqlite
```

Copie cohérente de l'index (SQLite `VACUUM INTO`, un seul instantané, WAL compris) dans un **nouveau** fichier :
une destination existante est refusée (`usage`, code 2), jamais écrasée. Sans danger pendant un `sync` (l'écrivain
continue, la copie reflète un état validé). La copie est ensuite rouverte en lecture seule et vérifiée. Sortie :
`destination`, `bytes`, `schema_version`, `counts` (`messages`, `chunks`, `vectors`, `attachment_chunks`,
`attachment_vectors` ; `null` si la table manque) et `integrity` (`ok` ; sinon erreur `inconsistent` et la copie
est supprimée). La sauvegarde est un fichier unique, sans `-wal` ni `-shm`.

Restauration, **tous les utilisateurs de l'index arrêtés** (aucun `sync`, `embed`, `search`, `status --watch`) :
supprimer `index.sqlite-wal` et `index.sqlite-shm` s'ils existent, puis copier la sauvegarde vérifiée sur
`index.sqlite`. Ne jamais copier `index.sqlite` (ni ses fichiers `-wal`/`-shm`) à la main pendant qu'il est ouvert :
un WAL obsolète peut être rejoué et rendre silencieusement un état ancien.

### doctor

```bash
outlook-index doctor --pretty                    # 200 messages tirés au hasard
outlook-index doctor --sample 1000 --pretty      # plus précis, plus lent
outlook-index doctor --attachments --pretty      # extraction de 300 pièces jointes (--sample N)
```

`doctor` n'affiche que des agrégats : machine (OS, architecture, CPU, RAM), SQLite/FTS5, état de l'index
(`exists`, `bytes`, `under_onedrive`, `permissions_warning`), type et taille du fichier, nombre de dossiers et de messages par préfixe de
classe, profondeur et année, durée du scan complet, et pour l'échantillon : temps d'ouverture et d'extraction du
texte (µs), longueurs de texte, estimation du nombre de chunks, couverture Message-ID, pièces jointes par
extension et méthode. `well_known` donne pour l'Inbox, les Éléments supprimés et le Courrier indésirable `found`,
`method` (`entry_ids`, `entry_id_nid`, `entry_id_source_key`, `name`, `not_found`) et le nombre de messages, plus
des indicateurs booléens/numériques sur leurs entry ids. Distributions : `{min, p50, p95, max, sum}`. Erreurs :
comptées par type ; une erreur fatale a un message fixe. Ni sujet, ni adresse, ni nom, ni nom de dossier ou de
fichier, ni texte, ni chemin : la sortie complète peut être recopiée telle quelle.

`doctor --attachments` ouvre des messages au hasard ayant des pièces jointes et extrait chaque pièce dans un
processus fils : `by_kind` (`pdf`, `docx`, …, `image`, `other`) × issue (`ok` ou motif d'exclusion), `by_outcome`,
`extracted` (caractères et temps d'extraction), `chunks` par type (échantillon et estimation pour la boîte),
`dedupe` (`hashed`, `distinct`, `distinct_ratio` par SHA-256), `limits`, `workers_started`.

### sync

```bash
outlook-index sync                                    # tous les dossiers de mails sauf Éléments supprimés et Courrier indésirable
outlook-index sync --include-folders deleted,junk     # les inclure (choix mémorisé ; none = défaut)
outlook-index sync --max-time 30m                     # s'arrête après 30 min, embeddings compris ; le suivant reprend
outlook-index sync --no-embed                         # indexation seule
outlook-index sync --no-attachments                   # sans les pièces jointes ; un sync ultérieur rattrape
```

`sync` réconcilie entièrement l'index avec les tables de contenus des dossiers : messages nouveaux, modifiés,
supprimés, déplacés/copiés, changements lu/non lu. Seuls les messages nouveaux ou modifiés sont ouverts. Un message est
identifié par son Message-ID Internet, sinon par une empreinte (expéditeur, date d'envoi, sujet, taille) ; le même
message dans deux dossiers est indexé une fois. Rien n'est fait si le fichier n'a pas changé depuis le dernier sync
complet. Les messages sont validés par lots : `Ctrl-C` ou `--max-time` ne perdent que le lot en cours, le sync
suivant reprend. Un sync qui supprimerait plus de max(100, 20 %) des messages indexés s'arrête sans rien changer
(erreur `usage`), sauf avec `--allow-mass-delete`.

Messages modifiés : chaque message garde une signature lue dans la table de contenus, sans l'ouvrir (date de
dernière modification, taille, indicateur de pièces jointes, sujet). Si elle change (texte modifié, pièce jointe
ajoutée, retirée ou remplacée), le message est relu (compteur `refreshed`) : métadonnées, chunks et index plein texte
remplacés dans la transaction de son lot ; un chunk dont le texte (et le sujet) n'a pas changé garde son vecteur,
seuls les chunks nouveaux ou modifiés sont à embedder. Ses pièces jointes sont relues : liens mis à jour, contenus
qui ne sont plus référencés supprimés, contenus stockés mais hors portée conservés. Lu/non lu, déplacement et copie
ne changent pas la signature : rien n'est rouvert. Un message dont une copie a encore la signature stockée n'est pas
relu. Index créé avant cette version : le premier sync adopte la signature actuelle sans rouvrir les messages ; une
modification faite avant la mise à jour n'est donc pas reprise (une suivante l'est). Si Outlook change la date de
modification sans toucher au texte (indicateur de suivi, catégorie), le message est relu, sans nouvel embedding.

Avant découpage, les citations (en-têtes « De : / From: », « Le … a écrit : », « On … wrote: », lignes `>`) et les
signatures/mentions légales courantes (français et anglais) sont retirées ; le reste est découpé en chunks
d'environ 1 750 caractères avec ~200 caractères de recouvrement.

Les Éléments supprimés et le Courrier indésirable (avec leurs sous-dossiers) sont exclus par défaut. Ils sont
reconnus par les entry ids bien connus du store, sinon par leur nom localisé au premier niveau.
`--include-folders deleted,junk` (ou `deleted`, `junk`, `none`) est mémorisé dans l'index (`status.include`). Les
messages qui sortent de l'index à cause de l'exclusion sont comptés dans `excluded_removed`, pas dans `deleted`,
et n'entrent pas dans la limite de suppression massive.

Pièces jointes : `sync` lit ensuite celles des messages nouveaux ou modifiés (et, une fois, celles des messages indexés
avant, du plus récent au plus ancien, par lots). Pièces inline, images, fichiers trop gros, messages joints, liens
et extensions inconnues sont écartés d'après leurs propriétés, sans lire les données. Le reste est lu, haché
(SHA-256) et, pour un contenu inédit, extrait dans un processus fils (`outlook-index extract-worker`, interne ;
30 s maximum, relancé après un échec). Un contenu est stocké et découpé une fois, partagé par tous les messages qui
le portent, supprimé avec le dernier. Une extraction `failed` ou `timeout` est retentée par les syncs suivants
(3 essais en tout).

Formats extraits : PDF (couche texte, pas d'OCR), DOCX, PPTX, XLSX (au plus 20 000 lignes par feuille et 200 000
cellules), TXT et CSV (UTF-8, UTF-16 avec BOM, sinon Windows-1252), HTML. Le type vient de l'extension, puis des
octets magiques, puis du type MIME. Texte limité à 2 000 000 caractères. Motifs d'exclusion : `inline`, `image`,
`too_large`, `unsupported_type` (archives, `.doc`/`.xls` anciens, Markdown…), `excluded_type`,
`embedded_message`, `encrypted`, `empty_text` (PDF scanné), `failed`, `timeout`.

Résumé (stdout, compteurs seulement), notamment : `complete`, `new`, `refreshed`, `deleted`, `excluded_removed`, `moved`,
`flags_changed`, `chunks_added`, `failed`, `remaining`, `errors` (par type), `totals`, `attachments_extracted`,
`attachments_deduplicated`, `attachments_failed`, `attachments_skipped_by_reason`, `attachments_errors`, `attachments_remaining`, puis
`embedding` (résultat de l'étape `embed`, ou `{"skipped": ...}` avec `--no-embed`, sans fichiers de modèle ou
quand `--max-time` est atteint).

### Modèle d'embedding (local, CPU)

`intfloat/multilingual-e5-base` (défaut d'un nouvel index, 768 dimensions) ou `multilingual-e5-small` (384
dimensions), exécutés sur le CPU avec candle (Rust pur, pas de DLL). Le modèle n'est pas dans l'exe ; ses fichiers
(`config.json`, `tokenizer.json`, `model.safetensors`) sont figés sur une révision Hugging Face et vérifiés par
SHA-256 avant chaque chargement. L'index mémorise son modèle : `sync`, `embed`, `model fetch` et `bench-embed`
l'utilisent sans `--model` (sinon e5-base).

```bash
outlook-index model list --pretty                # révisions figées, fichiers présents
outlook-index model fetch                        # modèle de l'index, sinon e5-base (~1,1 Go)
outlook-index model fetch --model e5-small       # ~490 Mo
outlook-index bench-embed --pretty               # ~30 s de passages synthétiques sur ce CPU
outlook-index bench-embed --model e5-small --seconds 60 --workers 4 --threads 3 --pretty
```

`model fetch` télécharge en HTTPS depuis huggingface.co avec `curl` (fourni avec Windows 10+), reprend un
téléchargement interrompu (`<nom>.part`) et n'installe un fichier que si son SHA-256 est correct. Téléchargement
manuel : mettre les trois fichiers de la révision figée dans un dossier et passer `--model-dir` (ou
`OUTLOOK_INDEX_MODEL_DIR`). Dossier par défaut : `%LOCALAPPDATA%\outlook-cli\models\multilingual-e5-base` (ou
`-small`). Les textes reçoivent le préfixe e5 obligatoire (`query: ` / `passage: `), sont coupés à 512 tokens,
puis moyennés et normalisés.

**Workers et threads.** `--workers N` (sur `sync`, `embed` et `bench-embed`) calcule N lots à la fois ;
`--threads` est le nombre de threads *par worker*. Défauts : workers = max(1, (CPU logiques − 2) / 3), threads = 3,
soit 4 × 3 pour 14 CPU. Au-delà d'un worker, chacun est un processus fils (`outlook-index embed-worker`, interne)
avec sa propre copie du modèle : la mémoire croît avec N (VPS, e5-small : ~1,25 Go de RSS par processus).
Plusieurs petits processus passent mieux à l'échelle qu'un seul avec beaucoup de threads. Portable du
propriétaire (Intel i5, 14 CPU logiques, e5-small, ~311 tokens par passage) : un processus à 2 / 4 / 6 / 8 / 13
threads donne 2,39 / 2,96 / 3,71 / 3,81 / 4,48 passages/s ; 4 processus × 3 threads 8,19 au total, 6 × 2 7,42 ;
la taille de lot (`--batch` 1/8/16) ne change rien.

### Portée des embeddings et calcul

La recherche couvre tous les dossiers indexés ; les embeddings sont limités par une portée (défaut : tout
l'index) mémorisée dans l'index et réutilisée par chaque run.

```bash
outlook-index scope --pretty                                         # portées + compteurs
outlook-index scope --embed-since 2025-01-01 --embed-until 2026-03-31 -F Inbox -F "Archive/2026"
outlook-index scope --attachment-chunks 10                           # chunks embeddés par pièce jointe (défaut 30)
outlook-index scope --attachment-types pdf,docx,xlsx,pptx,txt,csv,html --attachment-max-mb 20   # les défauts
outlook-index scope --clear                                          # tout remettre par défaut
outlook-index embed --max-time 30m                                   # plus récents d'abord ; reprend où il s'est arrêté
outlook-index embed --model e5-small --reembed                       # changer de modèle : supprime les vecteurs e5-base
```

`scope` : `--embed-since` (inclus) et `--embed-until` (exclu) sur la date de réception, même syntaxe que
`outlook-cli --since/--until`, stockés en dates absolues (`90d` reste la date qu'il désignait) ; `-F` comme
`search -F`, répétable, remplace les dossiers mémorisés ; les options absentes sont conservées. `--clear` remet
les deux portées par défaut et ne se combine pas avec les autres options. Sortie : `chunks_in_scope`, `embedded`,
`pending`, `embedded_pct`, `out_of_scope`, `out_of_scope_embedded` (un changement de portée ne supprime jamais de
vecteur), `attachment_chunks` et `attachments`. Les dossiers sont comptés, pas affichés.

Portée des pièces jointes : `--attachment-types` (liste qui remplace la précédente) et `--attachment-max-mb`
(1 à 100, défaut 20). La restreindre conserve les textes déjà extraits, leurs lignes FTS et leurs vecteurs, y
compris aux syncs suivants (comptés dans `attachments.out_of_scope`) ; seuls la suppression du dernier message
qui les porte ou le remplacement de la pièce dans la boîte les effacent. Les pièces écartées en `too_large` ou
`excluded_type` sont relues au sync suivant après un changement (`requeued_messages`) ; un contenu déjà stocké
n'est ni réextrait ni réembeddé.
`--attachment-chunks N` (défaut 30, ~50 000 caractères ; `0` = aucune) : seuls les chunks 0 à N−1 de chaque
contenu distinct sont embeddés ; la recherche par mots couvre tout le texte. Augmenter N embedde le reste au run
suivant ; le baisser conserve les vecteurs.

`embed` calcule les chunks de corps en attente dans la portée, du message le plus récent au plus ancien (passage :
sujet, saut de ligne, texte du chunk), puis les chunks de pièces jointes sous le plafond (passage : nom du
fichier, saut de ligne, texte). Chaque lot (`--batch`, défaut 8) est validé dès son retour : `Ctrl-C` ne perd que
les lots en cours (un par worker) ; `--max-time` ne lance plus de lot une fois atteint. Priorité basse par défaut
(Windows `BELOW_NORMAL_PRIORITY_CLASS`, Linux nice 10 ; `--priority normal` pour l'éviter). Un autre modèle que
celui des vecteurs existants est refusé sans `--reembed`. Fichiers de modèle absents : erreur `not_found` pour
`embed`, étape sautée pour `sync`. Sortie : `embedded`, `pending`, `attachment_chunks`, `dropped`, `complete`,
`interrupted`, `passages_per_s`, `load_ms`, `elapsed_ms`, modèle, `workers`, `threads`, `priority`.

Estimation de durée : (`pending` + `attachment_chunks.pending` de `scope`) ÷ passages/s (`bench-embed` avec les
mêmes `--workers` et `--threads`) = secondes. Exemple : 50 000 chunks à 8,19 passages/s (portable, 4 × 3,
e5-small) ≈ 1 h 42 min ; e5-base est plus lent, prendre son propre débit `bench-embed`. Taille : environ 2 Mo par
1 000 vecteurs e5-small.

### Suivi des runs longs

`sync` et `embed` écrivent leur progression sur stderr ; stdout ne reçoit que le résumé final.
`--progress auto|bar|jsonl|none` (défaut `auto` : `bar` sur un terminal, sinon `jsonl`), un rapport à chaque
changement de phase et au plus toutes les `--progress-interval` secondes (défaut 10), puis `done`. Phases :
`scan`, `index`, `attachments`, `embed_bodies`, `embed_attachments`.

```text
{"progress":{"phase":"embed_bodies","done":12345,"total":30112,"percent":40.9,"rate":3.9,"eta_s":4556,"elapsed_s":5210,"workers":4,"model":"e5-base"}}
embed_bodies       41 % 12,345/30,112  3.9/s  ETA 1h15m
```

Le même enregistrement (plus `pid`, `started_at`, `updated_at`, `interval_s`) est conservé dans l'index et lisible
par un autre processus pendant le run :

```bash
outlook-index status              # "running": null, ou {phase, done, total, percent, rate, eta_s, ..., stale}
outlook-index status --watch      # une ligne {"running": ...} toutes les 10 s (--watch 30 : 30 s) jusqu'à la fin
```

`running` vaut `null` sans run en cours ; `stale: true` signale un run dont le processus a disparu (arrêté ou
planté) : le `sync` suivant reprend. `status --watch` écrit une ligne par tick (même avec `--pretty`) et s'arrête
sur `null`, `done` ou `stale`. Compteurs et durées seulement.

`status` : `messages`, `chunks`, `folders`, `locations`, `schema` (`version`, `expected`, `upgrade_pending`),
`embedding` (modèle, révision, dimensions,
vecteurs, compteurs de portée), `include`, `attachments`, `db_bytes`, `last_sync`, `running`,
`permissions_warning` (voir Permissions).

### search

```bash
outlook-index search "budget café" -n 5 -F Inbox --since 30d --from alice   # hybride une fois embeddé
outlook-index search "facture impayée" --mode keyword                        # tous les mots
outlook-index search "unpaid invoice" --mode semantic                        # trouve « Facture impayée »
outlook-index search "server outage" --mode hybrid --format jsonl            # un résultat par ligne
```

Modes (`--mode`) :

- `keyword` : tous les mots requis, insensible aux accents et à la casse (FTS5), dans le sujet et le corps, ou dans
  un même chunk de pièce jointe. Opérateurs et guillemets sont traités comme des mots.
- `semantic` : la requête est embeddée localement (préfixe `query: `, modèle des vecteurs stockés) et comparée par
  cosinus à tous les chunks embeddés (force brute, sans index ANN). Trouve d'autres formulations et l'autre langue.
  Seuls les chunks embeddés (portée, plafond des pièces jointes) sont cherchés.
- `hybrid` : fusion RRF (k = 60) des 200 premiers résultats d'une recherche par mots « au moins un mot » (OU, tous
  les mots de la requête gardés) et des 200 premiers résultats sémantiques. Les messages hors portée des
  embeddings restent trouvés par leurs mots.
- Défaut : `hybrid` si l'index contient des vecteurs de son modèle, sinon `keyword`. `semantic`/`hybrid` sans ces
  vecteurs se rabattent sur `keyword` avec `{"warning":{"kind":"keyword_fallback",...}}` sur stderr ; le mode par
  défaut aussi quand les fichiers de modèle manquent (un `--mode semantic|hybrid` explicite donne alors une erreur
  `not_found`). Un `--model` différent de celui des vecteurs est une erreur `usage`.

Filtres, valables dans tous les modes : `-F/--folder` (nom, ou chemin avec ses sous-dossiers : `Inbox` couvre
`Inbox/Projets`, pas `Inboxes` ; répétable = OU), `-s/--since`, `-u/--until`, `--from` (expéditeur),
`--to` (destinataire To/Cc), `--has-attachments`. `-n/--limit` (défaut 10), `--excerpt-max` (défaut 300, max 2000),
`--format json|jsonl`.

Sortie : `{count, mode, hits}` (`jsonl` : un résultat par ligne). Un résultat par message, son meilleur chunk :
`id`, `message_id`, `folder`, `received`, `sent`, `subject`, `from` `{name, email}`, `to`, `cc`, `size`, `class`,
`has_attachments`, `read`, `excerpt`, `source`, `ranks`, `scores`. `source` vaut `"body"` ou
`{"kind":"attachment","index","name","type"}` (extrait tiré de la pièce jointe). `ranks`/`scores` contiennent
`keyword` (bm25) et/ou `semantic` (cosinus) pour les recherches qui ont trouvé le message, et en hybride `hybrid`
et `rrf` ; les résultats hybrides ont aussi `keyword_rank`, `semantic_rank` (`null` si absent de cette recherche)
et `score` (score RRF). `id` fonctionne avec `outlook-cli get` tant que le message n'est pas déplacé (sinon relancer
`sync`). Extrait : en `keyword`, autour des mots ; en `semantic`, le début du meilleur chunk.

Latence (VPS, 4 vCPU, 3 threads, build release, e5-small) : chargement du modèle ~1,4 s (vérification SHA-256
comprise), embedding de la requête ~30 ms, parcours des vecteurs ~80 ms pour 50 000 et ~320 ms pour 200 000 ;
recherche hybride complète ~0,17 s à 50 000 et ~0,7 s à 200 000 (pire cas : chaque chunk contient un mot). Mesuré
par les tests ignorés `semantic_bench` et `real_e5_search` :
`E5_SMALL_DIR=... cargo test --release --features index --lib -- --ignored --nocapture semantic_bench real_e5_search`.
Parité avec sentence-transformers (`scripts/e5_reference.py` → `tests/fixtures/e5_reference.json`) :
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
