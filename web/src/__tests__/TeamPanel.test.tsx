import "@testing-library/jest-dom/vitest";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { StrictMode } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import * as api from "../api";
import type { Entitlements, Member, Org } from "../api";
import { TeamPanel } from "../TeamPanel";
import * as vault from "../vault";

vi.mock("../api", () => ({
  organisationDeletionEnabled: false,
  createCheckout: vi.fn(),
  createPortal: vi.fn(),
  fetchAudit: vi.fn(),
  fetchEntitlements: vi.fn(),
  fetchMembers: vi.fn(),
  fetchOrgs: vi.fn(),
  fetchSponsoredSeats: vi.fn(),
  fetchSponsoredQuote: vi.fn(),
  createSponsoredCheckout: vi.fn(),
  grantOrgKey: vi.fn(),
  inviteMember: vi.fn(),
}));

vi.mock("../vault", () => ({
  decryptOrgName: vi.fn(),
  openOrgKey: vi.fn(),
  sealGrantTo: vi.fn(),
}));

afterEach(cleanup);

function resetMocks() {
  vi.resetAllMocks();
  vi.mocked(api.fetchSponsoredSeats).mockResolvedValue([]);
}

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason: Error) => void;
  const promise = new Promise<T>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}

function org(id: string): Org {
  return { id, encName: new Uint8Array(), role: "member", encOrgKey: null };
}

function member(userId: string): Member {
  return { userId, role: "member", publicKey: null };
}

const freePlan: Entitlements = {
  tier: "free",
  effectiveTier: "free",
  trialEndsAt: null,
  limits: { maxMembers: 3, maxOrgProjects: 1 },
  billingEnabled: false,
  purchasesEnabled: false,
};

describe("TeamPanel organisation loading", () => {
  beforeEach(() => {
    resetMocks();
    vi.mocked(api.fetchOrgs).mockResolvedValue([org("org-a"), org("org-b")]);
    vi.mocked(api.fetchEntitlements).mockResolvedValue(freePlan);
  });

  it("stops loading after a member failure and retries on organisation selection", async () => {
    const first = deferred<Member[]>();
    const retry = deferred<Member[]>();
    vi.mocked(api.fetchMembers)
      .mockReturnValueOnce(first.promise)
      .mockReturnValueOnce(retry.promise);

    render(
      <TeamPanel master={new Uint8Array(32)} encPrivateKeys={new Uint8Array([1])} />,
    );
    fireEvent.click(await screen.findByRole("button", { name: /org-a/ }));
    expect(screen.getByText("Loading…")).toBeInTheDocument();

    await act(async () => {
      first.reject(new Error("members unavailable"));
    });
    expect(screen.getByRole("alert")).toHaveTextContent("members unavailable");
    expect(screen.queryByText("Loading…")).not.toBeInTheDocument();
    // Only the organisation list remains; a failed request is not an empty member list.
    expect(screen.getAllByRole("list")).toHaveLength(1);

    fireEvent.click(screen.getByRole("button", { name: /org-a/ }));
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    expect(screen.getByText("Loading…")).toBeInTheDocument();
    await act(async () => {
      retry.resolve([member("member-a")]);
    });
    expect(screen.getByText("member-a")).toBeInTheDocument();
    expect(screen.queryByText("Loading…")).not.toBeInTheDocument();
    expect(api.fetchMembers).toHaveBeenCalledTimes(2);
    expect(api.fetchMembers).toHaveBeenNthCalledWith(2, "org-a");
  });

  it.each(["success", "failure"] as const)(
    "ignores a stale member %s while the next organisation is loading",
    async (outcome) => {
      const first = deferred<Member[]>();
      const second = deferred<Member[]>();
      vi.mocked(api.fetchMembers).mockImplementation((orgId) =>
        orgId === "org-a" ? first.promise : second.promise,
      );
      render(
        <TeamPanel master={new Uint8Array(32)} encPrivateKeys={new Uint8Array([1])} />,
      );
      fireEvent.click(await screen.findByRole("button", { name: /org-a/ }));
      fireEvent.click(screen.getByRole("button", { name: /org-b/ }));
      await act(async () => {
        if (outcome === "success") first.resolve([member("member-a")]);
        else first.reject(new Error("stale member failure"));
      });
      expect(screen.getByRole("heading", { name: "Members of org-b" })).toBeInTheDocument();
      expect(screen.getByText("Loading…")).toBeInTheDocument();
      expect(screen.queryByRole("alert")).not.toBeInTheDocument();
      expect(screen.queryByText("member-a")).not.toBeInTheDocument();
      await act(async () => {
        second.resolve([member("member-b")]);
      });
      expect(screen.getByText("member-b")).toBeInTheDocument();
      expect(screen.queryByText("Loading…")).not.toBeInTheDocument();
    },
  );

  it("keeps details from the latest organisation when requests resolve out of order", async () => {
    const first = deferred<Member[]>();
    const second = deferred<Member[]>();
    vi.mocked(api.fetchMembers).mockImplementation((orgId) =>
      orgId === "org-a" ? first.promise : second.promise,
    );

    render(
      <TeamPanel master={new Uint8Array(32)} encPrivateKeys={new Uint8Array([1])} />,
    );

    fireEvent.click(await screen.findByRole("button", { name: /org-a/ }));
    fireEvent.click(screen.getByRole("button", { name: /org-b/ }));

    await act(async () => {
      second.resolve([member("member-b")]);
    });
    expect(await screen.findByText("member-b")).toBeInTheDocument();

    await act(async () => {
      first.resolve([member("member-a")]);
    });
    expect(screen.getByRole("heading", { name: "Members of org-b" })).toBeInTheDocument();
    expect(screen.getByText("member-b")).toBeInTheDocument();
    expect(screen.queryByText("member-a")).not.toBeInTheDocument();
    expect(api.fetchEntitlements).toHaveBeenCalledOnce();
    expect(api.fetchEntitlements).toHaveBeenCalledWith("org-b");
  });
});

