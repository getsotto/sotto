// Guide page content: one entry per indexable guide route (`/<slug>`), plus
// the landing page's FAQ list so visible copy and JSON-LD stay identical.
//
// Each entry is the single source of truth for its page. React renders it
// (`SeoPage`) and the build inlines the identical string for crawlers
// (`snapshot.ts`), so the two cannot drift apart by construction.
//
// House rules, same as the landing page: British English, no em or en dashes.
// Inline `backticks` become <code> and `[label](url)` becomes <a> in prose
// fields; terminal lines are literal CLI transcripts and must match what the
// binary actually prints (see the format strings in crates/cli/src/main.rs).
//
export interface SeoStep {
  head: string;
  body: string;
}

export interface SeoFaq {
  q: string;
  a: string;
}

export type SeoTerminalKind = "cmd" | "dim" | "value";

export interface SeoTerminalLine {
  text: string;
  kind: SeoTerminalKind;
}

export interface SeoPageData {
  slug: string;
  navLabel: string;
  tabTitle: string;
  description: string;
  keywords: string[];
  h1: string;
  lead: string;
  ctaSecondary: { label: string; href: string };
  stepsTitle: string;
  steps: [SeoStep, SeoStep, SeoStep];
  terminal: SeoTerminalLine[];
  faqs: [SeoFaq, SeoFaq, SeoFaq];
  closingTitle: string;
  closingBody: string;
}

export const landingFaqs: SeoFaq[] = [
  {
    q: "What is Sotto (getsotto)?",
    a: "Sotto (available at [getsotto.co.uk](https://getsotto.co.uk) and [github.com/getsotto/sotto](https://github.com/getsotto/sotto)) is an [open-source secret manager](/open-source-secret-manager) for developer teams. It encrypts `.env` values, API keys, and credentials on your device with a single Rust crypto core shared between the native CLI and the browser WebAssembly vault.",
  },
  {
    q: "How is Sotto different from keeping plaintext .env files?",
    a: "Plaintext `.env` files end up in Slack, email, screenshots, and stale laptops. Sotto works as a [dotenv alternative](/dotenv-alternative): import your file once with `sotto import .env`, inject secrets into processes in memory via `sotto run`, and sync only encrypted ciphertext across your team.",
  },
  {
    q: "Can I use Sotto as a CLI secret manager in CI/CD?",
    a: "Yes. The [CLI secret manager](/cli-secret-manager) runs on macOS, Linux, and Windows, and [CI/CD secrets](/ci-cd-secret-manager) use scoped, expiring machine tokens (`SOTTO_TOKEN`) with the official GitHub Action or any CI runner.",
  },
  {
    q: "Can I self-host Sotto on my own infrastructure?",
    a: "Yes. Sotto is Apache-2.0 and ships with a single Docker Compose stack for [self-hosted secret management](/self-hosted-secret-management) with no plan limits. You can also run completely locally without any server.",
  },
];

