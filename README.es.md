[English](README.md) | [Español](README.es.md) | [Français](README.fr.md) | [Deutsch](README.de.md) | [Português (BR)](README.pt-BR.md)

*Traducción al español de [README.md](README.md). El texto original en inglés es la versión oficial y prevalece en caso de discrepancias.*

# Sotto

Sincronización de secretos con cifrado de extremo a extremo para equipos de desarrollo. Deja de enviar tu `.env` por Slack.

> [!WARNING]
> Sotto es una versión previa a la 1.0 y **no** ha pasado una auditoría criptográfica de terceros. Funciona de extremo a extremo, pero
> aún no deberías confiarle secretos críticos de producción. Consulta [SECURITY.md](SECURITY.md).

Sotto se basa en una única implementación criptográfica en Rust compartida por la CLI nativa y el cliente
del navegador mediante WebAssembly. El servidor almacena y sincroniza datos cifrados sin recibir nunca
secretos en texto plano ni claves utilizables.

## Estado actual

El flujo de extremo a extremo funciona: cifra en local, sincroniza texto cifrado, descifra en otro dispositivo o en el
navegador, y comparte un único secreto con un enlace de un solo uso. Los equipos también funcionan de extremo a extremo: organizaciones
con roles, concesiones de entorno por miembro, rotación de claves al eliminar a un miembro, tokens de máquina para CI
y recuperación de cuentas por pérdida de claves.

| Componente | Disponible ahora |
| --- | --- |
| Núcleo criptográfico | KDF, AEAD XChaCha20-Poly1305 + AAD, encapsulado de claves, concesiones mediante sealed box X25519, la jerarquía de bóvedas de entorno, reencapsulado de claves de datos (rotación), criptografía de enlaces para compartir y codificación de claves, con vectores de referencia nativo↔WASM |
| CLI | `init`, gestión local de secretos, inyección con `run`, sincronización `login`/`push`/`pull`, `setup` para dispositivos nuevos, `share`; equipos: `org create/ls/invite/members/remove`, `grant`, `clone`, `rotate`, `token create/ls/revoke` de máquina (con modo `SOTTO_TOKEN` para CI), `reset` por pérdida del kit de emergencia |
| Servidor | Inicio de sesión OAuth + sesiones, sincronización de cuentas + instantáneas (escrituras versionadas, ETag), organizaciones + membresías + roles, concesiones de claves de bóveda por miembro, rotación transaccional de claves, tokens de máquina, restablecimiento de cuentas y enlaces para compartir - solo texto cifrado |
| Web | Inicio de sesión (sesión con cookie), desbloqueo en el navegador + descifrado de la bóveda con tu propia concesión, creación y recepción de enlaces de un solo uso, y un panel de equipo: organizaciones, miembros, invitación por correo y compartición de un entorno con un miembro |

## Instalación

Binarios firmados y precompilados para macOS (Apple Silicon + Intel), Linux (x86_64 + ARM64) y
Windows x86_64:

```sh
curl -fsSL https://raw.githubusercontent.com/getsotto/sotto/main/install.sh | sh
```

```powershell
irm https://raw.githubusercontent.com/getsotto/sotto/main/install.ps1 | iex
```