describe("TeamPanel optional sponsored-seat discovery", () => {
  beforeEach(() => {
    resetMocks();
    vi.mocked(api.fetchOrgs).mockResolvedValue([{ ...org("org-a"), role: "admin" }]);
    vi.mocked(api.fetchMembers).mockResolvedValue([member("member-a")]);
    vi.mocked(api.fetchEntitlements).mockResolvedValue({
      ...freePlan, tier: "team", effectiveTier: "team",
    });
    vi.mocked(api.fetchAudit).mockResolvedValue([]);
  });

  it.each(["unavailable", "empty"] as const)(
    "keeps members, invitations and audit usable when sponsored seats are %s",
    async (outcome) => {
      if (outcome === "unavailable") {
        vi.mocked(api.fetchSponsoredSeats).mockRejectedValueOnce(new Error("synthetic seat failure"));
      }
      render(
        <TeamPanel master={new Uint8Array(32)} encPrivateKeys={new Uint8Array([1])} />,
      );
      fireEvent.click(await screen.findByRole("button", { name: /org-a/ }));
      expect(await screen.findByRole("heading", { name: "Audit log" })).toBeInTheDocument();

      expect(screen.getByText("member-a")).toBeInTheDocument();
      expect(screen.getByText("No events yet.")).toBeInTheDocument();
      const invite = screen.getByRole("textbox", { name: "Invite by email" });
      expect(invite).toBeEnabled();
      fireEvent.change(invite, { target: { value: "teammate@example.com" } });
      expect(invite).toHaveValue("teammate@example.com");
      expect(screen.getByRole("button", { name: "Invite" })).toBeEnabled();
      expect(screen.queryByRole("alert")).not.toBeInTheDocument();
      expect(api.fetchSponsoredSeats).toHaveBeenCalledExactlyOnceWith("org-a");
      expect(api.fetchAudit).toHaveBeenCalledExactlyOnceWith("org-a");
      expect(api.fetchSponsoredQuote).not.toHaveBeenCalled();
      expect(api.createSponsoredCheckout).not.toHaveBeenCalled();
      expect(api.inviteMember).not.toHaveBeenCalled();

      if (outcome === "unavailable") {
        expect(screen.queryByRole("heading", { name: "Sponsored Cloud seats" })).not.toBeInTheDocument();
        expect(screen.queryByText("No sponsored seats are active.")).not.toBeInTheDocument();
      } else {
        expect(screen.getByRole("heading", { name: "Sponsored Cloud seats" })).toBeInTheDocument();
        expect(screen.getByText("No sponsored seats are active.")).toBeInTheDocument();
      }
    },
  );
});

describe("TeamPanel organisation names", () => {
  const master = new Uint8Array([1]);
  const encPrivateKeys = new Uint8Array([2]);
  const encOrgKey = new Uint8Array([3]);
  const orgKey = new Uint8Array([4]);
  const encName = new Uint8Array([5]);

  beforeEach(() => {
    resetMocks();
    vi.mocked(api.fetchMembers).mockResolvedValue([]);
    vi.mocked(api.fetchEntitlements).mockResolvedValue(freePlan);
  });

  it("uses the id without a key while keeping a decryptable organisation", async () => {
    vi.mocked(api.fetchOrgs).mockResolvedValue([
      org("org-without-key"),
      { ...org("org-with-key"), encOrgKey, encName },
    ]);
    vi.mocked(vault.openOrgKey).mockReturnValue(orgKey);
    vi.mocked(vault.decryptOrgName).mockReturnValue("Friendly name");

    render(<TeamPanel master={master} encPrivateKeys={encPrivateKeys} />);

    fireEvent.click(await screen.findByRole("button", { name: /org-without-key/ }));
    expect(screen.getByRole("heading", { name: "Members of org-without-key" })).toBeInTheDocument();
    expect(api.fetchMembers).toHaveBeenCalledWith("org-without-key");
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: /Friendly name/ }));
    expect(screen.getByRole("heading", { name: "Members of Friendly name" })).toBeInTheDocument();
    expect(api.fetchMembers).toHaveBeenLastCalledWith("org-with-key");
    expect(vault.openOrgKey).toHaveBeenCalledExactlyOnceWith(master, encPrivateKeys, encOrgKey);
    expect(vault.decryptOrgName).toHaveBeenCalledExactlyOnceWith(orgKey, "org-with-key", encName);
  });

  it("keeps an organisation selectable when its key cannot be opened", async () => {
    vi.mocked(api.fetchOrgs).mockResolvedValue([{ ...org("org-a"), encOrgKey }]);
    vi.mocked(vault.openOrgKey).mockImplementation(() => {
      throw new Error("key unavailable");
    });

    render(<TeamPanel master={master} encPrivateKeys={encPrivateKeys} />);

    fireEvent.click(await screen.findByRole("button", { name: /org-a/ }));
    expect(screen.getByRole("heading", { name: "Members of org-a" })).toBeInTheDocument();
    expect(api.fetchMembers).toHaveBeenCalledExactlyOnceWith("org-a");
    expect(vault.decryptOrgName).not.toHaveBeenCalled();
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
  });

  it("keeps an organisation selectable when its name cannot be decrypted", async () => {
    vi.mocked(api.fetchOrgs).mockResolvedValue([
      { ...org("org-a"), encOrgKey, encName },
      { ...org("org-b"), encOrgKey, encName },
    ]);
    vi.mocked(vault.openOrgKey).mockReturnValue(orgKey);
    vi.mocked(vault.decryptOrgName).mockImplementation((_key, orgId) => {
      if (orgId === "org-a") throw new Error("name unavailable");
      return "Healthy org";
    });

    render(<TeamPanel master={master} encPrivateKeys={encPrivateKeys} />);

    fireEvent.click(await screen.findByRole("button", { name: /org-a/ }));
    expect(screen.getByRole("heading", { name: "Members of org-a" })).toBeInTheDocument();
    expect(api.fetchMembers).toHaveBeenCalledWith("org-a");
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: /Healthy org/ }));
    expect(screen.getByRole("heading", { name: "Members of Healthy org" })).toBeInTheDocument();
    expect(api.fetchMembers).toHaveBeenLastCalledWith("org-b");
    expect(vault.decryptOrgName).toHaveBeenNthCalledWith(1, orgKey, "org-a", encName);
    expect(vault.decryptOrgName).toHaveBeenNthCalledWith(2, orgKey, "org-b", encName);
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
  });
});