export const guidePages: SeoPageData[] = [
  {
    slug: "share-secrets-securely",
    navLabel: "Share secrets",
    tabTitle: "Share Secrets Securely with Encryption | Sotto",
    description:
      "Send passwords, API keys, and tokens without pasting them into chat. Sotto encrypts secrets locally and shares them through a burn-after-reading link.",
    keywords: [
      "share secrets securely",
      "encrypted secret sharing",
      "developer secret manager",
      "sotto",
      "getsotto",
    ],
    h1: "Share secrets securely.",
    lead: "Send passwords, tokens, and keys without pasting them into chat, tickets, or email. Sotto encrypts each secret on your machine. Anyone with the complete link can use it while it is active. An optional passphrase adds another factor.",
    ctaSecondary: { label: "Try a one-time link", href: "/one-time-secret-links" },
    stepsTitle: "How sharing works",
    steps: [
      {
        head: "Encrypt on your machine.",
        body: "The secret never leaves your device in readable form.",
      },
      {
        head: "Send a link, not the secret.",
        body: "One command produces a share link you can paste anywhere.",
      },
      {
        head: "It burns after reading.",
        body: "The link works once, then stops working. All that is left in chat is a dead link.",
      },
    ],
    terminal: [
      { text: "sotto share STRIPE_SECRET_KEY", kind: "cmd" },
      { text: "share link (acme-api/dev) - burns after 1 view(s):", kind: "dim" },
      {
        text: "https://getsotto.co.uk/s/00112233445566778899aabbccddeeff#AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8",
        kind: "value",
      },
    ],
    faqs: [
      {
        q: "Can Sotto read the secrets I share?",
        a: "No. Encryption happens on your machine before anything is uploaded. The server stores ciphertext it cannot read.",
      },
      {
        q: "Does the recipient need a Sotto account?",
        a: "No. They open the link in a browser and read the secret once. No account, no install.",
      },
      {
        q: "What happens once it has been read?",
        a: "The link burns. Anyone who tries it afterwards gets a message that it is no longer valid. The server retains the encrypted blob, which it cannot decrypt.",
      },
    ],
    closingTitle: "Stop pasting secrets into chat",
    closingBody:
      "Use Sotto as your [open-source secret manager](/open-source-secret-manager) or send a free one-time link to share your first secret. No account is needed by the recipient.",
  },
  {
    slug: "share-env-files",
    navLabel: "Share .env files",
    tabTitle: "Share .env Files Securely with Your Team | Sotto",
    description:
      "Your .env holds every key your app needs. Import and share .env files with your team using Sotto, an end-to-end encrypted secret manager.",
    keywords: [
      "share env files",
      "share .env securely",
      "sync .env files",
      "env secret manager",
      "sotto",
    ],
    h1: "Share .env files without the screenshot dance.",
    lead: "Your `.env` holds every key your app needs, and today it travels by screenshot, Slack paste, or shoulder surf. Import it into Sotto and share it with your team encrypted end to end.",
    ctaSecondary: { label: "Explore the dotenv alternative", href: "/dotenv-alternative" },
    stepsTitle: "From file to shared vault",
    steps: [
      {
        head: "Import what you have.",
        body: "`sotto import .env` encrypts every value locally. The file itself never leaves your machine.",
      },
      {
        head: "Grant your team.",
        body: "Teammates get access through cryptography, not a permission bit. They decrypt on their own machines.",
      },
      {
        head: "Stay in sync.",
        body: "Change a value in one place, push, and teammates get it on their next pull. Remove someone and the keys rotate away from them.",
      },
    ],
    terminal: [
      { text: "sotto import .env", kind: "cmd" },
      { text: "imported 14 secret(s) into acme-api (dev)", kind: "dim" },
      { text: "sotto push", kind: "cmd" },
      { text: "pushed acme-api/dev - revision 1", kind: "dim" },
    ],
    faqs: [
      {
        q: "Do I have to delete my .env file?",
        a: "No. Sotto reads it and encrypts the values into your vault. Keep the file, gitignore it, or delete it - your call.",
      },
      {
        q: "How do teammates get updates?",
        a: "They pull. Changed values sync as ciphertext and decrypt on their machines with their own grant.",
      },
      {
        q: "What if someone leaves the team?",
        a: "Remove them and Sotto rotates the affected keys, so their old grants decrypt nothing going forward.",
      },
    ],
    closingTitle: "One vault, whole team",
    closingBody:
      "Sotto is an open-source secret manager that can be [self-hosted](/self-hosted-secret-management). Import your `.env` in under a minute.",
  },
  {
    slug: "one-time-secret-links",
    navLabel: "One-time links",
    tabTitle: "One-Time Secret Links That Burn After Reading | Sotto",
    description:
      "Create an encrypted one-time secret link that burns after reading. Share credentials safely with optional passphrase protection and no recipient account.",
    keywords: [
      "one-time secret links",
      "burn after reading secret",
      "encrypted secret link",
      "onetimesecret alternative",
      "sotto",
    ],
    h1: "One-time links that burn after reading.",
    lead: "Create a link that reveals a secret exactly once, then stops working. The complete link is a bearer credential, so anyone who obtains it can use a remaining view unless you also require a passphrase.",
    ctaSecondary: { label: "How sharing works", href: "/share-secrets-securely" },
    stepsTitle: "Send once, read once",
    steps: [
      {
        head: "Create the link.",
        body: "From the command line or the web vault, in seconds. Need more than one view, or an expiry? `--views` and `--expire` have you covered.",
      },
      {
        head: "Send it anywhere.",
        body: "Email, chat, ticket. Anyone who obtains the complete link can use a remaining view. Add a passphrase when the link alone should not grant access.",
      },
      {
        head: "First view burns it.",
        body: "Once it is read, the server refuses every later request for it, so a forwarded link reveals nothing. The server retains the encrypted blob after the link is used, but never has the key.",
      },
    ],
    terminal: [
      { text: "sotto share WIFI_PASSWORD --views 1", kind: "cmd" },
      { text: "share link (acme-api/dev) - burns after 1 view(s):", kind: "dim" },
      {
        text: "https://getsotto.co.uk/s/00112233445566778899aabbccddeeff#AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8",
        kind: "value",
      },
    ],
    faqs: [
      {
        q: "Does the recipient need an account?",
        a: "No. They open the link, read the secret, and the link burns. Nothing to install or sign up for.",
      },
      {
        q: "Can the secret be read twice?",
        a: "No. The first read burns the link, unless you explicitly allowed more with `--views`. After that it only says the link is no longer valid.",
      },
      {
        q: "How is this different from emailing the secret?",
        a: "Email keeps a readable copy forever, in your sent folder and theirs. A burn-after-reading link stops working once it is read. The server retains only ciphertext it cannot decrypt.",
      },
    ],
    closingTitle: "Send your first burning link",
    closingBody:
      "The recipient opens a free one-time link without creating an account. Need ongoing team access? Use Sotto as your [CLI secret manager](/cli-secret-manager).",
  },
  {
    slug: "share-api-keys-securely",
    navLabel: "Share API keys",
    tabTitle: "Share API Keys Securely with Your Team | Sotto",
    description:
      "API keys unlock billing, email, and infrastructure. Store, share, and rotate API keys across your team and CI/CD with Sotto's encrypted secret manager.",
    keywords: [
      "share api keys securely",
      "api key manager",
      "rotate api keys",
      "developer secret manager",
      "sotto",
    ],
    h1: "Share API keys without pasting them into chat.",
    lead: "An API key in chat is a leak with extra steps. Store keys in Sotto, share them with the teammates who need them, and rotate them in one place when they leak.",
    ctaSecondary: { label: "CI/CD secrets", href: "/ci-cd-secret-manager" },
    stepsTitle: "Keys in, leaks out",
    steps: [
      {
        head: "Store the key once.",
        body: "`sotto set` takes the value through a hidden prompt, so it never lands in your shell history.",
      },
      {
        head: "Share it two ways.",
        body: "Grant an environment to a teammate for ongoing access, or send a one-time link for a single handover.",
      },
      {
        head: "Rotate in one place.",
        body: "Revoke the old key at the provider, set the new one, and push. Teammates get it on their next pull, and CI on its next run.",
      },
    ],
    terminal: [
      { text: "sotto set STRIPE_SECRET_KEY", kind: "cmd" },
      { text: "Value:", kind: "dim" },
      { text: "set STRIPE_SECRET_KEY (acme-api/dev)", kind: "dim" },
      { text: "sotto share STRIPE_SECRET_KEY", kind: "cmd" },
      { text: "share link (acme-api/dev) - burns after 1 view(s):", kind: "dim" },
      {
        text: "https://getsotto.co.uk/s/00112233445566778899aabbccddeeff#AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8",
        kind: "value",
      },
    ],
    faqs: [
      {
        q: "What is the difference between a grant and a link?",
        a: "A grant gives a teammate ongoing access to an environment. A link hands one secret to one reader, once, then burns.",
      },
      {
        q: "How does CI get secrets?",
        a: "Hand CI a scoped machine token (`SOTTO_TOKEN`) instead of the key itself. Revoke the token and CI loses access without touching the key.",
      },
      {
        q: "Someone pasted a key in chat. Now what?",
        a: "Rotate it: revoke the old key at the provider, set the new value, and push. Teammates get it on their next pull. Then delete the message.",
      },
    ],
    closingTitle: "Keys change. Chat is forever.",
    closingBody:
      "Run Sotto locally or self-host it, and share a free one-time link with anyone or set up [CI/CD secrets](/ci-cd-secret-manager).",
  },
  {
    slug: "send-password-securely",
    navLabel: "Send passwords",
    tabTitle: "Send a Password Securely with a One-Time Link | Sotto",
    description:
      "Send a password through a link that stops working after one read, without putting the password in chat or email. Encrypted locally, no recipient account.",
    keywords: [
      "send password securely",
      "share password safely",
      "one-time password link",
      "encrypted password sharing",
      "sotto",
    ],
    h1: "Send a password that can only be read once.",
    lead: "Some secrets need a short-lived handover: a wifi password, a door code, a temporary login. Sotto wraps them in a link that stops working after its allowed views, without putting the password in chat or email.",
    ctaSecondary: { label: "Try a one-time link", href: "/one-time-secret-links" },
    stepsTitle: "One secret, one view",
    steps: [
      {
        head: "Create the link.",
        body: "One command, from a secret you already store or one you type on the spot.",
      },
      {
        head: "Send it anywhere.",
        body: "Text, email, chat. The channel sees only the complete link, which grants access while active unless you require a passphrase.",
      },
      {
        head: "First view burns it.",
        body: "The moment it is read, the link stops working. Screenshots of it are useless afterwards.",
      },
    ],
    terminal: [
      { text: "sotto share WIFI_PASSWORD", kind: "cmd" },
      { text: "share link (acme-api/dev) - burns after 1 view(s):", kind: "dim" },
      {
        text: "https://getsotto.co.uk/s/00112233445566778899aabbccddeeff#AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8",
        kind: "value",
      },
    ],
    faqs: [
      {
        q: "Does my mum need to install anything?",
        a: "No. She opens the link, reads the password, and the link burns. A browser is the whole requirement.",
      },
      {
        q: "What if she opens it again later?",
        a: "She gets a message that the link is no longer valid. If she needs the password again, send a fresh link.",
      },
      {
        q: "Can I add a passphrase on top?",
        a: "Yes. `sotto share WIFI_PASSWORD --passphrase` prompts for one, so reading the link needs the link and the phrase.",
      },
    ],
    closingTitle: "Stop texting passwords in plain text",
    closingBody:
      "Create a free one-time link in under a minute; the recipient needs no account. Learn more about [sharing secrets securely](/share-secrets-securely).",
  },
  {
    slug: "self-hosted-secret-management",
    navLabel: "Self-hosting",
    tabTitle: "Self-Hosted Secret Manager with Docker Compose | Sotto",
    description:
      "Run your own self-hosted secret manager with one Docker Compose stack. Apache-2.0, zero-knowledge ciphertext only, no plan limits on your own server.",
    keywords: [
      "self-hosted secret manager",
      "self-hosted secret management",
      "docker secret manager",
      "open-source secret server",
      "sotto",
    ],
    h1: "Secret management you can self-host.",
    lead: "Sotto is an Apache-2.0 secret manager that ships as one Docker Compose stack: the sync server, Postgres, and Caddy for HTTPS. Your devices hold the keys; the box only ever sees ciphertext.",
    ctaSecondary: { label: "Open-source secret manager", href: "/open-source-secret-manager" },
    stepsTitle: "Your box in three moves",
    steps: [
      {
        head: "Start the box.",
        body: "Point DNS at a machine, fill in a handful of variables, and bring the stack up. The [deployment runbook](https://github.com/getsotto/sotto/blob/main/deploy/README.md) walks through each one.",
      },
      {
        head: "Log in against your own server.",
        body: "Point the CLI and the web vault at your origin. Same app, same flow, your infrastructure.",
      },
      {
        head: "Keep it yours.",
        body: "Backups run from one script, and the anonymous version ping switches off with `SOTTO_TELEMETRY=off`.",
      },
    ],
    terminal: [
      { text: "curl https://secrets.example.com/health", kind: "cmd" },
      { text: "ok", kind: "dim" },
    ],
    faqs: [
      {
        q: "What leaves my box?",
        a: "By default, one anonymous version ping a day. Set `SOTTO_TELEMETRY=off` to stop even that.",
      },
      {
        q: "What do I need?",
        a: "A box with Docker, a domain with DNS pointed at it, and a GitHub OAuth app for logins. The deploy runbook covers all three.",
      },
      {
        q: "Who can read my secrets?",
        a: "Only devices holding a grant. The server enforces the grant graph but stores ciphertext it cannot decrypt.",
      },
    ],
    closingTitle: "Your box, your ciphertext",
    closingBody:
      "Apache-2.0 and self-hostable with no plan limits or hosted-service checks. Follow the [deployment runbook](https://github.com/getsotto/sotto/blob/main/deploy/README.md) to run your own server.",
  },
  {
    slug: "open-source-secret-manager",
    navLabel: "Open-source secret manager",
    tabTitle: "Open-Source Zero-Knowledge Secret Manager | Sotto",
    description:
      "Sotto (getsotto) is an Apache-2.0, zero-knowledge secret manager built in Rust. Encrypt secrets locally, sync ciphertext, and inspect every line of code.",
    keywords: [
      "open source secret manager",
      "open-source secret manager",
      "zero-knowledge secret manager",
      "encrypted secret manager",
      "sotto secret manager",
      "getsotto",
    ],
    h1: "An open-source secret manager with zero-knowledge crypto.",
    lead: "Sotto (`getsotto`) is an Apache-2.0 secret manager built around one Rust cryptographic core shared by the native CLI and the browser WebAssembly vault. Plaintext never leaves your devices, whether you run locally, self-host, or sync through Sotto Cloud.",
    ctaSecondary: { label: "CLI secret manager", href: "/cli-secret-manager" },
    stepsTitle: "Verifiable security, three ways to run",
    steps: [
      {
        head: "One Rust crypto core everywhere.",
        body: "Argon2id key derivation, XChaCha20-Poly1305 AEAD, and X25519 sealed-box team grants compile to native binaries and WebAssembly, verified by byte-for-byte golden vectors in CI.",
      },
      {
        head: "Local-first, self-hosted, or Cloud.",
        body: "Start offline with an encrypted vault on your machine, bring up your own [self-hosted server](/self-hosted-secret-management) with Docker Compose, or sync via `getsotto.co.uk`.",
      },
      {
        head: "Transparent and Apache-2.0 licensed.",
        body: "Read the [threat model](https://github.com/getsotto/sotto/blob/main/THREAT-MODEL.md), verify Sigstore-signed releases, and inspect every line of the CLI, server, and web client on GitHub.",
      },
    ],
    terminal: [
      { text: "sotto init", kind: "cmd" },
      { text: "initialised `acme-api` (dev)", kind: "dim" },
      { text: "sotto set DATABASE_URL", kind: "cmd" },
      { text: "Value:", kind: "dim" },
      { text: "set DATABASE_URL (acme-api/dev)", kind: "dim" },
      { text: "sotto run -- npm start", kind: "cmd" },
      { text: "ready on http://localhost:3000", kind: "value" },
    ],
    faqs: [
      {
        q: "Has Sotto had a third-party cryptographic audit?",
        a: "Not yet. Sotto is pre-1.0 and publishes its threat model, Kani proofs, fuzz targets, and golden vectors openly. Use it for development and staging secrets until an independent audit is complete.",
      },
      {
        q: "How does team access work in a zero-knowledge secret manager?",
        a: "Each environment has its own encryption key. Granting access seals that key to a teammate's X25519 public key, and removing a member rotates the environment key so past grants stop working.",
      },
      {
        q: "Is Sotto free to use?",
        a: "Yes. Local usage and self-hosting are free under the Apache-2.0 licence with no plan limits. Optional hosted sync is available through Sotto Cloud.",
      },
    ],
    closingTitle: "Inspect the code, keep the keys",
    closingBody:
      "Install Sotto on macOS, Linux, or Windows, or read the source at [github.com/getsotto/sotto](https://github.com/getsotto/sotto).",
  },
  {
    slug: "cli-secret-manager",
    navLabel: "CLI secret manager",
    tabTitle: "CLI Secret Manager for Developers | Sotto",
    description:
      "Manage encrypted secrets from your terminal with Sotto. Inject env vars with sotto run, diff environments, and work offline on macOS, Linux, and Windows.",
    keywords: [
      "cli secret manager",
      "command line secret manager",
      "terminal secret manager",
      "developer secret manager",
      "sotto cli",
    ],
    h1: "A CLI secret manager built for your terminal.",
    lead: "Store secrets in an encrypted local vault, switch between `dev`, `staging`, and `prod`, and inject variables into any command with `sotto run`. No plaintext files on disk, no shell history leaks, and full offline support.",
    ctaSecondary: { label: "Dotenv alternative", href: "/dotenv-alternative" },
    stepsTitle: "From install to running your app",
    steps: [
      {
        head: "Store secrets without shell history leaks.",
        body: "`sotto set` reads values from a hidden prompt or stdin and encrypts them before they touch disk. `sotto get -c` copies a value and clears the clipboard after 45 seconds.",
      },
      {
        head: "Inject secrets into any process.",
        body: "`sotto run -- npm start` passes the active environment's secrets directly to the child process. Your parent shell stays clean and nothing is written to a `.env` file.",
      },
      {
        head: "Manage multiple environments cleanly.",
        body: "Use `sotto env use`, `sotto env diff`, and `sotto env copy` to compare and promote configuration across `dev`, `staging`, and `prod`.",
      },
    ],
    terminal: [
      { text: "sotto env use staging", kind: "cmd" },
      { text: "active environment: staging", kind: "dim" },
      { text: "sotto env diff dev staging", kind: "cmd" },
      { text: "! DATABASE_URL", kind: "dim" },
      { text: "< DEBUG_MODE", kind: "dim" },
      { text: "sotto run -- npm start", kind: "cmd" },
      { text: "ready on http://localhost:3000", kind: "value" },
    ],
    faqs: [
      {
        q: "Which operating systems does the Sotto CLI support?",
        a: "Prebuilt, Sigstore-signed binaries ship for macOS (Apple Silicon and Intel), Linux (x86_64 and ARM64), and Windows x86_64, plus a Homebrew preview tap on macOS.",
      },
      {
        q: "Does the CLI work offline without an account?",
        a: "Yes. Every local command (`init`, `set`, `get`, `ls`, `import`, `export`, `run`, `env`) works completely offline with no account and no server.",
      },
      {
        q: "Does Sotto support shell completions and an interactive view?",
        a: "Yes. Run `sotto completions bash`, `zsh`, or `fish` for tab completion, or launch the built-in terminal dashboard to browse environments and history.",
      },
    ],
    closingTitle: "Keep secrets out of your shell history",
    closingBody:
      "Install the Sotto CLI in one command and pair it with [CI/CD secrets](/ci-cd-secret-manager) or [encrypted .env sharing](/share-env-files).",
  },
  {
    slug: "dotenv-alternative",
    navLabel: "Dotenv alternative",
    tabTitle: "Encrypted Dotenv Alternative for .env Files | Sotto",
    description:
      "Replace scattered plaintext .env files with Sotto. Import existing dotenv files into an encrypted vault, inject secrets in memory, and sync teams safely.",
    keywords: [
      "dotenv alternative",
      "env file manager",
      "encrypted .env",
      "environment variable manager",
      "manage .env files",
      "sotto",
    ],
    h1: "An encrypted dotenv alternative that stays out of your way.",
    lead: "Plaintext `.env` files drift out of date, leak into screenshots, and accidentally get committed. Sotto imports your existing `.env` in one command, encrypts every value locally, and injects them only when your app runs.",
    ctaSecondary: { label: "Share .env files", href: "/share-env-files" },
    stepsTitle: "Leave plaintext .env files behind",
    steps: [
      {
        head: "Import your existing .env file.",
        body: "Run `sotto import .env` to pull every key into your encrypted project vault. Commit `sotto.toml` safely; it stores only project metadata, never secret values.",
      },
      {
        head: "Run apps without writing plaintext to disk.",
        body: "Prefix your dev or test command with `sotto run --` so secrets exist only in the child process environment. Export back to dotenv, shell, or JSON whenever a tool requires a file.",
      },
      {
        head: "Diff and sync across environments.",
        body: "Compare `dev`, `staging`, and `prod` with `sotto env diff` without printing secret values, and sync updates across your team as ciphertext.",
      },
    ],
    terminal: [
      { text: "sotto import .env", kind: "cmd" },
      { text: "imported 14 secret(s) into acme-api (dev)", kind: "dim" },
      { text: "sotto run -- npm start", kind: "cmd" },
      { text: "ready on http://localhost:3000", kind: "value" },
    ],
    faqs: [
      {
        q: "Do I have to change my application code to stop using .env?",
        a: "No. Your app still reads standard environment variables (`process.env`, `os.environ`, `std::env`). `sotto run -- <cmd>` injects them before the process starts.",
      },
      {
        q: "Can I still export a .env file if another tool expects one?",
        a: "Yes. `sotto export --format dotenv --reveal` prints dotenv syntax, and `--format` also supports `shell` and `json`. On a terminal it refuses to print plaintext unless you pass `--reveal`.",
      },
      {
        q: "Is sotto.toml safe to commit to Git?",
        a: "Yes. `sotto.toml` contains only the project id, project name, default environment, and optional organisation id. It holds no secret values and grants no access on its own.",
      },
    ],
    closingTitle: "Stop chasing out-of-date .env files",
    closingBody:
      "Import your project's `.env` into Sotto in under a minute, or see how to [share .env files with your team](/share-env-files).",
  },
  {
    slug: "ci-cd-secret-manager",
    navLabel: "CI/CD secrets",
    tabTitle: "CI/CD Secret Manager & GitHub Actions | Sotto",
    description:
      "Inject encrypted secrets into GitHub Actions and CI/CD pipelines with Sotto. Use scoped, expiring machine tokens (SOTTO_TOKEN) and revoke access in one command.",
    keywords: [
      "ci/cd secret manager",
      "github actions secret manager",
      "inject secrets in ci",
      "machine tokens",
      "devops secret manager",
      "sotto",
    ],
    h1: "Inject encrypted secrets into CI/CD pipelines.",
    lead: "Stop copying every API key into every CI provider by hand. Issue a scoped, expiring machine token for one environment, pass `SOTTO_TOKEN` to your pipeline, and let `sotto run` decrypt secrets directly inside the runner.",
    ctaSecondary: { label: "CLI secret manager", href: "/cli-secret-manager" },
    stepsTitle: "One scoped token per pipeline",
    steps: [
      {
        head: "Issue an expiring machine token.",
        body: "`sotto token create` wraps a single environment's key for CI with an expiry date and accountable issuer in the organisation audit log.",
      },
      {
        head: "Install Sotto in GitHub Actions or any runner.",
        body: "Use `getsotto/sotto-action` to install a pinned, checksum- and Sigstore-verified release, and set `SOTTO_SERVER` and `SOTTO_TOKEN`.",
      },
      {
        head: "Revoke and rotate without touching every key.",
        body: "Run `sotto token revoke` and `sotto rotate` to cut off a compromised pipeline token immediately without re-copying secrets across repositories.",
      },
    ],
    terminal: [
      { text: "sotto token create --name github-ci --expires-in-days 90", kind: "cmd" },
      {
        text: "machine token `github-ci` for acme-api/staging, expires 2027-02-22T00:00:00Z - save it now; it is never shown again:",
        kind: "dim",
      },
      { text: "smt_0011223344556677.MT1-XXXX-XXXX-XXXX", kind: "value" },
      { text: "SOTTO_SERVER=https://getsotto.co.uk SOTTO_TOKEN=... sotto run -- npm test", kind: "cmd" },
      { text: "ok", kind: "dim" },
    ],
    faqs: [
      {
        q: "Why does Sotto require SOTTO_SERVER in CI?",
        a: "When `SOTTO_TOKEN` is set, `sotto run` and `sotto export` never fall back to a default server. That guarantees a machine token is only sent to the exact server URL you configured.",
      },
      {
        q: "Can a machine token read other environments or projects?",
        a: "No. Each machine token is scoped to a single environment and can be given an expiry date and revoked independently.",
      },
      {
        q: "Does this work with a self-hosted Sotto server?",
        a: "Yes. Set `SOTTO_SERVER` to your self-hosted origin and `getsotto/sotto-action` or the standalone binary fetches and decrypts ciphertext from your own server.",
      },
    ],
    closingTitle: "Keep CI pipelines in sync automatically",
    closingBody:
      "Update a secret once in Sotto and every authorised pipeline picks it up on the next run. Works with Sotto Cloud or a [self-hosted secret manager](/self-hosted-secret-management).",
  },
];

export function guideBySlug(slug: string): SeoPageData | undefined {
  return guidePages.find((page) => page.slug === slug);
}
