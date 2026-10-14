import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { chromium } from '@playwright/test';

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);
const brandDir = __dirname;
const publicDir = path.resolve(__dirname, '../public');

// --- SVG DEFINITIONS ---

/**
 * The Interlocking Mechanical 'S' - Mark Only (Transparent background)
 * Precision-engineered from two interlocking titanium tumblers around a cryogenic cyan cipher core.
 */
function renderMarkSvg({ width = 512, height = 512, mode = 'dark', viewBox = '106 106 300 300' } = {}) {
  const isDark = mode === 'dark';
  const strokeColor = isDark ? '#334155' : '#CBD5E1';
  const cyanCore = '#06B6D4';
  const cyanGlow = '#38BDF8';
  const cyanBright = '#67E8F9';
  
  // Titanium gradients
  const tGrad1Start = isDark ? '#F1F5F9' : '#0F172A';
  const tGrad1End = isDark ? '#94A3B8' : '#334155';
  const tGrad2Start = isDark ? '#E2E8F0' : '#1E293B';
  const tGrad2End = isDark ? '#64748B' : '#475569';

  return `<?xml version="1.0" encoding="UTF-8"?>
<svg xmlns="http://www.w3.org/2000/svg" width="${width}" height="${height}" viewBox="${viewBox}" fill="none" role="img" aria-label="Sotto Mark">
  <defs>
    <!-- Titanium Upper Tumbler Gradient -->
    <linearGradient id="titaniumUpper" x1="120" y1="90" x2="380" y2="300" gradientUnits="userSpaceOnUse">
      <stop offset="0%" stop-color="${tGrad1Start}" />
      <stop offset="60%" stop-color="${tGrad1End}" />
      <stop offset="100%" stop-color="${tGrad2End}" />
    </linearGradient>

    <!-- Titanium Lower Tumbler Gradient -->
    <linearGradient id="titaniumLower" x1="392" y1="422" x2="132" y2="212" gradientUnits="userSpaceOnUse">
      <stop offset="0%" stop-color="${tGrad1Start}" />
      <stop offset="60%" stop-color="${tGrad1End}" />
      <stop offset="100%" stop-color="${tGrad2End}" />
    </linearGradient>

    <!-- Cryo Cyan Core Glow Gradient -->
    <linearGradient id="cyanCoreGrad" x1="220" y1="220" x2="292" y2="292" gradientUnits="userSpaceOnUse">
      <stop offset="0%" stop-color="${cyanBright}" />
      <stop offset="50%" stop-color="${cyanGlow}" />
      <stop offset="100%" stop-color="${cyanCore}" />
    </linearGradient>

    <!-- Cryo Glow Filter -->
    <filter id="coreGlow" x="180" y="180" width="152" height="152" filterUnits="userSpaceOnUse">
      <feGaussianBlur stdDeviation="12" result="blur" />
      <feComposite in="SourceGraphic" in2="blur" operator="over" />
    </filter>
  </defs>

  <!-- Cryo Cyan Radiant Energy Behind Core -->
  <circle cx="256" cy="256" r="36" fill="${cyanGlow}" opacity="${isDark ? '0.25' : '0.15'}" filter="url(#coreGlow)" />

  <!-- UPPER MECHANICAL TUMBLER / J-SHACKLE -->
  <!-- Top arm sweeps from top-right, curves through upper crown, down left wall, facets inward to interlock notch -->
  <path d="
    M 376 156
    L 336 108
    L 204 108
    C 152 108 116 144 116 196
    C 116 236 140 264 176 284
    L 242 320
    L 268 304
    L 220 274
    C 192 258 178 240 178 214
    C 178 188 198 168 228 168
    L 326 168
    L 376 156
    Z
  " fill="url(#titaniumUpper)" stroke="${strokeColor}" stroke-width="2" stroke-linejoin="round" />

  <!-- LOWER MECHANICAL TUMBLER / J-SHACKLE (Inverted Symmetry) -->
  <!-- Bottom arm sweeps from bottom-left, curves through lower crown, up right wall, facets inward to interlock notch -->
  <path d="
    M 136 356
    L 176 404
    L 308 404
    C 360 404 396 368 396 316
    C 396 276 372 248 336 228
    L 270 192
    L 244 208
    L 292 238
    C 320 254 334 272 334 298
    C 334 324 314 344 284 344
    L 186 344
    L 136 356
    Z
  " fill="url(#titaniumLower)" stroke="${strokeColor}" stroke-width="2" stroke-linejoin="round" />

  <!-- INTERLOCKING CRYOGENIC CIPHER CORE (Precision Diamond / Bevel Keyway) -->
  <polygon points="256,224 288,256 256,288 224,256" fill="url(#cyanCoreGrad)" />
  <circle cx="256" cy="256" r="4" fill="${isDark ? '#0B0F17' : '#FFFFFF'}" />
</svg>`;
}