describe("TeamPanel invitations", () => {
  const adminOrg: Org = {
    id: "org-a",
    encName: new Uint8Array(),
    role: "admin",
    encOrgKey: null,
  };

  beforeEach(() => {
    resetMocks();
    vi.mocked(api.fetchOrgs).mockResolvedValue([adminOrg]);
    vi.mocked(api.fetchMembers).mockResolvedValue([]);
    vi.mocked(api.fetchEntitlements).mockResolvedValue(freePlan);
  });

  async function openInviteForm() {
    render(<TeamPanel master={new Uint8Array(32)} encPrivateKeys={new Uint8Array([1])} />);
    fireEvent.click(await screen.findByRole("button", { name: /org-a/ }));
    return screen.findByRole("textbox", { name: "Invite by email" });
  }

  it("locks the form while an invitation is pending and uses the submitted email on success", async () => {
    const pending = deferred<{ userId: string; publicKey: Uint8Array | null }>();
    vi.mocked(api.inviteMember).mockReturnValue(pending.promise);
    const input = await openInviteForm();
    fireEvent.change(input, { target: { value: "  teammate@example.com  " } });
    fireEvent.submit(input.closest("form")!);

    expect(input).toBeDisabled();
    expect(screen.getByRole("status")).toBeEmptyDOMElement();
    expect(screen.getByRole("button", { name: "Inviting…" })).toBeDisabled();
    fireEvent.submit(input.closest("form")!);
    expect(api.inviteMember).toHaveBeenCalledOnce();
    expect(api.inviteMember).toHaveBeenCalledWith("org-a", "teammate@example.com");

    await act(async () => {
      pending.resolve({ userId: "user-b", publicKey: null });
    });
    expect(screen.getByRole("status")).toHaveTextContent("invited teammate@example.com (user-b)");
    expect(input).toHaveValue("");
    expect(input).not.toBeDisabled();
    expect(api.fetchMembers).toHaveBeenCalledTimes(2);
  });

  it.each(["open", "seal", "upload"] as const)(
    "keeps an invitation successful when the optional org-key %s fails",
    async (failure) => {
      const encOrgKey = new Uint8Array([2]);
      const orgKey = new Uint8Array([3]);
      const publicKey = new Uint8Array([4]);
      const sealedKey = new Uint8Array([5]);
      vi.mocked(api.fetchOrgs).mockResolvedValue([{ ...adminOrg, encOrgKey }]);
      vi.mocked(vault.openOrgKey).mockReturnValue(orgKey);
      vi.mocked(vault.decryptOrgName).mockReturnValue("org-a");
      vi.mocked(vault.sealGrantTo).mockReturnValue(sealedKey);
      vi.mocked(api.inviteMember).mockResolvedValue({ userId: "user-b", publicKey });
      vi.mocked(api.fetchMembers)
        .mockResolvedValueOnce([])
        .mockResolvedValueOnce([member("user-b")]);

      const input = await openInviteForm();
      expect(vault.openOrgKey).toHaveBeenCalledOnce();
      // Inject only after the organisation name has been decrypted successfully.
      if (failure === "open") {
        vi.mocked(vault.openOrgKey).mockImplementationOnce(() => {
          throw new Error("synthetic key-opening failure");
        });
      } else if (failure === "seal") {
        vi.mocked(vault.sealGrantTo).mockImplementationOnce(() => {
          throw new Error("synthetic sealing failure");
        });
      } else {
        vi.mocked(api.grantOrgKey).mockRejectedValueOnce(new Error("synthetic upload failure"));
      }
      fireEvent.change(input, { target: { value: "teammate@example.com" } });
      fireEvent.submit(input.closest("form")!);

      expect(await screen.findByText("user-b")).toBeInTheDocument();
      expect(screen.getByRole("status")).toHaveTextContent("invited teammate@example.com (user-b)");
      expect(screen.queryByRole("alert")).not.toBeInTheDocument();
      expect(input).toHaveValue("");
      expect(input).toBeEnabled();
      fireEvent.change(input, { target: { value: "next@example.com" } });
      expect(screen.getByRole("button", { name: "Invite" })).toBeEnabled();
      expect(api.inviteMember).toHaveBeenCalledExactlyOnceWith("org-a", "teammate@example.com");
      expect(api.fetchMembers).toHaveBeenCalledTimes(2);
      expect(api.fetchMembers).toHaveBeenNthCalledWith(2, "org-a");
      expect(vault.openOrgKey).toHaveBeenCalledTimes(2);
      expect(vault.openOrgKey).toHaveBeenLastCalledWith(new Uint8Array(32), new Uint8Array([1]), encOrgKey);
      if (failure === "open") {
        expect(vault.sealGrantTo).not.toHaveBeenCalled();
      } else {
        expect(vault.sealGrantTo).toHaveBeenCalledExactlyOnceWith(publicKey, orgKey);
      }
      if (failure === "upload") {
        expect(api.grantOrgKey).toHaveBeenCalledExactlyOnceWith("org-a", "user-b", sealedKey);
      } else {
        expect(api.grantOrgKey).not.toHaveBeenCalled();
      }
    },
  );

  it("preserves the email and restores the form after an invitation failure", async () => {
    const pending = deferred<{ userId: string; publicKey: Uint8Array | null }>();
    vi.mocked(api.inviteMember).mockReturnValue(pending.promise);
    const input = await openInviteForm();
    fireEvent.change(input, { target: { value: "retry@example.com" } });
    fireEvent.submit(input.closest("form")!);

    await act(async () => {
      pending.reject(new Error("invite unavailable"));
    });
    expect(screen.getByRole("alert")).toHaveTextContent("invite unavailable");
    expect(input).toHaveValue("retry@example.com");
    expect(input).not.toBeDisabled();
    expect(screen.getByRole("button", { name: "Invite" })).toBeEnabled();
  });

  it.each(["success", "failure"] as const)(
    "ignores an invitation %s after switching organisations and restores the form",
    async (outcome) => {
      const pending = deferred<{ userId: string; publicKey: Uint8Array | null }>();
      const orgKey = new Uint8Array([2]);
      const publicKey = new Uint8Array([3]);
      const sealedKey = new Uint8Array([4]);
      vi.mocked(api.fetchOrgs).mockResolvedValue([
        { ...adminOrg, encOrgKey: new Uint8Array([1]) },
        { ...adminOrg, id: "org-b" },
      ]);
      vi.mocked(vault.decryptOrgName).mockReturnValue("org-a");
      vi.mocked(vault.openOrgKey).mockReturnValue(orgKey);
      vi.mocked(vault.sealGrantTo).mockReturnValue(sealedKey);
      vi.mocked(api.fetchMembers).mockImplementation(async (orgId) => [member(`member-${orgId}`)]);
      vi.mocked(api.inviteMember).mockReturnValue(pending.promise);
      const input = await openInviteForm();
      fireEvent.change(input, { target: { value: "teammate@example.com" } });
      fireEvent.submit(input.closest("form")!);
      fireEvent.click(screen.getByRole("button", { name: /org-b/ }));
      await screen.findByText("member-org-b");

      await act(async () => {
        if (outcome === "success") pending.resolve({ userId: "user-c", publicKey });
        else pending.reject(new Error("stale invitation failure"));
      });

      expect(screen.getByRole("heading", { name: "Members of org-b" })).toBeInTheDocument();
      expect(screen.getByText("member-org-b")).toBeInTheDocument();
      expect(screen.queryByText("member-org-a")).not.toBeInTheDocument();
      expect(screen.queryByText(/invited teammate/)).not.toBeInTheDocument();
      expect(screen.queryByRole("alert")).not.toBeInTheDocument();
      expect(input).toHaveValue("teammate@example.com");
      expect(input).toBeEnabled();
      expect(screen.getByRole("button", { name: "Invite" })).toBeEnabled();
      expect(api.inviteMember).toHaveBeenCalledExactlyOnceWith("org-a", "teammate@example.com");
      if (outcome === "success") {
        expect(vault.sealGrantTo).toHaveBeenCalledWith(publicKey, orgKey);
        expect(api.grantOrgKey).toHaveBeenCalledExactlyOnceWith("org-a", "user-c", sealedKey);
      }
    },
  );

  it.each(["success", "failure"] as const)(
    "ignores an invitation's member refresh %s after switching organisations",
    async (outcome) => {
      const refresh = deferred<Member[]>();
      vi.mocked(api.fetchOrgs).mockResolvedValue([adminOrg, { ...adminOrg, id: "org-b" }]);
      vi.mocked(api.fetchMembers)
        .mockResolvedValueOnce([member("member-a")])
        .mockReturnValueOnce(refresh.promise)
        .mockResolvedValueOnce([member("member-b")]);
      vi.mocked(api.inviteMember).mockResolvedValue({ userId: "user-c", publicKey: null });
      const input = await openInviteForm();
      fireEvent.change(input, { target: { value: "teammate@example.com" } });
      fireEvent.submit(input.closest("form")!);
      await screen.findByText("invited teammate@example.com (user-c)");
      expect(api.fetchMembers).toHaveBeenNthCalledWith(2, "org-a");
      fireEvent.click(screen.getByRole("button", { name: /org-b/ }));
      await screen.findByText("member-b");

      await act(async () => {
        if (outcome === "success") refresh.resolve([member("new-member-a")]);
        else refresh.reject(new Error("stale refresh failure"));
      });

      expect(screen.getByRole("heading", { name: "Members of org-b" })).toBeInTheDocument();
      expect(screen.getByText("member-b")).toBeInTheDocument();
      expect(screen.queryByText("new-member-a")).not.toBeInTheDocument();
      expect(screen.queryByText(/invited teammate/)).not.toBeInTheDocument();
      expect(screen.queryByRole("alert")).not.toBeInTheDocument();
      expect(input).toHaveValue("");
      expect(input).toBeEnabled();
    },
  );
});


