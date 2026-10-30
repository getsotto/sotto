[English](README.md) | [Español](README.es.md) | [Français](README.fr.md) | [Deutsch](README.de.md) | [Português (BR)](README.pt-BR.md)

*Traducción al español de [README.md](README.md). El texto original en inglés es la versión oficial y prevalece en caso de discrepancias.*

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="web/public/mark-dark.svg">
    <source media="(prefers-color-scheme: light)" srcset="web/public/mark-light.svg">
    <img src="web/public/mark-dark.svg" alt="Sotto" width="64" height="64">
  </picture>
</p>

# Sotto

Sincronización de secretos con cifrado de extremo a extremo para equipos de desarrollo. Deja de enviar tu `.env` por Slack.

> [!WARNING]
> Sotto es una versión previa a la 1.0 y **no** ha pasado una auditoría criptográfica de terceros. Funciona de extremo a extremo, pero
> aún no deberías confiarle secretos críticos de producción. Consulta [SECURITY.md](SECURITY.md).

Sotto se basa en una única implementación criptográfica en Rust compartida por la CLI nativa y el cliente
del navegador mediante WebAssembly. El servidor almacena y sincroniza datos cifrados sin recibir nunca
secretos en texto plano ni claves utilizables.

## Formas de usar Sotto

Sotto es un único producto de código abierto que puedes usar de tres formas. Elijas la que elijas,
el servidor solo almacena texto cifrado.

