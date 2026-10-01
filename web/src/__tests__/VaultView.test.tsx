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

function secret(id: string, deleted = false): SecretEntry {
  return {
    id,
    encName: new Uint8Array([3]),
    encValue: new Uint8Array([4]),
    encDataKey: new Uint8Array([5]),
    version: 1,
    deleted,
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


  it("shows progress while opening an environment and clears it after success", async () => {
    const secrets = deferred<SecretEntry[]>();
    vi.mocked(api.fetchEnvironments).mockResolvedValue([environment("env-a")]);
    vi.mocked(api.fetchSecrets).mockReturnValue(secrets.promise);
    renderVault();

    fireEvent.click(await screen.findByRole("button", { name: /project-a/ }));
    const envButton = await screen.findByRole("button", { name: "env-a" });
    fireEvent.click(envButton);
    expect(await screen.findByText("Opening environment…", { selector: '[role="status"]' })).toHaveTextContent("Opening environment…");
    expect(envButton).toHaveAttribute("aria-busy", "true");
    expect(screen.queryByText("No secrets in this environment.")).not.toBeInTheDocument();

    await act(async () => secrets.resolve([]));
    expect(screen.queryByText("Opening environment…", { selector: '[role="status"]' })).not.toBeInTheDocument();
    expect(screen.getByText("No secrets in this environment.")).toBeInTheDocument();
  });

  it("keeps progress owned by the latest environment request", async () => {
    const first = deferred<SecretEntry[]>();
    const second = deferred<SecretEntry[]>();
    vi.mocked(api.fetchEnvironments).mockResolvedValue([environment("env-a"), environment("env-b")]);
    vi.mocked(api.fetchSecrets).mockImplementation((id) => id === "env-a" ? first.promise : second.promise);
    renderVault();

    fireEvent.click(await screen.findByRole("button", { name: /project-a/ }));
    fireEvent.click(await screen.findByRole("button", { name: "env-a" }));
    await waitFor(() => expect(api.fetchSecrets).toHaveBeenCalledWith("env-a"));
    fireEvent.click(screen.getByRole("button", { name: "env-b" }));
    await waitFor(() => expect(api.fetchSecrets).toHaveBeenCalledWith("env-b"));

    await act(async () => first.reject(new Error("stale failure")));
    expect(screen.getByText("Opening environment…", { selector: '[role="status"]' })).toHaveTextContent("Opening environment…");
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();

    await act(async () => second.resolve([]));
    expect(screen.queryByText("Opening environment…", { selector: '[role="status"]' })).not.toBeInTheDocument();
    expect(screen.getByText("No secrets in this environment.")).toBeInTheDocument();
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

  function mockOrgShareVault(envIds: string[] = ["env-a"]) {
    vi.mocked(api.fetchProjects).mockResolvedValue([
      { id: "project-a", encName: new Uint8Array([1]), orgId: "org-1" },
    ]);
    vi.mocked(api.fetchEnvironments).mockResolvedValue(envIds.map((id) => environment(id)));
    vi.mocked(api.fetchMembers).mockResolvedValue([
      { userId: "member-a", role: "member", publicKey: new Uint8Array([6]) },
    ]);
    vi.mocked(api.fetchMyGrant).mockResolvedValue(new Uint8Array([7]));
    vi.mocked(api.fetchSecrets).mockResolvedValue([]);
    vi.mocked(api.fetchOrgs).mockResolvedValue([
      { id: "org-1", encName: new Uint8Array(), role: "owner", encOrgKey: null },
    ]);
    vi.mocked(vault.decryptProjectName).mockReturnValue("project-a");
    vi.mocked(vault.decryptEnvName).mockImplementation((_key, id) => id);
    vi.mocked(vault.openEnvGrant).mockReturnValue(new Uint8Array([8]));
    vi.mocked(vault.sealGrantTo).mockReturnValue(new Uint8Array([9]));
  }

  async function openEnvAndPickMember(envName: string) {
    fireEvent.click(await screen.findByRole("button", { name: /project-a/ }));
    fireEvent.click(await screen.findByRole("button", { name: envName }));
    const memberSelect = await screen.findByRole("combobox", {
      name: /Share this environment with/,
    });
    fireEvent.change(memberSelect, { target: { value: "member-a" } });
    const shareForm = screen.getByRole("button", { name: "Share" }).closest("form");
    expect(shareForm).not.toBeNull();
    return shareForm;
  }

  it("shows a share notice when the environment is still selected", async () => {
    mockOrgShareVault();
    const shareRequest = deferred<void>();
    vi.mocked(api.createGrant).mockReturnValue(shareRequest.promise);

    renderVault();
    const shareForm = await openEnvAndPickMember("env-a");
    if (shareForm === null) {
      return;
    }

    fireEvent.submit(shareForm);
    expect(api.createGrant).toHaveBeenCalledTimes(1);

    await act(async () => {
      shareRequest.resolve();
    });

    expect(await screen.findByText("shared this environment with member-a", { selector: '[role="status"] *' })).toHaveTextContent("shared this environment with member-a");
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
  });

  it("does not show a share notice after switching environments", async () => {
    mockOrgShareVault(["env-a", "env-b"]);
    const shareRequest = deferred<void>();
    vi.mocked(api.createGrant).mockReturnValue(shareRequest.promise);

    renderVault();
    const shareForm = await openEnvAndPickMember("env-a");
    if (shareForm === null) {
      return;
    }

    fireEvent.submit(shareForm);
    expect(api.createGrant).toHaveBeenCalledTimes(1);

    fireEvent.click(screen.getByRole("button", { name: "env-b" }));
    expect(await screen.findByRole("button", { name: "env-b", current: true })).toBeInTheDocument();

    await act(async () => {
      shareRequest.resolve();
    });

    expect(screen.queryByText("shared this environment with member-a")).not.toBeInTheDocument();
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    expect(api.createGrant).toHaveBeenCalledTimes(1);
  });

  it("does not show a share error after switching environments", async () => {
    mockOrgShareVault(["env-a", "env-b"]);
    const shareRequest = deferred<void>();
    vi.mocked(api.createGrant).mockReturnValue(shareRequest.promise);

    renderVault();
    const shareForm = await openEnvAndPickMember("env-a");
    if (shareForm === null) {
      return;
    }

    fireEvent.submit(shareForm);
    fireEvent.click(screen.getByRole("button", { name: "env-b" }));
    expect(await screen.findByRole("button", { name: "env-b", current: true })).toBeInTheDocument();

    await act(async () => {
      shareRequest.reject(new Error("share failed"));
    });

    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    expect(screen.queryByText("share failed")).not.toBeInTheDocument();
  });

  it("does not show a share notice after switching projects", async () => {
    mockOrgShareVault(["env-a"]);
    vi.mocked(api.fetchProjects).mockResolvedValue([
      { id: "project-a", encName: new Uint8Array([1]), orgId: "org-1" },
      { id: "project-b", encName: new Uint8Array([1]), orgId: null },
    ]);
    vi.mocked(api.fetchEnvironments).mockImplementation(async (projectId) => {
      if (projectId === "project-a") {
        return [environment("env-a")];
      }
      if (projectId === "project-b") {
        return [environment("env-c")];
      }
      return [];
    });
    vi.mocked(vault.decryptProjectName).mockImplementation((_key, id) => id);
    const shareRequest = deferred<void>();
    vi.mocked(api.createGrant).mockReturnValue(shareRequest.promise);

    renderVault();
    const shareForm = await openEnvAndPickMember("env-a");
    if (shareForm === null) {
      return;
    }

    fireEvent.submit(shareForm);
    fireEvent.click(await screen.findByRole("button", { name: /project-b/ }));
    expect(await screen.findByRole("button", { name: "env-c" })).toBeInTheDocument();

    await act(async () => {
      shareRequest.resolve();
    });

    expect(screen.queryByText("shared this environment with member-a")).not.toBeInTheDocument();
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
  });

  it("renders only live secrets and never decrypts deleted entries", async () => {
    vi.mocked(api.fetchEnvironments).mockResolvedValue([environment("env-a")]);
    vi.mocked(api.fetchSecrets).mockResolvedValue([
      secret("secret-deleted", true),
      secret("secret-live"),
    ]);

    renderVault();

    fireEvent.click(await screen.findByRole("button", { name: /project-a/ }));
    fireEvent.click(await screen.findByRole("button", { name: "env-a" }));

    expect(await screen.findByRole("button", { name: "secret-live" })).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "secret-deleted" })).not.toBeInTheDocument();

    // Check the call arguments too: nameOr() falls back to the id, so visible output alone
    // could hide an unwanted decrypt attempt.
    expect(vault.decryptSecretName).toHaveBeenCalledTimes(1);
    expect(vault.decryptSecretName).toHaveBeenCalledWith(
      new Uint8Array([8]),
      "env-a",
      expect.objectContaining({ id: "secret-live" }),
    );
    expect(vault.decryptSecretName).not.toHaveBeenCalledWith(
      expect.anything(),
      expect.anything(),
      expect.objectContaining({ id: "secret-deleted" }),
    );
    expect(vault.decryptSecretValue).not.toHaveBeenCalled();
  });

  it("shows the empty state when the server returns only deleted entries", async () => {
    vi.mocked(api.fetchEnvironments).mockResolvedValue([environment("env-a")]);
    vi.mocked(api.fetchSecrets).mockResolvedValue([
      secret("secret-deleted-a", true),
      secret("secret-deleted-b", true),
    ]);

    renderVault();

    fireEvent.click(await screen.findByRole("button", { name: /project-a/ }));
    fireEvent.click(await screen.findByRole("button", { name: "env-a" }));

    expect(await screen.findByText("No secrets in this environment.")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "secret-deleted-a" })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "secret-deleted-b" })).not.toBeInTheDocument();

    expect(vault.decryptSecretName).not.toHaveBeenCalled();
    expect(vault.decryptSecretValue).not.toHaveBeenCalled();
  });

  it("decrypts a live secret value only when its button is selected", async () => {
    vi.mocked(api.fetchEnvironments).mockResolvedValue([environment("env-a")]);
    vi.mocked(api.fetchSecrets).mockResolvedValue([
      secret("secret-deleted", true),
      secret("secret-live"),
    ]);
    vi.mocked(vault.decryptSecretValue).mockReturnValue("synthetic-value");

    renderVault();

    fireEvent.click(await screen.findByRole("button", { name: /project-a/ }));
    fireEvent.click(await screen.findByRole("button", { name: "env-a" }));

    const liveButton = await screen.findByRole("button", { name: "secret-live" });
    expect(vault.decryptSecretValue).not.toHaveBeenCalled();

    fireEvent.click(liveButton);

    expect(await screen.findByDisplayValue("synthetic-value")).toBeInTheDocument();
    expect(vault.decryptSecretValue).toHaveBeenCalledTimes(1);
    expect(vault.decryptSecretValue).toHaveBeenCalledWith(
      new Uint8Array([8]),
      "env-a",
      expect.objectContaining({ id: "secret-live" }),
    );
    expect(vault.decryptSecretValue).not.toHaveBeenCalledWith(
      expect.anything(),
      expect.anything(),
      expect.objectContaining({ id: "secret-deleted" }),
    );
  });
});