describe("TeamPanel billing request ownership", () => {
  const admin = (id: string): Org => ({
    id, encName: new Uint8Array(), role: "admin", encOrgKey: null,
  });
  const billablePlan: Entitlements = { ...freePlan, billingEnabled: true, purchasesEnabled: true };
  const standardQuote: api.SponsoredQuote = {
    action: "add",
    offer: "standard_monthly",
    seatCount: 1,
    amountPence: 299,
    currency: "gbp",
    interval: "month",
    quoteVersion: 2,
    quoteExpiresAtEpoch: 2_000_000_000,
    founding: false,
    foundingRemainingPlaces: null,
    foundingTerm: null,
    nextRenewalAmountPence: 299,
  };

  beforeEach(() => {
    resetMocks();
    vi.mocked(api.fetchOrgs).mockResolvedValue([admin("org-a"), admin("org-b")]);
    vi.mocked(api.fetchMembers).mockResolvedValue([]);
    vi.mocked(api.fetchAudit).mockResolvedValue([]);
    vi.mocked(api.fetchEntitlements).mockResolvedValue(billablePlan);
  });

  it("keeps the selected organisation's quote when an older quote fails", async () => {
    const firstQuote = deferred<api.SponsoredQuote>();
    vi.mocked(api.fetchSponsoredQuote).mockImplementation((orgId) =>
      orgId === "org-a" ? firstQuote.promise : Promise.resolve(standardQuote),
    );
    render(<TeamPanel master={new Uint8Array(32)} encPrivateKeys={new Uint8Array([1])} />);

    fireEvent.click(await screen.findByRole("button", { name: /org-a/ }));
    await screen.findByRole("heading", { name: "Sponsored Cloud seats" });
    fireEvent.change(screen.getByLabelText("Beneficiary user id"), {
      target: { value: "beneficiary-a" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Review seat price" }));
    expect(api.fetchSponsoredQuote).toHaveBeenCalledWith("org-a", {
      action: "add",
      offer: "standard_monthly",
      beneficiaryIds: ["beneficiary-a"],
    });

    fireEvent.click(screen.getByRole("button", { name: /org-b/ }));
    await screen.findByRole("heading", { name: "Sponsored Cloud seats" });
    fireEvent.change(screen.getByLabelText("Beneficiary user id"), {
      target: { value: "beneficiary-b" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Review seat price" }));
    expect(await screen.findByRole("button", { name: "Continue to checkout" })).toBeEnabled();

    await act(async () => {
      firstQuote.reject(new Error("org-a quote failed (409)"));
    });

    expect(screen.getByRole("heading", { name: "Members of org-b" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Continue to checkout" })).toBeEnabled();
    expect(screen.getByText(/£2\.99 per month/)).toBeInTheDocument();
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
  });

  it("keeps a newer checkout pending and preserves its retry after an older conflict", async () => {
    const firstCheckout = deferred<Awaited<ReturnType<typeof api.createSponsoredCheckout>>>();
    const secondCheckout = deferred<Awaited<ReturnType<typeof api.createSponsoredCheckout>>>();
    let orgBCheckouts = 0;
    vi.mocked(api.fetchSponsoredQuote).mockResolvedValue(standardQuote);
    vi.mocked(api.createSponsoredCheckout).mockImplementation((orgId) => {
      if (orgId === "org-a") return firstCheckout.promise;
      orgBCheckouts += 1;
      return orgBCheckouts === 1
        ? secondCheckout.promise
        : Promise.resolve({ operationId: "org-b-operation", state: "pending", checkoutUrl: null });
    });
    render(<TeamPanel master={new Uint8Array(32)} encPrivateKeys={new Uint8Array([1])} />);

    fireEvent.click(await screen.findByRole("button", { name: /org-a/ }));
    await screen.findByRole("heading", { name: "Sponsored Cloud seats" });
    fireEvent.change(screen.getByLabelText("Beneficiary user id"), {
      target: { value: "beneficiary-a" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Review seat price" }));
    fireEvent.click(await screen.findByRole("button", { name: "Continue to checkout" }));
    expect(screen.getByRole("button", { name: "Preparing…" })).toBeDisabled();

    fireEvent.click(screen.getByRole("button", { name: /org-b/ }));
    await screen.findByRole("heading", { name: "Sponsored Cloud seats" });
    fireEvent.change(screen.getByLabelText("Beneficiary user id"), {
      target: { value: "beneficiary-b" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Review seat price" }));
    fireEvent.click(await screen.findByRole("button", { name: "Continue to checkout" }));
    expect(screen.getByRole("button", { name: "Preparing…" })).toBeDisabled();
    expect(api.createSponsoredCheckout).toHaveBeenCalledTimes(2);

    await act(async () => {
      firstCheckout.reject(new Error("org-a checkout failed (409)"));
    });

    expect(screen.getByRole("heading", { name: "Members of org-b" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Preparing…" })).toBeDisabled();
    expect(screen.getByText(/£2\.99 per month/)).toBeInTheDocument();
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();

    await act(async () => {
      secondCheckout.reject(new Error("org-b checkout failed (502)"));
    });
    expect(screen.getByRole("alert")).toHaveTextContent("org-b checkout failed (502)");
    fireEvent.click(screen.getByRole("button", { name: "Continue to checkout" }));
    expect(await screen.findByText(/pending provider confirmation/)).toBeInTheDocument();
    expect(api.createSponsoredCheckout).toHaveBeenCalledTimes(3);
    expect(vi.mocked(api.createSponsoredCheckout).mock.calls[2]).toEqual(
      vi.mocked(api.createSponsoredCheckout).mock.calls[1],
    );
  });

  it("does not restore an old request after switching away and back", async () => {
    const oldQuote = deferred<api.SponsoredQuote>();
    vi.mocked(api.fetchSponsoredQuote)
      .mockReturnValueOnce(oldQuote.promise)
      .mockResolvedValue(standardQuote);
    render(<TeamPanel master={new Uint8Array(32)} encPrivateKeys={new Uint8Array([1])} />);

    fireEvent.click(await screen.findByRole("button", { name: /org-a/ }));
    await screen.findByRole("heading", { name: "Sponsored Cloud seats" });
    fireEvent.change(screen.getByLabelText("Beneficiary user id"), {
      target: { value: "beneficiary-a" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Review seat price" }));
    fireEvent.click(screen.getByRole("button", { name: /org-b/ }));
    await screen.findByRole("heading", { name: "Members of org-b" });
    fireEvent.click(screen.getByRole("button", { name: /org-a/ }));
    await screen.findByRole("heading", { name: "Sponsored Cloud seats" });
    fireEvent.click(screen.getByRole("button", { name: "Review seat price" }));
    expect(await screen.findByRole("button", { name: "Continue to checkout" })).toBeEnabled();

    await act(async () => {
      oldQuote.reject(new Error("old org-a quote failed (409)"));
    });

    expect(screen.getByRole("button", { name: "Continue to checkout" })).toBeEnabled();
    expect(screen.getByText(/£2\.99 per month/)).toBeInTheDocument();
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
  });

  it("uses the server selected offer for sponsored checkout", async () => {
    vi.mocked(api.fetchSponsoredQuote).mockResolvedValue({
      action: "add",
      offer: "founding_monthly",
      seatCount: 1,
      amountPence: 199,
      currency: "gbp",
      interval: "month",
      quoteVersion: 1,
      quoteExpiresAtEpoch: 2_000_000_000,
      founding: true,
      foundingRemainingPlaces: 42,
      foundingTerm: "for the first 12 monthly billing periods",
      nextRenewalAmountPence: 299,
    });
    vi.mocked(api.createSponsoredCheckout).mockResolvedValue({
      operationId: "sponsored-operation-1",
      state: "pending",
      checkoutUrl: null,
    });
    render(<TeamPanel master={new Uint8Array(32)} encPrivateKeys={new Uint8Array([1])} />);

    fireEvent.click(await screen.findByRole("button", { name: /org-a/ }));
    await screen.findByRole("heading", { name: "Sponsored Cloud seats" });
    fireEvent.change(screen.getByLabelText("Beneficiary user id"), {
      target: { value: "beneficiary-a" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Review seat price" }));

    expect(await screen.findByText(/£1\.99 per month/)).toBeInTheDocument();
    expect(screen.getByText(/£2\.99 per month/)).toBeInTheDocument();
    expect(api.createSponsoredCheckout).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole("button", { name: "Continue to checkout" }));
    expect(await screen.findByText(/pending provider confirmation/)).toBeInTheDocument();
    expect(api.fetchSponsoredQuote).toHaveBeenCalledWith("org-a", {
      action: "add",
      offer: "standard_monthly",
      beneficiaryIds: ["beneficiary-a"],
    });
    expect(api.createSponsoredCheckout).toHaveBeenCalledWith(
      "org-a",
      expect.objectContaining({
        action: "add",
        offer: "founding_monthly",
        beneficiaryId: "beneficiary-a",
      }),
    );
  });

  it("requires a fresh sponsored quote after a checkout conflict", async () => {
    vi.mocked(api.fetchSponsoredQuote).mockResolvedValue({
      action: "add",
      offer: "founding_monthly",
      seatCount: 1,
      amountPence: 199,
      currency: "gbp",
      interval: "month",
      quoteVersion: 1,
      quoteExpiresAtEpoch: 2_000_000_000,
      founding: true,
      foundingRemainingPlaces: 42,
      foundingTerm: "for the first 12 monthly billing periods",
      nextRenewalAmountPence: 299,
    });
    vi.mocked(api.createSponsoredCheckout).mockRejectedValueOnce(new Error("server error (409)"));
    render(<TeamPanel master={new Uint8Array(32)} encPrivateKeys={new Uint8Array([1])} />);

    fireEvent.click(await screen.findByRole("button", { name: /org-a/ }));
    await screen.findByRole("heading", { name: "Sponsored Cloud seats" });
    fireEvent.change(screen.getByLabelText("Beneficiary user id"), {
      target: { value: "beneficiary-a" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Review seat price" }));
    await screen.findByText(/£1\.99 per month/);
    fireEvent.click(screen.getByRole("button", { name: "Continue to checkout" }));

    expect(await screen.findByText("server error (409)")).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Review seat price" }));
    await waitFor(() => expect(api.fetchSponsoredQuote).toHaveBeenCalledTimes(2));
    expect(screen.getByRole("button", { name: "Continue to checkout" })).toBeInTheDocument();
    expect(api.createSponsoredCheckout).toHaveBeenCalledTimes(1);
  });

  it("replays the same sponsored operation after a transient provider error", async () => {
    vi.mocked(api.fetchSponsoredQuote).mockResolvedValue({
      action: "add",
      offer: "founding_monthly",
      seatCount: 1,
      amountPence: 199,
      currency: "gbp",
      interval: "month",
      quoteVersion: 1,
      quoteExpiresAtEpoch: 2_000_000_000,
      founding: true,
      foundingRemainingPlaces: 42,
      foundingTerm: "for the first 12 monthly billing periods",
      nextRenewalAmountPence: 299,
    });
    vi.mocked(api.createSponsoredCheckout)
      .mockRejectedValueOnce(new Error("server error (502)"))
      .mockResolvedValueOnce({
        operationId: "sponsored-operation-1",
        state: "provider_pending",
        checkoutUrl: null,
      });
    render(<TeamPanel master={new Uint8Array(32)} encPrivateKeys={new Uint8Array([1])} />);

    fireEvent.click(await screen.findByRole("button", { name: /org-a/ }));
    await screen.findByRole("heading", { name: "Sponsored Cloud seats" });
    fireEvent.change(screen.getByLabelText("Beneficiary user id"), {
      target: { value: "beneficiary-a" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Review seat price" }));
    await screen.findByText(/£1\.99 per month/);
    fireEvent.click(screen.getByRole("button", { name: "Continue to checkout" }));
    await screen.findByText("server error (502)");
    fireEvent.click(screen.getByRole("button", { name: "Continue to checkout" }));

    expect(await screen.findByText(/pending provider confirmation/)).toBeInTheDocument();
    expect(api.createSponsoredCheckout).toHaveBeenCalledTimes(2);
    expect(vi.mocked(api.createSponsoredCheckout).mock.calls[1]).toEqual(
      vi.mocked(api.createSponsoredCheckout).mock.calls[0],
    );
  });

  it("hides upgrades while sales are paused but keeps the existing billing portal", async () => {
    vi.mocked(api.fetchEntitlements).mockResolvedValue({
      ...billablePlan,
      tier: "team",
      effectiveTier: "team",
      limits: null,
      purchasesEnabled: false,
    });
    render(<TeamPanel master={new Uint8Array(32)} encPrivateKeys={new Uint8Array([1])} />);

    fireEvent.click(await screen.findByRole("button", { name: /org-a/ }));

    expect(await screen.findByRole("button", { name: "Manage billing" })).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Upgrade to Team" })).not.toBeInTheDocument();
  });

  it("does not offer a new upgrade when sales are paused", async () => {
    vi.mocked(api.fetchEntitlements).mockResolvedValue({
      ...billablePlan,
      purchasesEnabled: false,
    });
    render(<TeamPanel master={new Uint8Array(32)} encPrivateKeys={new Uint8Array([1])} />);

    fireEvent.click(await screen.findByRole("button", { name: /org-a/ }));

    await screen.findByText(/Plan:/);
    expect(screen.queryByRole("button", { name: "Upgrade to Team" })).not.toBeInTheDocument();
  });

  it.each(["success", "failure"] as const)(
    "ignores a stale checkout %s after switching organisations",
    async (outcome) => {
      const pending = deferred<string>();
      vi.mocked(api.createCheckout).mockReturnValueOnce(pending.promise).mockResolvedValueOnce("/checkout-b");
      const assign = vi.spyOn(window.location, "assign").mockImplementation(() => {});
      render(<TeamPanel master={new Uint8Array(32)} encPrivateKeys={new Uint8Array([1])} />);

      fireEvent.click(await screen.findByRole("button", { name: /org-a/ }));
      fireEvent.click(await screen.findByRole("button", { name: "Upgrade to Team" }));
      expect(screen.getByRole("button", { name: "Opening checkout…" })).toBeDisabled();

      fireEvent.click(screen.getByRole("button", { name: /org-b/ }));
      expect(await screen.findByRole("button", { name: "Upgrade to Team" })).toBeEnabled();

      await act(async () => {
        if (outcome === "success") pending.resolve("/checkout-a");
        else pending.reject(new Error("stale checkout failure"));
      });
      expect(assign).not.toHaveBeenCalled();
      expect(screen.queryByRole("alert")).not.toBeInTheDocument();

      fireEvent.click(screen.getByRole("button", { name: "Upgrade to Team" }));
      await act(async () => {});
      expect(api.createCheckout).toHaveBeenLastCalledWith("org-b");
      expect(assign).toHaveBeenCalledWith("/checkout-b");
      assign.mockRestore();
    },
  );
});

describe("TeamPanel checkout return", () => {
  let originalUrl: string;

  beforeEach(() => {
    originalUrl = window.location.href;
    vi.resetAllMocks();
    vi.mocked(api.fetchOrgs).mockResolvedValue([org("org-a")]);
  });

  afterEach(() => {
    window.history.replaceState(null, "", originalUrl);
  });

  function renderTeamPanel() {
    return render(
      <StrictMode>
        <TeamPanel master={new Uint8Array(32)} encPrivateKeys={new Uint8Array([1])} />
      </StrictMode>,
    );
  }

  it.each([
    {
      outcome: "success",
      message: "Payment received. Your Team plan activates as soon as Stripe confirms, usually within seconds.",
    },
    { outcome: "cancelled", message: "Checkout cancelled. Nothing was charged." },
  ])("shows a $outcome return once and preserves the rest of the url", async ({ outcome, message }) => {
    window.history.replaceState(
      null,
      "",
      `/account/team?view=activity&billing=${outcome}&view=members#org-a`,
    );

    const { unmount } = renderTeamPanel();
    expect(await screen.findByRole("button", { name: /org-a/ })).toBeInTheDocument();
    expect(screen.getByText(message)).toBeInTheDocument();
    expect(
      screen.queryByText(outcome === "success" ? /Checkout cancelled\./ : /Payment received\./),
    ).not.toBeInTheDocument();
    expect(window.location.pathname + window.location.search + window.location.hash).toBe(
      "/account/team?view=activity&view=members#org-a",
    );

    unmount();
    renderTeamPanel();
    expect(await screen.findByRole("button", { name: /org-a/ })).toBeInTheDocument();
    expect(screen.queryByText(/Payment received\./)).not.toBeInTheDocument();
    expect(screen.queryByText(/Checkout cancelled\./)).not.toBeInTheDocument();
  });

  it.each([
    { name: "absent", url: "/account/team?view=activity&view=members#org-a" },
    { name: "unknown", url: "/account/team?view=activity&billing=unknown&view=members#org-a" },
  ])("leaves an $name outcome unchanged", async ({ url }) => {
    window.history.replaceState(null, "", url);

    renderTeamPanel();
    expect(await screen.findByRole("button", { name: /org-a/ })).toBeInTheDocument();
    expect(screen.queryByText(/Payment received\./)).not.toBeInTheDocument();
    expect(screen.queryByText(/Checkout cancelled\./)).not.toBeInTheDocument();
    expect(window.location.pathname + window.location.search + window.location.hash).toBe(url);
  });
});

describe("TeamPanel organisation-list recovery", () => {
  it("retries a failed organisation-list request", async () => {
    resetMocks();
    vi.mocked(api.fetchOrgs)
      .mockRejectedValueOnce(new Error("organisations unavailable"))
      .mockResolvedValueOnce([org("org-recovered")]);

    render(<TeamPanel master={new Uint8Array(32)} encPrivateKeys={new Uint8Array([1])} />);

    expect(await screen.findByRole("alert")).toHaveTextContent("organisations unavailable");
    fireEvent.click(screen.getByRole("button", { name: "Retry organisations" }));
    expect(await screen.findByRole("button", { name: /org-recovered/ })).toBeInTheDocument();
    expect(api.fetchOrgs).toHaveBeenCalledTimes(2);
  });
});

describe("TeamPanel audit visibility", () => {
  it.each([
    ["owner", "free", false],
    ["admin", "free", false],
    ["member", "free", false],
    ["owner", "team", true],
    ["admin", "team", true],
    ["member", "team", false],
  ] as const)("gates audit for %s with effective %s", async (role, effectiveTier, allowed) => {
    resetMocks();
    vi.mocked(api.fetchOrgs).mockResolvedValue([{ ...org("org-a"), role }]);
    vi.mocked(api.fetchMembers).mockResolvedValue([]);
    vi.mocked(api.fetchEntitlements).mockResolvedValue({
      ...freePlan,
      tier: "free",
      effectiveTier,
    });
    vi.mocked(api.fetchAudit).mockResolvedValue([]);

    render(<TeamPanel master={new Uint8Array(32)} encPrivateKeys={new Uint8Array([1])} />);
    fireEvent.click(await screen.findByRole("button", { name: /org-a/ }));
    await screen.findByRole("heading", { name: "Members of org-a" });
    await act(async () => { await Promise.resolve(); });

    expect(api.fetchAudit).toHaveBeenCalledTimes(allowed ? 1 : 0);
    if (allowed) expect(await screen.findByText("No events yet.")).toBeInTheDocument();
    else expect(screen.queryByRole("heading", { name: "Audit log" })).not.toBeInTheDocument();
  });
});

it("renders a populated audit response for an allowed organisation", async () => {
  resetMocks();
  vi.mocked(api.fetchOrgs).mockResolvedValue([{ ...org("org-a"), role: "owner" }]);
  vi.mocked(api.fetchMembers).mockResolvedValue([]);
  vi.mocked(api.fetchEntitlements).mockResolvedValue({ ...freePlan, effectiveTier: "team" });
  vi.mocked(api.fetchAudit).mockResolvedValue([{ id: 1, actor: "user-a", action: "secret.read", target: "secret-a", envId: "env-a", detail: "ok", at: "2026-09-30T00:00:00Z" }]);

  render(<TeamPanel master={new Uint8Array(32)} encPrivateKeys={new Uint8Array([1])} />);
  fireEvent.click(await screen.findByRole("button", { name: /org-a/ }));

  expect(await screen.findByText(/secret\.read/)).toBeInTheDocument();
  expect(api.fetchAudit).toHaveBeenCalledWith("org-a");
});

it("clears an old audit log when switching to a gated organisation", async () => {
  resetMocks();
  vi.mocked(api.fetchOrgs).mockResolvedValue([
    { ...org("org-a"), role: "owner" },
    { ...org("org-b"), role: "member" },
  ]);
  vi.mocked(api.fetchMembers).mockResolvedValue([]);
  vi.mocked(api.fetchEntitlements).mockImplementation(async (orgId) => ({
    ...freePlan,
    effectiveTier: orgId === "org-a" ? "team" : "free",
  }));
  vi.mocked(api.fetchAudit).mockResolvedValue([{ id: 1, actor: "user-a", action: "secret.read", target: null, envId: null, detail: null, at: "2026-09-30T00:00:00Z" }]);

  render(<TeamPanel master={new Uint8Array(32)} encPrivateKeys={new Uint8Array([1])} />);
  fireEvent.click(await screen.findByRole("button", { name: /org-a/ }));
  expect(await screen.findByText(/secret\.read/)).toBeInTheDocument();
  expect(api.fetchAudit).toHaveBeenCalledWith("org-a");

  fireEvent.click(screen.getByRole("button", { name: /org-b/ }));
  expect(await screen.findByRole("heading", { name: "Members of org-b" })).toBeInTheDocument();
  await act(async () => { await Promise.resolve(); });
  expect(screen.queryByRole("heading", { name: "Audit log" })).not.toBeInTheDocument();
  expect(api.fetchAudit).toHaveBeenCalledTimes(1);
});
