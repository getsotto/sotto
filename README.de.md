[English](README.md) | [Español](README.es.md) | [Français](README.fr.md) | [Deutsch](README.de.md) | [Português (BR)](README.pt-BR.md)

*Deutsche Übersetzung der [README.md](README.md). Der englische Originaltext ist die maßgebliche Version und gilt bei Abweichungen.*

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="web/public/mark-dark.svg">
    <source media="(prefers-color-scheme: light)" srcset="web/public/mark-light.svg">
    <img src="web/public/mark-dark.svg" alt="Sotto" width="64" height="64">
  </picture>
</p>

# Sotto

Ende-zu-Ende-verschlüsselte Secret-Synchronisierung für Entwicklerteams. Schluss mit `.env` per Slack.

> [!WARNING]
> Sotto ist Pre-1.0-Software und hat **kein** kryptografisches Audit durch Dritte durchlaufen. Es funktioniert Ende zu Ende,
> doch solltest du ihm noch keine kritischen Produktions-Secrets anvertrauen. Siehe [SECURITY.md](SECURITY.md).

Sotto basiert auf einer einzigen Rust-Kryptoimplementierung, die sich die native CLI und der Browser-Client über
WebAssembly teilen. Der Server speichert und synchronisiert verschlüsselte Daten, ohne jemals Klartext-Secrets
oder verwendbare Schlüssel zu erhalten.

## Möglichkeiten, Sotto zu betreiben

Sotto ist ein einziges Open-Source-Produkt, das du auf drei Arten betreiben kannst. Egal, wofür du
dich entscheidest: Der Server speichert immer nur Chiffrat.

