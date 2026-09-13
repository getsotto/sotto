[English](README.md) | [Español](README.es.md) | [Français](README.fr.md) | [Deutsch](README.de.md) | [Português (BR)](README.pt-BR.md)

*Deutsche Übersetzung der [README.md](README.md). Der englische Originaltext ist die maßgebliche Version und gilt bei Abweichungen.*

# Sotto

Ende-zu-Ende-verschlüsselte Secret-Synchronisierung für Entwicklerteams. Schluss mit `.env` per Slack.

> [!WARNING]
> Sotto ist Pre-1.0-Software und hat **kein** kryptografisches Audit durch Dritte durchlaufen. Es funktioniert Ende zu Ende,
> doch solltest du ihm noch keine kritischen Produktions-Secrets anvertrauen. Siehe [SECURITY.md](SECURITY.md).

Sotto basiert auf einer einzigen Rust-Kryptoimplementierung, die sich die native CLI und der Browser-Client über
WebAssembly teilen. Der Server speichert und synchronisiert verschlüsselte Daten, ohne jemals Klartext-Secrets
oder verwendbare Schlüssel zu erhalten.

## Aktueller Stand

Der Ende-zu-Ende-Ablauf funktioniert: lokal verschlüsseln, Chiffrat synchronisieren, auf einem anderen Gerät oder im
Browser entschlüsseln und ein einzelnes Secret über einen Einmal-Link teilen. Auch für Teams funktioniert der gesamte Ablauf:
Organisationen mit Rollen, Umgebungs-Grants pro Mitglied, Schlüsselrotation beim Entfernen von Mitgliedern,
Maschinen-Token für CI und Kontowiederherstellung bei Schlüsselverlust.

| Komponente | Jetzt verfügbar |
| --- | --- |
| Krypto-Kern | KDF, XChaCha20-Poly1305 AEAD + AAD, Key-Wrapping, X25519-Sealed-Box-Grants, die Tresor-Hierarchie der Umgebungen, Rewrap von Datenschlüsseln (Rotation), Share-Link-Krypto und Schlüsselkodierung, mit gemeinsamen Referenzvektoren für native und WASM-Builds |
| CLI | `init`, lokale Secret-Verwaltung, Injektion per `run`, Synchronisierung mit `login`/`push`/`pull`, `setup` für neue Geräte, `share`; Teams: `org create/ls/invite/members/remove`, `grant`, `clone`, `rotate`, Maschinen-`token create/ls/revoke` (mit `SOTTO_TOKEN`-Modus für CI), `reset` bei verlorenem Notfall-Kit |
| Server | OAuth-Anmeldung + Sitzungen, Konto- und Snapshot-Synchronisierung (versionierte Schreibvorgänge, ETag), Organisationen + Mitgliedschaften + Rollen, Tresorschlüssel-Grants pro Mitglied, transaktionale Schlüsselrotation, Maschinen-Token, Konto-Zurücksetzung und Share-Links - nur Chiffrat |
| Web | Anmeldung (Cookie-Sitzung), Entsperren im Browser + Tresorentschlüsselung mit dem eigenen Grant, Erstellen und Empfangen von Einmal-Links und ein Team-Panel: Organisationen, Mitglieder, Einladung per E-Mail, Teilen einer Umgebung mit einem Mitglied |

## Installation

Vorgefertigte, signierte Binärdateien für macOS (Apple Silicon + Intel), Linux (x86_64 + ARM64) und
Windows x86_64:

```sh
curl -fsSL https://raw.githubusercontent.com/getsotto/sotto/main/install.sh | sh
```

```powershell
irm https://raw.githubusercontent.com/getsotto/sotto/main/install.ps1 | iex
```

