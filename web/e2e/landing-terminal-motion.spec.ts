import { expect, test, type Page } from "@playwright/test";

// The landing terminal reveals its transcript over several seconds and leaves a blinking cursor.
// Under `prefers-reduced-motion: reduce`, landing.css must show the whole transcript at once with
// no typing or blinking. These tests read computed style, not animation frames, so they neither
// wait out the timeline nor depend on an intermediate frame.

type Motion = { animationName: string; opacity: string; clipPath: string };

async function readMotion(page: Page, selector: string): Promise<Motion[]> {
  return page.locator(`.landing .term ${selector}`).evaluateAll((elements) =>
    elements.map((element) => {
      const style = getComputedStyle(element);
      return {
        animationName: style.animationName,
        opacity: style.opacity,
        clipPath: style.clipPath,
      };
    }),
  );
}

// `animation-name: none` is also what an unstyled page reports, so wait for a declaration that
// only landing.css provides before trusting any "none" below.
async function waitForTerminalStyles(page: Page) {
  await expect(page.locator(".landing .term .cursor")).toHaveCSS("display", "inline-block");
}

async function openLanding(
  browser: import("@playwright/test").Browser,
  baseURL: string | undefined,
  reducedMotion: "reduce" | "no-preference",
) {
  // Emulation lives on this context only, so it cannot leak into other tests.
  const context = await browser.newContext({ baseURL, reducedMotion });
  const page = await context.newPage();
  await page.goto("/");
  await waitForTerminalStyles(page);
  return { context, page };
}

test("reduced motion shows the whole terminal transcript with no animation", async ({
  browser, baseURL,
}) => {
  const { context, page } = await openLanding(browser, baseURL, "reduce");
  try {
    const lines = await readMotion(page, ".line");
    const commands = await readMotion(page, ".cmd");
    const cursors = await readMotion(page, ".cursor");

    // The transcript is present, including the tail a hidden final line would drop.
    expect(lines).toHaveLength(15);
    expect(commands).toHaveLength(5);
    expect(cursors).toHaveLength(1);
    await expect(page.locator(".landing .term .l1")).toContainText("sotto init");
    await expect(page.locator(".landing .term .l14")).toContainText("https://getsotto.co.uk/s/");
    await expect(page.locator(".landing .term .l15")).toBeVisible();

    // Lines are opaque and commands are not clipped.
    for (const line of lines) expect(line.opacity).toBe("1");
    for (const command of commands) expect(command.clipPath).toBe("none");
    await expect(page.locator(".landing .term .l15")).toHaveCSS("opacity", "1");
    await expect(page.locator(".landing .term .c5")).toHaveCSS("clip-path", "none");

    // No typing, reveal or blink animation is configured. A visible cursor alone would not show
    // that blinking is off.
    for (const element of [...lines, ...commands, ...cursors]) {
      expect(element.animationName).toBe("none");
    }
  } finally {
    await context.close();
  }
});

test("without a reduced-motion preference the terminal animations stay configured", async ({
  browser, baseURL,
}) => {
  const { context, page } = await openLanding(browser, baseURL, "no-preference");
  try {
    const lines = await readMotion(page, ".line");
    const commands = await readMotion(page, ".cmd");
    const cursors = await readMotion(page, ".cursor");

    expect(lines).toHaveLength(15);
    expect(commands).toHaveLength(5);
    expect(cursors).toHaveLength(1);
    for (const line of lines) expect(line.animationName).toBe("sotto-appear");
    for (const command of commands) expect(command.animationName).toBe("sotto-type");
    expect(cursors[0].animationName).toBe("sotto-blink");
  } finally {
    await context.close();
  }
});
