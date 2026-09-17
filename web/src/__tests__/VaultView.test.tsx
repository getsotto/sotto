import "@testing-library/jest-dom/vitest";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import * as api from "../api";
import type { Environment, Project } from "../api";
import { VaultView } from "../VaultView";
import * as vault from "../vault";
import type { SecretEntry } from "../vault";

vi.mock("../TeamPanel", () => ({ TeamPanel: () => null }));

vi.mock("../api", () => ({
  createGrant: vi.fn(),
  createShare: vi.fn(),
  fetchEnvironments: vi.fn(),
  fetchGrantHolders: vi.fn(),
  fetchHistory: vi.fn(),
  fetchMachineTokens: vi.fn(),
  fetchMembers: vi.fn(),
  fetchMyGrant: vi.fn(),
  fetchOrgs: vi.fn(),
  fetchProjects: vi.fn(),
  fetchSecrets: vi.fn(),
  fetchSnapshot: vi.fn(),
  grantOrgKey: vi.fn(),
  postRotate: vi.fn(),
}));

vi.mock("../vault", () => ({
  decryptEnvName: vi.fn(),
  decryptProjectName: vi.fn(),
  decryptSecretName: vi.fn(),
  decryptSecretValue: vi.fn(),
  openEnvGrant: vi.fn(),
  openOrgKey: vi.fn(),
  rewrapDataKey: vi.fn(),
  sealForShare: vi.fn(),
  sealGrantTo: vi.fn(),
}));

afterEach(cleanup);

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason?: unknown) => void;
  const promise = new Promise<T>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}

function project(id: string): Project {
  return { id, encName: new Uint8Array([1]), orgId: null };
}

function environment(id: string): Environment {
  return { id, encName: new Uint8Array([2]), encVaultKey: null };
}

function secret(id: string): SecretEntry {
  return {
    id,
    encName: new Uint8Array([3]),
    encValue: new Uint8Array([4]),
    encDataKey: new Uint8Array([5]),
    version: 1,
    deleted: false,
  };
}

function renderVault() {
  render(
    <VaultView
      master={new Uint8Array(32)}
      encPrivateKeys={new Uint8Array([9])}
      onLogout={vi.fn()}
    />,
  );
}

