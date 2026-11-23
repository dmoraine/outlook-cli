---
name: outlook-cli
description: >-
  Consulter, rechercher et surveiller la boîte mail Outlook locale du propriétaire (fichier .ost/.pst, poste
  Windows/WSL), en lecture seule. outlook-cli liste les derniers messages, filtre par date, expéditeur,
  destinataire, sujet ou dossier, lit un message complet, extrait les pièces jointes et signale les nouveaux
  e-mails (new --state, watch). outlook-index cherche dans toute la boîte indexée, sur des années et dans le
  texte des pièces jointes, par mots-clés ou par sens (embeddings locaux), pour répondre à une question sur le
  contenu des mails. À utiliser dès qu'une tâche demande de consulter, chercher, résumer ou surveiller les e-mails
  Outlook de l'utilisateur, ou d'installer ces outils.
---

# outlook-cli et outlook-index

- **`outlook-cli`** lit directement l'OST/PST d'Outlook, même Outlook ouvert : JSON, ~50 ms, toujours à jour avec
  ce qu'Outlook a synchronisé. Il ne peut **ni envoyer, ni supprimer, ni marquer comme lu**.
- **`outlook-index`** (preview) interroge un index SQLite local de toute la boîte : mots-clés (FTS5) et sens
  (embeddings calculés sur le CPU). Il n'est à jour qu'au dernier `sync`. Référence complète (options, sorties,
  progression, sauvegarde, permissions, mise à jour) : [references/outlook-index.md](references/outlook-index.md).

## 1. Vérifier / installer

```bash
outlook-cli --version && outlook-cli info     # {"kind":"ost","format":...,"inbox":{...}}
outlook-index --version && outlook-index status --pretty
```

