---
name: outlook-cli
description: Lire les e-mails Outlook locaux (fichier .ost/.pst) avec le CLI outlook-cli — lister les derniers messages, filtrer par date/expéditeur/sujet/dossier, lire un message complet, extraire les pièces jointes, surveiller l'arrivée de nouveaux e-mails (new --state, watch), et rechercher dans toute la boîte par mots-clés ou par sens avec l'index local outlook-index. À utiliser dès qu'une tâche demande de consulter, rechercher ou surveiller la boîte mail Outlook de l'utilisateur sur ce poste Windows/WSL, ou d'installer ce CLI.
---

# outlook-cli

CLI en lecture seule qui lit directement le fichier de données d'Outlook (OST/PST), même pendant qu'Outlook tourne.
Sortie JSON, réponse en ~50 ms, quelques Mo de mémoire. Il ne peut **ni envoyer, ni supprimer, ni marquer comme lu** :
il ne lit que ce qu'Outlook a déjà synchronisé localement.

## 1. Vérifier / installer

```bash
outlook-cli --version        # WSL : ~/.local/bin/outlook-cli (lien vers le .exe Windows)
outlook-cli info             # doit renvoyer {"kind":"ost","format":...,"inbox":{...}}
```

PowerShell : `& "$env:LOCALAPPDATA\Programs\outlook-cli\outlook-cli.exe" --version`

Si la commande est absente, construire et installer depuis les sources (sous WSL, sans droits root) :

```bash
git clone https://github.com/dmoraine/outlook-cli.git && cd outlook-cli
scripts/setup-toolchain.sh   # une seule fois : rustup, cible x86_64-pc-windows-gnullvm, linker llvm-mingw
scripts/build.sh             # tests + dist/outlook-cli.exe, dist/outlook-index.exe
scripts/install.sh           # copie dans %LOCALAPPDATA%\Programs\outlook-cli\ + liens ~/.local/bin/outlook-cli, outlook-index
```

Règle importante : il faut toujours exécuter le **`.exe` Windows**, y compris depuis WSL. Le binaire Linux
(`dist/outlook-cli`) échoue avec une erreur `io` (code 4) sur un OST ouvert par Outlook, qui le verrouille ; il ne
convient qu'à des copies de fichiers.

Fichier lu : `$OUTLOOK_CLI_FILE`, sinon le `.ost` le plus récent de `%LOCALAPPDATA%\Microsoft\Outlook`, sinon
`-f <chemin>`. Les chemins WSL (`/mnt/c/...`, `/home/...`) sont acceptés.

## 2. Choisir la commande

| Besoin | Commande |
|---|---|
| Les N derniers e-mails reçus | `outlook-cli list -n 10` |
| Reçus depuis une date/heure | `outlook-cli list --since "2026-10-15 08:00"` ou `--since 2h` |
| D'un expéditeur | `outlook-cli list --from dupont` (nom ou adresse, sous-chaîne) |
| Recherche par sujet, tous dossiers | `outlook-cli list -F all --subject facture -n 20` |
| Lire un message en entier | `outlook-cli get <id>` (corps + destinataires + pièces jointes) |
| Récupérer les pièces jointes | `outlook-cli attachments <id> -d <dossier>` |
| Nouveaux messages depuis mon dernier passage | `outlook-cli new --state <fichier>` |
| Flux continu des arrivées | `outlook-cli watch --interval 5` (JSONL) |
| Noms et ids des dossiers | `outlook-cli folders --format text` |

## 3. Filtres (list, new, watch)

- `-F, --folder X` : chemin (`Inbox/Projets`), nom, ou id ; répétable ; `all` = tous les dossiers de mails.
  Défaut : Inbox (détectée quelle que soit la langue). En cas de nom ambigu, l'erreur liste les chemins possibles.
- `-s, --since T` (inclus), `-u, --until T` (exclu), `--after T` (exclu, prévu pour un curseur).
  Formats de T : `2026-10-15T08:00:00Z`, `2026-10-15T10:00+02:00`, `"2026-10-15 08:00"` et `2026-10-15`
  (**heure locale**), `90s` `15m` `2h` `3d` `1w`, `today`, `yesterday`.
- `--from S`, `--to S` (to/cc/bcc) : sous-chaîne insensible à la casse, répétable (= OU).
- `--subject S`, `--unread`, `--read`, `--has-attachments`, `--class IPM.Note` (e-mails seuls ; `IPM.Schedule` =
  invitations).
- `list` seulement : `-n N` (défaut **20** ; `0` = sans limite), `--oldest-first`.

## 4. Contenu renvoyé — économiser les tokens

Par défaut, `list` renvoie l'en-tête et un `preview` (jusqu'à ~500 caractères), sans corps. On ajoute du contenu seulement
si nécessaire :

- `--body` : corps texte (HTML converti) ; toujours le borner avec `--body-max 2000` à `4000`.
- `--recipients`, `--attachments`, ou `--full` (les trois).
- Démarche conseillée : `list` (léger) → choisir → `get <id> --body-max 8000` pour les messages utiles.
- `get` : `--no-body`, `--html` (HTML brut), `--headers` (en-têtes de transport : authentification, Received...).

