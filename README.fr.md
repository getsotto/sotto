[English](README.md) | [Español](README.es.md) | [Français](README.fr.md) | [Deutsch](README.de.md) | [Português (BR)](README.pt-BR.md)

*Traduction française de [README.md](README.md). Le texte original en anglais est la version officielle et prévaut en cas de divergence.*

# Sotto

La synchronisation de secrets chiffrés de bout en bout pour les équipes de développement. Arrêtez d'envoyer votre `.env` sur Slack.

> [!WARNING]
> Sotto est pré-1.0 et n'a **pas** fait l'objet d'un audit cryptographique par un tiers. Il fonctionne de bout en bout, mais
> vous ne devriez pas encore lui confier de secrets critiques de production. Voir [SECURITY.md](SECURITY.md).

Sotto repose sur une seule implémentation cryptographique Rust partagée par la CLI native et le client
navigateur via WebAssembly. Le serveur stocke et synchronise des données chiffrées sans jamais
recevoir de secrets en clair ni de clés utilisables.

## État actuel

Le flux de bout en bout fonctionne : chiffrez en local, synchronisez du texte chiffré, déchiffrez sur un autre appareil ou dans le
navigateur, et partagez un secret unique via un lien à usage unique. Les équipes fonctionnent aussi de bout en bout : organisations
avec rôles, grants d'environnement par membre, rotation des clés lors du retrait d'un membre, jetons machine pour la CI
et récupération de compte en cas de perte de clé.

| Composant | Disponible dès maintenant |
| --- | --- |
| Noyau cryptographique | KDF, AEAD XChaCha20-Poly1305 + AAD, encapsulation de clés, grants sealed-box X25519, la hiérarchie des coffres d'environnement, ré-encapsulation des clés de données (rotation), cryptographie des liens de partage et encodage des clés, avec des vecteurs de référence natif↔WASM |
| CLI | `init`, gestion locale des secrets, injection via `run`, synchronisation `login`/`push`/`pull`, `setup` pour un nouvel appareil, `share` ; équipes : `org create/ls/invite/members/remove`, `grant`, `clone`, `rotate`, `token create/ls/revoke` machine (avec le mode `SOTTO_TOKEN` pour la CI), `reset` en cas de perte du kit d'urgence |
| Serveur | Connexion OAuth + sessions, synchronisation des comptes + instantanés (écritures versionnées, ETag), organisations + appartenances + rôles, grants de clés de coffre par membre, rotation transactionnelle des clés, jetons machine, réinitialisation de compte et liens de partage - uniquement du texte chiffré |
| Web | Connexion (session cookie), déverrouillage dans le navigateur + déchiffrement du coffre via votre propre grant, création et réception de partages à usage unique, et un panneau d'équipe : organisations, membres, invitation par e-mail, partage d'un environnement avec un membre |

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
      - uses: getsotto/sotto-action@543d1af56ac81d1f1511d88c3d269106e8513a28 # merged v1.1 implementation
        with:
          sotto-version: v0.4.0
      - run: sotto run -- npm test
        env:
          SOTTO_SERVER: ${{ vars.SOTTO_SERVER }}
          SOTTO_TOKEN: ${{ secrets.SOTTO_TOKEN }}
