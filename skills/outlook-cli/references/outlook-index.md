# outlook-index — référence détaillée

Complément de [SKILL.md](../SKILL.md) (à lire d'abord : choix de l'outil, workflow de réponse, règles agent).
Vérifié contre `outlook-index --help` et le `--help` de chaque sous-commande (binaires construits au commit
`a2001e75a98e221558b4aaac281b8ce91bc9bea8`), le [README](../../../README.md#outlook-index-preview) et
[docs/WINDOWS_TESTING.md](../../../docs/WINDOWS_TESTING.md). Colonne « Agent » : ✅ autorisé, 👤 propriétaire seul.

## Sommaire

1. [Conventions et fichier d'index](#1-conventions-et-fichier-dindex)
2. [Commandes, lecteurs et écrivains](#2-commandes-lecteurs-et-écrivains)
3. [search](#3-search)
4. [status et suivi des runs](#4-status-et-suivi-des-runs)
5. [scope](#5-scope)
6. [sync](#6-sync)
7. [embed, modèle, bench-embed](#7-embed-modèle-bench-embed)
8. [backup et restauration](#8-backup-et-restauration)
9. [doctor](#9-doctor)
10. [Permissions](#10-permissions)
11. [Mise à jour du schéma](#11-mise-à-jour-du-schéma)
12. [Erreurs et avertissements](#12-erreurs-et-avertissements)
13. [Annexe : corrections d'exactitude](#13-annexe--corrections-dexactitude)

## 1. Conventions et fichier d'index

- Binaire séparé (preview) : index SQLite unique (métadonnées, FTS5, vecteurs), embeddings calculés sur le CPU.
  Lecture seule sur l'OST ; aucun accès réseau hors `model fetch`. Toujours le `.exe` Windows (lien
  `~/.local/bin/outlook-index` sous WSL).
- Options globales de toutes les commandes : `-f/--file` (env `OUTLOOK_CLI_FILE`), `--index` (env
  `OUTLOOK_INDEX_DB`), `--pretty`, `--local-time`, `--ascii` (env `OUTLOOK_CLI_ASCII`). Pas de `--format` global :
  seul `search` a `--format json|jsonl`. JSON sur stdout ; erreurs et avertissements en JSON sur stderr.
- Index par défaut : `%LOCALAPPDATA%\outlook-cli\index.sqlite` (Linux : `$XDG_DATA_HOME/outlook-cli/index.sqlite`).
  Le garder côté Windows (pas sous `~` de WSL, pas sous OneDrive : avertissement `onedrive`). Depuis WSL, une
  variable d'environnement n'atteint le `.exe` que via `WSLENV` (p. ex. `WSLENV=$WSLENV:OUTLOOK_INDEX_DB/p`) ;
  une option (`--index /mnt/c/...`) ne demande rien.
- L'index se reconstruit toujours depuis l'OST ; la sauvegarde évite seulement les heures d'embedding.

## 2. Commandes, lecteurs et écrivains

| Commande | Rôle | Accès à l'index | Agent |
|---|---|---|---|
| `search` | recherche | lecteur (instantané cohérent) | ✅ |
| `status [--watch [S]]` | compteurs, run en cours | lecteur | ✅ |
| `scope` sans option | portées + compteurs | lecteur | ✅ |
| `backup [--to P]` | copie vérifiée dans un nouveau fichier | lecteur | ✅ |
| `model list` | modèles connus, fichiers présents | n'ouvre pas l'index | ✅ |
| `model fetch` | télécharge ~1,1 Go (e5-base) / ~490 Mo (e5-small) | lecteur | 👤 réseau |
| `bench-embed` | débit d'embedding, texte synthétique | lecteur | 👤 CPU |
| `doctor` | diagnostic agrégé de la boîte et du poste | n'ouvre pas l'index | 👤 |
| `sync` | met l'index à jour, puis embeddings | **écrivain** | 👤 |
| `embed` | embeddings en attente | **écrivain** | 👤 |
| `scope` avec une option | change la portée | **écrivain** | 👤 |

Un seul écrivain à la fois par index (verrou `index.sqlite.lock`) : il attend jusqu'à 3 s puis échoue (`usage`,
« another sync is running »). Les lecteurs ne créent, ne migrent ni ne modifient jamais l'index et peuvent tourner
pendant un sync.

## 3. search

`outlook-index search [OPTIONS] <QUERY>` — meilleur chunk par message.

| Option | Défaut | Effet |
|---|---|---|
| `--mode keyword\|semantic\|hybrid` | `hybrid` si l'index a des vecteurs de son modèle, sinon `keyword` | voir modes |
| `-n, --limit N` | 10 | nombre maximal de résultats |
| `-F, --folder X` | tous les dossiers indexés | nom, ou chemin avec ses sous-dossiers (`Inbox` couvre `Inbox/Projets`, pas `Inboxes`) ; répétable = OU. Pas d'id ni de `all` (contrairement à `outlook-cli`) |
| `-s, --since T` / `-u, --until T` | — | réception à partir de (inclus) / strictement avant ; même syntaxe que `outlook-cli` |
| `--from S` | — | expéditeur (nom ou adresse) contient ; répétable = OU |
| `--to S` | — | un destinataire **To/Cc** contient (pas Bcc) ; répétable = OU |
| `--has-attachments` | — | messages avec pièces jointes |
| `--excerpt-max N` | 300 | longueur de l'extrait, max 2000 |
| `--format json\|jsonl` | `json` | `jsonl` : un résultat par ligne |
| `--model e5-small\|e5-base` | modèle des vecteurs stockés | un autre modèle → `usage` |
| `--model-dir D` | `<data dir>/models/<dépôt>` (env `OUTLOOK_INDEX_MODEL_DIR`) | fichiers du modèle |

Modes :
- `keyword` : tous les mots requis (FTS5), insensible à la casse et aux accents, dans sujet + corps ou dans un même
  chunk de pièce jointe. Opérateurs et guillemets sont traités comme des mots (pas de phrase exacte). Ne charge
  pas le modèle. Extrait autour des mots.
- `semantic` : requête embeddée localement (préfixe `query: `), cosinus contre tous les chunks embeddés (force
  brute). Trouve d'autres formulations et l'autre langue ; ne voit que les chunks embeddés (portée, plafond des
  pièces jointes). Extrait : début du meilleur chunk.
- `hybrid` : fusion RRF (k = 60) des 200 premiers résultats d'une recherche par mots « au moins un mot » et des
  200 premiers sémantiques ; les messages hors portée des embeddings restent trouvés par leurs mots.
- Repli : `semantic`/`hybrid` sans vecteurs → `keyword` avec `{"warning":{"kind":"keyword_fallback",...}}` ; le
  mode par défaut aussi quand les fichiers de modèle manquent. Un `--mode semantic|hybrid` explicite sans fichiers
  de modèle → `not_found`.
- Coût : `semantic`/`hybrid` chargent le modèle à chaque appel (≈ 6 s pour e5-base sur le portable du
  propriétaire, `load_ms` 6439 ; ~1,4 s pour e5-small sur le VPS de test) ; la recherche elle-même prend moins
  d'une seconde.

Sortie `json` : `{count, mode, hits}` (`mode` = mode réellement utilisé, après repli). Champs d'un résultat :
`id`, `message_id`, `folder` (chemin), `received`, `sent`, `subject`, `from {name, email}`, `to`, `cc`, `size`,
`class`, `has_attachments`, `read` (attention : `outlook-cli` donne `unread`), `excerpt`, `source`, `ranks`,
`scores`.
- `source` : `"body"` ou `{"kind":"attachment","index":N,"name":"...","type":"pdf|docx|..."}` ; `index` est
  celui de `outlook-cli attachments <id> -i <index>`.
- `ranks`/`scores` : `keyword` (bm25) et/ou `semantic` (cosinus) pour les recherches qui ont trouvé le message ;
  en hybride aussi `ranks.hybrid` et `scores.rrf`, plus les champs à plat `keyword_rank`, `semantic_rank` (`null`
  si absent de cette recherche) et `score` (RRF).
- `id` vaut pour `outlook-cli get` tant que le message n'a pas été déplacé ; sinon `get "<message_id>"`.

## 4. status et suivi des runs

`outlook-index status` : `messages`, `chunks`, `folders`, `locations`, `schema` (`version`, `expected`,
`upgrade_pending`), `embedding` (modèle, révision, dimensions, vecteurs, compteurs de portée), `include`,
`attachments`, `db_bytes`, `last_sync` (`at`, `revision`, `complete`, `summary` = résumé du dernier sync), `running`,
`permissions_warning`. Agrégats seulement.

- `running` : `null` sans run ; sinon `phase`, `done`, `total`, `percent`, `rate`, `eta_s`, `elapsed_s`, `workers`,
  `model`, `pid`, `started_at`, `updated_at`, `interval_s`… ; `stale: true` = processus disparu, le `sync`
  suivant reprend.
- `status --watch [SECONDES]` (défaut 10) : une ligne `{"running": ...}` par tick (même avec `--pretty`), arrêt sur
  `null`, `done` ou `stale`.
- `sync`/`embed` : progression sur stderr, résumé final seul sur stdout. `--progress auto|bar|jsonl|none` (défaut
  `auto` = `bar` sur un terminal, sinon `jsonl` : lignes `{"progress":{...}}`), `--progress-interval S` (défaut
  10 ; chaque changement de phase est signalé). Phases : `scan`, `index`, `attachments`, `embed_bodies`,
  `embed_attachments`.

## 5. scope

Sans option (✅) : affiche les portées et compteurs : `chunks_in_scope`, `embedded`, `pending`, `embedded_pct`,
`out_of_scope`, `out_of_scope_embedded`, `attachment_chunks`, `attachments`. Les dossiers sont comptés, pas
affichés. La recherche couvre tous les dossiers indexés ; seule la portée des embeddings est limitée.

Avec une option (👤, écrivain ; les options absentes sont conservées) :
- `--embed-since T` (inclus) / `--embed-until T` (exclu) : stockés en dates absolues (`90d` reste la date qu'il
  désignait).
- `-F, --folder X` : comme `search -F`, répétable, remplace les dossiers mémorisés.
- `--attachment-chunks N` : chunks embeddés par contenu distinct, depuis le début (défaut 30, ~50 000 caractères ;
  `0` = aucun). La recherche par mots couvre tout le texte ; augmenter embedde le reste, baisser garde les vecteurs.
- `--attachment-types` (liste séparée par des virgules, remplace la précédente ; valeurs `pdf`, `docx`, `xlsx`,
  `pptx`, `txt`, `csv`, `html`), `--attachment-max-mb N` (1 à 100, défaut 20).
- `--clear` : remet les deux portées par défaut (tout, 30 chunks, tous les types, 20 Mo) ; ne se combine pas.
Un changement de portée ne supprime jamais de vecteur ni de texte extrait.

## 6. sync

👤 Écrivain. Réconcilie l'index avec tous les dossiers de mails : nouveaux, modifiés (`refreshed`), supprimés,
déplacés/copiés, lu/non lu ; puis texte des pièces jointes ; puis embeddings de la portée si les fichiers de
modèle sont présents. Reprenable : un sync interrompu continue où il s'est arrêté. Rien n'est fait si le fichier
n'a pas changé depuis le dernier sync complet.

| Option | Défaut | Effet |
|---|---|---|
| `--max-time D` | — | arrêt après `90s`/`30m`/`2h`, embeddings compris |
| `--no-embed` | — | indexation seule |
| `--no-attachments` | — | saute l'étape pièces jointes ; un sync ultérieur rattrape |
| `--include-folders deleted\|junk\|none` | `none` | inclure Éléments supprimés / Courrier indésirable (mémorisé, `status.include`) |
| `--allow-mass-delete` | — | autorise la suppression de plus de max(100, 20 %) des messages indexés (sinon arrêt sans changement, `usage`) |
| `--model`, `--model-dir`, `--workers`, `--threads`, `--worker-timeout`, `--batch`, `--reembed`, `--priority`, `--progress`, `--progress-interval` | | comme `embed` (§7) |

Indexation : citations et signatures courantes retirées, chunks d'environ 1 750 caractères (recouvrement ~200).
Pièces jointes : inline, images, trop grosses, messages joints, liens et extensions inconnues écartés sans lecture ;
le reste extrait dans un processus fils (30 s max ; `failed`/`timeout` retentés, 3 essais en tout). Formats : PDF
(couche texte, pas d'OCR), DOCX, PPTX, XLSX, TXT, CSV, HTML ; texte limité à 2 000 000 caractères. Motifs
d'exclusion : `inline`, `image`, `too_large`, `unsupported_type` (archives, anciens `.doc`/`.xls`, Markdown…),
`excluded_type`, `embedded_message`, `encrypted`, `empty_text` (PDF scanné), `failed`, `timeout`.

Résumé (stdout, compteurs) : `complete`, `new`, `refreshed`, `deleted`, `excluded_removed`, `moved`,
`flags_changed`, `chunks_added`, `failed`, `remaining`, `errors`, `totals`, `attachments_extracted`,
`attachments_deduplicated`, `attachments_failed`, `attachments_skipped_by_reason`, `attachments_errors`,
`attachments_remaining`, `embedding` (résultat de l'étape, ou `{"skipped": ...}`).

## 7. embed, modèle, bench-embed

`embed` (👤, écrivain) : chunks de corps en attente, du message le plus récent au plus ancien, puis chunks de
pièces jointes sous le plafond. Chaque lot est validé : `Ctrl-C` ne perd que les lots en cours.

| Option (`embed`, `sync`) | Défaut |
|---|---|
| `--max-time D` | — (`embed` : plus de nouveau lot après D ; les lots en cours finissent) |
| `--model e5-small\|e5-base` | modèle de l'index, sinon `e5-base` ; un autre modèle exige `--reembed` |
| `--model-dir D` | `<data dir>/models/<dépôt>` (env `OUTLOOK_INDEX_MODEL_DIR`) |
| `--workers N` | (CPU logiques − 2) / 3, au moins 1 ; > 1 = autant de processus, chacun avec son modèle |
| `--threads N` | 3 (par worker) |
| `--worker-timeout D` | 180 s pour charger le modèle, 120 s par lot ; au-delà : worker tué, arrêt `io` |
| `--batch N` | 8 passages par lot |
| `--reembed` | — supprime les vecteurs d'un autre modèle |
| `--priority low\|normal` | `low` (Windows below normal, Linux nice 10) |

Sortie `embed` : `embedded`, `pending`, `attachment_chunks`, `dropped`, `complete`, `interrupted`,
`passages_per_s`, `load_ms`, `elapsed_ms`, modèle, `workers`, `threads`, `priority`. Fichiers de modèle absents :
`not_found` pour `embed`, étape sautée pour `sync`.

Modèles : `e5-base` (`intfloat/multilingual-e5-base`, 768 dimensions, défaut d'un nouvel index) ou `e5-small`
(384). Fichiers `config.json`, `tokenizer.json`, `model.safetensors`, révision figée, SHA-256 vérifié à chaque
chargement ; dossier par défaut `%LOCALAPPDATA%\outlook-cli\models\multilingual-e5-base` (ou `-small`).
- `model list` (✅) : révisions figées et présence des fichiers (tailles seulement).
- `model fetch [--model M] [--model-dir D]` (👤) : HTTPS depuis huggingface.co avec `curl`, reprise d'un
  téléchargement interrompu, contrôle SHA-256. Seul accès réseau de l'outil.
- `bench-embed` (👤) : `--seconds` (défaut 30), `--model`, `--model-dir`, `--workers`, `--threads`,
  `--worker-timeout`, `--batch` (8) ; texte synthétique, sortie `passages_per_s`.
  Durée estimée d'un embedding : (`pending` + `attachment_chunks.pending` de `scope`) ÷ `passages_per_s`.

## 8. backup et restauration

`outlook-index backup [--to <nouveau fichier>]` (✅, lecteur, sans danger pendant un sync) : copie cohérente
(SQLite `VACUUM INTO`) dans un **nouveau** fichier ; une destination existante est refusée (`usage`). Défaut :
`backups/index-YYYYMMDD-HHMMSS.sqlite` à côté de l'index. La copie est rouverte en lecture seule et vérifiée.
Sortie : `destination`, `bytes`, `schema_version`, `counts` (`messages`, `chunks`, `vectors`, `attachment_chunks`,
`attachment_vectors`), `integrity` (`ok` ; sinon erreur `inconsistent` et copie supprimée). Ne pas recopier
`destination` (chemin) dans un rapport. Ne jamais copier `index.sqlite` (ni `-wal`/`-shm`) à la main.

Restauration (👤), **tous les `outlook-index` arrêtés** (aucun `sync`, `embed`, `search`, `status --watch`) :
supprimer `index.sqlite-wal` et `index.sqlite-shm` s'ils existent, puis copier la sauvegarde vérifiée sur
`index.sqlite` :

```powershell
$d = "$env:LOCALAPPDATA\outlook-cli"
Remove-Item "$d\index.sqlite-wal", "$d\index.sqlite-shm" -ErrorAction SilentlyContinue
Copy-Item "$d\backups\index-YYYYMMDD-HHMMSS.sqlite" "$d\index.sqlite"
```

## 9. doctor

`doctor` (👤) : agrégats seulement (machine, SQLite/FTS5, état de l'index avec `permissions_warning`, comptes de
dossiers et messages, durée du scan, statistiques d'un échantillon). `--sample N` : messages ouverts (défaut 200) ;
avec `--attachments`, pièces jointes extraites chacune dans un processus fils (défaut 300, 30 s chacune). Aucune
donnée de mail dans la sortie.

## 10. Permissions

L'index contient une copie en clair des mails. Sous Unix, un dossier d'index créé par l'outil est en 0700 et ses
fichiers en 0600 ; un dossier ou fichier existant n'est jamais modifié. S'il est accessible au groupe ou aux
autres, `status` et `doctor` affichent `permissions_warning` (texte fixe ; `null` sinon). Agent : le signaler,
ne pas changer les droits. Sous Windows, `%LOCALAPPDATA%` est propre à l'utilisateur ; un `--index` ailleurs relève
des ACL choisies par son propriétaire.

## 11. Mise à jour du schéma

Après mise à jour de l'outil, `status` peut indiquer `"schema": {"version": 4, "expected": 6, "upgrade_pending":
true}` (compteurs des tables manquantes à `null`) ; `search` et `scope` sans option échouent alors avec
`upgrade_pending` (code 3). Le prochain écrivain (`sync`, 👤) applique la mise à jour dans une seule transaction.
Un index d'une version plus récente que l'outil est refusé (`format`, 5). Ne pas installer une nouvelle version
pendant qu'un `outlook-index` tourne (Windows refuse d'écraser un `.exe` en cours d'exécution).

## 12. Erreurs et avertissements

Codes : 0 ok, 2 `usage`, 3 `not_found` / `upgrade_pending`, 4 `io`, 5 `format`, 6 `inconsistent`. Sur stderr :
`{"error":{"kind","message"}}`. Le `message` est un texte fixe par type (plus nombres, noms d'options, valeurs
autorisées, noms de modèles ou emplacement dans le code) : jamais de chemin, nom de dossier ou de pièce jointe,
texte de requête ou de mail ; une ligne d'erreur peut être recopiée telle quelle. Les erreurs de syntaxe des
options sont aussi en JSON (`usage`). Certains messages suggèrent `OUTLOOK_INDEX_DEBUG=1` : **un agent ne le
définit jamais** (le champ `debug` contient le texte brut : chemins, noms, contenu de mails possible).

- `usage` (2) : requête sans mot, option invalide, `--model` différent des vecteurs, autre run en cours,
  suppression massive refusée, destination de `backup` existante.
- `not_found` (3) : pas encore d'index (→ `sync`, 👤), filtre `-F` sans dossier correspondant dans l'index,
  fichiers de modèle absents avec un mode explicite (→ `model fetch`, 👤).
- `upgrade_pending` (3) : voir §11. `io` (4) : fichier illisible, worker expiré. `format` (5) : fichier de boîte,
  d'index ou de modèle non géré. `inconsistent` (6) : la boîte a changé pendant la lecture, réessayer.
- Avertissements (stderr) : `{"warning":{"kind":"keyword_fallback",...}}`, `{"warning":{"kind":"onedrive",...}}`.

## 13. Annexe : corrections d'exactitude

Relevées en comparant l'ancien `SKILL.md` au `--help` des binaires de `a2001e7`, au README et aux docs :

1. `model fetch` et `bench-embed` étaient présentés parmi les commandes qu'un agent peut lancer : ils sont réservés
   au propriétaire (téléchargement réseau ~1,1 Go ; CPU occupé).
2. « `search` et `scope` échouent avec `upgrade_pending` » → seul `scope` **sans option** échoue ; avec une option
   c'est un écrivain, qui applique la mise à jour.
3. Ordre de résolution du fichier : `-f/--file` d'abord, sinon `$OUTLOOK_CLI_FILE`, sinon le `.ost` le plus récent
   (l'ancien texte plaçait `-f` en dernier).
4. Sortie de `search` : ajout des champs `sent`, `size`, `class` ; `folder` est un chemin ; le mode réel figure
   dans `mode` (le `--help` de `--format` écrit `{count, hits}`, la sortie réelle contient aussi `mode`).
5. `search -F` : nom ou chemin seulement (pas d'id, pas de `all`) ; `search --to` couvre To/Cc, pas Bcc.
6. `status --watch` accepte une période (`--watch 30`) ; `last_sync` contient `at`, `revision`, `complete`,
   `summary`.
7. `scope --attachment-chunks` : défaut 30 (~50 000 caractères) ; `--attachment-max-mb` défaut 20 ;
   `--attachment-types` limité à `pdf, docx, xlsx, pptx, txt, csv, html`.
8. `scripts/build.sh` produit aussi `dist/outlook-cli` (Linux) ; `outlook-cli list` : le court `-b` de `--body`,
   `--body-max` défaut `0` (sans limite) ; `watch -i` = `--interval`, `attachments -i` = `--index`, `-d` défaut `.`.
9. Retiré : « preview jusqu'à ~500 caractères » et « déjà retenté 5 fois » (non vérifiables dans `--help`/README) ;
   `preview` n'apparaît que sans `--body`.
10. `doctor` n'est plus listé comme sans restriction : il parcourt toute la boîte (et `--attachments` lance des
    extractions) ; propriétaire seul.
11. Ligne cassée en fin de section 9 (phrase « Détails : [README]… » collée au paragraphe des erreurs) : remplacée
    par les liens d'en-tête de cette référence.
12. Erreurs : un agent ne suit pas la suggestion `OUTLOOK_INDEX_DEBUG=1` présente dans certains messages.
