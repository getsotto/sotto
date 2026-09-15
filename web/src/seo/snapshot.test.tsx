import { describe, expect, it } from "vitest";
import { guideBySlug } from "./pages";
import { snapshotFor } from "./snapshot";

function guide(slug: string) {
  const page = guideBySlug(slug);
  if (!page) throw new Error(`unknown guide: ${slug}`);
  return page;
}

describe("share guide security copy", () => {
  it("describes bearer access and retained ciphertext", () => {
    const overviewPage = guide("share-secrets-securely");
    const overview = snapshotFor(overviewPage);
    expect(overview).toContain("Anyone with the complete link can use it while it is active.");
    expect(overview).not.toContain("only the person you send it to");
    expect(overviewPage.description).not.toContain("readable only by the recipient");

    const oneTime = snapshotFor(guide("one-time-secret-links"));
    expect(oneTime).toContain("The complete link is a bearer credential");
    expect(oneTime).toContain("The server retains the encrypted blob after the link is used");
    expect(oneTime).not.toContain("useless to anyone but its first reader");

    const passwordPage = guide("send-password-securely");
    const password = snapshotFor(passwordPage);
    expect(password).toContain("without putting the password in chat or email");
    expect(password).not.toContain("no copy left in chat or email");
    expect(passwordPage.description).toContain("without putting the password in chat or email");
  });
});
