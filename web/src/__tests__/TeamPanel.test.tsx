import "@testing-library/jest-dom/vitest";
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import * as api from "../api";
import type { Entitlements, Member, Org } from "../api";
import { TeamPanel } from "../TeamPanel";
import * as vault from "../vault";

vi.mock("../api", () => ({
  createCheckout: vi.fn(),
  createPortal: vi.fn(),
  fetchAudit: vi.fn(),
  fetchEntitlements: vi.fn(),
  fetchMembers: vi.fn(),
  fetchOrgs: vi.fn(),
  grantOrgKey: vi.fn(),
  inviteMember: vi.fn(),
}));

vi.mock("../vault", () => ({
  decryptOrgName: vi.fn(),
  openOrgKey: vi.fn(),
  sealGrantTo: vi.fn(),
}));

afterEach(cleanup);

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
};

describe("TeamPanel organisation loading", () => {
  beforeEach(() => {
    vi.resetAllMocks();
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

describe("TeamPanel invitations", () => {
  const adminOrg: Org = {
    id: "org-a",
    encName: new Uint8Array(),
    role: "admin",
    encOrgKey: null,
  };

  beforeEach(() => {
    vi.resetAllMocks();
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
    expect(screen.getByRole("button", { name: "Inviting…" })).toBeDisabled();
    fireEvent.submit(input.closest("form")!);
    expect(api.inviteMember).toHaveBeenCalledOnce();
    expect(api.inviteMember).toHaveBeenCalledWith("org-a", "teammate@example.com");

    await act(async () => {
      pending.resolve({ userId: "user-b", publicKey: null });
    });
    expect(screen.getByText("invited teammate@example.com (user-b)")).toBeInTheDocument();
    expect(input).toHaveValue("");
    expect(input).not.toBeDisabled();
    expect(api.fetchMembers).toHaveBeenCalledTimes(2);
  });

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
