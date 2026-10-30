[English](README.md) | [Español](README.es.md) | [Français](README.fr.md) | [Deutsch](README.de.md) | [Português (BR)](README.pt-BR.md)

*Traduction française de [README.md](README.md). Le texte original en anglais est la version officielle et prévaut en cas de divergence.*

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="web/public/mark-dark.svg">
    <source media="(prefers-color-scheme: light)" srcset="web/public/mark-light.svg">
    <img src="web/public/mark-dark.svg" alt="Sotto" width="64" height="64">
  </picture>
</p>

# Sotto

La synchronisation de secrets chiffrés de bout en bout pour les équipes de développement. Arrêtez d'envoyer votre `.env` sur Slack.

> [!WARNING]
> Sotto est pré-1.0 et n'a **pas** fait l'objet d'un audit cryptographique par un tiers. Il fonctionne de bout en bout, mais
> vous ne devriez pas encore lui confier de secrets critiques de production. Voir [SECURITY.md](SECURITY.md).

Sotto repose sur une seule implémentation cryptographique Rust partagée par la CLI native et le client
navigateur via WebAssembly. Le serveur stocke et synchronise des données chiffrées sans jamais
recevoir de secrets en clair ni de clés utilisables.

## Façons d'utiliser Sotto

Sotto est un seul produit open source que vous pouvez utiliser de trois façons. Quel que soit votre
choix, le serveur ne stocke jamais que du texte chiffré.