## 5. Format de sortie

`list` / `new` (format `json` par défaut) :

```json
{"cursor":"2026-10-15T18:26:40.732853300Z","messages":[{
  "id":2097444,"folder":"Inbox",
  "received":"2026-10-15T18:26:40.732853300Z","sent":"2026-10-15T18:26:38Z",
  "subject":"...","from":{"name":"...","email":"x@y.com"},
  "to":"Nom 1; Nom 2","cc":"...","unread":true,"has_attachments":true,
  "importance":"normal","class":"IPM.Note","size":199079,
  "message_id":"<...@...>","in_reply_to":"<...>","preview":"..."}],"count":1}
```

- Les heures sont en **UTC**. Avec `--local-time`, elles sont affichées avec le décalage local.
- `id` est interne au fichier (pour `get`/`attachments`). Il peut changer si le message est déplacé ;
  **dédupliquer sur `message_id`**.
- `new` ajoute `"initialized": true|false`. `get <un seul id>` renvoie directement l'objet message.
- `--format jsonl` : un message par ligne, sans enveloppe (le plus pratique à traiter en flux).
  `--format text` : à réserver aux humains.
- Champs optionnels : `body` et `body_truncated`, `recipients` `[{type:"to|cc|bcc",name,email}]`,
  `attachments` `[{index,name,size,mime_type,content_id,method,inline}]`, `html`, `headers`.

## 6. Surveiller les nouveaux e-mails

**Méthode recommandée : `new --state`**. Elle est sans état côté agent et résiste aux retards de synchronisation
d'Exchange.

```bash
outlook-cli new --state ~/.agent/inbox.state --format jsonl --full --body-max 4000
```

- Le 1er appel crée l'état et ne renvoie **rien**. Ajouter `--initial 5` pour recevoir aussi les 5 plus récents.
- Les appels suivants renvoient uniquement les messages jamais vus, du plus ancien au plus récent. L'état n'est
  enregistré qu'une fois la sortie écrite.
- Utiliser **un fichier d'état par consommateur et par usage** : deux agents qui partagent un fichier se volent
  les messages.
- Les filtres sont acceptés (`-F all`, `--from`...). Un message écarté par un filtre est quand même marqué comme vu.
- Fenêtre de suivi : `--window 7d` (défaut). Un message reçu plus de 7 jours avant le plus récent n'est jamais
  signalé.

**Processus permanent : `watch`**. Il écrit une ligne JSON par nouveau message, avec flush immédiat. Son coût est
quasi nul au repos.

```bash
outlook-cli watch --interval 5 --state ~/.agent/watch.state     # --max N pour s'arrêter après N messages
```

Sans `--state`, `watch` ne signale que ce qui arrive après son démarrage.

**Curseur (dépannage uniquement)** : `list --after <cursor> --oldest-first -n 50`. Un e-mail synchronisé en retard,
dont la date de réception est antérieure au curseur, serait manqué.

## 7. Erreurs

Code de retour, et sur stderr `{"error":{"kind":"...","message":"..."}}` :