/**
 * The Master Favicon / App Icon (With Slate/Titanium Enclosure Plate)
 */
function renderAppIconSvg({ width = 512, height = 512 } = {}) {
  const cyanCore = '#06B6D4';
  const cyanGlow = '#38BDF8';
  const cyanBright = '#67E8F9';

  return `<?xml version="1.0" encoding="UTF-8"?>
<svg xmlns="http://www.w3.org/2000/svg" width="${width}" height="${height}" viewBox="0 0 512 512" fill="none" role="img" aria-label="Sotto App Icon">
  <defs>
    <!-- Background Vault Plate Gradient -->
    <linearGradient id="vaultPlate" x1="0" y1="0" x2="512" y2="512" gradientUnits="userSpaceOnUse">
      <stop offset="0%" stop-color="#1E293B" />
      <stop offset="40%" stop-color="#0F172A" />
      <stop offset="100%" stop-color="#050811" />
    </linearGradient>

    <!-- Hairline Chamfer Border -->
    <linearGradient id="plateBorder" x1="0" y1="0" x2="512" y2="512" gradientUnits="userSpaceOnUse">
      <stop offset="0%" stop-color="#475569" />
      <stop offset="50%" stop-color="#1E293B" />
      <stop offset="100%" stop-color="#334155" />
    </linearGradient>

    <!-- Ambient Core Glow -->
    <radialGradient id="radialCyan" cx="256" cy="256" r="220" gradientUnits="userSpaceOnUse">
      <stop offset="0%" stop-color="#06B6D4" stop-opacity="0.32" />
      <stop offset="50%" stop-color="#0284C7" stop-opacity="0.08" />
      <stop offset="100%" stop-color="#000000" stop-opacity="0" />
    </radialGradient>

    <!-- Upper Tumbler Metallic -->
    <linearGradient id="tumblerUpper" x1="120" y1="90" x2="380" y2="300" gradientUnits="userSpaceOnUse">
      <stop offset="0%" stop-color="#FFFFFF" />
      <stop offset="40%" stop-color="#CBD5E1" />
      <stop offset="100%" stop-color="#64748B" />
    </linearGradient>

    <!-- Lower Tumbler Metallic -->
    <linearGradient id="tumblerLower" x1="392" y1="422" x2="132" y2="212" gradientUnits="userSpaceOnUse">
      <stop offset="0%" stop-color="#F8FAFC" />
      <stop offset="40%" stop-color="#94A3B8" />
      <stop offset="100%" stop-color="#475569" />
    </linearGradient>

    <!-- Core Cyan Gradient -->
    <linearGradient id="cipherCyan" x1="220" y1="220" x2="292" y2="292" gradientUnits="userSpaceOnUse">
      <stop offset="0%" stop-color="${cyanBright}" />
      <stop offset="60%" stop-color="${cyanGlow}" />
      <stop offset="100%" stop-color="${cyanCore}" />
    </linearGradient>
  </defs>

  <!-- Enclosure Plate (Micro-Radii Squircle with Chamfered Precision Feel) -->
  <rect x="16" y="16" width="480" height="480" rx="108" fill="url(#vaultPlate)" stroke="url(#plateBorder)" stroke-width="4" />

  <!-- Subtle Precision Grid Crosshairs in Background -->
  <line x1="256" y1="48" x2="256" y2="88" stroke="#334155" stroke-width="2" stroke-linecap="round" opacity="0.6" />
  <line x1="256" y1="424" x2="256" y2="464" stroke="#334155" stroke-width="2" stroke-linecap="round" opacity="0.6" />
  <line x1="48" y1="256" x2="88" y2="256" stroke="#334155" stroke-width="2" stroke-linecap="round" opacity="0.6" />
  <line x1="424" y1="256" x2="464" y2="256" stroke="#334155" stroke-width="2" stroke-linecap="round" opacity="0.6" />

  <!-- Radial Cyan Field -->
  <circle cx="256" cy="256" r="210" fill="url(#radialCyan)" />

  <!-- UPPER MECHANICAL TUMBLER -->
  <path d="
    M 372 152
    L 332 108
    L 204 108
    C 152 108 116 144 116 196
    C 116 236 140 264 176 284
    L 242 320
    L 268 304
    L 220 274
    C 192 258 178 240 178 214
    C 178 188 198 168 228 168
    L 326 168
    L 372 152
    Z
  " fill="url(#tumblerUpper)" stroke="#1E293B" stroke-width="3" stroke-linejoin="round" />

  <!-- LOWER MECHANICAL TUMBLER -->
  <path d="
    M 140 360
    L 180 404
    L 308 404
    C 360 404 396 368 396 316
    C 396 276 372 248 336 228
    L 270 192
    L 244 208
    L 292 238
    C 320 254 334 272 334 298
    C 334 324 314 344 284 344
    L 186 344
    L 140 360
    Z
  " fill="url(#tumblerLower)" stroke="#1E293B" stroke-width="3" stroke-linejoin="round" />

  <!-- MECHANICAL CLEARANCE GAP LINE (Precision Seam Highlight) -->
  <path d="M 244 208 L 270 192 L 336 228" stroke="#38BDF8" stroke-width="3" stroke-linecap="round" opacity="0.8" />
  <path d="M 268 304 L 242 320 L 176 284" stroke="#38BDF8" stroke-width="3" stroke-linecap="round" opacity="0.8" />

  <!-- CIPHER CORE DIAMOND -->
  <polygon points="256,222 290,256 256,290 222,256" fill="url(#cipherCyan)" />
  <circle cx="256" cy="256" r="5" fill="#0B0F17" />
  <circle cx="256" cy="256" r="2.5" fill="#67E8F9" />
</svg>`;
}