El instalador verifica la suma de comprobación SHA-256 del archivo comprimido, y su firma Sigstore cuando `cosign`
está instalado, antes de instalar (`~/.local/bin` en macOS/Linux, `%LOCALAPPDATA%\sotto\bin` en
Windows). ¿Prefieres revisar antes? Descarga un archivo comprimido desde la
[página de versiones](https://github.com/getsotto/sotto/releases) y verifícalo manualmente según
[SECURITY.md](SECURITY.md), o compila desde el código fuente (consulta [Desarrollo](#desarrollo)).

### GitHub Actions

Para GitHub Actions, usa la [acción Sotto Setup](https://github.com/getsotto/sotto-action) para
instalar una versión exacta de la CLI y verificar su suma de comprobación y sus bundles Sigstore antes de dejar `sotto`
disponible para los pasos siguientes:

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

La referencia de la acción y `sotto-version` son independientes. El ejemplo fija la implementación v1.1 fusionada
por SHA completo del commit porque aún no se ha publicado una versión numerada de la acción. Mantén
`sotto-version` como una versión exacta `vX.Y.Z`. Define la variable de repositorio opcional `SOTTO_SERVER`
para un servidor autoalojado. Consulta la [documentación de la acción](https://github.com/getsotto/sotto-action#readme)
para ejemplos de matrices, Windows y flujos reutilizables.

## Inicio rápido

¿Quieres una demo funcional? [**sotto-example**](https://github.com/getsotto/sotto-example)
muestra la inyección local de secretos con un GIF de Python y pasos copiables, además de pequeños ejemplos
en JavaScript, TypeScript, Java, C#, PHP, Go y C++. No necesitas cuenta; las instrucciones cubren
macOS, Linux y Windows.

```sh
sotto init                   # create your identity + first project - SAVE the printed Emergency Kit
sotto set DATABASE_URL       # hidden prompt; encrypted locally before it ever touches disk
sotto import .env            # optional: pull in an existing file, still encrypted locally
sotto run -- npm start       # inject the environment's secrets into any command
sotto login && sotto push    # optional: sync ciphertext via the hosted instance (getsotto.co.uk)
sotto share DATABASE_URL     # one-time, burn-after-reading link for a single secret
```

Usa `--env` para elegir un entorno para un solo comando sin cambiar el entorno predeterminado del proyecto:

```sh
sotto run --env staging -- npm test
sotto ls --env staging
```

`--env` se aplica solo a ese comando; `sotto env use` cambia el entorno predeterminado.

`sotto login` usa la instancia alojada en [getsotto.co.uk](https://getsotto.co.uk) salvo que apuntes
a otro lugar con `--server <url>` (consulta [Despliegue](deploy/README.md) para alojar el tuyo). En cualquier caso
el servidor solo almacena texto cifrado: la bóveda web en la misma dirección descifra en tu
navegador, con claves que nunca salen de tus dispositivos.

Trabajo en equipo:

```sh
sotto org create acme                      # prints the org id
sotto init --org <org-id>                  # an org-owned project
sotto org invite <org-id> dev@example.com  # invite an existing Sotto user
sotto grant <user-id>                      # share the active environment (they run `sotto clone`)
sotto token create --name ci               # SOTTO_TOKEN: run/export in CI, no password needed
```

### Otro dispositivo

```sh
sotto login                  # same account as the first machine
sotto setup                  # unpack the Emergency Kit onto this device
sotto pull                   # download the ciphertext you already pushed
```

Necesitas el Emergency Kit que imprime `sotto init`; sin él, un dispositivo nuevo no puede descifrar la bóveda.

## Arquitectura

```text
CLI (native) ─────┐
                  ├── sotto-core ── versioned encrypted data
Web client (WASM) ┘                         │
                                            ▼
                                  sync/API server
                                  (ciphertext only)
```

El espacio de trabajo contiene cuatro crates:

- `crates/core`: los tipos criptográficos compartidos y la implementación criptográfica completa.
- `crates/cli`: la interfaz de línea de comandos `sotto` y el cliente nativo principal.
- `crates/server`: la API de sincronización basada en Axum.
- `crates/wasm`: enlaces `wasm-bindgen` que exponen el núcleo a los clientes web.

## Requisitos previos

- [Rustup](https://rustup.rs/) con Rust estable 1.89 o posterior
- Los componentes `clippy` y `rustfmt`
- El destino `wasm32-unknown-unknown`

El archivo `rust-toolchain.toml` incluido pide a Rustup que instale automáticamente los componentes y el
destino necesarios.

## Desarrollo

Clona el repositorio y luego compila y prueba el espacio de trabajo completo:

```sh
git clone https://github.com/getsotto/sotto.git
cd sotto

cargo build --workspace
cargo test --workspace
```

Usa la CLI en local (sin servidor):

```sh
cargo run -p sotto-cli -- --help
cargo run -p sotto-cli -- init                 # create an identity + project; prints your Emergency Kit
cargo run -p sotto-cli -- set DATABASE_URL     # hidden prompt
cargo run -p sotto-cli -- run -- your-command  # inject secrets as env vars into a subprocess
```

Los secretos se cifran en reposo en un almacén SQLite local; la clave maestra se almacena en caché en el llavero del SO
con un TTL. Sincronizar con un servidor (`login`/`push`/`pull`/`setup`/`share`) es opcional.

### Ejecución del servidor

El servidor necesita Postgres (un `docker compose up -d` lo levanta para uso local):

```sh
DATABASE_URL=postgres://sotto:sotto@localhost:5432/sotto cargo run -p sotto-server
curl http://127.0.0.1:8080/health   # → ok
```

El inicio de sesión con GitHub OAuth requiere `GITHUB_CLIENT_ID` / `GITHUB_CLIENT_SECRET`. Para cualquier despliegue no local
define también `SOTTO_PUBLIC_URL` con el origen del servidor accesible desde fuera (construye la
URL de callback de GitHub y debe coincidir con la registrada en la app OAuth; si no, se usa
`http://localhost:8080`) y, para el cliente web, `SOTTO_WEB_ORIGIN`. Sin OAuth el servidor
sigue arrancando (sirve `/health` y ejecuta las migraciones), pero el inicio de sesión y todos los endpoints autenticados
(sincronización, creación de enlaces para compartir) no están disponibles.

### Cliente web

El cliente del navegador ejecuta el mismo núcleo criptográfico con WebAssembly (`web/`):

```sh
cd web
npm ci
npm run dev      # dev server (proxies the API to localhost:8080)
npm run build    # production bundle → web/dist (strict CSP + Subresource Integrity)
```

### Despliegue

Un solo comando levanta una instancia alojada completa: Postgres, el servidor y Caddy con HTTPS
automático, desde [`deploy/docker-compose.prod.yml`](deploy/docker-compose.prod.yml); el manual está en
[`deploy/README.md`](deploy/README.md). Las piezas también funcionan por separado: sirve la app web y la API
desde **un mismo origen** (para que la cookie de sesión y la CSP sigan siendo del mismo origen); el
[`Caddyfile`](Caddyfile) incluido sirve `web/dist` y actúa como proxy inverso de la API, con cabeceras de seguridad;
el [`Dockerfile`](Dockerfile) construye la imagen del servidor (las migraciones se ejecutan al arrancar).

## Comprobaciones de desarrollo

Ejecuta las mismas comprobaciones básicas que usa CI:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

La política de cadena de suministro se define en `deny.toml` y se comprueba en CI con
[`cargo-deny`](https://github.com/EmbarkStudios/cargo-deny):

```sh
cargo deny check
```

La auditoría completa del lockfile también se ejecuta en el job `supply-chain` de CI.
Requiere Python 3.11 o posterior; CI usa 3.12. cargo-audit debe ser exactamente la versión 0.22.2:

```sh
cargo install cargo-audit --version 0.22.2 --locked
python3 -B -m unittest discover -s scripts/tests -v
scripts/check-cargo-audit
```

La [política de auditoría](.ci/cargo-audit-policy.toml) registra excepciones exactas para las entradas
inactivas `rsa` y `spin`. Deben coincidir `package`, `version`, `source`, `kind` y `finding`.
No debe haber rutas de dependencia `normal`, `build` ni `dev` en ningún destino, con las features
por defecto y todas las del espacio de trabajo. Las aristas `dev` corresponden a `dev-dependencies`.

Nuevos hallazgos, identidades cambiadas, paquetes alcanzables, análisis fallidos y excepciones
obsoletas hacen fallar CI. Elimina cada excepción en el mismo PR que elimine su hallazgo.
Una lista vacía requiere una auditoría limpia. El comprobador devuelve 0 si tiene éxito y 1 ante
un fallo de política o análisis. `cargo audit` sigue reportando los hallazgos inactivos;
las excepciones no corrigen las versiones vulnerables o retiradas.

Los [valores de auditoría](.cargo/audit.toml) mantienen activas la obtención de avisos y las
comprobaciones de versiones retiradas, y anulan la configuración global del desarrollador.
No añadas exclusiones de avisos ni filtros de análisis.

El modo JSON de cargo-audit 0.22.2 puede omitir silenciosamente las comprobaciones de versiones
retiradas. Se exige un análisis independiente en formato de terminal, sin fallos del registro y
con identidades de hallazgos coincidentes. Esto acredita otro análisis completo, no que el análisis
JSON anterior haya finalizado.

El control cruzado entre implementaciones demuestra que las compilaciones nativa y WASM coinciden: el texto cifrado producido en nativo
se descifra byte a byte en WASM a partir de vectores de referencia compartidos:

```sh
wasm-pack test --node crates/wasm
```

La compilación web y su auditoría de dependencias se ejecutan en CI (`.github/workflows/ci.yml`).

## Telemetría

El **servidor** envía un ping anónimo al día (en los primeros 10-20 minutos tras el arranque) a
`https://getsotto.co.uk/telemetry/v1/ping`, para contar instancias activas y ver qué
versiones están en uso. La respuesta nombra la última versión, y el servidor registra una línea cuando
ejecuta una versión desactualizada. Esta es la carga útil **completa**: el código de envío está en
[`crates/server/src/telemetry.rs`](crates/server/src/telemetry.rs), y una prueba unitaria fija
la carga útil en exactamente estos cuatro campos:

```json
{ "instance_id": "0d0972a6-…", "version": "0.2.0", "os": "linux", "arch": "x86_64" }
```

`instance_id` es un UUID aleatorio generado una vez y guardado en tu base de datos, sin derivarse de nada,
así que no identifica hardware, equipo ni cuenta; eliminarlo convierte la instancia en un contador anónimo
nuevo. El lado de ingesta no guarda direcciones IP ni ubicación derivada. No hay conteos de organizaciones, miembros
ni secretos, ni eventos de uso. La **CLI, el cliente web y WASM nunca envían nada**.

Desactívalo con `SOTTO_TELEMETRY=off` (o la variable común a varias herramientas
[`DO_NOT_TRACK=1`](https://consoledonottrack.com)): cuando está desactivado la tarea nunca se inicia y
nunca se hace ninguna petición. `SOTTO_TELEMETRY_URL` redirige el ping (por ejemplo, para agregar una flota privada),
y los registros que llevan 12 meses inactivos se purgan del censo alojado.

## Seguridad

El modelo de Sotto es de conocimiento cero: los secretos en texto plano y las claves de descifrado utilizables permanecen en
los dispositivos cliente, y el servidor solo ve texto cifrado más metadatos mínimos. Esto está implementado pero aún **no
auditado de forma independiente**: consulta [SECURITY.md](SECURITY.md) para el modelo, la exposición honesta de metadatos,
cómo se refuerza la superficie web (que se vuelve a descargar en cada visita y es, por tanto, más débil) y cómo verificar las
versiones firmadas. El modelo de adversario completo, las garantías y los no objetivos explícitos están publicados en
[THREAT-MODEL.md](THREAT-MODEL.md). Reporta vulnerabilidades en privado según SECURITY.md.

## Contribución

Sotto es Apache-2.0 y acepta contribuciones. Empieza en [CONTRIBUTING.md](CONTRIBUTING.md);
los [good first issues](https://github.com/getsotto/sotto/issues?q=is%3Aissue+is%3Aopen+label%3A%22good+first+issue%22)
están etiquetados, y las preguntas que no sean errores pertenecen a
[Discussions](https://github.com/getsotto/sotto/discussions).

Reporta vulnerabilidades en privado según [SECURITY.md](SECURITY.md).

## Licencia

Licenciado bajo la [Licencia Apache, versión 2.0](LICENSE): todos los crates y el cliente web.
No puedes usar este proyecto salvo conforme a la Licencia. Salvo que la ley aplicable lo exija o se acuerde por
escrito, el software distribuido bajo la Licencia se distribuye «TAL CUAL», SIN GARANTÍAS NI CONDICIONES DE NINGÚN TIPO.