describe("VaultView selection loading", () => {
  beforeEach(() => {
    vi.resetAllMocks();
    vi.mocked(api.fetchOrgs).mockResolvedValue([]);
    vi.mocked(api.fetchProjects).mockResolvedValue([project("project-a"), project("project-b")]);
    vi.mocked(api.fetchMembers).mockResolvedValue([]);
    vi.mocked(api.fetchMyGrant).mockResolvedValue(new Uint8Array([7]));
    vi.mocked(api.fetchSecrets).mockResolvedValue([]);
    vi.mocked(vault.decryptProjectName).mockImplementation((_key, id) => id);
    vi.mocked(vault.decryptEnvName).mockImplementation((_key, id) => id);
    vi.mocked(vault.decryptSecretName).mockImplementation((_key, _envId, entry) => entry.id);
    vi.mocked(vault.openEnvGrant).mockReturnValue(new Uint8Array([8]));
  });

  it("keeps environments from the latest project when requests resolve out of order", async () => {
    const first = deferred<Environment[]>();
    const second = deferred<Environment[]>();
    vi.mocked(api.fetchEnvironments).mockImplementation((projectId) => {
      if (projectId === "project-a") {
        return first.promise;
      }
      if (projectId === "project-b") {
        return second.promise;
      }
      return Promise.resolve([]);
    });

    renderVault();

    fireEvent.click(await screen.findByRole("button", { name: /project-a/ }));
    fireEvent.click(screen.getByRole("button", { name: /project-b/ }));

    await act(async () => {
      second.resolve([environment("env-b")]);
    });
    expect(await screen.findByRole("button", { name: "env-b" })).toBeInTheDocument();

    await act(async () => {
      first.resolve([environment("env-a")]);
    });
    expect(screen.getByRole("button", { name: "env-b" })).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "env-a" })).not.toBeInTheDocument();
  });

  it("keeps secrets from the latest environment when requests resolve out of order", async () => {
    const first = deferred<SecretEntry[]>();
    const second = deferred<SecretEntry[]>();
    vi.mocked(api.fetchEnvironments).mockResolvedValue([
      environment("env-a"),
      environment("env-b"),
    ]);
    vi.mocked(api.fetchSecrets).mockImplementation((envId) => {
      if (envId === "env-a") {
        return first.promise;
      }
      if (envId === "env-b") {
        return second.promise;
      }
      return Promise.resolve([]);
    });

    renderVault();

    fireEvent.click(await screen.findByRole("button", { name: /project-a/ }));
    fireEvent.click(await screen.findByRole("button", { name: "env-a" }));
    await waitFor(() => expect(api.fetchSecrets).toHaveBeenCalledWith("env-a"));

    fireEvent.click(screen.getByRole("button", { name: "env-b" }));
    await waitFor(() => expect(api.fetchSecrets).toHaveBeenCalledWith("env-b"));

    await act(async () => {
      second.resolve([secret("secret-b")]);
    });
    expect(await screen.findByRole("button", { name: "secret-b" })).toBeInTheDocument();

    await act(async () => {
      first.resolve([secret("secret-a")]);
    });
    expect(screen.getByRole("button", { name: "secret-b" })).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "secret-a" })).not.toBeInTheDocument();
  });

  it("sends only one rotation request when rotate is clicked twice", async () => {
    vi.mocked(api.fetchProjects).mockResolvedValue([
      { id: "project-a", encName: new Uint8Array([1]), orgId: "org-1" },
    ]);
    vi.mocked(api.fetchEnvironments).mockResolvedValue([environment("env-a")]);
    vi.mocked(api.fetchMembers).mockResolvedValue([]);
    vi.mocked(api.fetchMyGrant).mockResolvedValue(new Uint8Array([7]));
    vi.mocked(api.fetchSnapshot).mockResolvedValue({ revision: 1, secrets: [] });
    vi.mocked(api.fetchHistory).mockResolvedValue([]);
    vi.mocked(api.fetchGrantHolders).mockResolvedValue([]);
    vi.mocked(api.fetchMachineTokens).mockResolvedValue([]);
    vi.mocked(api.fetchOrgs).mockResolvedValue([
      { id: "org-1", encName: new Uint8Array(), role: "owner", encOrgKey: null },
    ]);
    vi.mocked(vault.decryptProjectName).mockReturnValue("project-a");
    vi.mocked(vault.decryptEnvName).mockReturnValue("env-a");
    vi.mocked(vault.openEnvGrant).mockReturnValue(new Uint8Array([8]));

    const rotation = deferred<void>();
    vi.mocked(api.postRotate).mockReturnValue(rotation.promise);

    renderVault();

    fireEvent.click(await screen.findByRole("button", { name: /project-a/ }));
    fireEvent.click(await screen.findByRole("button", { name: "env-a" }));
    const rotateButton = await screen.findByRole("button", { name: "Rotate environment key" });

    await act(async () => {
      fireEvent.click(rotateButton);
      fireEvent.click(rotateButton);
    });

    await waitFor(() =>
      expect(api.postRotate).toHaveBeenCalledTimes(1),
    );

    await act(async () => {
      rotation.resolve();
    });
  });

  it("sends only one grant request when share is submitted twice", async () => {
    vi.mocked(api.fetchProjects).mockResolvedValue([
      { id: "project-a", encName: new Uint8Array([1]), orgId: "org-1" },
    ]);
    vi.mocked(api.fetchEnvironments).mockResolvedValue([environment("env-a")]);
    vi.mocked(api.fetchMembers).mockResolvedValue([
      { userId: "member-a", role: "member", publicKey: new Uint8Array([6]) },
    ]);
    vi.mocked(api.fetchMyGrant).mockResolvedValue(new Uint8Array([7]));
    vi.mocked(api.fetchSecrets).mockResolvedValue([]);
    vi.mocked(api.fetchOrgs).mockResolvedValue([
      { id: "org-1", encName: new Uint8Array(), role: "owner", encOrgKey: null },
    ]);
    vi.mocked(vault.decryptProjectName).mockReturnValue("project-a");
    vi.mocked(vault.decryptEnvName).mockReturnValue("env-a");
    vi.mocked(vault.openEnvGrant).mockReturnValue(new Uint8Array([8]));
    vi.mocked(vault.sealGrantTo).mockReturnValue(new Uint8Array([9]));

    const shareRequest = deferred<void>();
    vi.mocked(api.createGrant).mockReturnValue(shareRequest.promise);

    renderVault();

    fireEvent.click(await screen.findByRole("button", { name: /project-a/ }));
    fireEvent.click(await screen.findByRole("button", { name: "env-a" }));
    const memberSelect = await screen.findByRole("combobox", { name: /Share this environment with/ });
    fireEvent.change(memberSelect, { target: { value: "member-a" } });
    const shareForm = screen.getByRole("button", { name: "Share" }).closest("form");
    expect(shareForm).not.toBeNull();
    if (shareForm === null) {
      return;
    }

    await act(async () => {
      fireEvent.submit(shareForm);
      fireEvent.submit(shareForm);
    });

    expect(api.createGrant).toHaveBeenCalledTimes(1);
    expect(screen.getByRole("button", { name: "Sharing…" })).toBeDisabled();
    expect(memberSelect).toBeDisabled();

    await act(async () => {
      shareRequest.resolve();
    });

    await waitFor(() => {
      expect(screen.getByRole("button", { name: "Share" })).toBeEnabled();
    });
    expect(memberSelect).toBeEnabled();
  });
});