Der Installer prüft die SHA-256-Prüfsumme des Archivs, und wenn `cosign` installiert ist, auch dessen
Sigstore-Signatur, bevor er installiert (`~/.local/bin` unter macOS/Linux, `%LOCALAPPDATA%\sotto\bin` unter
Windows). Lieber erst ansehen? Lade ein Archiv von der
[Release-Seite](https://github.com/getsotto/sotto/releases) herunter und verifiziere es manuell gemäß
[SECURITY.md](SECURITY.md), oder baue aus den Quellen (siehe [Entwicklung](#entwicklung)).

### GitHub Actions

Für GitHub Actions verwende die [Sotto-Setup-Action](https://github.com/getsotto/sotto-action), um ein exaktes
CLI-Release zu installieren und dessen Prüfsumme sowie Sigstore-Bundles zu verifizieren, bevor `sotto` für spätere
Schritte bereitsteht:

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

Die Action-Referenz und `sotto-version` sind unabhängig. Das Beispiel pinnt die zusammengeführte v1.1-Implementierung
per vollständigen Commit-SHA, weil noch kein nummeriertes Action-Release veröffentlicht wurde. Belasse
`sotto-version` bei einem exakten `vX.Y.Z`-Release. Setze die optionale Repository-Variable `SOTTO_SERVER`
für einen selbst gehosteten Server. Siehe die [Action-Dokumentation](https://github.com/getsotto/sotto-action#readme)
für Matrix-, Windows- und Reusable-Workflow-Beispiele.

## Schnellstart

Lust auf eine lauffähige Demo? [**sotto-example**](https://github.com/getsotto/sotto-example)
zeigt lokale Secret-Injektion mit einem Python-GIF und kopierbaren Schritten, plus winzige Beispiele
in JavaScript, TypeScript, Java, C#, PHP, Go und C++. Kein Konto erforderlich; die Anleitungen decken
macOS, Linux und Windows ab.

```sh
sotto init                   # create your identity + first project - SAVE the printed Emergency Kit
sotto set DATABASE_URL       # hidden prompt; encrypted locally before it ever touches disk
sotto import .env            # optional: pull in an existing file, still encrypted locally
sotto export --format dotenv --reveal   # print a .env; refuses a terminal without --reveal
sotto run -- npm start       # inject the environment's secrets into any command
sotto login && sotto push    # optional: sync ciphertext via the hosted instance (getsotto.co.uk)
sotto share DATABASE_URL     # one-time, burn-after-reading link for a single secret
```

<<<<<<< HEAD
Mit `--env` wählst du eine Umgebung für einen einzelnen Befehl aus, ohne die Standardumgebung des Projekts zu ändern:

```sh
sotto run --env staging -- npm test
sotto ls --env staging
```

`--env` gilt nur für diesen Befehl; `sotto env use` ändert die Standardumgebung.
=======
Der Export schreibt Klartext und benötigt daher in einem Terminal `--reveal`, genau wie `sotto get`.
>>>>>>> a6652a5 (docs: document dotenv export)

`sotto login` verwendet die gehostete Instanz unter [getsotto.co.uk](https://getsotto.co.uk), sofern du die CLI nicht mit
`--server <url>` auf einen anderen Server richtest (siehe [Bereitstellung](deploy/README.md) für den eigenen Betrieb). In jedem Fall
speichert der Server nur Chiffrat: Der Web-Tresor unter derselben Adresse entschlüsselt in deinem
Browser, mit Schlüsseln, die deine Geräte nie verlassen.

Arbeit im Team:

```sh
sotto org create acme                      # prints the org id
sotto init --org <org-id>                  # an org-owned project
sotto org invite <org-id> dev@example.com  # invite an existing Sotto user
sotto grant <user-id>                      # share the active environment (they run `sotto clone`)
sotto token create --name ci               # SOTTO_TOKEN: run/export in CI, no password needed
```

### Ein weiteres Gerät

```sh
sotto login                  # same account as the first machine
sotto setup                  # unpack the Emergency Kit onto this device
sotto pull                   # download the ciphertext you already pushed
```

Du brauchst das von `sotto init` ausgegebene Emergency Kit. Ohne es kann ein neues Gerät den Tresor nicht entschlüsseln.

## Architektur

```text
CLI (native) ─────┐
                  ├── sotto-core ── versioned encrypted data
Web client (WASM) ┘                         │
                                            ▼
                                  sync/API server
                                  (ciphertext only)
```

Der Workspace enthält vier Crates:

- `crates/core`: gemeinsame kryptografische Typen und die vollständige Kryptoimplementierung.
- `crates/cli`: die `sotto`-Kommandozeile und der primäre native Client.
- `crates/server`: die Axum-basierte Synchronisierungs-API.
- `crates/wasm`: `wasm-bindgen`-Anbindungen, die den Kern für Web-Clients bereitstellen.

## Voraussetzungen

- [Rustup](https://rustup.rs/) mit stabilem Rust 1.89 oder neuer
- Die Komponenten `clippy` und `rustfmt`
- Das Target `wasm32-unknown-unknown`

Die eingecheckte `rust-toolchain.toml` lässt Rustup die benötigten Komponenten und das Target automatisch installieren.

## Entwicklung

Klone das Repository, baue und teste dann den gesamten Workspace:

```sh
git clone https://github.com/getsotto/sotto.git
cd sotto

cargo build --workspace
cargo test --workspace
```

Nutze die CLI lokal (kein Server erforderlich):

```sh
cargo run -p sotto-cli -- --help
cargo run -p sotto-cli -- init                 # create an identity + project; prints your Emergency Kit
cargo run -p sotto-cli -- set DATABASE_URL     # hidden prompt
cargo run -p sotto-cli -- run -- your-command  # inject secrets as env vars into a subprocess
```

Secrets liegen in einem lokalen SQLite-Speicher verschlüsselt; der Hauptschlüssel wird mit einer TTL im OS-Schlüsselbund
zwischengespeichert. Die Synchronisierung mit einem Server (`login`/`push`/`pull`/`setup`/`share`) ist optional.

### Server ausführen

Der Server braucht Postgres (ein `docker compose up -d` startet eines für den lokalen Gebrauch):

```sh
DATABASE_URL=postgres://sotto:sotto@localhost:5432/sotto cargo run -p sotto-server
curl http://127.0.0.1:8080/health   # → ok
```

Die GitHub-OAuth-Anmeldung erfordert `GITHUB_CLIENT_ID` / `GITHUB_CLIENT_SECRET`. Für jedes nicht-lokale Deployment
setze zusätzlich `SOTTO_PUBLIC_URL` auf den extern erreichbaren Origin des Servers (daraus wird die
GitHub-Callback-URL gebaut; sie muss mit dem registrierten Callback der OAuth-App übereinstimmen, andernfalls gilt
der Standard `http://localhost:8080`) und für den Web-Client `SOTTO_WEB_ORIGIN`. Ohne OAuth startet der Server trotzdem
(bedient `/health` und führt Migrationen aus), aber Anmeldung und alle authentifizierten Endpunkte (Sync,
Share-Link-Erstellung) sind nicht verfügbar.

### Web-Client

Der Browser-Client nutzt denselben Krypto-Kern über WebAssembly (`web/`):

```sh
cd web
npm ci
npm run dev      # dev server (proxies the API to localhost:8080)
npm run build    # production bundle → web/dist (strict CSP + Subresource Integrity)
```

### Bereitstellung

Ein Befehl bringt eine komplette gehostete Instanz hoch: Postgres, Server und Caddy mit automatischem HTTPS,
aus [`deploy/docker-compose.prod.yml`](deploy/docker-compose.prod.yml); die Anleitung steht in
[`deploy/README.md`](deploy/README.md). Die Teile funktionieren auch eigenständig: Betreibe Web-App und API unter
**einem Origin** (damit Sitzungs-Cookie und CSP same-origin bleiben); die mitgelieferte
[`Caddyfile`](Caddyfile) liefert `web/dist` aus und leitet die API per Reverse-Proxy weiter, mit Sicherheits-Headern;
das [`Dockerfile`](Dockerfile) baut das Server-Image (Migrationen laufen beim Start).

## Entwicklungsprüfungen

Führe dieselben Basisprüfungen wie die CI aus:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Die Lieferketten-Richtlinie steht in `deny.toml` und wird in der CI mit
[`cargo-deny`](https://github.com/EmbarkStudios/cargo-deny) geprüft:

```sh
cargo deny check
```

Der vollständige Lockfile-Audit läuft ebenfalls im `supply-chain`-CI-Job.
Python 3.11 oder neuer ist erforderlich; die CI verwendet 3.12. cargo-audit muss genau 0.22.2 sein:

```sh
cargo install cargo-audit --version 0.22.2 --locked
python3 -B -m unittest discover -s scripts/tests -v
scripts/check-cargo-audit
```

Die [Audit-Richtlinie](.ci/cargo-audit-policy.toml) dokumentiert exakte Ausnahmen für ruhende `rsa`- und
`spin`-Lockfile-Einträge. `package`, `version`, `source`, `kind` und `finding` müssen übereinstimmen.
Über alle Targets darf mit Standard- und allen Workspace-Features kein Abhängigkeitspfad vom Typ
`normal`, `build` oder `dev` bestehen. Cargos `dev`-Kanten entsprechen `dev-dependencies`.

Neue Befunde, geänderte Identitäten, erreichbare Pakete, fehlgeschlagene Scans und veraltete Ausnahmen
lassen die CI fehlschlagen. Entferne eine Ausnahme im selben PR, der ihren Befund entfernt.
Eine leere Ausnahmeliste erfordert einen sauberen Audit. Der Prüfer beendet sich bei Erfolg mit 0,
bei Richtlinien- oder Scanfehlern mit 1. Rohes `cargo audit` meldet die ruhenden Befunde weiterhin;
die Ausnahmen beheben die verwundbaren oder zurückgezogenen Releases nicht.

Die [Audit-Standards](.cargo/audit.toml) aktivieren Advisory-Abruf und Yanked-Prüfungen und
überschreiben die globale Audit-Konfiguration. Füge keine Advisory-Ausnahmen oder Scan-Filter hinzu.

Der JSON-Modus von cargo-audit 0.22.2 kann Yanked-Prüfungen stillschweigend überspringen. Deshalb
verlangt der Prüfer einen unabhängigen Terminal-Scan ohne Registry-Fehler und mit übereinstimmenden
Befundidentitäten. Dies belegt einen separaten erfolgreichen Scan, nicht den Abschluss des JSON-Scans.

Das implementationsübergreifende Gate beweist, dass native und WASM-Builds übereinstimmen: nativ erzeugtes Chiffrat
entschlüsselt in WASM Byte für Byte aus gemeinsamen Referenzvektoren:

```sh
wasm-pack test --node crates/wasm
```

Der Web-Build und sein Abhängigkeits-Audit laufen in der CI (`.github/workflows/ci.yml`).

## Telemetrie

Der **Server** sendet einen anonymen Ping pro Tag (in den ersten 10-20 Minuten nach dem Start) an
`https://getsotto.co.uk/telemetry/v1/ping`, um aktive Instanzen zu zählen und zu sehen, welche
Versionen im Umlauf sind. Die Antwort nennt das neueste Release, und der Server protokolliert eine Zeile, wenn er
in einer veralteten Version läuft. Das ist die **gesamte** Nutzlast: Der Sendecode steht in
[`crates/server/src/telemetry.rs`](crates/server/src/telemetry.rs), und ein Unit-Test nagelt die Nutzlast auf genau
diese vier Felder fest:

```json
{ "instance_id": "0d0972a6-…", "version": "0.2.0", "os": "linux", "arch": "x86_64" }
```

`instance_id` ist eine zufällige UUID, die einmal erzeugt und in deiner Datenbank gespeichert wird, aus nichts abgeleitet,
sodass sie keine Hardware, keinen Host und kein Konto identifiziert; lösche sie, und die Instanz wird ein frischer
anonymer Zähler. Die Empfangsseite speichert keine IP-Adressen und keinen abgeleiteten Standort. Keine Org-, Mitglieder-
oder Secret-Zähler, keine Nutzungsereignisse. **CLI, Web-Client und WASM senden niemals etwas.**

Opt-out mit `SOTTO_TELEMETRY=off` (oder dem werkzeugübergreifenden [`DO_NOT_TRACK=1`](https://consoledonottrack.com)): Ist
die Telemetrie deaktiviert, wird der Task nie gestartet und nie eine Anfrage gesendet. `SOTTO_TELEMETRY_URL` leitet den Ping
um (zum Beispiel, um eine private Flotte zu aggregieren), und Datensätze, die 12 Monate inaktiv waren, werden aus der
gehosteten Erhebung gelöscht.

## Sicherheit

Sottos Modell ist Zero-Knowledge: Klartext-Secrets und verwendbare Entschlüsselungsschlüssel bleiben auf den
Client-Geräten, und der Server sieht nur Chiffrat plus minimale Metadaten. Das ist implementiert, aber **noch nicht
unabhängig auditiert**: siehe [SECURITY.md](SECURITY.md) für das Modell, die ehrliche Metadaten-Offenlegung, wie die
(erneut geholte, schwächere) Web-Oberfläche gehärtet ist und wie man signierte Releases verifiziert. Das vollständige
Angreifermodell, Garantien und explizite Nicht-Ziele stehen in [THREAT-MODEL.md](THREAT-MODEL.md). Melde Schwachstellen
privat gemäß SECURITY.md.

## Mitwirken

Sotto ist Apache-2.0 und freut sich über Beiträge. Starte bei [CONTRIBUTING.md](CONTRIBUTING.md);
[good first issues](https://github.com/getsotto/sotto/issues?q=is%3Aissue+is%3Aopen+label%3A%22good+first+issue%22)
sind markiert, und Fragen, die keine Fehler sind, gehören in
[Discussions](https://github.com/getsotto/sotto/discussions).

Melde Schwachstellen privat gemäß [SECURITY.md](SECURITY.md).

## Lizenz

Lizenziert unter der [Apache-Lizenz, Version 2.0](LICENSE): alle Crates und der Web-Client. Du darfst dieses Projekt
nur in Übereinstimmung mit der Lizenz nutzen. Sofern nicht durch geltendes Recht erforderlich oder schriftlich vereinbart,
wird die unter der Lizenz vertriebene Software "WIE BESEHEN" OHNE GEWÄHRLEISTUNGEN ODER BEDINGUNGEN JEGLICHER ART vertrieben.