| Mode | Ce que vous obtenez |
| --- | --- |
| Local | La CLI et un coffre chiffré sur votre propre machine. Ni compte ni serveur. |
| Auto-hébergé | Le même serveur Apache-2.0 sur une infrastructure que vous exploitez. Il n'applique ni limites de forfait ni contrôles du service hébergé ; voir [Auto-hébergement](#auto-hébergement). |
| Sotto Cloud | Le serveur que Sotto exploite sur [getsotto.co.uk](https://getsotto.co.uk), si vous préférez ne pas en gérer un. `sotto login` l'utilise par défaut. Les forfaits et les limites figurent sur la [page des tarifs](https://getsotto.co.uk/#pricing), et l'état du service sur [status.getsotto.co.uk](https://status.getsotto.co.uk). |

Vous pouvez commencer en local et ajouter un serveur plus tard : après `sotto login`, `sotto push`
envoie l'environnement actif tel qu'il est.

## État actuel

Le flux de bout en bout fonctionne : chiffrez en local, synchronisez du texte chiffré, déchiffrez sur un autre appareil ou dans le
navigateur, et partagez un secret unique via un lien à usage unique. Les équipes fonctionnent aussi de bout en bout : organisations
avec rôles, grants d'environnement par membre, rotation des clés lors du retrait d'un membre, jetons machine pour la CI,
un journal d'audit de l'organisation et la réinitialisation du compte en cas de perte de l'Emergency Kit.

| Composant | Disponible dès maintenant |
| --- | --- |
| Noyau cryptographique | KDF, AEAD XChaCha20-Poly1305 + AAD, encapsulation de clés, grants sealed-box X25519, la hiérarchie des coffres d'environnement, ré-encapsulation des clés de données (rotation), cryptographie des liens de partage et encodage des clés, avec des vecteurs de référence natif↔WASM, du fuzzing des codecs et des preuves Kani pour l'analyse des en-têtes d'enveloppe et des symboles de clé |
| CLI | Secrets locaux (`init`, `set`, `get`, `ls`, `rm`, `import`, `export`, `run`), environnements (`env ls/use/diff/copy`), un tableau de bord interactif, synchronisation `login`/`push`/`pull`, `setup` pour un nouvel appareil, `history` et `rollback`, liens à usage unique (`share`, `shares ls/revoke`) ; équipes : `org create/ls/invite/members/remove/plan/audit`, `grant`, `clone`, `rotate`, `token create/ls/revoke` machine avec expiration (avec le mode `SOTTO_TOKEN` pour la CI) ; `status`, `cloud status`, `reset` en cas de perte du kit d'urgence, thèmes et complétions du shell |
| Serveur | Connexion OAuth + sessions, synchronisation des comptes + instantanés (écritures versionnées, ETag), organisations + appartenances + rôles, grants de clés de coffre par membre, rotation transactionnelle des clés, jetons machine avec expiration, historique des secrets, un journal d'audit de l'organisation, suppression optionnelle d'organisations avec une période de récupération, réinitialisation de compte et liens à usage unique - uniquement du texte chiffré. Un déploiement Sotto Cloud applique aussi les limites de forfait et peut facturer via Stripe |
| Web | Une page d'accueil et des guides ; le coffre sur `/app`, qui se déverrouille dans le navigateur pour parcourir, rechercher, afficher et copier les secrets, créer des liens à usage unique, partager un environnement avec un membre et faire tourner la clé d'un environnement ; un panneau d'équipe avec les organisations, les membres, les invitations par e-mail, le forfait, le journal d'audit et la suppression d'organisation ; la page de réception des liens à usage unique ; et une page de compte Sotto Cloud sur `/cloud` |

## Installation

Des binaires précompilés et signés pour macOS (Apple Silicon + Intel), Linux (x86_64 + ARM64) et
Windows x86_64 :

```sh
curl -fsSL https://raw.githubusercontent.com/getsotto/sotto/main/install.sh | sh
```

```powershell
irm https://raw.githubusercontent.com/getsotto/sotto/main/install.ps1 | iex
```

L'installateur vérifie la somme de contrôle SHA-256 de l'archive, ainsi que sa signature Sigstore lorsque `cosign`
est installé, avant d'installer (`~/.local/bin` sous macOS/Linux, `%LOCALAPPDATA%\sotto\bin` sous
Windows). Vous préférez vérifier d'abord ? Récupérez une archive sur la
[page des versions](https://github.com/getsotto/sotto/releases) et vérifiez-la manuellement selon
[SECURITY.md](SECURITY.md), ou compilez depuis les sources (voir [Développement](#développement)).

Les installateurs, la formule Homebrew et l'action GitHub installent tous des versions publiées,
alors que ce README suit la branche `main`, qui peut être en avance sur la dernière version. Une
commande décrite ici n'existe donc peut-être pas encore dans la version que vous avez installée ;
`sotto --help` liste les commandes qu'elle prend en charge.

Consultez [COMPATIBILITY.md](COMPATIBILITY.md) pour connaître les versions requises par les
intégrations, les plateformes annoncées et les tests actuellement disponibles.

### Homebrew (aperçu)

Le [tap Sotto](https://github.com/getsotto/homebrew-tap) propose une autre
méthode d'installation pour macOS. La formule distribue des versions de la CLI
qui ont été examinées et requiert macOS 15 ou ultérieur. Apple Silicon est la
plateforme principale ; Intel est pris en charge au mieux et relève du niveau
Tier 3 de Homebrew. Homebrew prévoit de mettre fin à la prise en charge d'Intel
en septembre 2027 ou après. Consultez le [guide d'installation et de
prise en charge du tap](https://github.com/getsotto/homebrew-tap#readme) et le
[registre de préparation au lancement](https://github.com/getsotto/homebrew-tap/blob/main/releases/READINESS.md)
avant de l'utiliser. Évitez cet aperçu pour les secrets de production critiques.

Installez et mettez à jour la formule avec :

```sh
brew install getsotto/tap/sotto
```

```sh
brew update
brew upgrade getsotto/tap/sotto
```

Si une autre installation de `sotto` figure dans votre `PATH`, vérifiez les
exécutables trouvés et le binaire géré par Homebrew avant de choisir celui à
utiliser :

```sh
which -a sotto
brew list --versions sotto
"$(brew --prefix)/bin/sotto" --version
```

Pour supprimer uniquement la formule Homebrew, exécutez :

```sh
brew uninstall getsotto/tap/sotto
```

La désinstallation de la formule conserve les fichiers du projet, les données
locales Sotto, les entrées du trousseau et les autres installations de `sotto`.
Vérifiez le binaire sélectionné et conservez votre Emergency Kit avant de
supprimer ou de remplacer une installation existante.

### Complétions du shell

`sotto completions <shell>` affiche un script de complétion sur la sortie standard. Par exemple :

```sh
# bash
mkdir -p ~/.local/share/bash-completion/completions
sotto completions bash > ~/.local/share/bash-completion/completions/sotto

# zsh (add the directory to fpath in ~/.zshrc first)
mkdir -p ~/.zfunc
sotto completions zsh > ~/.zfunc/_sotto

# fish
mkdir -p ~/.config/fish/completions
sotto completions fish > ~/.config/fish/completions/sotto.fish
```

Les tarballs et zips des releases contiennent également les fichiers correspondants `completions/sotto.*`
si vous préférez les copier directement.

### GitHub Actions

Pour GitHub Actions, utilisez l'[action Sotto Setup](https://github.com/getsotto/sotto-action) pour
installer une version exacte de la CLI et vérifier sa somme de contrôle ainsi que ses bundles Sigstore avant de mettre `sotto`
à disposition des étapes suivantes :

```yaml
jobs:
  ci:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@11d5960a326750d5838078e36cf38b85af677262 # v4.4.0
      - uses: getsotto/sotto-action@v1.1.0
        with:
          sotto-version: v0.9.0
      - run: sotto run -- npm test
        env:
          SOTTO_SERVER: ${{ vars.SOTTO_SERVER }}
          SOTTO_TOKEN: ${{ secrets.SOTTO_TOKEN }}
```

La référence de l'action et `sotto-version` sont indépendantes. Gardez `sotto-version` sur une
version exacte `vX.Y.Z`. Définissez la variable de dépôt `SOTTO_SERVER` sur l'URL de votre serveur,
y compris `https://getsotto.co.uk` pour Sotto Cloud : lorsque `SOTTO_TOKEN` est défini, `sotto run`
et `sotto export` ne se rabattent jamais sur un serveur par défaut, si bien que le jeton ne part que
vers un serveur que vous avez désigné. Voir la
[documentation de l'action](https://github.com/getsotto/sotto-action#readme) pour des exemples de
matrices, Windows et workflows réutilisables.

## Démarrage rapide

Envie d'une démo fonctionnelle ? [**sotto-example**](https://github.com/getsotto/sotto-example)
présente l'injection locale de secrets avec un GIF Python et des étapes copiables, plus de petits exemples
en JavaScript, TypeScript, Java, C#, PHP, Go, Rust et C++. Aucun compte requis ; les instructions couvrent
macOS, Linux et Windows.

```sh
sotto init                   # create your identity + first project - SAVE the printed Emergency Kit
sotto set DATABASE_URL       # hidden prompt; encrypted locally before it ever touches disk
sotto import .env            # optional: pull in an existing file, still encrypted locally
sotto run -- npm start       # inject the environment's secrets into any command
sotto get DATABASE_URL -c    # copy a secret without printing it; clipboard clears after 45s when unchanged
sotto export --format dotenv --reveal   # print a .env; refuses a terminal without --reveal
```

`sotto init` crée les environnements `dev`, `staging` et `prod` et sélectionne `dev`. L'export écrit
en clair ; il nécessite donc `--reveal` dans un terminal, comme `sotto get`, et `--format` accepte
aussi `shell` et `json`. L'effacement du presse-papiers est une mesure de meilleur effort : remplacer
son contenu protège la nouvelle valeur, mais les gestionnaires de presse-papiers, la suspension ou
l'arrêt du processus auxiliaire peuvent conserver une copie.

Jusqu'ici, tout reste sur votre machine. Pour synchroniser un autre appareil, travailler en équipe
ou utiliser la CI, connectez-vous à un serveur :

```sh
sotto login                  # Sotto Cloud by default; --server <url> for your own server
sotto push                   # upload the active environment's ciphertext
sotto share DATABASE_URL     # a one-time link whose key never reaches the server
```

`sotto login` ouvre un navigateur pour vous connecter avec GitHub. Il utilise Sotto Cloud sur
[getsotto.co.uk](https://getsotto.co.uk) sauf si vous passez `--server <url>` (voir
[Auto-hébergement](#auto-hébergement) pour héberger le vôtre), et les commandes suivantes réutilisent
le serveur choisi. Dans tous les cas, le serveur ne stocke que du texte chiffré : le coffre web sur
`/app` du même serveur déchiffre dans votre navigateur, avec des clés qui ne quittent jamais vos
appareils.

Pour parcourir les secrets de façon interactive, consultez le [guide du tableau de bord](#tableau-de-bord-interactif).

## Flux de travail avec des agents

Les agents de programmation locaux peuvent découvrir et exécuter les tâches déclarées par le projet via le serveur MCP local de Sotto. Consultez le [guide des tâches pour agents](AGENT-WORKFLOWS.md) pour la configuration, les approbations et les limites de sécurité.

## Utiliser Sotto

### Un autre appareil

```sh
sotto login                  # same account as the first machine
sotto setup                  # unpack the Emergency Kit onto this device
sotto pull                   # download the ciphertext you already pushed
```

Vous avez besoin de l'Emergency Kit imprimé par `sotto init` ; sans lui, un nouvel appareil ne peut pas déchiffrer le coffre.

Si vous perdez l'Emergency Kit, `sotto reset` donne de nouvelles clés à votre compte. C'est une
réinitialisation, pas une récupération : tout ce qui est chiffré avec les anciennes clés devient
définitivement illisible, et les administrateurs de l'organisation doivent vous accorder à nouveau
les environnements partagés. Si un appareil peut encore déverrouiller le coffre, exportez d'abord ce
dont vous avez besoin.

### Environnements

Utilisez `--env` pour sélectionner un environnement pour une seule commande sans modifier celui par défaut du projet :

```sh
sotto run --env staging -- npm test
sotto ls --env staging
```

`--env` ne s'applique qu'à cette commande ; `sotto env use` modifie l'environnement par défaut.

Listez les environnements, changez celui par défaut, comparez-en deux ou prévisualisez une copie entre eux :

```sh
sotto env ls                          # list environments; the active one is marked
sotto env use staging                 # switch the project's active environment
sotto env diff dev staging            # compare keys; values stay hidden without --reveal
sotto env copy dev staging            # preview additions and updates without writing
sotto env copy dev staging --confirm  # apply the copy; never delete destination keys
```

`env diff` marque chaque clé avec `=` (même valeur), `!` (valeurs différentes), `<` (seulement dans
le premier environnement) ou `>` (seulement dans le second). Sans `--confirm`, la copie est une
simulation. Une copie confirmée ajoute et met à jour les secrets, mais laisse intactes les clés
présentes uniquement dans la destination.

### Importer un fichier dotenv

`sotto import` lit un fichier dotenv et définit chaque affectation dans un environnement, en chiffrant
chaque valeur en local. Il écrit dans l'environnement actif, sauf si vous en nommez un autre avec `--env` :

```sh
sotto import .env                         # into the active environment
sotto import --env staging .env.staging   # into staging; the default environment is unchanged
```

Un import fusionne avec l'existant. Supposons que `staging` contienne déjà `API_URL` et `REGION`,
et que `.env.staging` contienne :

```dotenv
API_URL=https://staging.example.test
FEATURE_FLAG=on
```

Après l'avoir importé dans `staging` :

- `API_URL` prend la valeur du fichier, car un nom existant est mis à jour.
- `FEATURE_FLAG` est ajouté, car un nouveau nom est créé.
- `REGION` est conservé, car un nom absent du fichier n'est jamais supprimé.

L'import n'écrit que dans votre magasin local. Après `sotto login`, envoyez le résultat lors d'une étape distincte :

```sh
sotto push --env staging
```

Le fichier dotenv est seulement lu : l'import ne le modifie pas, ne le consomme pas et ne le supprime pas,
donc protégez-le ou supprimez-le vous-même s'il contient de vraies valeurs. Un fichier qui ne s'analyse pas
ne change rien, mais un échec pendant l'écriture n'annule pas les affectations déjà enregistrées ;
vérifiez donc `sotto ls --env staging` après un import interrompu.

Pour retrouver une valeur antérieure, voir [Restaurer une ancienne version d'un secret](#restaurer-une-ancienne-version-dun-secret).
Cet historique provient du serveur : il ne sert donc que pour des valeurs déjà envoyées. Exécutez
`sotto import --help` pour les règles de guillemets et de valeurs littérales plutôt que de les répéter ici.

### Supprimer un secret

Pour supprimer un secret obsolète de `staging` sans changer l'environnement par défaut :

```sh
sotto --env staging rm DEMO_OBSOLETE_KEY
sotto --env staging push
sotto --env staging pull
```

`sotto rm` supprime le secret de l'environnement sélectionné dans le coffre local.
`sotto push` synchronise la suppression avec le serveur après `sotto login` ;
exécutez `sotto pull` pour le même environnement sur un autre appareil pour
récupérer ce changement. La dernière commande s'exécute sur cet autre appareil.

Dans un terminal interactif, `sotto rm` sans nom ouvre le sélecteur de secrets.
Une confirmation est alors demandée, avec Non par défaut ; annuler ou refuser
laisse le secret inchangé. `--yes` (ou `-y`) évite la confirmation, mais pas
le sélecteur si aucun nom n'est fourni, ni une demande de déverrouillage requise.

Les invites interactives exigent que stdin, stdout et stderr soient des terminaux,
que `TERM` ne soit pas « dumb » et que `CI` ne soit pas actif. Sans interaction,
le nom est obligatoire ; une suppression nommée ne demande pas de confirmation,
même sans `--yes`.

Les secrets supprimés disparaissent des lectures et listes ordinaires, mais leur
historique de versions est conservé. Ce n'est pas un effacement sécurisé et cela
ne révoque pas l'identifiant chez son fournisseur externe. Consultez
`sotto history` et `sotto rollback` pour la restauration.

### Partager un secret

`sotto share` chiffre un secret dans un lien à usage unique. La clé se trouve après le `#` du lien,
une partie que les navigateurs n'envoient jamais à un serveur : le serveur ne détient donc que du
texte chiffré. Créer un lien nécessite `sotto login` ; le destinataire n'a besoin d'aucun compte.
Ouvrir le lien ne consomme pas de consultation ; révéler le secret, si.

```sh
sotto share DATABASE_URL                           # choose view and lifetime limits in an interactive terminal
sotto share DATABASE_URL --views 3 --expire 3600   # skip both limit prompts
sotto share DATABASE_URL --passphrase              # also require a passphrase; send it separately
```

Dans un terminal interactif, chaque limite omise ouvre son propre menu. Le menu des consultations
propose 1, 2, 3, 5 ou 10 consultations ainsi qu'une valeur personnalisée ; le menu de durée propose
aucune expiration, 1 heure, 1 jour, 7 jours, 30 jours ou un nombre de secondes personnalisé. Les
valeurs personnalisées doivent être comprises entre 1 et 100 consultations ou entre 1 et 2592000
secondes. Fournir `DATABASE_URL` ignore uniquement la sélection du secret : `--views` ignore le
menu des consultations et `--expire` ignore indépendamment le menu de durée. Appuyez sur Échap ou
Ctrl-C dans un menu de sélection pour annuler sans créer de partage.

L'utilisation non interactive exige un nom de secret explicite. Lorsqu'une limite est omise, sa
valeur par défaut s'applique : une consultation et aucune expiration. Pour ignorer les trois menus
de sélection dans un terminal interactif, fournissez le nom et les deux limites, par exemple
`sotto share DATABASE_URL --views 3 --expire 3600`. Les invites de déverrouillage et de phrase
secrète peuvent toujours s'appliquer.

Un serveur peut appliquer des limites plus strictes à certains comptes. Lorsqu'il refuse une demande,
la CLI affiche la raison donnée par le serveur et ne crée aucun lien.

Utilisez `sotto share --no-copy` pour désactiver la copie interactive, ou `--copy` pour la demander explicitement.
Ces options contrôlent uniquement le presse-papiers ; `--no-copy` n'ignore pas les menus de limites du partage.

Un lien cesse de fonctionner lorsque ses consultations sont épuisées, lorsqu'il expire ou lorsque
vous le révoquez. La révocation empêche le serveur de continuer à fournir le texte chiffré, mais ne
l'efface pas du serveur :

```sh
sotto shares ls              # your 100 most recent links: views used, expiry, revocation
sotto shares ls --json
sotto shares revoke <token>  # the token is the part of the link between /s/ and #
```

`sotto shares ls` n'affiche jamais le texte chiffré ni la clé du lien.

Attention au pluriel : `sotto share` crée un lien, et `sotto shares` gère les liens que vous avez
créés. Les deux dialoguent avec le serveur configuré et nécessitent donc `sotto login` ; le coffre
local n'a pas besoin d'être déverrouillé. Une liste ressemble à ceci (jeton fictif, valeurs
d'exemple) :

```text
active free links: 1
3f9a1c0e7b2d4a6f8e1c3b5d7a9f0e2c  class=free  views=1/3  expires 2026-11-01 12:00:00+00
```

Passez la première colonne à `sotto shares revoke`. C'est le jeton public, pas le lien complet :
pour `https://getsotto.co.uk/s/3f9a1c0e7b2d4a6f8e1c3b5d7a9f0e2c#...`, passez seulement
`3f9a1c0e7b2d4a6f8e1c3b5d7a9f0e2c` et jamais ce qui suit le `#`. La liste ne lit que des métadonnées. Elle
n'ouvre aucun lien, ne consomme aucune vue et ne peut pas reconstituer un lien, car la clé située
après le `#` n'a jamais atteint le serveur.

`sotto shares ls --json` affiche un objet et non un simple tableau : `active_free_count` et `links`,
où chaque lien contient `token`, `share_class`, `max_views`, `view_count`, `expires_at`,
`revoked_at` et `created_at`. Le serveur ne renvoie que vos 100 liens les plus récents : ce n'est
donc pas un historique complet.

La révocation empêche les récupérations futures via le lien. Elle ne peut pas annuler une
récupération déjà effectuée : si le destinataire a pu révéler le secret, changez-le à la source.

### Travailler en équipe

Les équipes partagent des environnements au sein d'une organisation. Chaque membre reçoit sa propre
copie scellée de la clé d'un environnement (un grant), et le serveur vérifie les rôles et les grants
à chaque requête sans pouvoir rien lire.

```sh
sotto org create acme                      # prints the org id
sotto init --org <org-id>                  # an org-owned project
sotto push                                 # create it on the server before sharing
sotto org invite <org-id> dev@example.com  # invite an existing Sotto user; prints their user id
sotto grant <user-id>                      # share the active environment; prints their clone command
```

Le membre exécute ensuite la commande affichée. Ce n'est pas la même chose que **Un autre appareil**, qui restaure votre propre compte sur une
deuxième machine. Le membre utilise son propre compte sur le même serveur : il exécute
`sotto login` et `sotto unlock`, puis la commande affichée dans un répertoire qui n'a pas encore de
`sotto.toml` :

```sh
mkdir acme-api && cd acme-api
sotto clone <project-id> <env-id> --org <org-id>
```

L'appartenance à l'organisation et l'accès à un environnement sont deux étapes distinctes.
`sotto org invite` fait de quelqu'un un membre ; `sotto grant <user-id>` lui donne un environnement.
Si vous n'avez plus l'identifiant utilisateur, `sotto org members <org-id>` l'affiche.
`sotto clone` n'accorde rien par lui-même : il récupère le grant que le membre possède déjà,
télécharge cet environnement et écrit `sotto.toml`. Sans grant, il s'arrête avec
`you have not been granted this environment (ask an admin to share it)`, et un administrateur doit
exécuter `sotto grant` pour ce membre. Ne donnez jamais votre Emergency Kit à un coéquipier ; il
déverrouille votre compte, pas le sien.

Gardez `--org` de la commande affichée : il enregistre l'organisation propriétaire pour que les push
suivants correspondent au serveur. `--as <environment-name>` et `--name <project-label>` ne
définissent que des libellés locaux : par défaut, l'environnement garde son vrai nom et le projet
s'appelle `shared`.

Pour voir qui a accès, retirer quelqu'un ou faire tourner vous-même la clé d'un environnement :

```sh
sotto org members <org-id>                 # members, roles and user ids
sotto org remove <org-id> <user-id>        # remove a member and rotate every environment they held
sotto rotate                               # rotate the active environment's key
```

Retirer un membre fait tourner la clé de chaque environnement auquel il avait accès et révoque les
tokens machine qu'il a créés dans l'organisation, en les nommant pour que vous puissiez les recréer.
La rotation protège ce qui est écrit ensuite. Les valeurs que le membre a déjà vues restent exposées :
changez-les à leur source.

#### Anciens forfaits d'organisation

`sotto org plan <org-id>` lit la vue d'entitlements actuelle du serveur configuré pour cette
organisation. Vous devez être connecté (`sotto login`) ; le coffre local n'a pas besoin d'être
déverrouillé. Il affiche le niveau assigné, le niveau effectif, la fin d'essai enregistrée et les
limites applicables, comme `limits: none (Team)`. Ces champs préservent la compatibilité avec les anciens enregistrements et
déploiements à forfait d'organisation ; ils ne décrivent ni les tarifs Cloud actuels par personne ni
une nouvelle offre d'essai Cloud.

Sortie illustrative d'un ancien enregistrement de forfait d'organisation :

```text
tier:      free
effective: free
trial:     ends 2026-07-16T09:00:00Z
limits:    3 members, 1 org project(s)
```

Un serveur auto-hébergé n'applique aucune limite de forfait : il indique donc toujours `team` comme
niveau effectif, quels que soient le niveau et la fin d'essai enregistrés pour l'organisation :

```text
tier:      free
effective: team
trial:     ends 2026-07-16T09:00:00Z
limits:    none (Team)
```

Ces exemples montrent des champs de compatibilité enregistrés, pas les offres Cloud actuelles. Les
déploiements réels peuvent présenter d'autres niveaux, dates d'essai et limites enregistrés.

#### Journal d’audit de l’organisation

Les propriétaires et administrateurs d’une organisation peuvent consulter l’activité récente de
l’équipe avec :

```sh
sotto org audit <org-id>
sotto org audit <org-id> --limit 20
```

Utilisez l’identifiant affiché par `sotto org create <name>` ou listé par `sotto org ls`. Les résultats
sont classés du plus récent au plus ancien ; la valeur par défaut est de 100 événements et le serveur
ramène `--limit` à une valeur comprise entre 1 et 500. La commande affiche une ligne par événement
au format `timestamp  action  actor`, avec éventuellement les champs `target=...`, `env=...` et
`(detail)`. Il s’agit d’une vue limitée de l’activité et des métadonnées, pas d’un moyen de récupérer
des valeurs secrètes ; elle ne fournit ni pagination ni archive historique complète.

Cette commande conserve le contrôle d'accès historique par forfait Team d'organisation. Les
déploiements auto-hébergés ignorent ce contrôle de forfait, mais l’appelant doit toujours avoir le
rôle de propriétaire ou d’administrateur. S’il n’y a aucun événement, la commande n’affiche aucune
ligne et se termine avec succès.

### Tokens machine et CI

Un token machine permet à la CI ou à un service de lire un environnement sans mot de passe ni
Emergency Kit. Lorsque `SOTTO_TOKEN` est défini, `sotto run` et `sotto export` récupèrent et
déchiffrent cet environnement à chaque exécution ; définissez aussi `SOTTO_SERVER`, comme le montre
l'exemple [GitHub Actions](#github-actions).

Les tokens machine expirent. Un nouveau token dure 90 jours, sauf si vous passez
`--expires-in-days` avec une valeur de 1 à 365 ; `sotto token ls` indique quand chacun expire.
Deux semaines avant, `sotto run` et `sotto export` affichent un avertissement dans le journal de la CI.

Les commandes de gestion des tokens utilisent l'accès de l'utilisateur connecté et l'environnement
sélectionné. Dans un environnement d'organisation, seuls un administrateur ou un propriétaire
peut gérer les tokens machine. La liste contient uniquement les métadonnées des tokens actifs :
les tokens révoqués et expirés sont omis, et elle ne permet pas de retrouver l'identifiant secret
affiché une seule fois lors de la création avec `sotto token create`. Le JSON contient `token_id`,
`name`, `created_by`, `beneficiary_id`, `beneficiary_status`, `expires_at` et
`expires_in_days` ; les champs d'expiration peuvent être null. Le `token_id` est l'ID à révoquer,
pas le nom du token ni le secret `SOTTO_TOKEN`.

Utilisez ces commandes pour consulter, remplacer et révoquer un token. Le choix d'environnement
ne s'applique qu'à cette commande :

```sh
sotto token ls
sotto token ls --json
sotto --env staging token ls --json
sotto token create --name ci
sotto token revoke <token-id>
```

Pour remplacer un token CI, identifiez l'ancien ID dans la liste, créez un remplacement, mettez
à jour le secret CI et vérifiez que la CI l'utilise, puis révoquez l'ancien ID avec
`sotto token revoke <token-id>`. La révocation empêche immédiatement ce token de s'authentifier
auprès de l'API, mais ne renouvelle pas la clé d'environnement. Pour un environnement
d'organisation, suivez les instructions existantes de `sotto rotate` si la rotation de la clé
est nécessaire. Ni la révocation ni la rotation ne peuvent effacer du texte brut déjà copié.

### Tableau de bord interactif

Depuis un projet initialisé, lancez `sotto` sans sous-commande pour parcourir ses secrets locaux :

```sh
sotto
```

Vous devez disposer d'une identité locale déverrouillée avant d'ouvrir le tableau de bord. Si
nécessaire, Sotto demande le mot de passe principal. L'entrée, la sortie et la sortie d'erreur standard
doivent toutes être reliées à des terminaux. Si l'une manque, si la CI est active ou si
`TERM=dumb`, l'appel sans sous-commande écrit l'aide sur la sortie d'erreur standard et se termine
avec le code 2. Utilisez `sotto --help` pour demander l'aide explicitement.

Utilisez les flèches ou `j` et `k` pour naviguer. Appuyez sur `/` pour rechercher, `?` pour afficher
l'aide et Tab pour changer d'environnement. Ce changement ne dure que pendant la session du
tableau de bord ; `sotto env use` modifie l'environnement par défaut du projet. Appuyez sur `r`
pour afficher ou masquer la valeur sélectionnée, `c` pour la copier dans le presse-papiers et `q`
pour quitter. `Esc` ferme la recherche ou la boîte de dialogue en cours ; dans la vue principale,
il efface une recherche active ou quitte.

Appuyez sur `t` pour parcourir les thèmes avec un aperçu en direct. Utilisez les flèches pour
prévisualiser chaque thème, puis appuyez sur `Enter` pour enregistrer votre choix ou sur `Esc` pour
annuler. Les [règles de priorité des thèmes](#thèmes-de-sortie) expliquent comment les options de
commande et les variables d'environnement influent sur la préférence enregistrée.

### Consulter l'état local

Exécutez `sotto status` pour savoir si une identité locale existe, si la session est déverrouillée
et quels projet et environnement le répertoire courant sélectionne. Cette commande lit l'état local.
Elle ne vérifie pas la connexion au serveur ni la possibilité de déchiffrer chaque secret stocké.

```sh
sotto status
sotto status --json
```

Utilisez `sotto status --json` dans les scripts. `initialized` indique si une identité locale existe,
et `unlocked` si une clé maîtresse est en cache. `project` contient `name` et `environment` issus du
`sotto.toml` le plus proche, ou vaut `null` hors d'un projet configuré. La commande peut réussir avec
`unlocked: false` ; les scripts qui exigent une session déverrouillée doivent lire ce champ.

Ces exemples JSON sont fictifs. Avec un `sotto.toml` mais sans identité locale :

```json
{"initialized":false,"project":{"environment":"dev","name":"demo"},"unlocked":false}
```

Sans `sotto.toml` ni identité locale :

```json
{"initialized":false,"project":null,"unlocked":false}
```

L'absence de `sotto.toml` signifie qu'aucun projet n'est sélectionné. Un fichier invalide ou illisible provoque une erreur.

### Verrouiller, déverrouiller et se déconnecter

Pour une identité locale déjà initialisée, verrouillez le coffre, vérifiez son
état, puis déverrouillez-le lorsque vous en avez de nouveau besoin :

```sh
sotto lock
sotto status
sotto unlock
```

`sotto lock` efface la session locale de clé maîtresse en cache utilisée par les
commandes CLI suivantes. Cette commande ne supprime ni votre identité, ni le coffre
chiffré, ni la connexion enregistrée au serveur de synchronisation. `sotto unlock`
utilise votre mot de passe maître lorsqu'aucune session valide n'est en cache. Les
commandes nécessitant un déchiffrement local demandent aussi le déverrouillage si
nécessaire. Consultez [Consulter l'état local](#consulter-létat-local) pour les champs d'état.

La session locale dure 12 heures à partir de la mise en cache de la clé, et non de
votre dernière activité. L'expiration est vérifiée lors de la lecture de la clé en
cache. Exécuter `sotto unlock` alors que le coffre est déjà déverrouillé ne prolonge
pas cette durée. Ni le verrouillage ni l'expiration n'effacent le texte en clair
déjà détenu par les applications en cours d'exécution.

Pour effacer aussi la connexion de la CLI au serveur de synchronisation,
exécutez les deux commandes :

```sh
sotto lock
sotto logout
```

`sotto logout` supprime le jeton de session du serveur enregistré par la CLI,
indépendamment du cache de déverrouillage local. Cette commande ne déconnecte pas
les sessions du navigateur ni les autres appareils.

### État Cloud

Pour consulter le serveur configuré et l'éligibilité Cloud de votre compte, connectez-vous d'abord :

```sh
sotto login
sotto cloud status
sotto cloud status --json
```

La commande nécessite une session active, mais ne déverrouille pas le coffre local et n'ouvre pas de navigateur. Elle affiche le `server` configuré, le `server_info` du serveur, l'`eligibility` du compte et un `billing_url` uniquement lorsque l'état renvoyé indique que la facturation est disponible. La sortie lisible affiche également `actions`, `next_actions` et les valeurs facultatives `paid_through_epoch`, `recovery_until_epoch` ou `export_until_epoch`. L'absence de `server_info` ou d'`eligibility` signifie qu'un serveur ancien n'expose pas cet endpoint ; elle ne prouve pas que l'accès est autorisé ou refusé.

Le format JSON est pratique pour les scripts. Sa structure de premier niveau est :

```json
{
  "server": "https://getsotto.co.uk",
  "billing_url": "https://getsotto.co.uk/cloud",
  "server_info": {
    "deployment_mode": "cloud",
    "entitlement_model": "organisation_tiers_v1"
  },
  "eligibility": {
    "model": "person_eligibility_v1",
    "state": "paid",
    "deployment_mode": "cloud",
    "account_initialized": true,
    "billing_available": true,
    "paid_through_epoch": 1798761600,
    "recovery_until_epoch": null,
    "export_until_epoch": null,
    "actions": {
      "setup": false,
      "billing": false,
      "export": false,
      "revoke": true
    },
    "next_actions": ["revoke"],
    "payer": "personal"
  }
}
```

L'exemple est synthétique ; `actions` et les dates facultatives proviennent de l'état renvoyé du compte, et les serveurs anciens peuvent renvoyer null pour l'un ou l'autre endpoint. `sotto cloud status` ne fait que signaler l'état et peut afficher une URL de facturation. Il ne lance pas d'achat et ne modifie pas l'éligibilité. Pour diagnostiquer le coffre local et les environnements, utilisez `sotto status` ; cette vue est distincte des informations du serveur et du compte ci-dessus.

### Thèmes de sortie

La CLI fournit cinq thèmes intégrés : `nord` (par défaut), `sordino`, `terminal`, `monochrome`
et `tokyo-night`. Gérez-les avec `sotto theme` :

```sh
sotto theme ls        # list available themes; the active one is marked
sotto theme set nord  # save a preference for later commands
sotto theme current   # print the theme this shell resolves to
```

`--theme <name>` choisit un thème pour une seule commande. `SOTTO_THEME` le définit pour un
shell et `sotto theme set` l'enregistre. La priorité est `--theme`, puis `SOTTO_THEME`, puis la
préférence enregistrée, puis `nord` ; un nom inconnu avertit sur la sortie d'erreur standard et
revient à `nord`.

Le style ne s'applique qu'aux terminaux interactifs. `--plain`, un `NO_COLOR` non vide (le
contrat [no-color.org](https://no-color.org)), les environnements de CI, la sortie standard
redirigée et l'entrée standard redirigée suppriment chacun la couleur et la décoration tout en
conservant la palette choisie, donc les scripts et les journaux restent en texte brut.

Les thèmes personnalisés sont des fichiers TOML dans le répertoire `themes` du répertoire de
données de votre plateforme : `~/Library/Application Support/sotto` sous macOS, `%APPDATA%\sotto`
sous Windows et `$XDG_DATA_HOME/sotto` ou `~/.local/share/sotto` sous Linux (`SOTTO_DATA_DIR`
déplace ce répertoire). Chaque fichier `*.toml` a besoin des jetons ci-dessous et tire son nom
du champ `name`, ou du nom du fichier quand ce champ est absent ; un fichier qui ne peut pas
être analysé est ignoré. Les couleurs acceptent des valeurs hexadécimales, des noms de couleurs
ANSI, des indices `ansi(<index>)` et `default`.

```toml
bg = "#120024"
fg = "#ffffff"
accent = "#ff007f"
success = "#00ff66"
warning = "#ffaa00"
error = "#ff0033"
muted = "#775588"
border = "#331144"
```

Enregistré sous `synth.toml`, ce fichier ajoute un thème `synth`. Appliquez-le avec
`sotto theme set synth`, ou pour une seule commande avec `sotto --theme synth <command>`.

### Restaurer une ancienne version d'un secret

Partez d'un projet déjà configuré et gardez le même environnement sélectionné pendant toute la procédure.
Vous devez être connecté et le stockage local doit être déverrouillé. L'historique vient du serveur :
les valeurs qui n'ont jamais été envoyées n'y sont pas disponibles pour une restauration.

```sh
sotto history DATABASE_URL
```

Choisissez dans l'historique un numéro de version positif et lisible. L'exemple ci-dessous utilise `2` ;
remplacez-le par une version disponible, sans le préfixe `v` affiché.

```sh
sotto rollback DATABASE_URL 2
sotto push
```

Rollback restaure la valeur choisie sous forme d'une nouvelle version locale. Il n'efface
ni ne réécrit l'ancien historique, et le serveur n'est mis à jour qu'au prochain envoi.

Pour obtenir du JSON contenant uniquement les métadonnées :

```sh
sotto history DATABASE_URL --json
```

Chaque ligne contient `version`, `readable` (si la valeur a pu être déchiffrée) et `size_bytes`
(le nombre d'octets en clair, ou null pour une version illisible). Aucune valeur n'est affichée.
À l'inverse, `--reveal` affiche les valeurs secrètes et ne peut pas être combiné avec `--json`.

En l'absence d'historique sur le serveur, vérifiez l'environnement sélectionné et s'il a été envoyé.
Un envoi maintenant ne récupère pas les anciennes valeurs jamais synchronisées. Pour une version
illisible, suivez la suggestion de la CLI : lancez `sotto pull` et réessayez ; cette opération ne
garantit pas que les clés disponibles permettent de déchiffrer la version.

Pour les scripts shell et la CI, consultez la [référence des scripts et des codes de sortie de la CLI](CLI-SCRIPTING.md).

### Variables d'environnement

La CLI lit ces variables :

- `SOTTO_SERVER` : l'URL du serveur, qui remplace celle enregistrée par `sotto login`. Avec
  `SOTTO_TOKEN`, elle est obligatoire sauf si `sotto login` a enregistré un serveur sur cette machine.
- `SOTTO_TOKEN` : un token machine. `sotto run` et `sotto export` lisent alors l'environnement du
  token au lieu du coffre local.
- `SOTTO_PASSWORD` : le mot de passe principal, pour les scripts qui ne peuvent pas répondre à une
  invite. Préférez l'invite masquée lorsque c'est possible.
- `SOTTO_SECRET_KEY` : la Secret Key de votre Emergency Kit, pour lancer `sotto setup` sans invite.
- `SOTTO_DATA_DIR` : déplace le répertoire de données local, qui contient le magasin chiffré, les
  réglages enregistrés (comme le serveur) et les thèmes personnalisés.
- `SOTTO_THEME` et `NO_COLOR` : le style de la sortie ; voir [Thèmes de sortie](#thèmes-de-sortie).

## Auto-hébergement

Une seule commande démarre un déploiement complet : Postgres, le serveur et Caddy avec HTTPS
automatique, depuis [`deploy/docker-compose.prod.yml`](deploy/docker-compose.prod.yml) ; le guide est dans
[`deploy/README.md`](deploy/README.md). Les éléments fonctionnent aussi séparément : servez l'application web et l'API
depuis **une même origine** (pour que le cookie de session et la CSP restent sur la même origine) ; le
[`Caddyfile`](Caddyfile) inclus sert `web/dist` et relaie l'API en proxy inverse, avec des en-têtes de sécurité ;
le [`Dockerfile`](Dockerfile) construit l'image du serveur (les migrations s'exécutent au démarrage).

`SOTTO_DEPLOYMENT_MODE` vaut `self_hosted` par défaut. Dans ce mode, le serveur n'applique ni
limites de forfait ni autres restrictions du service hébergé, et il n'a besoin d'aucun compte Stripe.
Le mode `cloud` est destiné au service exploité par Sotto ; lisez le guide avant de l'activer.

Faites pointer la CLI vers votre serveur une fois ; les commandes suivantes le réutilisent :

```sh
sotto login --server https://sotto.example.com
```

## Architecture

```text
CLI (native) ─────┐
                  ├── sotto-core ── versioned encrypted data
Web client (WASM) ┘                         │
                                            ▼
                                  sync/API server
                                  (ciphertext only)
```

L'espace de travail contient quatre crates :

- `crates/core` : les types cryptographiques partagés et l'implémentation cryptographique complète.
- `crates/cli` : l'interface en ligne de commande `sotto` et le principal client natif.
- `crates/server` : l'API de synchronisation basée sur Axum.
- `crates/wasm` : les liaisons `wasm-bindgen` qui exposent le noyau aux clients web.

Le client navigateur dans `web/` est une application React et Vite qui exécute le noyau via `crates/wasm`.

## Prérequis

- [Rustup](https://rustup.rs/) avec Rust stable 1.89 ou plus récent
- Les composants `clippy` et `rustfmt`
- La cible `wasm32-unknown-unknown`
- Pour le client web : Node.js 22 (la version de la CI) et `wasm-pack` (la CI épingle la v0.15.0)
- Pour le serveur et ses tests avec base de données : Postgres 16, que fournit `docker compose up -d`
- Pour les vérifications des scripts : Python 3.11 ou plus récent

Le fichier `rust-toolchain.toml` inclus demande à Rustup d'installer automatiquement les composants et la
cible requis.

## Développement

Clonez le dépôt, puis compilez et testez l'espace de travail :

```sh
git clone https://github.com/getsotto/sotto.git
cd sotto

cargo build --workspace
cargo test --workspace
```

`cargo test --workspace` n'a besoin d'aucune base de données : il ignore donc, sans échouer, les
tests d'intégration du serveur et le test de bout en bout de la CLI. Pour les exécuter, démarrez le
Postgres local et activez-les avec `SOTTO_RUN_DB_TESTS=1` ; ils refusent un `DATABASE_URL` qui
pointe ailleurs que sur cette machine :

```sh
docker compose up -d
DATABASE_URL=postgres://sotto:sotto@localhost:5432/sotto SOTTO_RUN_DB_TESTS=1 \
  cargo test -p sotto-server -p sotto-cli
```

Utilisez la CLI en local (sans serveur) :

```sh
cargo run -p sotto-cli -- --help
cargo run -p sotto-cli -- init                 # create an identity + project; prints your Emergency Kit
cargo run -p sotto-cli -- set DATABASE_URL     # hidden prompt
cargo run -p sotto-cli -- run -- your-command  # inject secrets as env vars into a subprocess
```

Les secrets sont chiffrés au repos dans un magasin SQLite local ; la clé maîtresse est mise en cache dans le trousseau du système
avec un TTL. La synchronisation vers un serveur (`login`/`push`/`pull`/`setup`/`share`) est optionnelle.

### Exécution du serveur

Le serveur a besoin de Postgres (un `docker compose up -d` en lance un pour un usage local) :

```sh
DATABASE_URL=postgres://sotto:sotto@localhost:5432/sotto cargo run -p sotto-server
curl http://127.0.0.1:8080/health   # → ok
```

La connexion GitHub OAuth requiert `GITHUB_CLIENT_ID` / `GITHUB_CLIENT_SECRET`. Pour tout déploiement non local, définissez
aussi `SOTTO_PUBLIC_URL` sur l'origine publique du serveur, celle qui est joignable depuis l'extérieur (elle construit l'URL
de callback GitHub et doit correspondre au callback enregistré dans l'application OAuth, à défaut `http://localhost:8080`).
Définissez également `SOTTO_WEB_ORIGIN` pour le client web. Sans OAuth, le serveur démarre quand même (il sert `/health` et exécute les
migrations), mais la connexion et tous les points de terminaison authentifiés (synchronisation, création de partages) sont
indisponibles.

### Client web

Le client navigateur exécute le même noyau cryptographique via WebAssembly (`web/`) :

```sh
cd web
npm ci
npm run dev        # dev server (proxies the API to localhost:8080)
npm run build      # production bundle → web/dist (strict CSP + Subresource Integrity)
npm run typecheck  # TypeScript checks; needs the WASM bindings a build generates
npm test           # unit tests (Vitest)
npm run smoke      # load the WASM core under Node and exercise it
```

La suite Playwright du tunnel (`npm run e2e`) nécessite une base de données et un build de
production préalable ; [`web/e2e/README.md`](web/e2e/README.md) explique la mise en place.

## Vérifications de développement

Exécutez les mêmes vérifications de base que la CI :

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

La politique de chaîne d'approvisionnement est définie dans `deny.toml` et vérifiée en CI avec
[`cargo-deny`](https://github.com/EmbarkStudios/cargo-deny) :

```sh
cargo deny check
```

Le job `supply-chain` de la CI audite tout le lockfile avec `scripts/check-cargo-audit`. Le
[guide d'audit du lockfile](.ci/cargo-audit.md) décrit la version épinglée de cargo-audit, les tests
de la politique et les règles des exceptions dormantes du lockfile.

Le contrôle inter-implémentations prouve que les compilations native et WASM concordent : le texte chiffré produit en natif
se déchiffre à l'octet près en WASM à partir de vecteurs de référence partagés :

```sh
wasm-pack test --node crates/wasm
```

Le build web, son audit de dépendances et la vérification des traductions du README
(`scripts/check-readme-i18n`) s'exécutent aussi en CI (`.github/workflows/ci.yml`).
[CONTRIBUTING.md](CONTRIBUTING.md) renvoie aux guides du fuzzing, des preuves Kani, de la couverture
et des contrôles d'assurance.

## Télémétrie

Le **serveur** envoie un ping anonyme par jour (dans les 10 à 20 premières minutes après le démarrage) à
`https://getsotto.co.uk/telemetry/v1/ping`, afin de compter les instances actives et de voir quelles
versions circulent. La réponse nomme la dernière version, et le serveur journalise une ligne lorsqu'il exécute
une version dépassée. C'est la charge utile **entière** : le code d'envoi est dans
[`crates/server/src/telemetry.rs`](crates/server/src/telemetry.rs), et un test unitaire fige la charge utile à
exactement ces quatre champs :

```json
{ "instance_id": "0d0972a6-…", "version": "0.2.0", "os": "linux", "arch": "x86_64" }
```

`instance_id` est un UUID aléatoire généré une fois et stocké dans votre base de données, dérivé de rien,
donc il n'identifie ni matériel, ni hôte, ni compte ; le supprimer fait de l'instance un compteur anonyme
tout neuf. Côté ingestion, aucune adresse IP ni localisation dérivée n'est stockée. Il n'y a pas non plus de décomptes d'organisations,
de membres ou de secrets, ni d'événements d'usage. La **CLI, le client web et WASM n'envoient jamais rien**.

Désactivez avec `SOTTO_TELEMETRY=off` (ou la variable inter-outils
[`DO_NOT_TRACK=1`](https://consoledonottrack.com)) : désactivée, la tâche n'est jamais démarrée et aucune
requête n'est effectuée. `SOTTO_TELEMETRY_URL` redirige le ping (par exemple pour agréger une flotte privée),
et les enregistrements inactifs depuis 12 mois sont purgés du recensement hébergé.

## Sécurité

Le modèle de Sotto est zero-knowledge : les secrets en clair et les clés de déchiffrement utilisables restent sur les
appareils clients, et le serveur ne voit que du texte chiffré plus des métadonnées minimales. C'est implémenté mais
**pas encore audité de façon indépendante** : voir [SECURITY.md](SECURITY.md) pour le modèle, l'exposition honnête
des métadonnées, la manière dont la surface web (retéléchargée à chaque visite, donc plus faible) est durcie, et comment vérifier
les versions signées. Le modèle d'adversaire complet, les garanties et les non-objectifs explicites sont publiés dans
[THREAT-MODEL.md](THREAT-MODEL.md). Signalez les vulnérabilités en privé selon SECURITY.md.

Le périmètre et les éléments préparés pour un audit indépendant se trouvent dans [SECURITY-AUDIT-SCOPE.md](SECURITY-AUDIT-SCOPE.md).

## Contribution

Sotto est Apache-2.0 et accueille les contributions. Commencez par [CONTRIBUTING.md](CONTRIBUTING.md) ;
les [good first issues](https://github.com/getsotto/sotto/issues?q=is%3Aissue+is%3Aopen+label%3A%22good+first+issue%22)
sont étiquetées, et les questions qui ne sont pas des bogues vont dans
[Discussions](https://github.com/getsotto/sotto/discussions).

Signalez les vulnérabilités en privé selon [SECURITY.md](SECURITY.md).

## Licence

Publié sous la [licence Apache, version 2.0](LICENSE) : tous les crates et le client web. Vous ne pouvez utiliser
ce projet qu'en conformité avec la licence. Sauf exigence de la loi applicable ou accord écrit, le logiciel distribué
sous la licence est distribué « EN L'ÉTAT », SANS GARANTIES NI CONDITIONS D'AUCUNE SORTE.
