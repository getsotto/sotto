import { useEffect, useState } from "react";

import "./styles/landing.css";

const REPO = "https://github.com/getsotto/sotto";
const INSTALL_CMD =
  "curl -fsSL https://raw.githubusercontent.com/getsotto/sotto/main/install.sh | sh";

// The marketing page an anonymous visitor gets at `/`. The vault app lives at /app; this page's
// job is the top of the funnel: see it → install it. Everything is real, selectable text — the
// terminal below is a transcript of actual CLI output, not an image.
export function Landing() {
  return (
    <main className="landing">
      <header>
        <span className="wordmark">Sotto</span>
        <nav aria-label="Site">
          <a href="#how">How it works</a>
          <a href="#trust">Trust</a>
          <a href="#pricing">Pricing</a>
          <a href={REPO}>GitHub</a>
          <a className="login" href="/app">
            Log in
          </a>
        </nav>
      </header>

      <section className="hero">
        <h1>
          Stop Slacking your <code>.env</code> files.
        </h1>
        <p className="lead">
          Sotto syncs secrets across your team with end-to-end encryption. Values are encrypted on
          your machine before they leave it and decrypted only on your teammates&rsquo; — the
          server stores ciphertext it cannot read.
        </p>
        <div className="install">
          <code>{INSTALL_CMD}</code>
          <CopyButton text={INSTALL_CMD} />
        </div>
        <p className="muted">
          Signed binaries for macOS and Linux. The installer verifies the checksum — and the
          Sigstore signature, when <code>cosign</code> is installed. Prefer to{" "}
          <a href={`${REPO}/blob/main/install.sh`}>read it first</a>? Or grab a tarball from{" "}
          <a href={`${REPO}/releases`}>releases</a>.
        </p>
      </section>

      <Terminal />

      <section id="how">
        <h2>How it works</h2>
        <ol className="steps">
          <li>
            <strong>Encrypt locally.</strong> Your vault key is derived on your machine from your
            master password and secret key. Neither is ever sent anywhere.
          </li>
          <li>
            <strong>Sync ciphertext.</strong> The server stores and versions encrypted blobs. It
            never receives a plaintext value or a usable key — there is nothing on it worth
            stealing.
          </li>
          <li>
            <strong>Decrypt on your devices.</strong> One Rust crypto core runs everywhere: the CLI
            natively, the browser through WebAssembly — with golden vectors in CI proving both
            produce identical bytes.
          </li>
        </ol>
        <p>
          Teams work the same way: sharing an environment grants its key to a member (an X25519
          sealed box), so access is cryptographic — not a permission bit on the server. Removing a
          member rotates the keys.
        </p>
      </section>

      <section id="trust">
        <h2>Should you trust this?</h2>
        <p>
          Not blindly. Sotto is pre-1.0 and has <strong>not had a third-party cryptographic
          audit</strong> yet — you should know that before putting anything important in it. What
          you can verify yourself, today:
        </p>
        <ul>
          <li>
            A published <a href={`${REPO}/blob/main/THREAT-MODEL.md`}>threat model</a> with
            explicit non-goals.
          </li>
          <li>
            One shared crypto core — the CLI and the browser client run the same Rust code, held
            to byte-for-byte golden vectors in CI.
          </li>
          <li>
            Sigstore-signed releases with a documented{" "}
            <a href={`${REPO}/blob/main/SECURITY.md`}>verification procedure</a>.
          </li>
          <li>
            Telemetry is four anonymous fields, opt-out, and pinned by a unit test so it cannot
            quietly grow.
          </li>
          <li>Apache-2.0, and self-hostable from one docker-compose.</li>
        </ul>
        <p>
          Honest guidance: use it for your team&rsquo;s development and staging secrets today.
          Keep the production crown jewels where they are until the audit.
        </p>
      </section>

      <section id="pricing">
        <h2>Pricing</h2>
        <div className="plans">
          <div className="plan">
            <h3>Free</h3>
            <p className="price">$0</p>
            <ul>
              <li>Personal projects — unlimited, free forever</li>
              <li>Organizations with up to 3 members and 1 shared project</li>
              <li>One-time, burn-after-reading share links</li>
              <li>Every new org starts a 14-day Team trial</li>
            </ul>
          </div>
          <div className="plan">
            <h3>Team</h3>
            <p className="price">
              $15<span className="per"> / month per organization</span>
            </p>
            <ul>
              <li>Unlimited members</li>
              <li>Unlimited shared projects</li>
              <li>Audit log</li>
              <li>Flat — the price doesn&rsquo;t scale with team size</li>
            </ul>
          </div>
        </div>
        <p className="muted">
          Or run it yourself: the <a href={`${REPO}/blob/main/deploy/README.md`}>server is
          self-hostable</a> and Apache-2.0. Self-hosting has no tiers.
        </p>
      </section>

      <section id="start">
        <h2>Get started</h2>
        <pre className="quickstart">
          <code>{`sotto init                   # create your identity — SAVE the Emergency Kit
sotto set DATABASE_URL       # hidden prompt; encrypted before it touches disk
sotto run -- npm start       # inject secrets into any command
sotto login && sotto push    # optional: sync ciphertext via getsotto.co.uk
sotto share DATABASE_URL     # one-time link for a single secret`}</code>
        </pre>
        <p>
          Sotto works fully offline until you <code>sotto login</code> — sync is a feature, not a
          requirement. The web vault at this address decrypts in your browser, with keys that
          never leave your devices.
        </p>
      </section>

      <footer>
        <nav aria-label="Footer">
          <a href={REPO}>GitHub</a>
          <a href={`${REPO}/releases`}>Releases</a>
          <a href={`${REPO}/blob/main/THREAT-MODEL.md`}>Threat model</a>
          <a href={`${REPO}/blob/main/SECURITY.md`}>Security policy</a>
          <a href={`${REPO}/blob/main/deploy/README.md`}>Run your own</a>
          <a href="/app">Log in</a>
        </nav>
        <p className="muted">
          Sotto — from <em>sotto voce</em>: in a low voice, in confidence. Apache-2.0.
        </p>
      </footer>
    </main>
  );
}