describe("VaultView rotation ownership", () => {
  it("does not reopen a rotated environment after the selection changes", async () => {
    vi.resetAllMocks();
    vi.mocked(api.fetchProjects).mockResolvedValue([
      { id: "project-a", encName: new Uint8Array([1]), orgId: "org-1" },
    ]);
    vi.mocked(api.fetchOrgs).mockResolvedValue([
      { id: "org-1", encName: new Uint8Array(), role: "owner", encOrgKey: null },
    ]);
    vi.mocked(api.fetchEnvironments).mockResolvedValue([environment("env-a"), environment("env-b")]);
    vi.mocked(api.fetchMembers).mockResolvedValue([]);
    vi.mocked(api.fetchMyGrant).mockResolvedValue(new Uint8Array([7]));
    vi.mocked(api.fetchSecrets).mockResolvedValue([]);
    vi.mocked(api.fetchSnapshot).mockResolvedValue({ revision: 1, secrets: [] });
    vi.mocked(api.fetchHistory).mockResolvedValue([]);
    vi.mocked(api.fetchGrantHolders).mockResolvedValue([]);
    vi.mocked(api.fetchMachineTokens).mockResolvedValue([]);
    vi.mocked(vault.decryptProjectName).mockReturnValue("project-a");
    vi.mocked(vault.decryptEnvName).mockImplementation((_key, id) => id);
    vi.mocked(vault.openEnvGrant).mockReturnValue(new Uint8Array([8]));
    const rotation = deferred<void>();
    vi.mocked(api.postRotate).mockReturnValue(rotation.promise);

    renderVault();
    fireEvent.click(await screen.findByRole("button", { name: /project-a/ }));
    fireEvent.click(await screen.findByRole("button", { name: "env-a" }));
    fireEvent.click(await screen.findByRole("button", { name: "Rotate environment key" }));
    await waitFor(() => expect(api.postRotate).toHaveBeenCalledOnce());
    fireEvent.click(screen.getByRole("button", { name: "env-b" }));
    await waitFor(() => expect(screen.getByRole("button", { name: "env-b" })).toHaveAttribute("aria-current", "true"));
    await act(async () => rotation.resolve());
    expect(screen.getByRole("button", { name: "env-b" })).toHaveAttribute("aria-current", "true");
    expect(api.fetchMyGrant).toHaveBeenCalledTimes(2);
  });

  it("ignores a late rotation error after the selection changes", async () => {
    vi.resetAllMocks();
    vi.mocked(api.fetchProjects).mockResolvedValue([
      { id: "project-a", encName: new Uint8Array([1]), orgId: "org-1" },
    ]);
    vi.mocked(api.fetchOrgs).mockResolvedValue([
      { id: "org-1", encName: new Uint8Array(), role: "owner", encOrgKey: null },
    ]);
    vi.mocked(api.fetchEnvironments).mockResolvedValue([environment("env-a"), environment("env-b")]);
    vi.mocked(api.fetchMembers).mockResolvedValue([]);
    vi.mocked(api.fetchMyGrant).mockResolvedValue(new Uint8Array([7]));
    vi.mocked(api.fetchSecrets).mockResolvedValue([]);
    vi.mocked(api.fetchSnapshot).mockResolvedValue({ revision: 1, secrets: [] });
    vi.mocked(api.fetchHistory).mockResolvedValue([]);
    vi.mocked(api.fetchGrantHolders).mockResolvedValue([]);
    vi.mocked(api.fetchMachineTokens).mockResolvedValue([]);
    vi.mocked(vault.decryptProjectName).mockReturnValue("project-a");
    vi.mocked(vault.decryptEnvName).mockImplementation((_key, id) => id);
    vi.mocked(vault.openEnvGrant).mockReturnValue(new Uint8Array([8]));
    const rotation = deferred<void>();
    vi.mocked(api.postRotate).mockReturnValue(rotation.promise);

    renderVault();
    fireEvent.click(await screen.findByRole("button", { name: /project-a/ }));
    fireEvent.click(await screen.findByRole("button", { name: "env-a" }));
    fireEvent.click(await screen.findByRole("button", { name: "Rotate environment key" }));
    await waitFor(() => expect(api.postRotate).toHaveBeenCalledOnce());
    fireEvent.click(screen.getByRole("button", { name: "env-b" }));
    await waitFor(() => expect(screen.getByRole("button", { name: "env-b" })).toHaveAttribute("aria-current", "true"));
    await act(async () => rotation.reject(new Error("stale rotation failure")));
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "env-b" })).toHaveAttribute("aria-current", "true");
  });

  it("reloads and reports success when the rotated environment stays selected", async () => {
    vi.resetAllMocks();
    vi.mocked(api.fetchProjects).mockResolvedValue([
      { id: "project-a", encName: new Uint8Array([1]), orgId: "org-1" },
    ]);
    vi.mocked(api.fetchOrgs).mockResolvedValue([
      { id: "org-1", encName: new Uint8Array(), role: "owner", encOrgKey: null },
    ]);
    vi.mocked(api.fetchEnvironments).mockResolvedValue([environment("env-a")]);
    vi.mocked(api.fetchMembers).mockResolvedValue([]);
    vi.mocked(api.fetchMyGrant).mockResolvedValue(new Uint8Array([7]));
    vi.mocked(api.fetchSecrets).mockResolvedValue([]);
    vi.mocked(api.fetchSnapshot).mockResolvedValue({ revision: 1, secrets: [] });
    vi.mocked(api.fetchHistory).mockResolvedValue([]);
    vi.mocked(api.fetchGrantHolders).mockResolvedValue([]);
    vi.mocked(api.fetchMachineTokens).mockResolvedValue([]);
    vi.mocked(vault.decryptProjectName).mockReturnValue("project-a");
    vi.mocked(vault.decryptEnvName).mockImplementation((_key, id) => id);
    vi.mocked(vault.openEnvGrant).mockReturnValue(new Uint8Array([8]));
    const rotation = deferred<void>();
    vi.mocked(api.postRotate).mockReturnValue(rotation.promise);

    renderVault();
    fireEvent.click(await screen.findByRole("button", { name: /project-a/ }));
    fireEvent.click(await screen.findByRole("button", { name: "env-a" }));
    await waitFor(() => expect(api.fetchMyGrant).toHaveBeenCalledTimes(1));
    fireEvent.click(await screen.findByRole("button", { name: "Rotate environment key" }));
    await waitFor(() => expect(api.postRotate).toHaveBeenCalledOnce());
    await act(async () => rotation.resolve());

    await waitFor(() => expect(api.fetchMyGrant).toHaveBeenCalledTimes(2));
    expect(await screen.findByText("environment key rotated", { selector: '[role="status"] *' })).toHaveTextContent("environment key rotated");
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "env-a" })).toHaveAttribute("aria-current", "true");
  });
});