| Modus | Was du bekommst |
| --- | --- |
| Lokal | Die CLI und ein verschlüsselter Tresor auf deinem eigenen Rechner. Kein Konto und kein Server. |
| Selbst gehostet | Derselbe Apache-2.0-Server auf Infrastruktur, die du betreibst. Er wendet keine Plan-Limits und keine Prüfungen des gehosteten Dienstes an; siehe [Selbst hosten](#selbst-hosten). |
| Sotto Cloud | Der Server, den Sotto unter [getsotto.co.uk](https://getsotto.co.uk) betreibt, wenn du lieber keinen eigenen betreibst. `sotto login` verwendet ihn standardmäßig. Pläne und Limits stehen auf der [Preisseite](https://getsotto.co.uk/#pricing), den Dienststatus findest du unter [status.getsotto.co.uk](https://status.getsotto.co.uk). |

Du kannst lokal anfangen und später einen Server hinzufügen: Nach `sotto login` lädt `sotto push`
die aktive Umgebung so hoch, wie sie ist.

## Aktueller Stand

Der Ende-zu-Ende-Ablauf funktioniert: lokal verschlüsseln, Chiffrat synchronisieren, auf einem anderen Gerät oder im
Browser entschlüsseln und ein einzelnes Secret über einen Einmal-Link teilen. Auch für Teams funktioniert der gesamte Ablauf:
Organisationen mit Rollen, Umgebungs-Grants pro Mitglied, Schlüsselrotation beim Entfernen von Mitgliedern,
Maschinen-Token für CI, ein Organisations-Auditprotokoll und das Zurücksetzen des Kontos bei verlorenem Emergency Kit.

| Komponente | Jetzt verfügbar |
| --- | --- |
| Krypto-Kern | KDF, XChaCha20-Poly1305 AEAD + AAD, Key-Wrapping, X25519-Sealed-Box-Grants, die Tresor-Hierarchie der Umgebungen, Rewrap von Datenschlüsseln (Rotation), Share-Link-Krypto und Schlüsselkodierung, mit gemeinsamen Referenzvektoren für native und WASM-Builds, Codec-Fuzzing und Kani-Beweisen für das Parsen von Umschlag-Headern und Schlüsselsymbolen |
| CLI | Lokale Secrets (`init`, `set`, `get`, `ls`, `rm`, `import`, `export`, `run`), Umgebungen (`env ls/use/diff/copy`), ein interaktives Dashboard, Synchronisierung mit `login`/`push`/`pull`, `setup` für neue Geräte, `history` und `rollback`, Einmal-Links (`share`, `shares ls/revoke`); Teams: `org create/ls/invite/members/remove/plan/audit`, `grant`, `clone`, `rotate`, ablaufende Maschinen-`token create/ls/revoke` (mit `SOTTO_TOKEN`-Modus für CI); `status`, `cloud status`, `reset` bei verlorenem Notfall-Kit, Themes und Shell-Vervollständigungen |
| Server | OAuth-Anmeldung + Sitzungen, Konto- und Snapshot-Synchronisierung (versionierte Schreibvorgänge, ETag), Organisationen + Mitgliedschaften + Rollen, Tresorschlüssel-Grants pro Mitglied, transaktionale Schlüsselrotation, ablaufende Maschinen-Token, Secret-Verlauf, ein Organisations-Auditprotokoll, optionale Organisationslöschung mit Wiederherstellungsfrist, Konto-Zurücksetzung und Einmal-Links - nur Chiffrat. Ein Sotto-Cloud-Deployment setzt außerdem Plan-Limits durch und kann über Stripe abrechnen |
| Web | Eine Landingpage und Anleitungen; der Tresor unter `/app`, der sich im Browser entsperren lässt, um Secrets zu durchsuchen, anzuzeigen und zu kopieren, Einmal-Links zu erstellen, eine Umgebung mit einem Mitglied zu teilen und den Schlüssel einer Umgebung zu rotieren; ein Team-Panel mit Organisationen, Mitgliedern, Einladungen per E-Mail, dem Plan, dem Auditprotokoll und der Organisationslöschung; die Empfangsseite für Einmal-Links; und eine Sotto-Cloud-Kontoseite unter `/cloud` |

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

Die Installer, die Homebrew-Formel und die GitHub-Action installieren veröffentlichte Releases,
diese README folgt dagegen dem Branch `main`, der dem neuesten Release voraus sein kann. Ein hier
beschriebener Befehl existiert in deiner installierten Version daher möglicherweise noch nicht.
`sotto --help` listet die Befehle auf, die deine Version unterstützt.

In [COMPATIBILITY.md](COMPATIBILITY.md) findest du die Versionsanforderungen der Integrationen,
die angegebenen Plattformen und die derzeit verfügbaren Testergebnisse.

### Homebrew (Vorschau)

Der [Sotto-Tap](https://github.com/getsotto/homebrew-tap) bietet einen weiteren
Installationsweg für macOS. Die Formula stellt geprüfte CLI-Versionen bereit
und setzt macOS 15 oder neuer voraus. Apple Silicon ist die primäre Plattform;
Intel wird bestmöglich unterstützt und ist bei Homebrew Tier 3. Homebrew plant,
den Intel-Support im September 2027 oder später einzustellen. Lies vor der Nutzung die [Installations- und Supporthinweise des Taps](https://github.com/getsotto/homebrew-tap#readme)
und den [Bericht zur Launch-Bereitschaft](https://github.com/getsotto/homebrew-tap/blob/main/releases/READINESS.md).
Verwende diese Vorschau nicht für kritische Produktionsgeheimnisse.

Installiere und aktualisiere die Formula mit:

```sh
brew install getsotto/tap/sotto
```

```sh
brew update
brew upgrade getsotto/tap/sotto
```

Wenn eine weitere `sotto`-Installation in deinem `PATH` liegt, prüfe vor der
Auswahl die gefundenen ausführbaren Dateien und das von Homebrew verwaltete
Binary:

```sh
which -a sotto
brew list --versions sotto
"$(brew --prefix)/bin/sotto" --version
```

Um nur die Homebrew-Formula zu entfernen, führe Folgendes aus:

```sh
brew uninstall getsotto/tap/sotto
```

Beim Entfernen der Formula bleiben Projektdateien, lokale Sotto-Daten,
Schlüsselbund-Einträge und andere `sotto`-Installationen erhalten. Prüfe das
ausgewählte Binary und sichere dein Notfall-Kit, bevor du eine vorhandene
Installation entfernst oder wechselst.

### Shell-Vervollständigungen

`sotto completions <shell>` gibt ein Vervollständigungsskript auf der Standardausgabe aus. Zum Beispiel:

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

Release-Tarballs und -zips enthalten ebenfalls die passenden Dateien `completions/sotto.*`,
falls du sie lieber direkt kopieren möchtest.

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
      - uses: getsotto/sotto-action@v1.1.0
        with:
          sotto-version: v0.9.0
      - run: sotto run -- npm test
        env:
          SOTTO_SERVER: ${{ vars.SOTTO_SERVER }}
          SOTTO_TOKEN: ${{ secrets.SOTTO_TOKEN }}
```

Die Action-Referenz und `sotto-version` sind unabhängig. Belasse `sotto-version` bei einem exakten
`vX.Y.Z`-Release. Setze die Repository-Variable `SOTTO_SERVER` auf die URL deines Servers, auch bei
Sotto Cloud (`https://getsotto.co.uk`): Ist `SOTTO_TOKEN` gesetzt, greifen `sotto run` und
`sotto export` nie auf einen Standardserver zurück, sodass das Token nur an einen Server geht, den du
benannt hast. Siehe die [Action-Dokumentation](https://github.com/getsotto/sotto-action#readme) für
Matrix-, Windows- und Reusable-Workflow-Beispiele.

## Schnellstart

Lust auf eine lauffähige Demo? [**sotto-example**](https://github.com/getsotto/sotto-example)
zeigt lokale Secret-Injektion mit einem Python-GIF und kopierbaren Schritten, plus winzige Beispiele
in JavaScript, TypeScript, Java, C#, PHP, Go, Rust und C++. Kein Konto erforderlich; die Anleitungen decken
macOS, Linux und Windows ab.

```sh
sotto init                   # create your identity + first project - SAVE the printed Emergency Kit
sotto set DATABASE_URL       # hidden prompt; encrypted locally before it ever touches disk
sotto import .env            # optional: pull in an existing file, still encrypted locally
sotto run -- npm start       # inject the environment's secrets into any command
sotto get DATABASE_URL -c    # copy a secret without printing it; clipboard clears after 45s when unchanged
sotto export --format dotenv --reveal   # print a .env; refuses a terminal without --reveal
```

`sotto init` legt die Umgebungen `dev`, `staging` und `prod` an und wählt `dev` aus. Der Export
schreibt Klartext und benötigt daher in einem Terminal `--reveal`, genau wie `sotto get`; `--format`
akzeptiert außerdem `shell` und `json`. Das Löschen der Zwischenablage erfolgt nach bestem Bemühen:
Das Ersetzen des Inhalts schützt den neuen Wert, während Zwischenablage-Manager, Ruhezustand oder
ein beendeter Hilfsprozess eine Kopie behalten können.

Bis hierhin bleibt alles auf deinem Rechner. Um ein weiteres Gerät zu synchronisieren, im Team zu
arbeiten oder CI zu nutzen, melde dich bei einem Server an:

```sh
sotto login                  # Sotto Cloud by default; --server <url> for your own server
sotto push                   # upload the active environment's ciphertext
sotto share DATABASE_URL     # a one-time link whose key never reaches the server
```

`sotto login` öffnet einen Browser für die Anmeldung mit GitHub. Es verwendet Sotto Cloud unter
[getsotto.co.uk](https://getsotto.co.uk), sofern du nicht `--server <url>` angibst (siehe
[Selbst hosten](#selbst-hosten) für den eigenen Betrieb), und spätere Befehle verwenden den gewählten
Server weiter. In jedem Fall speichert der Server nur Chiffrat: Der Web-Tresor unter `/app` auf
demselben Server entschlüsselt in deinem Browser, mit Schlüsseln, die deine Geräte nie verlassen.

Zum interaktiven Durchsuchen der Secrets siehe die [Dashboard-Anleitung](#interaktives-dashboard).

## Agent-Workflows

Lokale Coding-Agenten können projektdefinierte Aufgaben über den lokalen MCP-Server von Sotto finden und ausführen. Die [Anleitung für Agent-Aufgaben](AGENT-WORKFLOWS.md) erklärt Einrichtung, Freigaben und Sicherheitsgrenzen.

## Sotto verwenden

### Ein weiteres Gerät

```sh
sotto login                  # same account as the first machine
sotto setup                  # unpack the Emergency Kit onto this device
sotto pull                   # download the ciphertext you already pushed
```

Du brauchst das von `sotto init` ausgegebene Emergency Kit. Ohne es kann ein neues Gerät den Tresor nicht entschlüsseln.

Wenn du das Emergency Kit verlierst, gibt `sotto reset` deinem Konto neue Schlüssel. Das ist ein
Zurücksetzen, keine Wiederherstellung: Alles, was mit den alten Schlüsseln verschlüsselt wurde, ist
dauerhaft unlesbar, und Organisationsadmins müssen dir die geteilten Umgebungen erneut gewähren.
Wenn ein Gerät den Tresor noch entsperren kann, exportiere zuerst, was du brauchst.

### Umgebungen

Mit `--env` wählst du eine Umgebung für einen einzelnen Befehl aus, ohne die Standardumgebung des Projekts zu ändern:

```sh
sotto run --env staging -- npm test
sotto ls --env staging
```

`--env` gilt nur für diesen Befehl; `sotto env use` ändert die Standardumgebung.

Liste die Umgebungen auf, wechsle die Standardumgebung, vergleiche zwei Umgebungen oder prüfe eine Kopie zwischen ihnen:

```sh
sotto env ls                          # list environments; the active one is marked
sotto env use staging                 # switch the project's active environment
sotto env diff dev staging            # compare keys; values stay hidden without --reveal
sotto env copy dev staging            # preview additions and updates without writing
sotto env copy dev staging --confirm  # apply the copy; never delete destination keys
```

`env diff` markiert jeden Schlüssel mit `=` (gleicher Wert), `!` (unterschiedliche Werte), `<` (nur
in der ersten Umgebung) oder `>` (nur in der zweiten). Ohne `--confirm` ist die Kopie ein Probelauf.
Eine bestätigte Kopie fügt Geheimnisse hinzu und aktualisiert sie, lässt aber Schlüssel unverändert,
die nur in der Zielumgebung vorhanden sind.

### Eine dotenv-Datei importieren

`sotto import` liest eine dotenv-Datei und setzt jede Zuweisung in einer Umgebung, wobei jeder Wert lokal
verschlüsselt wird. Es schreibt in die aktive Umgebung, sofern Sie mit `--env` keine andere nennen:

```sh
sotto import .env                         # into the active environment
sotto import --env staging .env.staging   # into staging; the default environment is unchanged
```

Ein Import wird mit dem Vorhandenen zusammengeführt. Angenommen, `staging` enthält bereits `API_URL` und
`REGION`, und `.env.staging` enthält:

```dotenv
API_URL=https://staging.example.test
FEATURE_FLAG=on
```

Nach dem Import in `staging`:

- `API_URL` übernimmt den Wert aus der Datei, weil ein vorhandener Name aktualisiert wird.
- `FEATURE_FLAG` wird hinzugefügt, weil ein neuer Name angelegt wird.
- `REGION` bleibt erhalten, weil ein Name, der in der Datei fehlt, nie entfernt wird.

Der Import schreibt nur in Ihren lokalen Speicher. Laden Sie das Ergebnis nach `sotto login` in einem separaten Schritt hoch:

```sh
sotto push --env staging
```

Die dotenv-Datei wird nur gelesen: Der Import ändert, verbraucht oder löscht sie nicht, schützen oder
löschen Sie sie also selbst, wenn sie echte Werte enthält. Eine Datei, die sich nicht parsen lässt, ändert
nichts, aber ein Fehler beim Schreiben macht bereits gespeicherte Zuweisungen nicht rückgängig; prüfen Sie
nach einem unterbrochenen Import daher `sotto ls --env staging`.

Um einen früheren Wert zurückzuholen, siehe [Eine frühere Secret-Version wiederherstellen](#eine-frühere-secret-version-wiederherstellen).
Dieser Verlauf stammt vom Server und hilft daher nur bei Werten, die Sie bereits hochgeladen hatten. Führen Sie
`sotto import --help` aus, um die Regeln für Anführungszeichen und literale Werte nachzulesen, statt sie hier zu wiederholen.

### Ein Secret entfernen

Um ein veraltetes Secret aus `staging` zu entfernen, ohne die Standardumgebung zu ändern:

```sh
sotto --env staging rm DEMO_OBSOLETE_KEY
sotto --env staging push
sotto --env staging pull
```

`sotto rm` entfernt das Secret aus der gewählten Umgebung im lokalen Tresor.
`sotto push` synchronisiert die Entfernung nach `sotto login` mit dem Server.
Führe `sotto pull` für dieselbe Umgebung auf einem anderen Gerät aus, um die
Entfernung zu übernehmen. Der letzte Befehl gehört auf das andere Gerät.

Im interaktiven Terminal öffnet `sotto rm` ohne Namen die Secret-Auswahl.
Danach wird eine Bestätigung abgefragt, wobei Nein voreingestellt ist;
Abbrechen oder Ablehnen lässt das Secret unverändert. `--yes` (oder `-y`)
überspringt nur die Bestätigung, nicht die Auswahl bei fehlendem Namen oder
eine erforderliche Entsperrabfrage.

Interaktive Abfragen erfordern Terminals für stdin, stdout und stderr, einen
`TERM`-Wert ungleich »dumb« und eine inaktive `CI`-Umgebung. Ohne interaktives
Terminal ist ein Name erforderlich; das benannte Secret wird auch ohne `--yes`
ohne Bestätigungsfrage entfernt.

Entfernte Secrets erscheinen nicht mehr in normalen Abfragen oder Listen;
der Versionsverlauf bleibt jedoch erhalten. Das ist keine sichere Löschung und
widerruft keine Zugangsdaten beim externen Anbieter. Zur Wiederherstellung
dienen `sotto history` und `sotto rollback`.

### Ein Secret teilen

`sotto share` verschlüsselt ein Secret in einen Einmal-Link. Der Schlüssel steht im Link hinter dem
`#`, einem Teil, den Browser nie an einen Server senden; der Server hält also nur Chiffrat. Zum
Erstellen eines Links brauchst du `sotto login`; die empfangende Person braucht kein Konto. Das
Öffnen des Links verbraucht keinen Aufruf, das Anzeigen des Secrets schon.

```sh
sotto share DATABASE_URL                           # choose view and lifetime limits in an interactive terminal
sotto share DATABASE_URL --views 3 --expire 3600   # skip both limit prompts
sotto share DATABASE_URL --passphrase              # also require a passphrase; send it separately
```

In einem interaktiven Terminal öffnet jede ausgelassene Freigabegrenze ein eigenes Menü. Das
Aufrufmenü bietet 1, 2, 3, 5 oder 10 Aufrufe sowie einen benutzerdefinierten Wert; das
Gültigkeitsmenü bietet kein Ablaufdatum, 1 Stunde, 1 Tag, 7 Tage, 30 Tage oder eine
benutzerdefinierte Anzahl von Sekunden. Benutzerdefinierte Werte müssen 1 bis 100 Aufrufe oder 1
bis 2592000 Sekunden umfassen. Die Angabe von `DATABASE_URL` überspringt nur die Secret-Auswahl:
`--views` überspringt das Aufrufmenü und `--expire` unabhängig davon das Gültigkeitsmenü. Drücke
Esc oder Strg-C in einem Auswahlmenü, um abzubrechen, ohne eine Freigabe zu erstellen.

Die nicht interaktive Verwendung erfordert einen ausdrücklichen Secret-Namen. Wird eine Grenze
ausgelassen, gilt deren Standardwert: ein Aufruf und kein Ablaufdatum. Um alle drei Auswahlmenüs
in einem interaktiven Terminal zu überspringen, gib den Namen und beide Grenzen an, zum Beispiel
`sotto share DATABASE_URL --views 3 --expire 3600`. Aufforderungen zum Entsperren und zur Eingabe
einer Passphrase können weiterhin erscheinen.

Ein Server kann für manche Konten strengere Grenzen setzen. Lehnt er eine Anfrage ab, gibt die CLI
die Begründung des Servers aus und erstellt keinen Link.

Verwende `sotto share --no-copy`, um das interaktive Kopieren zu deaktivieren, oder `--copy`, um es ausdrücklich anzufordern.
Diese Optionen steuern nur die Zwischenablage; `--no-copy` überspringt die Freigabegrenzen-Menüs nicht.

Ein Link funktioniert nicht mehr, wenn seine Aufrufe aufgebraucht sind, wenn er abläuft oder wenn du
ihn widerrufst. Der Widerruf verhindert, dass der Server das Chiffrat weiter ausliefert, löscht es
aber nicht vom Server:

```sh
sotto shares ls              # your 100 most recent links: views used, expiry, revocation
sotto shares ls --json
sotto shares revoke <token>  # the token is the part of the link between /s/ and #
```

`sotto shares ls` zeigt nie das Chiffrat oder den Schlüssel im Link.

Achte auf den Plural: `sotto share` erstellt einen Link, `sotto shares` verwaltet die Links, die du
erstellt hast. Beide sprechen mit dem konfigurierten Server und brauchen daher `sotto login`; der
lokale Tresor muss nicht entsperrt sein. Eine Auflistung sieht so aus (synthetisches Token,
Beispielwerte):

```text
active free links: 1
3f9a1c0e7b2d4a6f8e1c3b5d7a9f0e2c  class=free  views=1/3  expires 2026-11-01 12:00:00+00
```

Übergib die erste Spalte an `sotto shares revoke`. Das ist das öffentliche Token, nicht der ganze
Link: Bei `https://getsotto.co.uk/s/3f9a1c0e7b2d4a6f8e1c3b5d7a9f0e2c#...` übergibst du nur
`3f9a1c0e7b2d4a6f8e1c3b5d7a9f0e2c` und nie etwas nach dem `#`. Die Auflistung liest nur Metadaten. Sie öffnet
keinen Link, verbraucht keinen Aufruf und kann dir keinen Link wiederherstellen, weil der Schlüssel
nach dem `#` den Server nie erreicht hat.

`sotto shares ls --json` gibt ein Objekt statt eines reinen Arrays aus: `active_free_count` und
`links`, wobei jeder Link `token`, `share_class`, `max_views`, `view_count`, `expires_at`,
`revoked_at` und `created_at` enthält. Der Server liefert nur deine 100 neuesten Links, das ist also
keine vollständige Historie.

Widerrufen verhindert künftige Abrufe über den Link. Ein bereits erfolgter Abruf lässt sich nicht
rückgängig machen: Wenn der Empfänger das Secret schon angezeigt haben könnte, ändere es an der
Quelle.

### Im Team arbeiten

Teams teilen Umgebungen über eine Organisation. Jedes Mitglied erhält eine eigene versiegelte Kopie
des Umgebungsschlüssels (einen Grant), und der Server prüft Rollen und Grants bei jeder Anfrage, ohne
etwas lesen zu können.

```sh
sotto org create acme                      # prints the org id
sotto init --org <org-id>                  # an org-owned project
sotto push                                 # create it on the server before sharing
sotto org invite <org-id> dev@example.com  # invite an existing Sotto user; prints their user id
sotto grant <user-id>                      # share the active environment; prints their clone command
```

Danach führt das Mitglied den ausgegebenen Befehl aus. Das ist etwas anderes als **Ein weiteres Gerät**, wo du dein eigenes Konto auf einem zweiten Rechner
wiederherstellst. Das Mitglied nutzt sein eigenes Konto auf demselben Server: Es führt `sotto login`
und `sotto unlock` aus und dann den ausgegebenen Befehl in einem Verzeichnis, das noch keine
`sotto.toml` enthält:

```sh
mkdir acme-api && cd acme-api
sotto clone <project-id> <env-id> --org <org-id>
```

Mitgliedschaft in der Organisation und Zugriff auf eine Umgebung sind getrennte Schritte.
`sotto org invite` macht jemanden zum Mitglied, `sotto grant <user-id>` gibt ihm eine Umgebung. Wenn
du die Benutzer-ID nicht mehr hast, zeigt sie `sotto org members <org-id>` an. `sotto clone` gewährt
selbst keinen Zugriff: Es holt den vorhandenen Grant des Mitglieds, lädt diese Umgebung und schreibt
`sotto.toml`. Ohne Grant bricht es mit
`you have not been granted this environment (ask an admin to share it)` ab, und ein Admin muss für
das Mitglied `sotto grant` ausführen. Gib dein Emergency Kit nie an Teammitglieder weiter; es
entsperrt dein Konto, nicht ihres.

Behalte `--org` aus dem ausgegebenen Befehl bei, denn es speichert die besitzende Organisation, damit
spätere Pushes zum Server passen. `--as <environment-name>` und `--name <project-label>` setzen nur
lokale Bezeichnungen: Standardmäßig behält die Umgebung ihren echten Namen und das Projekt heißt
`shared`.

Um zu sehen, wer Zugriff hat, jemanden zu entfernen oder den Schlüssel einer Umgebung selbst zu rotieren:

```sh
sotto org members <org-id>                 # members, roles and user ids
sotto org remove <org-id> <user-id>        # remove a member and rotate every environment they held
sotto rotate                               # rotate the active environment's key
```

Das Entfernen eines Mitglieds rotiert den Schlüssel jeder Umgebung, auf die es Zugriff hatte, und
widerruft die Maschinen-Token, die es in der Organisation erstellt hat; jedes wird namentlich genannt,
damit du es neu erstellen kannst. Die Rotation schützt, was danach geschrieben wird. Werte, die das
Mitglied bereits gesehen hat, bleiben offengelegt; ändere sie daher an ihrer Quelle.

#### Ältere Organisationsberechtigungen

`sotto org plan <org-id>` liest die aktuelle Berechtigungsansicht des konfigurierten Servers für
diese Organisation. Du musst angemeldet sein (`sotto login`); der lokale Tresor muss nicht
entsperrt sein. Es zeigt die zugewiesene und wirksame Stufe, das gespeicherte Trial-Ende und die
geltenden Limits wie `limits: none (Team)`. Diese Felder erhalten die Kompatibilität mit älteren Organisationsplan-Datensätzen
und Deployments; sie beschreiben weder die aktuellen personenbezogenen Cloud-Preise noch ein neues
Cloud-Trial-Angebot.

Illustrative Ausgabe aus einem älteren Organisationsplan-Datensatz:

```text
tier:      free
effective: free
trial:     ends 2026-07-16T09:00:00Z
limits:    3 members, 1 org project(s)
```

Ein selbst gehosteter Server wendet keine Plan-Limits an und meldet daher immer `team` als wirksame
Stufe, unabhängig davon, welche Stufe und welches Trial-Ende für die Organisation gespeichert sind:

```text
tier:      free
effective: team
trial:     ends 2026-07-16T09:00:00Z
limits:    none (Team)
```

Diese Beispiele zeigen gespeicherte Kompatibilitätsfelder, keine aktuellen Cloud-Angebote. Reale
Deployments können andere gespeicherte Stufen, Trial-Daten und Limits anzeigen.

#### Organisations-Auditprotokoll

Organisationsinhaber und Administratoren können die aktuelle Teamaktivität anzeigen:

```sh
sotto org audit <org-id>
sotto org audit <org-id> --limit 20
```

Verwende die ID, die `sotto org create <name>` ausgibt oder `sotto org ls` auflistet. Die Ergebnisse
werden zuerst mit den neuesten Ereignissen angezeigt; standardmäßig werden 100 Ereignisse abgerufen,
und der Server begrenzt `--limit` auf 1-500. Der Befehl zeigt pro Ereignis eine Zeile im Format
`timestamp  action  actor` sowie optional `target=...`, `env=...` und `(detail)`. Es handelt sich um
eine begrenzte Aktivitäts-/Metadatenansicht, nicht um eine Möglichkeit, Secret-Werte abzurufen; eine
Seitennavigation oder ein vollständiges historisches Archiv gibt es nicht.

Dieser Befehl behält die ältere Zugriffsprüfung anhand der Team-Berechtigung der Organisation bei.
Selbst gehostete Deployments überspringen diese Planprüfung, aber der Aufrufer braucht weiterhin die
Rolle als Inhaber oder Administrator. Wenn keine Ereignisse vorhanden sind, gibt der Befehl keine
Zeilen aus und wird erfolgreich beendet.

### Maschinen-Token und CI

Mit einem Maschinen-Token kann CI oder ein Dienst eine Umgebung ohne Passwort oder Emergency Kit
lesen. Ist `SOTTO_TOKEN` gesetzt, holen und entschlüsseln `sotto run` und `sotto export` diese
Umgebung bei jedem Lauf; setze auch `SOTTO_SERVER`, wie das [GitHub-Actions](#github-actions)-Beispiel
zeigt.

Maschinen-Tokens laufen ab. Ein neues Token gilt 90 Tage, sofern du nicht `--expires-in-days`
mit einem Wert von 1 bis 365 angibst; `sotto token ls` zeigt, wann jedes abläuft. Zwei Wochen
vorher geben `sotto run` und `sotto export` eine Warnung im CI-Log aus.

Token-Befehle verwenden den Zugriff des angemeldeten Benutzers und die ausgewählte Umgebung. In
einer Organisationsumgebung dürfen nur Admins oder Eigentümer Maschinen-Token verwalten. Die
Liste enthält nur Metadaten aktiver Token: widerrufene und abgelaufene Token werden ausgelassen,
und das einmal bei `sotto token create` ausgegebene Zugangstoken lässt sich über die Liste nicht
wiederherstellen. Das JSON enthält `token_id`, `name`, `created_by`, `beneficiary_id`,
`beneficiary_status`, `expires_at` und `expires_in_days`; Ablauf-Felder können null sein.
Die `token_id` ist die ID für den Widerruf, nicht der Token-Name oder das `SOTTO_TOKEN`-Secret.

Mit diesen Befehlen kannst du Token anzeigen, ersetzen und widerrufen. Die Umgebungs-Auswahl
gilt nur für diesen Befehl:

```sh
sotto token ls
sotto token ls --json
sotto --env staging token ls --json
sotto token create --name ci
sotto token revoke <token-id>
```

Um ein CI-Token zu ersetzen, ermittle zuerst die alte ID in der Liste, erstelle ein Ersatz-Token,
aktualisiere das CI-Secret und prüfe, ob die CI es verwendet, und widerrufe dann die alte ID mit
`sotto token revoke <token-id>`. Der Widerruf verhindert sofort, dass sich dieses Token bei
der API authentifiziert, rotiert aber nicht den Umgebungsschlüssel. Für eine
Organisationsumgebung beachte bei Bedarf die vorhandenen Hinweise zu `sotto rotate`. Weder
Widerruf noch Rotation können bereits kopierten Klartext löschen.

### Interaktives Dashboard

Führe `sotto` ohne Unterbefehl in einem initialisierten Projekt aus, um dessen lokale Secrets zu
durchsuchen:

```sh
sotto
```

Eine lokale Identität muss vorhanden und entsperrt sein, bevor sich das Dashboard öffnet. Falls
nötig, fragt Sotto nach dem Master-Passwort. Standardeingabe, Standardausgabe und
Standardfehlerausgabe müssen mit Terminals verbunden sein. Fehlt eines davon, ist die CI aktiv
oder gilt `TERM=dumb`,
schreibt ein Aufruf ohne Unterbefehl die Hilfe auf die Standardfehlerausgabe und endet mit Code 2.
Mit `sotto --help` rufst du die Hilfe ausdrücklich auf.

Navigiere mit den Pfeiltasten oder `j` und `k`. Drücke `/` zum Suchen, `?` für die Hilfe und Tab
zum Wechseln der Umgebung. Der Wechsel gilt nur für die Dashboard-Sitzung; `sotto env use` ändert
die Standardumgebung des Projekts. Mit `r` zeigst du den ausgewählten Wert an oder verbirgst ihn,
mit `c` kopierst du ihn in die Zwischenablage und mit `q` beendest du das Dashboard. `Esc`
schließt die aktuelle Suche oder ein Dialogfenster; in der Hauptansicht löscht es eine aktive
Suche oder beendet das Dashboard.

Drücke `t`, um Themes mit einer Live-Vorschau durchzugehen. Mit den Pfeiltasten siehst du jedes
Theme. Drücke `Enter`, um deine Wahl zu speichern, oder `Esc`, um abzubrechen. Die
[Vorrangregeln für Themes](#ausgabe-themes) erklären, wie Befehlsoptionen und Umgebungsvariablen
die gespeicherte Einstellung beeinflussen.

### Lokalen Status prüfen

Mit `sotto status` siehst du, ob eine lokale Identität vorhanden ist, ob die Sitzung entsperrt ist
und welches Projekt mit welcher Umgebung das aktuelle Verzeichnis auswählt. Der Befehl liest den
lokalen Zustand. Er prüft weder die Serververbindung noch, ob alle gespeicherten Secrets entschlüsselbar sind.

```sh
sotto status
sotto status --json
```

Verwende `sotto status --json` in Skripten. `initialized` zeigt an, ob eine lokale Identität vorhanden ist,
und `unlocked`, ob ein Hauptschlüssel zwischengespeichert ist. `project` enthält `name` und `environment`
aus der nächsten `sotto.toml` oder ist außerhalb eines konfigurierten Projekts `null`. Ein erfolgreicher
Befehl kann `unlocked: false` melden. Skripte, die eine entsperrte Sitzung benötigen, müssen dieses Feld prüfen.

Diese JSON-Beispiele dienen nur zur Veranschaulichung. Mit einer `sotto.toml`, aber ohne lokale Identität:

```json
{"initialized":false,"project":{"environment":"dev","name":"demo"},"unlocked":false}
```

Ohne `sotto.toml` und ohne lokale Identität:

```json
{"initialized":false,"project":null,"unlocked":false}
```

Fehlt `sotto.toml`, ist kein Projekt ausgewählt. Eine fehlerhafte oder unlesbare Datei führt zu einem Fehler.

### Sperren, entsperren und abmelden

Bei einer bereits initialisierten lokalen Identität kannst du den Tresor sperren,
seinen Status prüfen und ihn bei Bedarf wieder entsperren:

```sh
sotto lock
sotto status
sotto unlock
```

`sotto lock` löscht die zwischengespeicherte lokale Hauptschlüssel-Sitzung, die
nachfolgende CLI-Befehle verwenden. Deine Identität, der verschlüsselte Tresor und
die gespeicherte Anmeldung am Synchronisierungsserver bleiben erhalten.
`sotto unlock` verwendet dein Hauptpasswort, wenn keine gültige Sitzung im Cache
vorliegt. Befehle, die lokale Daten entschlüsseln müssen, fordern bei Bedarf
ebenfalls zum Entsperren auf. Die Statusfelder erklärt
[Lokalen Status prüfen](#lokalen-status-prüfen).

Die lokale Sitzung gilt 12 Stunden ab dem Zwischenspeichern des Schlüssels, nicht
ab deiner letzten Aktivität. Der Ablauf wird beim Lesen des Schlüssels aus dem
Cache geprüft. `sotto unlock` verlängert diese Dauer nicht, wenn der Tresor bereits
entsperrt ist. Weder Sperren noch Ablauf löschen Klartext, den laufende Anwendungen
bereits enthalten.

Um auch die gespeicherte CLI-Anmeldung am Synchronisierungsserver zu löschen,
führe beide Befehle aus:

```sh
sotto lock
sotto logout
```

`sotto logout` entfernt das gespeicherte Server-Sitzungstoken der CLI, unabhängig
vom lokalen Entsperr-Cache. Browser-Sitzungen und andere Geräte werden dadurch
nicht abgemeldet.

### Cloud-Status

Um den konfigurierten Server und die Cloud-Berechtigung deines Kontos zu prüfen, melde dich zuerst an:

```sh
sotto login
sotto cloud status
sotto cloud status --json
```

Der Befehl benötigt eine aktive Anmeldung, entsperrt aber weder den lokalen Tresor noch öffnet er einen Browser. Er zeigt den konfigurierten `server`, die `server_info` des Servers, die `eligibility` des Kontos und nur dann eine `billing_url`, wenn der zurückgegebene Status verfügbare Abrechnung meldet. Die lesbare Ausgabe zeigt außerdem `actions`, `next_actions` und optionale Werte für `paid_through_epoch`, `recovery_until_epoch` oder `export_until_epoch`. Fehlt `server_info` oder `eligibility`, stellt ein älterer Server diesen Endpoint nicht bereit; daraus folgt nicht, dass der Zugriff erlaubt oder verweigert ist.

Das JSON-Format eignet sich für Skripte. Die Struktur auf oberster Ebene sieht so aus:

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

Das Beispiel ist synthetisch; `actions` und die optionalen Zeitwerte stammen aus dem zurückgegebenen Kontostatus, und ältere Server können für einen der beiden Endpoints null liefern. `sotto cloud status` meldet nur den Status und kann eine Abrechnungs-URL ausgeben. Der Befehl startet keinen Kauf und ändert die Berechtigung nicht. Für Diagnosen des lokalen Tresors und der Umgebungen verwende `sotto status`; diese Ansicht ist von den Server- und Kontoinformationen oben getrennt.

### Ausgabe-Themes

Die CLI enthält fünf integrierte Themes: `nord` (die Standardeinstellung), `sordino`,
`terminal`, `monochrome` und `tokyo-night`. Verwalte sie mit `sotto theme`:

```sh
sotto theme ls        # list available themes; the active one is marked
sotto theme set nord  # save a preference for later commands
sotto theme current   # print the theme this shell resolves to
```

`--theme <name>` wählt ein Theme für einen einzelnen Befehl. `SOTTO_THEME` legt es für eine
Shell fest, und `sotto theme set` speichert es. Die Reihenfolge ist `--theme`, dann
`SOTTO_THEME`, dann die gespeicherte Einstellung, dann `nord`; bei einem unbekannten Namen
erscheint eine Warnung auf der Standardfehlerausgabe und es gilt `nord`.

Stil wird nur in interaktiven Terminals ausgegeben. `--plain`, ein nicht leeres `NO_COLOR`
(der Vertrag von [no-color.org](https://no-color.org)), CI-Umgebungen, eine umgeleitete
Standardausgabe und eine umgeleitete Standardeingabe unterdrücken jeweils Farbe und Schmuck,
behalten aber die gewählte Palette; Skripte und Protokolle bleiben daher reiner Text.

Eigene Themes sind TOML-Dateien im Verzeichnis `themes` im Datenverzeichnis deiner Plattform:
`~/Library/Application Support/sotto` unter macOS, `%APPDATA%\sotto` unter Windows und
`$XDG_DATA_HOME/sotto` oder `~/.local/share/sotto` unter Linux (`SOTTO_DATA_DIR` verschiebt
dieses Verzeichnis). Jede Datei `*.toml` braucht die folgenden Token und übernimmt ihren Namen
aus dem Feld `name` oder, wenn es fehlt, aus dem Dateinamen; eine Datei, die nicht geparst
werden kann, wird übersprungen. Farben akzeptieren Hex-Werte, ANSI-Farbnamen,
`ansi(<index>)`-Indizes und `default`.

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

Als `synth.toml` gespeichert, fügt diese Datei ein Theme `synth` hinzu. Mit
`sotto theme set synth` wendest du es an, oder für einen einzelnen Befehl mit
`sotto --theme synth <command>`.

### Eine frühere Secret-Version wiederherstellen

Beginne in einem bereits konfigurierten Projekt und behalte dieselbe Umgebung ausgewählt.
Du brauchst eine angemeldete Sitzung und einen entsperrten lokalen Speicher. Der Verlauf
kommt vom Server: Werte, die nie hochgeladen wurden, stehen dort nicht zur Wiederherstellung bereit.

```sh
sotto history DATABASE_URL
```

Wähle eine lesbare, positive Versionsnummer aus dem Verlauf. Das folgende Beispiel verwendet `2`;
ersetze sie durch eine verfügbare Version, ohne das angezeigte Präfix `v`.

```sh
sotto rollback DATABASE_URL 2
sotto push
```

Rollback stellt den gewählten Wert als neue lokale Version wieder her. Es löscht oder
überschreibt keinen älteren Verlauf. Der Server wird erst beim anschließenden Push aktualisiert.

Für JSON mit ausschließlich Metadaten:

```sh
sotto history DATABASE_URL --json
```

Jede Zeile enthält `version`, `readable` (ob der Wert entschlüsselt werden konnte) und `size_bytes`
(die Byteanzahl des Klartexts oder null bei einer unlesbaren Version). Werte werden nicht ausgegeben.
Dagegen gibt `--reveal` Secret-Werte aus und kann nicht mit `--json` kombiniert werden.

Wenn kein Serververlauf vorliegt, prüfe die ausgewählte Umgebung und ob sie hochgeladen wurde.
Ein jetziger Push kann ältere, nie synchronisierte Werte nicht wiederherstellen. Bei einer
unlesbaren Version empfiehlt die CLI, `sotto pull` auszuführen und es erneut zu versuchen;
ein Pull garantiert nicht, dass die verfügbaren Schlüssel diese Version entschlüsseln können.

Für Shell-Skripte und CI siehe die [Referenz zu CLI-Skripten und Exit-Status](CLI-SCRIPTING.md).

### Umgebungsvariablen

Die CLI liest diese Variablen:

- `SOTTO_SERVER`: die Server-URL; sie überschreibt die von `sotto login` gespeicherte. Mit
  `SOTTO_TOKEN` ist sie Pflicht, sofern `sotto login` auf diesem Rechner keinen Server gespeichert hat.
- `SOTTO_TOKEN`: ein Maschinen-Token. `sotto run` und `sotto export` lesen dann die Umgebung des
  Tokens statt des lokalen Tresors.
- `SOTTO_PASSWORD`: das Master-Passwort, für Skripte, die keine Eingabeaufforderung beantworten
  können. Nutze nach Möglichkeit die verdeckte Eingabe.
- `SOTTO_SECRET_KEY`: der Secret Key aus deinem Emergency Kit, für `sotto setup` ohne
  Eingabeaufforderung.
- `SOTTO_DATA_DIR`: verschiebt das lokale Datenverzeichnis mit dem verschlüsselten Speicher,
  gespeicherten Einstellungen wie dem Server und eigenen Themes.
- `SOTTO_THEME` und `NO_COLOR`: Ausgabestil; siehe [Ausgabe-Themes](#ausgabe-themes).

## Selbst hosten

Ein Befehl bringt ein komplettes Deployment hoch: Postgres, Server und Caddy mit automatischem HTTPS,
aus [`deploy/docker-compose.prod.yml`](deploy/docker-compose.prod.yml); die Anleitung steht in
[`deploy/README.md`](deploy/README.md). Die Teile funktionieren auch eigenständig: Betreibe Web-App und API unter
**einem Origin** (damit Sitzungs-Cookie und CSP same-origin bleiben); die mitgelieferte
[`Caddyfile`](Caddyfile) liefert `web/dist` aus und leitet die API per Reverse-Proxy weiter, mit Sicherheits-Headern;
das [`Dockerfile`](Dockerfile) baut das Server-Image (Migrationen laufen beim Start).

`SOTTO_DEPLOYMENT_MODE` steht standardmäßig auf `self_hosted`. In diesem Modus wendet der Server
keine Plan-Limits oder anderen Einschränkungen des gehosteten Dienstes an und braucht kein
Stripe-Konto. Der Modus `cloud` ist für den Dienst gedacht, den Sotto betreibt; lies die Anleitung,
bevor du ihn setzt.

Richte die CLI einmal auf deinen Server aus; spätere Befehle verwenden ihn weiter:

```sh
sotto login --server https://sotto.example.com
```

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

Der Browser-Client in `web/` ist eine React- und Vite-App, die den Kern über `crates/wasm` nutzt.

## Voraussetzungen

- [Rustup](https://rustup.rs/) mit stabilem Rust 1.89 oder neuer
- Die Komponenten `clippy` und `rustfmt`
- Das Target `wasm32-unknown-unknown`
- Für den Web-Client: Node.js 22 (wie in der CI) und `wasm-pack` (die CI pinnt v0.15.0)
- Für den Server und seine Datenbanktests: Postgres 16, das `docker compose up -d` bereitstellt
- Für die Skriptprüfungen: Python 3.11 oder neuer

Die eingecheckte `rust-toolchain.toml` lässt Rustup die benötigten Komponenten und das Target automatisch installieren.

## Entwicklung

Klone das Repository, baue und teste dann den Workspace:

```sh
git clone https://github.com/getsotto/sotto.git
cd sotto

cargo build --workspace
cargo test --workspace
```

`cargo test --workspace` braucht keine Datenbank und überspringt daher die Integrationstests des
Servers und den Ende-zu-Ende-Test der CLI, ohne fehlzuschlagen. Um sie auszuführen, starte das
lokale Postgres und aktiviere sie mit `SOTTO_RUN_DB_TESTS=1`; sie verweigern eine `DATABASE_URL`,
die auf etwas anderes als diesen Rechner zeigt:

```sh
docker compose up -d
DATABASE_URL=postgres://sotto:sotto@localhost:5432/sotto SOTTO_RUN_DB_TESTS=1 \
  cargo test -p sotto-server -p sotto-cli
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
npm run dev        # dev server (proxies the API to localhost:8080)
npm run build      # production bundle → web/dist (strict CSP + Subresource Integrity)
npm run typecheck  # TypeScript checks; needs the WASM bindings a build generates
npm test           # unit tests (Vitest)
npm run smoke      # load the WASM core under Node and exercise it
```

Die Playwright-Funnel-Suite (`npm run e2e`) braucht eine Datenbank und vorher einen
Produktions-Build; [`web/e2e/README.md`](web/e2e/README.md) erklärt die Einrichtung.

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

Der `supply-chain`-CI-Job prüft das gesamte Lockfile mit `scripts/check-cargo-audit`. Die
[Anleitung zum Lockfile-Audit](.ci/cargo-audit.md) beschreibt die gepinnte cargo-audit-Version, die
Richtlinientests und die Regeln für die ruhenden Lockfile-Ausnahmen.

Das implementationsübergreifende Gate beweist, dass native und WASM-Builds übereinstimmen: nativ erzeugtes Chiffrat
entschlüsselt in WASM Byte für Byte aus gemeinsamen Referenzvektoren:

```sh
wasm-pack test --node crates/wasm
```

Der Web-Build, sein Abhängigkeits-Audit und die Prüfung der README-Übersetzungen
(`scripts/check-readme-i18n`) laufen ebenfalls in der CI (`.github/workflows/ci.yml`).
[CONTRIBUTING.md](CONTRIBUTING.md) verlinkt die Anleitungen für Fuzzing, Kani-Beweise, Coverage und
die Assurance-Prüfungen.

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

Umfang und Nachweise, die für ein unabhängiges Audit vorbereitet wurden, stehen in [SECURITY-AUDIT-SCOPE.md](SECURITY-AUDIT-SCOPE.md).

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