function CopyButton({ text }: { text: string }) {
  const [copied, setCopied] = useState(false);

  useEffect(() => {
    if (!copied) {
      return;
    }
    const timer = setTimeout(() => setCopied(false), 2000);
    return () => clearTimeout(timer);
  }, [copied]);

  return (
    <button
      className="sm"
      aria-live="polite"
      onClick={() => {
        void navigator.clipboard.writeText(text).then(() => setCopied(true));
      }}
    >
      {copied ? "Copied" : "Copy"}
    </button>
  );
}

// A transcript of a real session — the strings match what the CLI actually prints. CSS reveals it
// line by line on load (see landing.css); with reduced motion, or once finished, it is simply a
// static, selectable code block. The share link is the page's one loud value.
function Terminal() {
  return (
    <pre className="term">
      <code>
        <span className="line l1">
          <span className="prompt">$ </span>
          <span className="cmd c1">sotto init</span>
        </span>
        {"\n"}
        <span className="line l2 dim">{"  Save your Emergency Kit — these cannot be recovered:"}</span>
        {"\n"}
        <span className="line l3 dim">
          {"    Secret Key:   "}
          <span className="accent">SK1-9FKQ-XXXX-XXXX-XXXX</span>
        </span>
        {"\n"}
        <span className="line l4 dim">{"initialized `acme-api` (dev)"}</span>
        {"\n\n"}
        <span className="line l5">
          <span className="prompt">$ </span>
          <span className="cmd c2">sotto set DATABASE_URL</span>
        </span>
        {"\n"}
        <span className="line l6 dim">Value:</span>
        {"\n"}
        <span className="line l7 dim">set DATABASE_URL (acme-api/dev)</span>
        {"\n\n"}
        <span className="line l8">
          <span className="prompt">$ </span>
          <span className="cmd c3">sotto run -- npm start</span>
        </span>
        {"\n"}
        <span className="line l9">ready on http://localhost:3000</span>
        {"\n\n"}
        <span className="line l10">
          <span className="prompt">$ </span>
          <span className="cmd c4">sotto push</span>
        </span>
        {"\n"}
        <span className="line l11 dim">pushed acme-api/dev — revision 1</span>
        {"\n\n"}
        <span className="line l12">
          <span className="prompt">$ </span>
          <span className="cmd c5">sotto share DATABASE_URL</span>
        </span>
        {"\n"}
        <span className="line l13 dim">{"share link (acme-api/dev) — burns after 1 view(s):"}</span>
        {"\n"}
        <span className="line l14 value">{"https://getsotto.co.uk/s/9fK2xQ#k=Vq3TzEjm…"}</span>
        {"\n\n"}
        <span className="line l15">
          <span className="prompt">$ </span>
          <span className="cursor" />
        </span>
      </code>
    </pre>
  );
}