```

La référence de l'action et `sotto-version` sont indépendantes. L'exemple épingle, par le SHA complet du commit,
l'implémentation v1.1 fusionnée car aucune version numérotée de l'action n'a encore été publiée. Gardez `sotto-version` sur
une version exacte `vX.Y.Z`. Définissez la variable de dépôt optionnelle `SOTTO_SERVER` pour un serveur auto-hébergé. Voir la
[documentation de l'action](https://github.com/getsotto/sotto-action#readme) pour des exemples de matrices, Windows et
workflows réutilisables.

## Démarrage rapide

Envie d'une démo fonctionnelle ? [**sotto-example**](https://github.com/getsotto/sotto-example)
présente l'injection locale de secrets avec un GIF Python et des étapes copiables, plus de petits exemples
en JavaScript, TypeScript, Java, C#, PHP, Go et C++. Aucun compte requis ; les instructions couvrent
macOS, Linux et Windows.

```sh
sotto init                   # create your identity + first project - SAVE the printed Emergency Kit
sotto set DATABASE_URL       # hidden prompt; encrypted locally before it ever touches disk
sotto import .env            # optional: pull in an existing file, still encrypted locally
sotto run -- npm start       # inject the environment's secrets into any command
sotto login && sotto push    # optional: sync ciphertext via the hosted instance (getsotto.co.uk)
sotto share DATABASE_URL     # one-time, burn-after-reading link for a single secret
```

Utilisez `--env` pour sélectionner un environnement pour une seule commande sans modifier celui par défaut du projet :

```sh
sotto run --env staging -- npm test
sotto ls --env staging
```

`--env` ne s'applique qu'à cette commande ; `sotto env use` modifie l'environnement par défaut.

`sotto login` utilise l'instance hébergée sur [getsotto.co.uk](https://getsotto.co.uk) sauf si vous le faites pointer
ailleurs avec `--server <url>` (voir [Déploiement](deploy/README.md) pour héberger le vôtre). Dans tous les cas,
le serveur ne stocke que du texte chiffré : le coffre web à la même adresse déchiffre dans votre
navigateur, avec des clés qui ne quittent jamais vos appareils.

Travail en équipe :

```sh
sotto org create acme                      # prints the org id
sotto init --org <org-id>                  # an org-owned project
sotto org invite <org-id> dev@example.com  # invite an existing Sotto user
sotto grant <user-id>                      # share the active environment (they run `sotto clone`)
sotto token create --name ci               # SOTTO_TOKEN: run/export in CI, no password needed
```

### Un autre appareil

```sh
sotto login                  # same account as the first machine
sotto setup                  # unpack the Emergency Kit onto this device
sotto pull                   # download the ciphertext you already pushed
```

Vous avez besoin de l'Emergency Kit imprimé par `sotto init` ; sans lui, un nouvel appareil ne peut pas déchiffrer le coffre.

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

## Prérequis

- [Rustup](https://rustup.rs/) avec Rust stable 1.89 ou plus récent
- Les composants `clippy` et `rustfmt`
- La cible `wasm32-unknown-unknown`

Le fichier `rust-toolchain.toml` inclus demande à Rustup d'installer automatiquement les composants et la
cible requis.

## Développement

Clonez le dépôt, puis compilez et testez l'espace de travail complet :

```sh
git clone https://github.com/getsotto/sotto.git
cd sotto

cargo build --workspace
cargo test --workspace
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
npm run dev      # dev server (proxies the API to localhost:8080)
npm run build    # production bundle → web/dist (strict CSP + Subresource Integrity)
```

### Déploiement

Une seule commande démarre une instance hébergée complète : Postgres, le serveur et Caddy avec HTTPS
automatique, depuis [`deploy/docker-compose.prod.yml`](deploy/docker-compose.prod.yml) ; le guide est dans
[`deploy/README.md`](deploy/README.md). Les éléments fonctionnent aussi séparément : servez l'application web et l'API
depuis **une même origine** (pour que le cookie de session et la CSP restent sur la même origine) ; le
[`Caddyfile`](Caddyfile) inclus sert `web/dist` et relaie l'API en proxy inverse, avec des en-têtes de sécurité ;
le [`Dockerfile`](Dockerfile) construit l'image du serveur (les migrations s'exécutent au démarrage).

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

L'audit complet du lockfile s'exécute aussi dans le job `supply-chain` de la CI.
Python 3.11 ou ultérieur est requis ; la CI utilise 3.12. cargo-audit doit être exactement en 0.22.2 :

```sh
cargo install cargo-audit --version 0.22.2 --locked
python3 -B -m unittest discover -s scripts/tests -v
scripts/check-cargo-audit
```

La [politique d'audit](.ci/cargo-audit-policy.toml) consigne des exceptions exactes pour les entrées
dormantes `rsa` et `spin`. Les champs `package`, `version`, `source`, `kind` et `finding` doivent
correspondre. Aucun chemin de dépendance `normal`, `build` ou `dev` ne doit exister sur aucune cible,
avec les fonctionnalités par défaut ou toutes celles de l'espace de travail. Les arêtes `dev` de Cargo
correspondent aux `dev-dependencies`.

Les nouveaux résultats, identités modifiées, paquets atteignables, analyses en échec et exceptions
obsolètes font échouer la CI. Supprimez chaque exception dans la PR qui supprime son résultat.
Une liste vide exige un audit propre. Le vérificateur renvoie 0 en cas de succès et 1 en cas d'échec
de la politique ou de l'analyse. `cargo audit` rapporte toujours les résultats dormants ;
les exceptions ne corrigent pas les versions vulnérables ou retirées.

Les [paramètres d'audit](.cargo/audit.toml) maintiennent la récupération des avis et les vérifications
des versions retirées, et remplacent la configuration globale du développeur.
N'ajoutez ni exclusions d'avis ni filtres d'analyse.

Le mode JSON de cargo-audit 0.22.2 peut omettre silencieusement les vérifications des versions
retirées. Une analyse indépendante au format terminal est donc exigée, sans erreur de registre
et avec des identités de résultats correspondantes. Cela établit une analyse distincte réussie,
sans prouver que l'analyse JSON précédente s'est terminée.

Le contrôle inter-implémentations prouve que les compilations native et WASM concordent : le texte chiffré produit en natif
se déchiffre à l'octet près en WASM à partir de vecteurs de référence partagés :

```sh
wasm-pack test --node crates/wasm
```

La compilation web et son audit de dépendances s'exécutent en CI (`.github/workflows/ci.yml`).

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