describe("VaultView project loading recovery", () => {
  it("retries a failed project load without re-unlocking", async () => {
    vi.resetAllMocks();
    vi.mocked(api.fetchOrgs).mockResolvedValue([]);
    vi.mocked(api.fetchProjects)
      .mockRejectedValueOnce(new Error("projects unavailable"))
      .mockResolvedValueOnce([project("project-recovered")]);
    vi.mocked(vault.decryptProjectName).mockImplementation((_key, id) => id);

    renderVault();

    expect(await screen.findByRole("alert")).toHaveTextContent("projects unavailable");
    fireEvent.click(screen.getByRole("button", { name: "Retry projects" }));
    expect(await screen.findByRole("button", { name: /project-recovered/ })).toBeInTheDocument();
    expect(api.fetchProjects).toHaveBeenCalledTimes(2);
  });
});


describe("VaultView independent initial loads", () => {
  it("loads personal projects when organisation discovery fails", async () => {
    vi.resetAllMocks();
    vi.mocked(api.fetchOrgs).mockRejectedValue(new Error("offline"));
    vi.mocked(api.fetchProjects).mockResolvedValue([project("personal-a")]);
    vi.mocked(vault.decryptProjectName).mockImplementation((_key, id) => id);

    renderVault();

    expect(await screen.findByRole("button", { name: /personal-a/ })).toBeInTheDocument();
    expect(screen.getByRole("alert")).toHaveTextContent("organisations unavailable: offline");
    expect(api.fetchProjects).toHaveBeenCalledOnce();
  });
});


