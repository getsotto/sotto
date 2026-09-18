// Guide page content: one entry per indexable guide route (`/<slug>`).
//
// Each entry is the single source of truth for its page. React renders it
// (`SeoPage`) and the build inlines the identical string for crawlers
// (`snapshot.ts`), so the two cannot drift apart by construction.
//
// House rules, same as the landing page: British English, no em or en dashes.
// Inline `backticks` become <code> in prose fields; terminal lines are literal
// CLI transcripts and must match what the binary actually prints (see the
// format strings in crates/cli/src/main.rs).
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

export const guidePages: SeoPageData[] = [
  {
    slug: "share-secrets-securely",
    navLabel: "Share secrets",
    tabTitle: "Sotto: share secrets securely with end-to-end encryption",
    description:
      "Send passwords, tokens, and keys without pasting them into chat. Encrypted on your machine and readable through the complete link while it is active.",
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
      { text: "https://getsotto.co.uk/s/00112233445566778899aabbccddeeff#AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8", kind: "value" },
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
    closingBody: "Free for personal use. One command to install, one command to share your first secret.",
  },
  {
    slug: "share-env-files",
    navLabel: "Share .env files",
    tabTitle: "Share .env files with your team, encrypted | Sotto",
    description:
      "Your .env holds every key your app needs. Share it with your team encrypted end to end, never as a screenshot or a Slack paste again.",
    h1: "Share .env files without the screenshot dance.",
    lead: "Your `.env` holds every key your app needs, and today it travels by screenshot, Slack paste, or shoulder surf. Import it into Sotto and share it with your team encrypted end to end.",
    ctaSecondary: { label: "How sharing works", href: "/share-secrets-securely" },
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
    closingBody: "Free for teams of up to three, with one shared project. Import your .env in under a minute.",
  },
  {
    slug: "one-time-secret-links",
    navLabel: "One-time links",
    tabTitle: "One-time secret links that burn after reading | Sotto",
    description:
      "Create a link that reveals a secret exactly once, then stops working. No account needed for the recipient.",
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
      { text: "https://getsotto.co.uk/s/00112233445566778899aabbccddeeff#AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8", kind: "value" },
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
    closingBody: "Free for personal use. No recipient account required, ever.",
  },
  {
    slug: "share-api-keys-securely",
    navLabel: "Share API keys",
    tabTitle: "Share API keys with your team, encrypted | Sotto",
    description:
      "API keys unlock billing, email, and infrastructure. Share them with teammates encrypted end to end, and rotate them in one place.",
    h1: "Share API keys without pasting them into chat.",
    lead: "An API key in chat is a leak with extra steps. Store keys in Sotto, share them with the teammates who need them, and rotate them in one place when they leak.",
    ctaSecondary: { label: "Share .env files", href: "/share-env-files" },
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
      { text: "https://getsotto.co.uk/s/00112233445566778899aabbccddeeff#AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8", kind: "value" },
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
    closingBody: "Free for personal use, and for teams of up to three sharing one project.",
  },
  {
    slug: "send-password-securely",
    navLabel: "Send passwords",
    tabTitle: "Send a password securely with a one-time link | Sotto",
    description:
      "Send a password through a link that stops working after one read, without putting the password in chat or email. No recipient account needed.",
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
      { text: "https://getsotto.co.uk/s/00112233445566778899aabbccddeeff#AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8", kind: "value" },
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
    closingBody: "Free for personal use. First burning link in under a minute.",
  },
  {
    slug: "self-hosted-secret-management",
    navLabel: "Self-hosting",
    tabTitle: "Self-hosted secret management in one command | Sotto",
    description:
      "Run your own secret sync server with one docker compose file. Apache-2.0, ciphertext only, your keys never leave your devices.",
    h1: "Secret management you can self-host.",
    lead: "Sotto is Apache-2.0 and ships as one compose file: the sync server, Postgres, and Caddy for HTTPS. Your devices hold the keys; the box only ever sees ciphertext.",
    ctaSecondary: { label: "How sharing works", href: "/share-secrets-securely" },
    stepsTitle: "Your box in three moves",
    steps: [
      {
        head: "Start the box.",
        body: "Point DNS at a machine, fill in a handful of variables, and bring the stack up. The deploy runbook walks through each one.",
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
    closingBody: "Apache-2.0 and self-hostable. Organisations use the same plan limits by default: Free allows up to 3 members and 1 shared project. Operators can assign tiers manually; see the [deployment runbook](https://github.com/getsotto/sotto/blob/main/deploy/README.md#billing-optional).",
  },
];

export function guideBySlug(slug: string): SeoPageData | undefined {
  return guidePages.find((page) => page.slug === slug);
}