| Modo | Qué obtienes |
| --- | --- |
| Local | La CLI y una bóveda cifrada en tu propia máquina. Sin cuenta y sin servidor. |
| Autoalojado | El mismo servidor Apache-2.0 en infraestructura que tú operas. No aplica límites de plan ni comprobaciones del servicio alojado; consulta [Autoalojamiento](#autoalojamiento). |
| Sotto Cloud | El servidor que Sotto opera en [getsotto.co.uk](https://getsotto.co.uk), para cuando prefieres no gestionar uno. `sotto login` lo usa por defecto. Los planes y límites están en la [página de precios](https://getsotto.co.uk/#pricing), y el estado del servicio, en [status.getsotto.co.uk](https://status.getsotto.co.uk). |

Puedes empezar en local y añadir un servidor más adelante: después de `sotto login`, `sotto push`
sube el entorno activo tal como está.

## Estado actual

El flujo de extremo a extremo funciona: cifra en local, sincroniza texto cifrado, descifra en otro dispositivo o en el
navegador, y comparte un único secreto con un enlace de un solo uso. Los equipos también funcionan de extremo a extremo: organizaciones
con roles, concesiones de entorno por miembro, rotación de claves al eliminar a un miembro, tokens de máquina para CI,
un registro de auditoría de la organización y el restablecimiento de la cuenta si se pierde el Emergency Kit.

| Componente | Disponible ahora |
| --- | --- |
| Núcleo criptográfico | KDF, AEAD XChaCha20-Poly1305 + AAD, encapsulado de claves, concesiones mediante sealed box X25519, la jerarquía de bóvedas de entorno, reencapsulado de claves de datos (rotación), criptografía de enlaces para compartir y codificación de claves, con vectores de referencia nativo↔WASM, fuzzing de los códecs y pruebas Kani del análisis de cabeceras de sobre y de símbolos de clave |
| CLI | Secretos locales (`init`, `set`, `get`, `ls`, `rm`, `import`, `export`, `run`), entornos (`env ls/use/diff/copy`), un panel interactivo, sincronización `login`/`push`/`pull`, `setup` para dispositivos nuevos, `history` y `rollback`, enlaces de un solo uso (`share`, `shares ls/revoke`); equipos: `org create/ls/invite/members/remove/plan/audit`, `grant`, `clone`, `rotate`, `token create/ls/revoke` de máquina con caducidad (con modo `SOTTO_TOKEN` para CI); `status`, `cloud status`, `reset` por pérdida del kit de emergencia, temas y completado del shell |
| Servidor | Inicio de sesión OAuth + sesiones, sincronización de cuentas + instantáneas (escrituras versionadas, ETag), organizaciones + membresías + roles, concesiones de claves de bóveda por miembro, rotación transaccional de claves, tokens de máquina con caducidad, historial de secretos, un registro de auditoría de la organización, eliminación opcional de organizaciones con un periodo de recuperación, restablecimiento de cuentas y enlaces de un solo uso - solo texto cifrado. Una implementación de Sotto Cloud también aplica límites de plan y puede facturar mediante Stripe |
| Web | Una página de inicio y guías; la bóveda en `/app`, que se desbloquea en el navegador para explorar, buscar, mostrar y copiar secretos, crear enlaces de un solo uso, compartir un entorno con un miembro y rotar la clave de un entorno; un panel de equipo con organizaciones, miembros, invitaciones por correo, el plan, el registro de auditoría y la eliminación de organizaciones; la página de recepción de enlaces de un solo uso; y una página de cuenta de Sotto Cloud en `/cloud` |

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

Los instaladores, la fórmula de Homebrew y la acción de GitHub instalan versiones publicadas,
mientras que este README sigue la rama `main`, que puede ir por delante de la última versión. Por
eso es posible que un comando descrito aquí aún no exista en la versión que tienes instalada;
`sotto --help` muestra los comandos que admite tu versión.

Consulta [COMPATIBILITY.md](COMPATIBILITY.md) para ver los requisitos de versión de las
integraciones, las plataformas declaradas y las pruebas disponibles actualmente.

### Homebrew (versión preliminar)

El [tap de Sotto](https://github.com/getsotto/homebrew-tap) ofrece otra vía de
instalación para macOS. La fórmula empaqueta versiones de la CLI revisadas y
requiere macOS 15 o posterior. Apple Silicon es la plataforma principal; Intel
se ofrece como mejor esfuerzo y Homebrew Tier 3. Homebrew prevé finalizar el
soporte para Intel en septiembre de 2027 o después. Consulta la
[guía de soporte e instalación del tap](https://github.com/getsotto/homebrew-tap#readme)
y el [registro de preparación para el lanzamiento](https://github.com/getsotto/homebrew-tap/blob/main/releases/READINESS.md)
antes de usarlo. Evita esta versión preliminar para secretos críticos de
producción.

Instala y actualiza la fórmula con:

```sh
brew install getsotto/tap/sotto
```

```sh
brew update
brew upgrade getsotto/tap/sotto
```

Si hay otra instalación de `sotto` en tu `PATH`, inspecciona los ejecutables
resueltos y el binario de Homebrew antes de elegir cuál usar:

```sh
which -a sotto
brew list --versions sotto
"$(brew --prefix)/bin/sotto" --version
```

Para eliminar solo la fórmula de Homebrew, ejecuta:

```sh
brew uninstall getsotto/tap/sotto
```

Al desinstalar la fórmula se conservan los archivos del proyecto, los datos
locales de Sotto, las entradas del Llavero y otras instalaciones de `sotto`.
Confirma qué binario está seleccionado y guarda tu Kit de emergencia
antes de eliminar o cambiar una instalación existente.

### Completado del shell

`sotto completions <shell>` muestra un script de completado en la salida estándar. Por ejemplo:

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

Los tarballs y zips de las versiones también contienen los archivos correspondientes `completions/sotto.*`
si prefieres copiarlos directamente.

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
      - uses: getsotto/sotto-action@v1.1.0
        with:
          sotto-version: v0.9.0
      - run: sotto run -- npm test
        env:
          SOTTO_SERVER: ${{ vars.SOTTO_SERVER }}
          SOTTO_TOKEN: ${{ secrets.SOTTO_TOKEN }}
```

La referencia de la acción y `sotto-version` son independientes. Mantén `sotto-version` como una
versión exacta `vX.Y.Z`. Define la variable de repositorio `SOTTO_SERVER` con la URL de tu servidor,
también `https://getsotto.co.uk` para Sotto Cloud: con `SOTTO_TOKEN` definido, `sotto run` y
`sotto export` nunca recurren a un servidor predeterminado, así que el token solo llega a un servidor
que hayas indicado. Consulta la [documentación de la acción](https://github.com/getsotto/sotto-action#readme)
para ejemplos de matrices, Windows y flujos reutilizables.

## Inicio rápido

¿Quieres una demo funcional? [**sotto-example**](https://github.com/getsotto/sotto-example)
muestra la inyección local de secretos con un GIF de Python y pasos copiables, además de pequeños ejemplos
en JavaScript, TypeScript, Java, C#, PHP, Go, Rust y C++. No necesitas cuenta; las instrucciones cubren
macOS, Linux y Windows.

```sh
sotto init                   # create your identity + first project - SAVE the printed Emergency Kit
sotto set DATABASE_URL       # hidden prompt; encrypted locally before it ever touches disk
sotto import .env            # optional: pull in an existing file, still encrypted locally
sotto run -- npm start       # inject the environment's secrets into any command
sotto get DATABASE_URL -c    # copy a secret without printing it; clipboard clears after 45s when unchanged
sotto export --format dotenv --reveal   # print a .env; refuses a terminal without --reveal
```

`sotto init` crea los entornos `dev`, `staging` y `prod` y selecciona `dev`. Exportar escribe texto
plano, por lo que necesita `--reveal` en un terminal, igual que `sotto get`; `--format` también admite
`shell` y `json`. El borrado del portapapeles es una medida de mejor esfuerzo: si reemplazas el
contenido se protege el nuevo valor, mientras que los gestores del portapapeles, la suspensión o la
terminación del proceso auxiliar pueden conservar una copia.

Hasta aquí todo se queda en tu máquina. Para sincronizar otro dispositivo, trabajar en equipo o usar
CI, inicia sesión en un servidor:

```sh
sotto login                  # Sotto Cloud by default; --server <url> for your own server
sotto push                   # upload the active environment's ciphertext
sotto share DATABASE_URL     # a one-time link whose key never reaches the server
```

`sotto login` abre un navegador para iniciar sesión con GitHub. Usa Sotto Cloud en
[getsotto.co.uk](https://getsotto.co.uk) salvo que indiques `--server <url>` (consulta
[Autoalojamiento](#autoalojamiento) para alojar el tuyo), y los comandos posteriores reutilizan el
servidor que elegiste. En cualquier caso el servidor solo almacena texto cifrado: la bóveda web en
`/app` del mismo servidor descifra en tu navegador, con claves que nunca salen de tus dispositivos.

Para explorar los secretos de forma interactiva, consulta la [guía del panel](#panel-interactivo).

## Flujos de trabajo con agentes

Los agentes de código locales pueden descubrir y ejecutar tareas declaradas por el proyecto mediante el servidor MCP local de Sotto. Consulta la [guía de tareas para agentes](AGENT-WORKFLOWS.md) para ver la configuración, las aprobaciones y los límites de seguridad.

## Uso de Sotto

### Otro dispositivo

```sh
sotto login                  # same account as the first machine
sotto setup                  # unpack the Emergency Kit onto this device
sotto pull                   # download the ciphertext you already pushed
```

Necesitas el Emergency Kit que imprime `sotto init`; sin él, un dispositivo nuevo no puede descifrar la bóveda.

Si pierdes el Emergency Kit, `sotto reset` da claves nuevas a tu cuenta. Es un restablecimiento, no
una recuperación: todo lo cifrado con las claves antiguas queda ilegible para siempre, y los
administradores de la organización deben volver a concederte los entornos compartidos. Si algún
dispositivo todavía puede desbloquear la bóveda, exporta antes lo que necesites.

### Entornos

Usa `--env` para elegir un entorno para un solo comando sin cambiar el entorno predeterminado del proyecto:

```sh
sotto run --env staging -- npm test
sotto ls --env staging
```

`--env` se aplica solo a ese comando; `sotto env use` cambia el entorno predeterminado.

Lista los entornos, cambia el predeterminado, compara dos o previsualiza una copia entre ellos:

```sh
sotto env ls                          # list environments; the active one is marked
sotto env use staging                 # switch the project's active environment
sotto env diff dev staging            # compare keys; values stay hidden without --reveal
sotto env copy dev staging            # preview additions and updates without writing
sotto env copy dev staging --confirm  # apply the copy; never delete destination keys
```

`env diff` marca cada clave con `=` (mismo valor), `!` (valores distintos), `<` (solo en el primer
entorno) o `>` (solo en el segundo). Sin `--confirm`, la copia es una simulación. Una copia confirmada
añade y actualiza secretos, pero deja intactas las claves que solo existen en el destino.

### Importar un archivo dotenv

`sotto import` lee un archivo dotenv y asigna cada variable en un entorno, cifrando cada valor en local.
Escribe en el entorno activo, salvo que indiques otro con `--env`:

```sh
sotto import .env                         # into the active environment
sotto import --env staging .env.staging   # into staging; the default environment is unchanged
```

Una importación se fusiona con lo que ya existe. Supón que `staging` ya contiene `API_URL` y `REGION`,
y que `.env.staging` contiene:

```dotenv
API_URL=https://staging.example.test
FEATURE_FLAG=on
```

Tras importarlo en `staging`:

- `API_URL` toma el valor del archivo, porque un nombre existente se actualiza.
- `FEATURE_FLAG` se añade, porque se crea un nombre nuevo.
- `REGION` se conserva, porque un nombre que falta en el archivo nunca se elimina.

La importación escribe solo en tu almacén local. Tras `sotto login`, sube el resultado en un paso aparte:

```sh
sotto push --env staging
```

El archivo dotenv solo se lee: la importación no lo modifica, no lo consume ni lo elimina, así que
protégelo o bórralo tú si contiene valores reales. Un archivo que no se puede analizar no cambia nada,
pero un fallo durante la escritura no deshace las asignaciones ya guardadas, así que revisa
`sotto ls --env staging` después de una importación interrumpida.

Para recuperar un valor anterior, consulta [Restaurar una versión anterior de un secreto](#restaurar-una-versión-anterior-de-un-secreto).
Ese historial procede del servidor, así que solo sirve para valores que ya habías subido. Ejecuta
`sotto import --help` para ver las reglas de comillas y de valores literales en lugar de repetirlas aquí.

### Eliminar un secreto

Para eliminar un secreto obsoleto de `staging` sin cambiar el entorno predeterminado:

```sh
sotto --env staging rm DEMO_OBSOLETE_KEY
sotto --env staging push
sotto --env staging pull
```

`sotto rm` elimina el secreto del entorno seleccionado en el almacén local.
`sotto push` sincroniza la eliminación con el servidor después de `sotto login`;
ejecuta `sotto pull` para ese mismo entorno en otro dispositivo para recibir el cambio.
Ejecuta el último comando en el otro dispositivo, no en el que eliminó el secreto.

En un terminal interactivo, `sotto rm` sin nombre abre el selector de secretos.
Después se solicita confirmación, con No como opción predeterminada; cancelar o
rechazar deja el secreto intacto. `--yes` (o `-y`) omite la confirmación, pero
no evita el selector si falta el nombre ni una solicitud de desbloqueo necesaria.

Las solicitudes interactivas requieren que stdin, stdout y stderr sean terminales,
que `TERM` no sea «dumb» y que `CI` no esté activo. En un entorno no interactivo
se debe indicar el nombre; la eliminación con nombre no pide confirmación aunque
no se incluya `--yes`.

Los secretos eliminados desaparecen de las lecturas y listas habituales, pero
se conserva su historial de versiones. No es un borrado seguro ni revoca la
credencial en su proveedor externo. Para recuperarla, usa `sotto history`
y `sotto rollback`.

### Compartir un secreto

`sotto share` cifra un secreto en un enlace de un solo uso. La clave va después del `#` del enlace,
una parte que los navegadores nunca envían a un servidor, así que el servidor solo guarda texto
cifrado. Crear un enlace requiere `sotto login`; quien lo recibe no necesita cuenta. Abrir el enlace
no consume una visualización; revelar el secreto sí.

```sh
sotto share DATABASE_URL                           # choose view and lifetime limits in an interactive terminal
sotto share DATABASE_URL --views 3 --expire 3600   # skip both limit prompts
sotto share DATABASE_URL --passphrase              # also require a passphrase; send it separately
```

En un terminal interactivo, cada límite omitido abre su propio menú. El menú de visualizaciones
ofrece 1, 2, 3, 5 o 10 visualizaciones y un valor personalizado; el menú de duración ofrece sin
caducidad, 1 hora, 1 día, 7 días, 30 días o un número de segundos personalizado. Los valores
personalizados deben ser de 1 a 100 visualizaciones o de 1 a 2592000 segundos. Proporcionar
`DATABASE_URL` omite solo la selección del secreto: `--views` omite el menú de visualizaciones y
`--expire` omite el menú de duración de forma independiente. Pulsa Esc o Ctrl-C en cualquier menú
de selección para cancelar sin crear un enlace.

El uso no interactivo requiere un nombre de secreto explícito. Cuando se omite cualquiera de los
límites, se aplica su valor predeterminado: una visualización y sin caducidad. Para omitir los tres
menús de selección en un terminal interactivo, proporciona el nombre y ambos límites, por ejemplo
`sotto share DATABASE_URL --views 3 --expire 3600`. Es posible que todavía se soliciten el
desbloqueo y la frase de contraseña.

Un servidor puede aplicar límites más estrictos a algunas cuentas. Cuando rechaza una solicitud, la
CLI muestra el motivo que da el servidor y no crea ningún enlace.

Usa `sotto share --no-copy` para desactivar la copia interactiva, o `--copy` para solicitarla explícitamente.
Estas opciones solo controlan el portapapeles; `--no-copy` no omite los menús de límites del enlace.

Un enlace deja de funcionar cuando se agotan sus visualizaciones, cuando caduca o cuando lo revocas.
Revocarlo impide que el servidor siga entregando el texto cifrado, pero no lo borra del servidor:

```sh
sotto shares ls              # your 100 most recent links: views used, expiry, revocation
sotto shares ls --json
sotto shares revoke <token>  # the token is the part of the link between /s/ and #
```

`sotto shares ls` nunca muestra el texto cifrado ni la clave del enlace.

Fíjate en el plural: `sotto share` crea un enlace y `sotto shares` gestiona los enlaces que has
creado. Ambos hablan con el servidor configurado, así que necesitan `sotto login`; no hace falta
desbloquear el almacén local. Un listado tiene este aspecto (token sintético, valores de ejemplo):

```text
active free links: 1
3f9a1c0e7b2d4a6f8e1c3b5d7a9f0e2c  class=free  views=1/3  expires 2026-11-01 12:00:00+00
```

Pasa la primera columna a `sotto shares revoke`. Es el token público, no el enlace completo: para
`https://getsotto.co.uk/s/3f9a1c0e7b2d4a6f8e1c3b5d7a9f0e2c#...`, pasa solo `3f9a1c0e7b2d4a6f8e1c3b5d7a9f0e2c` y nunca nada de
lo que va después de `#`. El listado solo lee metadatos. No abre ningún enlace ni gasta una vista, y
no puede reconstruir un enlace, porque la clave que va después de `#` nunca llegó al servidor.

`sotto shares ls --json` imprime un objeto y no un array: `active_free_count` y `links`, donde cada
enlace tiene `token`, `share_class`, `max_views`, `view_count`, `expires_at`, `revoked_at` y
`created_at`. El servidor devuelve solo tus 100 enlaces más recientes, así que no es un historial
completo.

Revocar impide futuras recuperaciones a través del enlace. No puede deshacer una recuperación que
ya ocurrió, así que si el destinatario pudo haber revelado el secreto, cámbialo en su origen.

### Trabajo en equipo

Los equipos comparten entornos mediante una organización. Cada miembro recibe su propia copia
sellada de la clave de un entorno (una concesión), y el servidor comprueba roles y concesiones en
cada solicitud sin poder leer nada.

```sh
sotto org create acme                      # prints the org id
sotto init --org <org-id>                  # an org-owned project
sotto push                                 # create it on the server before sharing
sotto org invite <org-id> dev@example.com  # invite an existing Sotto user; prints their user id
sotto grant <user-id>                      # share the active environment; prints their clone command
```

Después, el miembro ejecuta el comando impreso. Esto no es lo mismo que **Otro dispositivo**, que restaura tu propia cuenta en una segunda máquina.
El miembro usa su propia cuenta en el mismo servidor: ejecuta `sotto login` y `sotto unlock` y
después el comando impreso en un directorio que todavía no tiene `sotto.toml`:

```sh
mkdir acme-api && cd acme-api
sotto clone <project-id> <env-id> --org <org-id>
```

Ser miembro de la organización y tener acceso a un entorno son pasos distintos. `sotto org invite`
convierte a alguien en miembro; `sotto grant <user-id>` le da un entorno. Si ya no tienes el ID de
usuario, `sotto org members <org-id>` lo muestra. `sotto clone` no concede nada por sí solo: obtiene
el grant que el miembro ya tiene, descarga ese entorno y escribe `sotto.toml`. Sin grant se detiene
con `you have not been granted this environment (ask an admin to share it)`, y un administrador
tiene que ejecutar `sotto grant` para esa persona. No entregues nunca tu Emergency Kit a un
compañero; desbloquea tu cuenta, no la suya.

Conserva `--org` del comando impreso, porque guarda la organización propietaria para que los push
posteriores coincidan con el servidor. `--as <environment-name>` y `--name <project-label>` solo
fijan etiquetas locales: por defecto el entorno conserva su nombre real y el proyecto se llama
`shared`.

Para ver quién tiene acceso, eliminar a alguien o rotar tú mismo la clave de un entorno:

```sh
sotto org members <org-id>                 # members, roles and user ids
sotto org remove <org-id> <user-id>        # remove a member and rotate every environment they held
sotto rotate                               # rotate the active environment's key
```

Eliminar a un miembro rota la clave de todos los entornos a los que tenía acceso y revoca los tokens
de máquina que creó en la organización, nombrando cada uno para que puedas volver a crearlo. La
rotación protege lo que se escriba después. Los valores que el miembro ya vio siguen expuestos, así
que cámbialos en su origen.

#### Derechos antiguos de la organización

`sotto org plan <org-id>` lee la vista de derechos actual del servidor configurado para esa
organización. Debes haber iniciado sesión (`sotto login`); el cofre local no necesita estar
desbloqueado. Muestra los niveles asignado y efectivo, la fecha de prueba guardada y los límites
aplicables, como `limits: none (Team)`. Estos campos mantienen la compatibilidad con registros y despliegues antiguos de planes
por organización; no describen los precios actuales de Cloud por persona ni una nueva prueba de
Cloud.

Salida ilustrativa de un registro antiguo de plan de organización:

```text
tier:      free
effective: free
trial:     ends 2026-07-16T09:00:00Z
limits:    3 members, 1 org project(s)
```

Un servidor autoalojado no aplica límites de plan, así que siempre informa `team` como nivel
efectivo, sean cuales sean el nivel y el fin de prueba que la organización tenga guardados:

```text
tier:      free
effective: team
trial:     ends 2026-07-16T09:00:00Z
limits:    none (Team)
```

Estos ejemplos muestran campos de compatibilidad guardados, no las ofertas actuales de Cloud. Los
despliegues reales pueden mostrar otros niveles, fechas de prueba y límites guardados.

#### Registro de auditoría de la organización

Los propietarios y administradores de una organización pueden consultar la actividad reciente del
equipo con:

```sh
sotto org audit <org-id>
sotto org audit <org-id> --limit 20
```

Usa el ID que imprime `sotto org create <name>` o que muestra `sotto org ls`. Los resultados aparecen
primero con los eventos más recientes; el valor predeterminado es de 100 eventos y el servidor limita
`--limit` al rango 1-500. El comando muestra una línea por evento con el formato `timestamp  action  actor` y, de forma opcional, los campos `target=...`, `env=...` y `(detail)`. Es una vista acotada de
actividad y metadatos, no una forma de recuperar valores secretos; no ofrece paginación ni un archivo
histórico completo.

Este comando conserva el control de acceso antiguo basado en el plan Team de la organización. Las
implementaciones autoalojadas omiten esta comprobación del plan, pero quien ejecute el comando debe
conservar el rol de propietario o administrador. Si no hay eventos, el comando no muestra filas y
termina correctamente.

### Tokens de máquina y CI

Un token de máquina permite que la CI o un servicio lean un entorno sin contraseña ni Emergency Kit.
Con `SOTTO_TOKEN` definido, `sotto run` y `sotto export` obtienen y descifran ese entorno en cada
ejecución; define también `SOTTO_SERVER`, como muestra el ejemplo de [GitHub Actions](#github-actions).

Los tokens de máquina caducan. Uno nuevo dura 90 días, salvo que indiques `--expires-in-days` con
un valor de 1 a 365; `sotto token ls` muestra cuándo termina cada uno. Dos semanas antes,
`sotto run` y `sotto export` muestran un aviso en el registro de la CI.

Los comandos de tokens usan el acceso del usuario con sesión iniciada y el entorno seleccionado.
En un entorno de una organización, solo un administrador o propietario puede gestionar tokens
de máquina. Los listados incluyen solo metadatos de tokens activos: se omiten los revocados y
caducados, y el listado no permite recuperar la credencial que se muestra una sola vez al crear
un token con `sotto token create`. El JSON incluye `token_id`, `name`, `created_by`,
`beneficiary_id`, `beneficiary_status`, `expires_at` y `expires_in_days`; los campos de
caducidad pueden ser null. El `token_id` es el ID que se revoca, no el nombre del token ni el
secreto `SOTTO_TOKEN`.

Usa estos comandos para consultar, sustituir y revocar un token. La selección del entorno solo
se aplica a este comando:

```sh
sotto token ls
sotto token ls --json
sotto --env staging token ls --json
sotto token create --name ci
sotto token revoke <token-id>
```

Para sustituir un token de CI, identifica el ID antiguo en el listado, crea uno nuevo, actualiza
el secreto de la CI y verifica que la CI lo use; después revoca el ID antiguo con
`sotto token revoke <token-id>`. La revocación impide de inmediato que ese token se autentique
en la API, pero no rota la clave del entorno. Para un entorno de una organización, consulta las
instrucciones existentes de `sotto rotate` cuando sea necesario rotar la clave. Ni la revocación
ni la rotación pueden borrar el texto plano que ya se haya copiado.

### Panel interactivo

Desde un proyecto inicializado, ejecuta `sotto` sin subcomandos para explorar sus secretos locales:

```sh
sotto
```

Debes tener una identidad local desbloqueada antes de abrir el panel. Si hace falta, Sotto pide la
contraseña maestra. La entrada, la salida y la salida de error estándar deben estar conectadas a
terminales.
Si falta alguna, la CI está activa o `TERM=dumb`, la invocación sin subcomandos escribe la ayuda
en la salida de error estándar y termina con el código 2. Usa `sotto --help` para solicitar la
ayuda expresamente.

Usa las flechas o `j` y `k` para desplazarte. Pulsa `/` para buscar, `?` para abrir la ayuda y Tab
para cambiar de entorno. Este cambio solo dura mientras el panel está abierto; `sotto env use`
cambia el entorno predeterminado del proyecto. Pulsa `r` para mostrar u ocultar el valor
seleccionado, `c` para copiarlo al portapapeles y `q` para salir. `Esc` cierra la búsqueda o el
diálogo actual; en la vista principal, borra una búsqueda activa o sale.

Pulsa `t` para recorrer los temas con vista previa en directo. Usa las flechas para ver cada tema
y pulsa `Enter` para guardar tu elección o `Esc` para cancelar. Las
[reglas de precedencia de los temas](#temas-de-salida) explican cómo los ajustes del comando y del
entorno afectan a la preferencia guardada.

### Consultar el estado local

Ejecuta `sotto status` para ver si existe una identidad local, si la sesión está desbloqueada y
qué proyecto y entorno selecciona el directorio actual. Lee el estado local. No comprueba la
conexión con el servidor ni garantiza que todos los secretos guardados se puedan descifrar.

```sh
sotto status
sotto status --json
```

Usa `sotto status --json` en scripts. `initialized` indica si existe una identidad local y
`unlocked` indica si hay una clave maestra en caché. `project` contiene `name` y `environment`
del `sotto.toml` más cercano, o es `null` fuera de un proyecto configurado. Un comando correcto
puede mostrar `unlocked: false`; los scripts que necesitan una sesión desbloqueada deben consultar ese campo.

Estos ejemplos JSON son ilustrativos. Con un `sotto.toml` pero sin identidad local:

```json
{"initialized":false,"project":{"environment":"dev","name":"demo"},"unlocked":false}
```

Sin un `sotto.toml` ni identidad local:

```json
{"initialized":false,"project":null,"unlocked":false}
```

Si falta `sotto.toml`, no hay ningún proyecto seleccionado. Un archivo mal formado o ilegible produce un error.

### Bloquear, desbloquear y cerrar sesión

Con una identidad local ya inicializada, bloquea la bóveda, consulta su estado y
desbloquéala cuando vuelvas a necesitarla:

```sh
sotto lock
sotto status
sotto unlock
```

`sotto lock` borra la sesión local de la clave maestra en caché que utilizan los
siguientes comandos de la CLI. No elimina tu identidad, la bóveda cifrada ni el
inicio de sesión guardado del servidor de sincronización. `sotto unlock` utiliza
tu contraseña maestra cuando no hay una sesión válida en caché. Los comandos que
necesitan descifrar datos locales también solicitan el desbloqueo cuando hace falta.
Consulta [Consultar el estado local](#consultar-el-estado-local) para conocer los campos de estado.

La sesión local dura 12 horas desde que se guarda la clave en caché, no desde tu
última actividad. La caducidad se comprueba al leer la clave en caché. Ejecutar
`sotto unlock` cuando ya está desbloqueada no prolonga ese plazo. Ni el bloqueo ni
la caducidad borran el texto sin cifrar que ya tienen las aplicaciones en ejecución.

Para borrar también el inicio de sesión guardado de la CLI en el servidor de
sincronización, ejecuta ambos comandos:

```sh
sotto lock
sotto logout
```

`sotto logout` elimina el token de sesión del servidor guardado por la CLI,
independientemente de la caché de desbloqueo local. No cierra las sesiones del
navegador ni las de otros dispositivos.

### Estado de Cloud

Para consultar el servidor configurado y la elegibilidad de tu cuenta para Cloud, inicia sesión primero:

```sh
sotto login
sotto cloud status
sotto cloud status --json
```

El comando necesita una sesión activa, pero no desbloquea el almacén local ni abre un navegador. Muestra el `server` configurado, el `server_info` del servidor, la `eligibility` de la cuenta y un `billing_url` solo cuando el estado devuelto indica que la facturación está disponible. La salida legible también muestra `actions`, `next_actions` y los valores opcionales `paid_through_epoch`, `recovery_until_epoch` o `export_until_epoch`. Si falta `server_info` o `eligibility`, significa que un servidor antiguo no expone ese endpoint; no demuestra que el acceso esté permitido o denegado.

El formato JSON es útil para scripts. Su estructura de nivel superior es:

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

El ejemplo es sintético; `actions` y las fechas opcionales proceden del estado devuelto de la cuenta, y los servidores antiguos pueden devolver null para cualquiera de los dos endpoints. `sotto cloud status` solo informa del estado y puede mostrar una URL de facturación. No inicia una compra ni cambia la elegibilidad. Para diagnosticar el almacén local y los entornos, usa `sotto status`; esa vista es independiente de la información del servidor y de la cuenta anterior.

### Temas de salida

La CLI incluye cinco temas integrados: `nord` (el predeterminado), `sordino`, `terminal`,
`monochrome` y `tokyo-night`. Gestiónalos con `sotto theme`:

```sh
sotto theme ls        # list available themes; the active one is marked
sotto theme set nord  # save a preference for later commands
sotto theme current   # print the theme this shell resolves to
```

`--theme <name>` elige un tema para un solo comando. `SOTTO_THEME` lo fija para un shell y
`sotto theme set` lo guarda. La precedencia es `--theme`, luego `SOTTO_THEME`, luego la
preferencia guardada y por último `nord`; un nombre desconocido avisa por la salida de error
estándar y vuelve a `nord`.

El estilo solo se aplica en terminales interactivos. `--plain`, un `NO_COLOR` no vacío (el
contrato de [no-color.org](https://no-color.org)), los entornos de CI, la salida estándar en
una tubería y la entrada estándar redirigida suprimen cada uno el color y la decoración, pero
conservan la paleta elegida, así que los scripts y los registros se leen en texto plano.

Los temas personalizados son archivos TOML en el directorio `themes` dentro del directorio de
datos de tu plataforma: `~/Library/Application Support/sotto` en macOS, `%APPDATA%\sotto` en
Windows y `$XDG_DATA_HOME/sotto` o `~/.local/share/sotto` en Linux (`SOTTO_DATA_DIR` mueve ese
directorio). Cada archivo `*.toml` necesita los tokens de abajo y toma su nombre del campo
`name`, o del nombre del archivo cuando ese campo no está; un archivo que no se pueda
interpretar se omite. Los colores aceptan valores hexadecimales, nombres de colores ANSI,
índices `ansi(<index>)` y `default`.

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

Guardado como `synth.toml`, ese archivo añade un tema `synth`. Aplícalo con
`sotto theme set synth`, o para un solo comando con `sotto --theme synth <command>`.

### Restaurar una versión anterior de un secreto

Empieza en un proyecto ya configurado y mantén seleccionado el mismo entorno durante todo el proceso.
Necesitas una sesión iniciada y el almacén local desbloqueado. El historial procede del servidor:
los valores que nunca se han enviado no están disponibles allí para recuperarlos.

```sh
sotto history DATABASE_URL
```

Elige un número de versión positivo y legible del historial. El ejemplo siguiente usa `2`;
sustitúyelo por una versión disponible, sin el prefijo `v` que se muestra.

```sh
sotto rollback DATABASE_URL 2
sotto push
```

Rollback restaura el valor elegido como una nueva versión local. No borra ni reescribe el
historial anterior, y el servidor no se actualiza hasta el envío posterior.

Para obtener JSON con solo metadatos:

```sh
sotto history DATABASE_URL --json
```

Cada fila contiene `version`, `readable` (si se pudo descifrar el valor) y `size_bytes`
(el número de bytes del texto sin cifrar, o null si la versión es ilegible). No se imprimen valores.
En cambio, `--reveal` imprime los valores secretos y no se puede combinar con `--json`.

Si no hay historial en el servidor, comprueba el entorno seleccionado y si se ha enviado.
Un envío ahora no recupera valores antiguos que nunca se sincronizaron. Para una versión
ilegible, sigue la sugerencia de la CLI: ejecuta `sotto pull` e inténtalo de nuevo;
esto no garantiza que las claves disponibles permitan descifrar esa versión.

Para scripts de shell y CI, consulta la [referencia de scripting y códigos de salida de la CLI](CLI-SCRIPTING.md).

### Variables de entorno

La CLI lee estas variables:

- `SOTTO_SERVER`: la URL del servidor, que sustituye a la guardada por `sotto login`. Con
  `SOTTO_TOKEN` es obligatoria salvo que `sotto login` haya guardado un servidor en esa máquina.
- `SOTTO_TOKEN`: un token de máquina. `sotto run` y `sotto export` leen entonces el entorno del token
  en lugar de la bóveda local.
- `SOTTO_PASSWORD`: la contraseña maestra, para scripts que no pueden responder a una solicitud.
  Siempre que puedas, usa la solicitud oculta.
- `SOTTO_SECRET_KEY`: la Secret Key de tu Emergency Kit, para ejecutar `sotto setup` sin solicitud.
- `SOTTO_DATA_DIR`: mueve el directorio de datos local, que contiene el almacén cifrado, los ajustes
  guardados (como el servidor) y los temas personalizados.
- `SOTTO_THEME` y `NO_COLOR`: el estilo de la salida; consulta [Temas de salida](#temas-de-salida).

## Autoalojamiento

Un solo comando levanta un despliegue completo: Postgres, el servidor y Caddy con HTTPS
automático, desde [`deploy/docker-compose.prod.yml`](deploy/docker-compose.prod.yml); el manual está en
[`deploy/README.md`](deploy/README.md). Las piezas también funcionan por separado: sirve la app web y la API
desde **un mismo origen** (para que la cookie de sesión y la CSP sigan siendo del mismo origen); el
[`Caddyfile`](Caddyfile) incluido sirve `web/dist` y actúa como proxy inverso de la API, con cabeceras de seguridad;
el [`Dockerfile`](Dockerfile) construye la imagen del servidor (las migraciones se ejecutan al arrancar).

`SOTTO_DEPLOYMENT_MODE` vale `self_hosted` por defecto. En ese modo el servidor no aplica límites
de plan ni otras restricciones del servicio alojado, y no necesita una cuenta de Stripe. El modo
`cloud` es para el servicio que opera Sotto; lee el manual antes de activarlo.

Apunta la CLI a tu servidor una vez; los comandos posteriores lo reutilizan:

```sh
sotto login --server https://sotto.example.com
```

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

El cliente del navegador en `web/` es una aplicación React y Vite que ejecuta el núcleo a través de `crates/wasm`.

## Requisitos previos

- [Rustup](https://rustup.rs/) con Rust estable 1.89 o posterior
- Los componentes `clippy` y `rustfmt`
- El destino `wasm32-unknown-unknown`
- Para el cliente web: Node.js 22 (el que usa CI) y `wasm-pack` (CI fija la v0.15.0)
- Para el servidor y sus pruebas con base de datos: Postgres 16, que proporciona `docker compose up -d`
- Para las comprobaciones de scripts: Python 3.11 o posterior

El archivo `rust-toolchain.toml` incluido pide a Rustup que instale automáticamente los componentes y el
destino necesarios.

## Desarrollo

Clona el repositorio y luego compila y prueba el espacio de trabajo:

```sh
git clone https://github.com/getsotto/sotto.git
cd sotto

cargo build --workspace
cargo test --workspace
```

`cargo test --workspace` no necesita base de datos, así que omite las pruebas de integración del
servidor y la prueba de extremo a extremo de la CLI sin fallar. Para ejecutarlas, inicia el Postgres
local y actívalas con `SOTTO_RUN_DB_TESTS=1`; rechazan un `DATABASE_URL` que apunte a cualquier
lugar que no sea esta máquina:

```sh
docker compose up -d
DATABASE_URL=postgres://sotto:sotto@localhost:5432/sotto SOTTO_RUN_DB_TESTS=1 \
  cargo test -p sotto-server -p sotto-cli
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
npm run dev        # dev server (proxies the API to localhost:8080)
npm run build      # production bundle → web/dist (strict CSP + Subresource Integrity)
npm run typecheck  # TypeScript checks; needs the WASM bindings a build generates
npm test           # unit tests (Vitest)
npm run smoke      # load the WASM core under Node and exercise it
```

La batería de pruebas del embudo con Playwright (`npm run e2e`) necesita una base de datos y una
compilación de producción previa; [`web/e2e/README.md`](web/e2e/README.md) explica la preparación.

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

El job `supply-chain` de CI audita todo el lockfile con `scripts/check-cargo-audit`. La
[guía de auditoría del lockfile](.ci/cargo-audit.md) explica la versión fijada de cargo-audit, las
pruebas de la política y las reglas de las excepciones inactivas del lockfile.

El control cruzado entre implementaciones demuestra que las compilaciones nativa y WASM coinciden: el texto cifrado producido en nativo
se descifra byte a byte en WASM a partir de vectores de referencia compartidos:

```sh
wasm-pack test --node crates/wasm
```

La compilación web, su auditoría de dependencias y la comprobación de las traducciones del README
(`scripts/check-readme-i18n`) también se ejecutan en CI (`.github/workflows/ci.yml`).
[CONTRIBUTING.md](CONTRIBUTING.md) enlaza las guías de fuzzing, pruebas Kani, cobertura y
comprobaciones de garantía.

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

El alcance y las pruebas preparados para una auditoría independiente están en [SECURITY-AUDIT-SCOPE.md](SECURITY-AUDIT-SCOPE.md).

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