describe("VaultView navigation ordering", () => {
  it("sorts projects and environments by decrypted display name", async () => {
    vi.resetAllMocks();
    vi.mocked(api.fetchOrgs).mockResolvedValue([]);
    vi.mocked(api.fetchProjects).mockResolvedValue([project("project-b"), project("project-a")]);
    vi.mocked(api.fetchEnvironments).mockResolvedValue([environment("env-b"), environment("env-a")]);
    vi.mocked(vault.decryptProjectName).mockImplementation((_key, id) => id);
    vi.mocked(vault.decryptEnvName).mockImplementation((_key, id) => id);

    renderVault();

    const projects = screen.getByRole("heading", { name: "Projects" }).parentElement!;
    await screen.findByRole("button", { name: /project-a/ });
    expect([...projects.querySelectorAll("button")].map((button) => button.textContent)).toEqual([
      "project-apersonal",
      "project-bpersonal",
    ]);
    fireEvent.click(screen.getByRole("button", { name: /project-a/ }));
    const environments = await screen.findByRole("heading", { name: "Environments" });
    expect([...environments.parentElement!.querySelectorAll("button")].map((button) => button.textContent))
      .toEqual(["env-a", "env-b"]);
  });
});

describe("VaultView secret copying", () => {
  it("copies the exact revealed value only after explicit activation", async () => {
    vi.resetAllMocks();
    vi.mocked(api.fetchOrgs).mockResolvedValue([]);
    vi.mocked(api.fetchProjects).mockResolvedValue([project("project-a")]);
    vi.mocked(api.fetchEnvironments).mockResolvedValue([environment("env-a")]);
    vi.mocked(api.fetchMyGrant).mockResolvedValue(new Uint8Array([7]));
    vi.mocked(api.fetchSecrets).mockResolvedValue([secret("secret-a")]);
    vi.mocked(vault.decryptProjectName).mockImplementation((_key, id) => id);
    vi.mocked(vault.decryptEnvName).mockImplementation((_key, id) => id);
    vi.mocked(vault.decryptSecretName).mockImplementation((_key, _env, entry) => entry.id);
    vi.mocked(vault.openEnvGrant).mockReturnValue(new Uint8Array([8]));
    vi.mocked(vault.decryptSecretValue).mockReturnValue("  first line\nsecond line  ");
    const writeText = vi.fn().mockResolvedValue(undefined);
    Object.defineProperty(navigator, "clipboard", { configurable: true, value: { writeText } });

    renderVault();
    fireEvent.click(await screen.findByRole("button", { name: /project-a/ }));
    fireEvent.click(await screen.findByRole("button", { name: "env-a" }));
    fireEvent.click(await screen.findByRole("button", { name: "secret-a" }));
    expect(writeText).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole("button", { name: "Copy secret" }));
    expect(await screen.findByText("Secret copied.", { selector: '[role="status"]' })).toHaveTextContent("Secret copied.");
    expect(writeText).toHaveBeenCalledWith("  first line\nsecond line  ");
  });
});