| Code | kind | Action |
|---|---|---|
| 0 | — | succès (une liste vide n'est pas une erreur) |
| 2 | usage | corriger les arguments. Les erreurs de syntaxe d'options (option inconnue, valeur manquante) sont en texte clair, pas en JSON |
| 3 | not_found | id, Message-ID ou dossier inexistant : vérifier avec `folders` / `list` |
| 4 | io | fichier introuvable ou verrouillé : utiliser le `.exe`, vérifier `-f` / `OUTLOOK_CLI_FILE` |
| 5 | format | fichier non géré (PST ANSI d'Outlook 97–2002, ou pas un PST/OST) |
| 6 | inconsistent | Outlook écrivait pendant la lecture (déjà retenté 5 fois) : réessayer quelques secondes plus tard |

## 8. Pièges

- **Windows PowerShell 5.1** corrompt les accents de la sortie native : ajouter `--ascii` (ou
  `$env:OUTLOOK_CLI_ASCII="1"`) avant `ConvertFrom-Json`. Inutile depuis WSL ou bash.
- `attachments` ignore les images intégrées au corps (`inline`), sauf avec `--include-inline`. Utiliser `-i <index>`
  pour une pièce précise. Les fichiers existants ne sont pas écrasés : un suffixe ` (1)` est ajouté.
  Un e-mail joint (`method: "embedded_message"`) est listé mais non extractible.
- Ne voit que ce qui est dans l'OST : si Outlook est fermé ou hors ligne, il n'y a pas de nouveaux messages.
  Les dossiers non synchronisés (mode Exchange en cache limité dans le temps) sont absents.
- Une valeur commençant par `-` ou contenant des espaces doit être mise entre guillemets :
  `--subject "Re: projet"`, `--since "2026-10-15 08:00"`.
- Le corps des invitations et des réponses aux réunions est souvent vide : ce n'est pas une erreur.

## 9. outlook-index : recherche dans l'index local (preview)

`outlook-index` (installé avec `outlook-cli`, `~/.local/bin/outlook-index`) cherche dans un index SQLite local de
toute la boîte : mots-clés (FTS5) et sens (embeddings calculés sur le CPU, aucun accès réseau). Mêmes conventions
que `outlook-cli` : JSON sur stdout, `--pretty`, `--ascii`, `--local-time`, mêmes codes de retour et
`{"error":{"kind","message"}}`. Toujours le `.exe` Windows, comme pour `outlook-cli`.

**Quand l'utiliser** :

| Besoin | Commande |
|---|---|
| Messages récents, nouveaux, d'un dossier, filtres exacts sur les métadonnées | `outlook-cli list` / `new` (toujours à jour, ~50 ms) |
| Retrouver un sujet dans toute la boîte, sur des années, y compris dans les pièces jointes | `outlook-index search "<requête>"` |
| Formulation inconnue, autre langue (requête française, mail anglais) | `outlook-index search "<requête>" --mode semantic` |
| Lire le message trouvé | `outlook-cli get <id>` (ou `get "<message_id>"` si l'id a changé) |

L'index n'est à jour qu'après le dernier `sync` : un message arrivé depuis est absent. Vérifier `status` →
`last_sync.at` avant de conclure qu'un message n'existe pas.

**Modes** (`--mode`) : `keyword` (tous les mots requis, insensible aux accents), `semantic` (proximité de sens),
`hybrid` (fusion RRF des deux ; la recherche par mots y prend au moins un mot). Défaut : `hybrid` si l'index a des
vecteurs, sinon `keyword` ; un repli sur `keyword` est signalé sur stderr par
`{"warning":{"kind":"keyword_fallback",...}}`. Le mode sémantique ne voit que les messages de la portée des
embeddings (`outlook-index scope`).

**Filtres** : `-F/--folder` (nom, ou chemin avec ses sous-dossiers ; répétable), `-s/--since`, `-u/--until` (même
syntaxe que `outlook-cli`), `--from`, `--to` (To/Cc), `--has-attachments`, `-n/--limit` (défaut 10),
`--excerpt-max` (défaut 300, max 2000), `--format json|jsonl`. Les Éléments supprimés et le Courrier indésirable ne
sont pas indexés par défaut.

```bash
outlook-index search "renouvellement contrat assurance" -n 5 --since 2024-01-01
outlook-index search "facture" --mode keyword -F Inbox --from dupont --format jsonl
```

**Sortie** : `{count, mode, hits}`, un résultat par message (son meilleur chunk) : `id`, `message_id`, `folder`,
`received`, `subject`, `from`, `to`, `cc`, `read`, `has_attachments`, `excerpt` (borné), `source`, `ranks`,
`scores`. `source` = `"body"` ou `{"kind":"attachment","index","name","type"}` : l'extrait vient alors de la pièce
jointe `index` (à récupérer avec `outlook-cli attachments <id> -i <index>`). `ranks`/`scores` : `keyword` (bm25),
`semantic` (cosinus), et en hybride `hybrid`/`rrf` ; les résultats hybrides ont aussi `keyword_rank`,
`semantic_rank` (`null` si cette recherche ne l'a pas trouvé) et `score`. `id` est valable pour `outlook-cli get`
tant que le message n'a pas été déplacé ; dédupliquer sur `message_id`.

**Suivre un sync en cours** : `outlook-index status` → `running` (`null` si aucun run ; sinon `phase`, `done`,
`total`, `percent`, `eta_s`… ; `stale: true` = processus disparu, le `sync` suivant reprend), ou
`outlook-index status --watch` (une ligne JSON toutes les 10 s jusqu'à la fin). Un run lancé avec `--progress jsonl`
(défaut hors terminal) écrit des lignes `{"progress":{...}}` sur stderr ; stdout ne reçoit que le résumé final.

**Règles pour un agent** :

- **Ne pas lancer `sync`, `embed` ni modifier `scope`** sans la demande du propriétaire, et jamais pendant qu'un
  autre run tourne (`status` → `running` non `null` et non `stale`) : un premier sync dure des heures et occupe le
  CPU. Un second run échoue d'ailleurs avec `usage` (« another sync is running »).
- `search`, `status`, `scope` sans option et `doctor` sont en lecture seule et peuvent tourner pendant un sync.
- **Confidentialité** : les résultats de `search` contiennent des sujets, adresses et extraits de mails. Ne jamais
  les transmettre à un outil ou service tiers (API externe, ticket, dépôt, log partagé) sans l'accord du
  propriétaire. `doctor`, `status`, `scope` et les résumés de `sync` ne contiennent que des agrégats.

**Erreurs** : `usage` (2 : requête sans mot, option invalide, autre run en cours, suppression massive refusée),
`not_found` (3 : pas encore d'index → `sync` ; dossier inconnu de l'index ; fichiers de modèle absents avec
`--mode semantic|hybrid` explicite → `outlook-index model fetch`), `io` (4), `format` (5), `inconsistent` (6 :
réessayer). Détails : [README](../../README.md#outlook-index-preview), procédure de test :
[docs/WINDOWS_TESTING.md](../../docs/WINDOWS_TESTING.md).