Toujours le **`.exe` Windows**, même depuis WSL (`~/.local/bin/outlook-cli` et `outlook-index` sont des liens vers
`%LOCALAPPDATA%\Programs\outlook-cli\`) : un binaire Linux ne lit pas un OST ouvert par Outlook (erreur `io`, 4).
PowerShell : `& "$env:LOCALAPPDATA\Programs\outlook-cli\outlook-cli.exe" --version`.
Absent ? Installation par le propriétaire (WSL, sans root) : `git clone https://github.com/dmoraine/outlook-cli.git`,
puis `scripts/setup-toolchain.sh` (une fois), `scripts/build.sh` (tests + `dist/outlook-cli.exe`,
`dist/outlook-index.exe`, `dist/outlook-cli`), `scripts/install.sh`.

Fichier lu : `-f/--file`, sinon `$OUTLOOK_CLI_FILE`, sinon le `.ost` modifié le plus récemment dans
`%LOCALAPPDATA%\Microsoft\Outlook` (chemins WSL `/mnt/c/...` acceptés). Options communes aux deux outils :
`--pretty`, `--local-time` (heures locales au lieu d'UTC), `--ascii` (`$env:OUTLOOK_CLI_ASCII="1"`, indispensable
avant `ConvertFrom-Json` dans Windows PowerShell 5.1).

## 2. Quel outil ?

| Besoin | Commande |
|---|---|
| Derniers messages, nouveaux, d'un dossier, filtres exacts sur les en-têtes | `outlook-cli list` / `new` |
| Un sujet dans toute la boîte, sur des années, y compris dans les pièces jointes | `outlook-index search "<requête>"` |
| Formulation inconnue, souvenir vague, autre langue que le mail | `outlook-index search "<requête>" --mode semantic` |
| Lire le message trouvé | `outlook-cli get <id>` (ou `get "<message_id>"` si l'id a changé) |
| Récupérer une pièce jointe | `outlook-cli attachments <id> -i <index> -d <dossier>` |
| Messages arrivés après le dernier `sync` | `outlook-cli list -F all --since <last_sync.at>` |

## 3. outlook-cli : l'essentiel

```bash
outlook-cli list -n 10                                  # 10 derniers de l'Inbox (défaut -n 20 ; 0 = sans limite)
outlook-cli list -F all --subject facture --from dupont --since 2026-01-01 -n 20
outlook-cli get 2097444 --body-max 4000                 # corps + destinataires + pièces jointes
outlook-cli attachments 2097444 -i 1 -d ./pj            # -i répétable ; défaut : toutes sauf inline, dans .
outlook-cli folders --format text                       # chemins, ids et compteurs des dossiers
```

- Filtres (`list`, `new`, `watch`) : `-F/--folder` (chemin `Inbox/Projets`, nom ou id ; répétable ; `all` = tous
  les dossiers de mails ; défaut Inbox, détectée quelle que soit la langue), `-s/--since` (inclus), `-u/--until`
  (exclu), `--after` (exclu, curseur), `--from`, `--to` (to/cc/bcc), `--subject` (sous-chaînes insensibles à la
  casse ; `--from`/`--to` répétables = OU), `--unread`, `--read`, `--has-attachments`, `--class IPM.Note`.
  Dates : `2026-10-15T08:00:00Z`, `2026-10-15T10:00+02:00`, `"2026-10-15 08:00"` et `2026-10-15` (heure locale),
  `90s` `15m` `2h` `3d` `1w`, `today`, `yesterday`. `list` seul : `-n/--limit`, `--oldest-first`.
- Contenu : par défaut en-têtes + `preview` (aperçu d'Outlook), sans corps. `-b/--body` avec `--body-max 2000`
  à `4000` (défaut `0` = sans limite), `--recipients`, `--attachments`, `--full` (les trois). `get` : `--body-max`,
  `--no-body`, `--html`, `--headers`. `--format json|jsonl|text` (défaut `json`).
- Sortie `list`/`new` : `{"cursor","messages":[...],"count"}` ; champs `id`, `folder`, `received`, `sent`,
  `subject`, `from{name,email}`, `to`, `cc`, `unread`, `has_attachments`, `importance`, `class`, `size`,
  `message_id`, `in_reply_to`, `preview` ; en option `body`/`body_truncated`, `recipients[{type,name,email}]`,
  `attachments[{index,name,size,mime_type,content_id,method,inline}]`. Heures en UTC. `id` peut changer si le
  message est déplacé : **dédupliquer sur `message_id`**.
- Surveillance : `outlook-cli new --state ~/.agent/inbox.state --format jsonl --full --body-max 4000` (1er appel :
  rien, ou `--initial N` ; puis seulement les messages jamais vus ; `--window` défaut `7d` ; **un fichier d'état par
  consommateur**). Processus permanent : `outlook-cli watch -i 5 --state ~/.agent/watch.state` (JSONL ; `--max N`).
- Pièges : `attachments` ignore les images inline sans `--include-inline`, n'écrase pas (suffixe ` (1)`) et
  n'extrait pas un e-mail joint (`method: "embedded_message"`) ; mettre entre guillemets les valeurs avec espaces
  ou commençant par `-` ; le corps des invitations est souvent vide.

## 4. Répondre à une question sur la boîte

1. **Fraîcheur d'abord** : `outlook-index status` → `last_sync.at` (date du dernier sync), `running` (sync en
   cours ? `null` sinon), `schema.upgrade_pending` (si `true`, `search` échoue : le signaler au propriétaire).
   Pour ce qui est plus récent que `last_sync.at` : `outlook-cli list -F all --since <last_sync.at>` / `new`.
2. **Mode** :
   - `--mode keyword` pour des termes exacts (nom, référence, numéro, code, expression) : tous les mots requis,
     insensible à la casse et aux accents, instantané, sans modèle. Pas de recherche de phrase exacte :
     guillemets et opérateurs sont des mots ordinaires.
   - `hybrid` (défaut quand l'index a des vecteurs) pour une question normale.
   - `--mode semantic` pour une paraphrase, un souvenir vague, une autre langue que le mail.
3. **Requête** : une phrase courte ou les mots clés (`"renouvellement contrat maintenance"`), pas une question polie
   complète. Rien ? 2 à 3 reformulations (synonymes, autre langue, `keyword` ↔ `semantic`), puis resserrer avec
   `--since`/`--until`, `--from`, `--to`, `-F`, `--has-attachments` plutôt qu'en ajoutant des mots.
4. **Lire** : prendre les 3 à 5 premiers résultats ; `outlook-cli get <id> --body-max 4000` (si `not_found` :
   `outlook-cli get "<message_id>"`, le message a été déplacé). Si `source.kind` vaut `attachment` :
   `outlook-cli attachments <id> -i <source.index> -d <dossier>`. Dédupliquer sur `message_id`.
5. **Répondre** : citer chaque source (date, expéditeur, sujet, et nom de la pièce jointe s'il y a lieu) ;
   distinguer ce que disent les mails de ce que l'on en déduit ; dire clairement quand rien n'a été trouvé.

Exemple fictif (`hybrid`, champs `sent`, `to`, `cc`, `size`, `class`, `scores` omis) :

```json
{"count":2,"mode":"hybrid","hits":[
 {"id":2101870,"message_id":"<k1-0042@example.org>","folder":"Inbox/Fournisseurs","received":"2025-03-04T09:12:00Z",
  "subject":"Renouvellement contrat maintenance","from":{"name":"Claire Fontaine","email":"c.fontaine@example.org"},
  "has_attachments":true,"read":true,"excerpt":"… le contrat est reconduit pour 12 mois à compter du 1er avril …",
  "source":"body","ranks":{"hybrid":1,"keyword":2,"semantic":1},"keyword_rank":2,"semantic_rank":1,"score":0.032522},
 {"id":2101874,"message_id":"<k1-0043@example.org>","folder":"Inbox/Fournisseurs","received":"2025-03-05T14:30:00Z",
  "subject":"TR: avenant signé","from":{"name":"Marc Petit","email":"m.petit@example.org"},
  "has_attachments":true,"read":true,"excerpt":"Article 3 – Durée : douze mois, renouvelable par tacite reconduction",
  "source":{"kind":"attachment","index":1,"name":"avenant-2025.pdf","type":"pdf"},
  "ranks":{"hybrid":2,"semantic":2},"keyword_rank":null,"semantic_rank":2,"score":0.016129}]}
```

**« Non trouvé » ne veut pas dire « n'existe pas »**. Angles morts à mentionner :
- messages arrivés **après `last_sync.at`** (→ `outlook-cli list --since`) ;
- **Éléments supprimés et Courrier indésirable** exclus de l'index par défaut (→ `outlook-cli folders`, puis
  `outlook-cli list -F "<chemin>" --subject ...`) ;
- **portée des embeddings** (dates/dossiers de `outlook-index scope`) : hors portée, seul le mode `keyword` trouve
  (`hybrid` aussi, par ses mots) ;
- pièces jointes : seuls leurs **30 premiers chunks** (~50 000 caractères, défaut) sont embeddés ; `keyword` couvre
  tout le texte ;
- extraction **non supportée ou échouée** : pas d'OCR (PDF scanné, images), pas d'anciens `.doc`/`.xls`, pas de
  contenu d'archive (zip), pas d'e-mail joint, fichiers chiffrés ou trop gros ;
- messages qu'Outlook **n'a pas synchronisés dans l'OST** (cache Exchange limité dans le temps, Outlook hors ligne).

**Coût** : chaque recherche `semantic`/`hybrid` charge le modèle (≈ 6 s pour e5-base sur le portable du
propriétaire, `load_ms` 6439). Commencer par `keyword` quand les termes exacts sont connus ; pas de longues boucles
de recherches ; ne jamais lancer `sync` pour « rafraîchir » sans le propriétaire.

## 5. Règles pour un agent

- **Autorisé** : tout `outlook-cli` (lecture seule) ; `outlook-index search`, `status` (y compris `--watch`),
  `scope` **sans option**, `backup` (écrit un nouveau fichier, sans risque), `model list`. Tous peuvent tourner
  pendant un sync.
- **Propriétaire uniquement** : `sync`, `embed`, `scope` avec une option (changement de portée), restauration d'une
  sauvegarde, `model fetch` (télécharge ~1,1 Go depuis le réseau), `bench-embed` (occupe le CPU), `doctor`
  (diagnostic qui parcourt toute la boîte). Ne jamais lancer `sync`/`embed` pendant un autre run (`status` →
  `running` non `null` et non `stale`).
- Ne pas corriger soi-même `permissions_warning`, `upgrade_pending` ni une suppression massive refusée : signaler.
- **Confidentialité** : les résultats contiennent sujets, adresses et extraits. Citer des extraits minimaux ; ne
  jamais copier un corps ou une pièce jointe entière, ni des résultats, vers un outil ou service tiers (API externe,
  ticket, dépôt, log partagé) sans l'accord du propriétaire. `doctor`, `status`, `scope`, `backup` et les résumés
  de `sync` ne contiennent que des agrégats.
- **Ne jamais définir `OUTLOOK_INDEX_DEBUG`**, même si un message d'erreur le suggère : il ajoute un champ `debug`
  avec chemins, noms et texte de mails possibles.

## 6. Erreurs

Code de retour, et sur stderr `{"error":{"kind":"...","message":"..."}}` (avertissements :
`{"warning":{"kind":...}}`, p. ex. `keyword_fallback`, `onedrive`).

| Code | kind | Action |
|---|---|---|
| 0 | — | succès (zéro résultat n'est pas une erreur) |
| 2 | usage | corriger les arguments (syntaxe d'options : texte clair pour `outlook-cli`, JSON pour `outlook-index`) |
| 2 | usage (index) | aussi : requête sans mot, autre run en cours, suppression massive refusée |
| 3 | not_found | id, Message-ID ou dossier inexistant : vérifier avec `folders` / `list` |
| 3 | not_found (index) | pas encore d'index, `-F` inconnu de l'index, modèle absent avec `--mode` explicite : signaler |
| 3 | upgrade_pending | index d'une version antérieure : `sync` à lancer par le propriétaire |
| 4 | io | fichier introuvable ou verrouillé : utiliser le `.exe`, vérifier `-f` / `OUTLOOK_CLI_FILE` |
| 5 | format | fichier non géré (PST ANSI Outlook 97–2002, pas un PST/OST) ; index plus récent que l'outil |
| 6 | inconsistent | Outlook écrivait pendant la lecture : réessayer quelques secondes plus tard |