/**
 * Full Horizontal Wordmark Lockup (Mark + SOTTO Wordmark)
 */
function renderLogoLockupSvg({ mode = 'dark', width = 480, height = 120 } = {}) {
  const isDark = mode === 'dark';
  const textColor = isDark ? '#F8FAFC' : '#0F172A';
  const tagColor = isDark ? '#38BDF8' : '#0284C7';
  const metaColor = isDark ? '#64748B' : '#94A3B8';

  return `<?xml version="1.0" encoding="UTF-8"?>
<svg xmlns="http://www.w3.org/2000/svg" width="${width}" height="${height}" viewBox="0 0 480 120" fill="none" role="img" aria-label="Sotto Logo">
  <defs>
    <!-- Scaled Mark Transform -->
  </defs>

  <!-- Scaled Mark matching sotto text cap-height -->
  <g transform="translate(20, 26) scale(0.098)">
    ${renderMarkSvg({ mode, width: 512, height: 512 }).replace(/<\?xml.*?\?>/, '').replace(/<svg.*?>/, '').replace(/<\/svg>/, '')}
  </g>

  <!-- SOTTO Technical Grotesque Wordmark -->
  <text x="82" y="66" font-family="'Space Grotesk', -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif" font-size="46" font-weight="700" letter-spacing="-0.04em" fill="${textColor}">sotto</text>

  <!-- Subtitle / Wedge Tagline -->
  <text x="84" y="90" font-family="'JetBrains Mono', 'SF Mono', ui-monospace, monospace" font-size="12" font-weight="500" letter-spacing="-0.01em" fill="${metaColor}">zero-knowledge secret sync</text>
</svg>`;
}

// --- MAIN GENERATION SCRIPT ---
async function generateAll() {
  console.log('Generating Sotto Engineered Industrial Brand Assets...');

  // Ensure directories exist
  fs.mkdirSync(brandDir, { recursive: true });
  fs.mkdirSync(publicDir, { recursive: true });

  // 1. Generate SVGs
  const markDarkSvg = renderMarkSvg({ mode: 'dark' });
  const markLightSvg = renderMarkSvg({ mode: 'light' });
  const appIconSvg = renderAppIconSvg();
  const logoDarkSvg = renderLogoLockupSvg({ mode: 'dark' });
  const logoLightSvg = renderLogoLockupSvg({ mode: 'light' });

  fs.writeFileSync(path.join(brandDir, 'mark-dark.svg'), markDarkSvg, 'utf8');
  fs.writeFileSync(path.join(brandDir, 'mark-light.svg'), markLightSvg, 'utf8');
  fs.writeFileSync(path.join(brandDir, 'app-icon.svg'), appIconSvg, 'utf8');
  fs.writeFileSync(path.join(brandDir, 'logo-dark.svg'), logoDarkSvg, 'utf8');
  fs.writeFileSync(path.join(brandDir, 'logo-light.svg'), logoLightSvg, 'utf8');

  // Copy marks to public for adaptive CSS usage
  fs.writeFileSync(path.join(publicDir, 'mark-dark.svg'), markDarkSvg, 'utf8');
  fs.writeFileSync(path.join(publicDir, 'mark-light.svg'), markLightSvg, 'utf8');
  fs.writeFileSync(path.join(publicDir, 'favicon.svg'), appIconSvg, 'utf8');
  console.log('✓ Vector SVGs written to web/brand/ and web/public/');

  // 2. Launch Chromium via Playwright to rasterize PNGs
  const browser = await chromium.launch();
  const page = await browser.newPage();

  const iconTargets = [
    { name: 'favicon-16.png', size: 16 },
    { name: 'favicon-32.png', size: 32 },
    { name: 'favicon-48.png', size: 48 },
    { name: 'apple-touch-icon.png', size: 180 },
    { name: 'icon-192.png', size: 192 },
    { name: 'icon-512.png', size: 512 },
    { name: 'icon-512-maskable.png', size: 512 },
  ];

  for (const target of iconTargets) {
    const html = `<!DOCTYPE html>
<html>
  <head>
    <style>
      * { margin: 0; padding: 0; box-sizing: border-box; }
      body { width: ${target.size}px; height: ${target.size}px; background: transparent; overflow: hidden; }
      svg { width: 100%; height: 100%; display: block; }
    </style>
  </head>
  <body>
    ${appIconSvg}
  </body>
</html>`;

    await page.setViewportSize({ width: target.size, height: target.size });
    await page.setContent(html);
    const buffer = await page.screenshot({ omitBackground: true });
    fs.writeFileSync(path.join(publicDir, target.name), buffer);
    console.log(`✓ Generated ${target.name} (${target.size}x${target.size})`);
  }

  // Generate Brand Preview Showcase Card in web/brand/
  const showcaseHtml = `<!DOCTYPE html>
<html>
  <head>
    <meta charset="utf-8">
    <style>
      * { margin: 0; padding: 0; box-sizing: border-box; }
      body {
        width: 1200px;
        height: 630px;
        background: #0B0F17;
        font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif;
        color: #F8FAFC;
        padding: 60px;
        display: flex;
        flex-direction: column;
        justify-content: space-between;
        border: 2px solid #1E293B;
      }
      .header {
        display: flex;
        justify-content: space-between;
        align-items: center;
      }
      .badge {
        font-family: ui-monospace, monospace;
        font-size: 13px;
        letter-spacing: 0.1em;
        color: #38BDF8;
        background: #111827;
        border: 1px solid #1E293B;
        padding: 6px 14px;
        border-radius: 4px;
      }
      .content {
        display: flex;
        gap: 60px;
        align-items: center;
      }
      .icon-box {
        width: 200px;
        height: 200px;
        flex-shrink: 0;
      }
      .icon-box svg {
        width: 100%;
        height: 100%;
        display: block;
      }
      .info h1 {
        font-size: 48px;
        font-weight: 700;
        letter-spacing: -0.03em;
        margin-bottom: 12px;
      }
      .info .tagline {
        font-size: 24px;
        color: #38BDF8;
        font-weight: 500;
        margin-bottom: 16px;
      }
      .info p {
        font-size: 17px;
        color: #94A3B8;
        max-width: 650px;
        line-height: 1.5;
      }
      .footer {
        display: flex;
        gap: 32px;
        border-top: 1px solid #1E293B;
        padding-top: 24px;
        font-family: ui-monospace, monospace;
        font-size: 13px;
        color: #64748B;
      }
      .swatch {
        display: inline-block;
        width: 12px;
        height: 12px;
        border-radius: 2px;
        margin-right: 6px;
        vertical-align: middle;
      }
    </style>
  </head>
  <body>
    <div class="header">
      <div style="width: 280px">${logoDarkSvg}</div>
      <div class="badge">ENGINEERED VAULT // V2</div>
    </div>
    <div class="content">
      <div class="icon-box">${appIconSvg}</div>
      <div class="info">
        <h1>Sotto</h1>
        <div class="tagline">Frictionless secret sync. Zero-knowledge by design.</div>
        <p>End-to-end encrypted secret sync for developer teams. Stop Slacking your .env files. Built on an audited Rust cryptographic core with micro-radii industrial precision.</p>
      </div>
    </div>
    <div class="footer">
      <div><span class="swatch" style="background:#06B6D4"></span>#06B6D4 Titanium Cyan</div>
      <div><span class="swatch" style="background:#0B0F17"></span>#0B0F17 Obsidian Slate</div>
      <div><span class="swatch" style="background:#94A3B8"></span>#94A3B8 Brushed Titanium</div>
      <div><span class="swatch" style="background:#1E293B"></span>#1E293B Hairline Border</div>
    </div>
  </body>
</html>`;

  await page.setViewportSize({ width: 1200, height: 630 });
  await page.setContent(showcaseHtml);
  const showcaseBuffer = await page.screenshot();
  fs.writeFileSync(path.join(brandDir, 'showcase.png'), showcaseBuffer);
  fs.writeFileSync(path.join(publicDir, 'og-image.png'), showcaseBuffer);
  console.log('✓ Generated showcase.png and og-image.png (1200x630)');

  // 3. Generate GitHub Organization / Repository Avatars (1024x1024 & 512x512)
  console.log('Generating GitHub Organization & Repository Avatar Assets...');
  const avatarDarkHtml = `<!DOCTYPE html>
<html>
  <head>
    <style>
      * { margin: 0; padding: 0; box-sizing: border-box; }
      body {
        width: 1024px;
        height: 1024px;
        background: #0B0F17;
        display: flex;
        align-items: center;
        justify-content: center;
      }
      .mark-box {
        width: 740px;
        height: 740px;
      }
      svg { width: 100%; height: 100%; display: block; }
    </style>
  </head>
  <body>
    <div class="mark-box">${markDarkSvg}</div>
  </body>
</html>`;

  await page.setViewportSize({ width: 1024, height: 1024 });
  await page.setContent(avatarDarkHtml);
  const avatar1024Buffer = await page.screenshot();
  fs.writeFileSync(path.join(brandDir, 'github-avatar-1024.png'), avatar1024Buffer);
  fs.writeFileSync(path.join(brandDir, 'github-avatar.png'), avatar1024Buffer);
  fs.writeFileSync(path.join(publicDir, 'github-avatar.png'), avatar1024Buffer);
  fs.writeFileSync(path.join(brandDir, 'org-avatar-dark.png'), avatar1024Buffer);
  console.log('✓ Generated github-avatar.png & org-avatar-dark.png (1024x1024)');

  // Light Mode Organization Avatar
  const avatarLightHtml = `<!DOCTYPE html>
<html>
  <head>
    <style>
      * { margin: 0; padding: 0; box-sizing: border-box; }
      body {
        width: 1024px;
        height: 1024px;
        background: #F8FAFC;
        display: flex;
        align-items: center;
        justify-content: center;
      }
      .mark-box {
        width: 740px;
        height: 740px;
      }
      svg { width: 100%; height: 100%; display: block; }
    </style>
  </head>
  <body>
    <div class="mark-box">${markLightSvg}</div>
  </body>
</html>`;

  await page.setContent(avatarLightHtml);
  const avatarLightBuffer = await page.screenshot();
  fs.writeFileSync(path.join(brandDir, 'org-avatar-light.png'), avatarLightBuffer);
  fs.writeFileSync(path.join(publicDir, 'org-avatar-light.png'), avatarLightBuffer);
  console.log('✓ Generated org-avatar-light.png (1024x1024)');

  // 4. Generate GitHub Social Preview (1280x640)
  const socialPreviewHtml = `<!DOCTYPE html>
<html>
  <head>
    <style>
      * { margin: 0; padding: 0; box-sizing: border-box; }
      body {
        width: 1280px;
        height: 640px;
        background: #0B0F17;
        font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif;
        color: #F8FAFC;
        padding: 56px 64px;
        display: flex;
        flex-direction: column;
        justify-content: space-between;
        border: 1px solid #1E293B;
      }
      .top-row {
        display: flex;
        justify-content: space-between;
        align-items: center;
      }
      .top-brand {
        display: flex;
        align-items: center;
        gap: 14px;
      }
      .top-brand .mark-wrap {
        width: 36px;
        height: 36px;
      }
      .top-brand .brand-text {
        font-size: 26px;
        font-weight: 700;
        letter-spacing: -0.03em;
        color: #F8FAFC;
      }
      .top-badge {
        font-family: ui-monospace, monospace;
        font-size: 13px;
        letter-spacing: 0.08em;
        color: #38BDF8;
        background: #111827;
        border: 1px solid #1E293B;
        padding: 5px 12px;
        border-radius: 4px;
        text-transform: uppercase;
      }
      .center-content {
        display: flex;
        justify-content: space-between;
        align-items: center;
        gap: 48px;
      }
      .hero-text h1 {
        font-size: 46px;
        font-weight: 700;
        letter-spacing: -0.03em;
        line-height: 1.15;
        margin-bottom: 12px;
      }
      .hero-text .cyan {
        color: #38BDF8;
      }
      .hero-text p {
        font-size: 18px;
        color: #94A3B8;
        line-height: 1.5;
        max-width: 580px;
      }
      .term-preview {
        background: #070A10;
        border: 1px solid #1E293B;
        border-radius: 6px;
        padding: 20px 24px;
        font-family: ui-monospace, "SF Mono", monospace;
        font-size: 14px;
        line-height: 1.7;
        width: 480px;
        box-shadow: 0 10px 30px rgba(0,0,0,0.5);
      }
      .term-preview .prompt { color: #06B6D4; }
      .term-preview .cmd { color: #F8FAFC; font-weight: 600; }
      .term-preview .dim { color: #64748B; }
      .term-preview .val { color: #38BDF8; }
      .bottom-row {
        display: flex;
        justify-content: space-between;
        align-items: center;
        border-top: 1px solid #1E293B;
        padding-top: 20px;
        font-family: ui-monospace, monospace;
        font-size: 13px;
        color: #64748B;
      }
      .bottom-tags {
        display: flex;
        gap: 16px;
      }
      .tag {
        color: #94A3B8;
      }
      .tag span {
        color: #06B6D4;
        margin-right: 4px;
      }
    </style>
  </head>
  <body>
    <div class="top-row">
      <div class="top-brand">
        <div class="mark-wrap">${markDarkSvg}</div>
        <div class="brand-text">Sotto</div>
      </div>
      <div class="top-badge">Apache-2.0 · Rust Crypto</div>
    </div>

    <div class="center-content">
      <div class="hero-text">
        <h1>Frictionless secret sync.<br><span class="cyan">Zero-knowledge by design.</span></h1>
        <p>Stop Slacking your .env files. Built on an audited Rust cryptographic core with native CLI and WebAssembly parity.</p>
      </div>

      <div class="term-preview">
        <div><span class="prompt">$ </span><span class="cmd">sotto run -- npm start</span></div>
        <div class="dim">injected 14 secrets into environment</div>
        <div><span class="prompt">$ </span><span class="cmd">sotto share DATABASE_URL</span></div>
        <div class="val">https://getsotto.co.uk/s/8f2a...</div>
        <div class="dim"># burns on first read</div>
      </div>
    </div>

    <div class="bottom-row">
      <div class="bottom-tags">
        <div class="tag"><span>●</span> XChaCha20-Poly1305</div>
        <div class="tag"><span>●</span> X25519 Key Wrapping</div>
        <div class="tag"><span>●</span> Argon2id KDF</div>
        <div class="tag"><span>●</span> Anycast Edge Mesh</div>
      </div>
      <div>getsotto.co.uk</div>
    </div>
  </body>
</html>`;

  await page.setViewportSize({ width: 1280, height: 640 });
  await page.setContent(socialPreviewHtml);
  const socialPreviewBuffer = await page.screenshot();
  fs.writeFileSync(path.join(brandDir, 'github-social-preview.png'), socialPreviewBuffer);
  fs.writeFileSync(path.join(publicDir, 'github-social-preview.png'), socialPreviewBuffer);
  console.log('✓ Generated github-social-preview.png (1280x640)');

  await browser.close();
  console.log('All brand assets successfully generated!');
}

generateAll().catch(err => {
  console.error(err);
  process.exit(1);
});
