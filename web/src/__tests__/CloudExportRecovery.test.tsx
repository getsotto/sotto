import "@testing-library/jest-dom/vitest";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import * as api from "../api";
import { CloudAccountPanel } from "../CloudAccountPanel";

vi.mock("../api", () => ({
  confirmCloudContactVerification: vi.fn(),
  fetchCloudContactStatus: vi.fn(),
  UnsupportedCloudContactError: class UnsupportedCloudContactError extends Error {},
  revokeCloudContact: vi.fn(),
  startCloudContactVerification: vi.fn(),
  cancelPersonalBilling: vi.fn(),
  acceptPayerTransfer: vi.fn(),
  beginPayerTransfer: vi.fn(),
  createPayerTransferPaymentSetup: vi.fn(),
  createPersonalCheckout: vi.fn(),
  createPersonalPortal: vi.fn(),
  fetchCloudExportChunk: vi.fn(),
  fetchCloudNotices: vi.fn(),
  fetchPayerTransferCapabilities: vi.fn(),
  fetchPayerTransfer: vi.fn(),
  fetchPayerTransfers: vi.fn(),
  fetchPersonalLifecycle: vi.fn(),
  fetchMembers: vi.fn(),
  fetchOrgs: vi.fn(),
  fetchEligibility: vi.fn(),
  fetchPersonalQuote: vi.fn(),
  me: vi.fn(),
  quotePayerTransfer: vi.fn(),
  fetchPersonalOperation: vi.fn(),
  requestPersonalRefund: vi.fn(),
  withdrawPayerTransfer: vi.fn(),
  startCloudExport: vi.fn(),
}));

const manifest: api.ExportManifest = {
  version: 1, exportId: "export-first", manifestHash: "synthetic-manifest-hash",
  expiresAt: "2030-01-01T00:00:00Z", totalChunks: 2, complete: true,
  projects: [], environments: [], notSharedEnvironmentIds: [], omittedEnvironmentCount: 0,
};

const lifecycleFixture: api.PersonalLifecycle = {
  state: "active",
  paidThroughDate: "2030-01-01",
  cancelAtPeriodEnd: false,
  portalUrl: null,
  foundingPriceEndEpoch: null,
  standardRenewalAmountPence: null,
  foundingTransitionStatus: null,
  foundingTransitionReason: null,
};

beforeEach(() => {
  vi.resetAllMocks();
  vi.mocked(api.fetchPayerTransferCapabilities).mockResolvedValue({ available: false, setupAvailable: false });
  vi.mocked(api.fetchPayerTransfers).mockResolvedValue({ transfers: [], nextCursor: null });
  vi.mocked(api.me).mockResolvedValue(null);
  vi.mocked(api.fetchOrgs).mockResolvedValue([]);
  vi.mocked(api.fetchMembers).mockResolvedValue([]);
  vi.mocked(api.fetchEligibility).mockResolvedValue({
    state: "export_only", accountInitialized: true, billingAvailable: false,
    paidThroughEpoch: null, recoveryUntilEpoch: null, exportUntilEpoch: null,
    actions: { setup: false, billing: false, export: true, revoke: false },
    payer: null, deploymentMode: "cloud",
  });
  vi.mocked(api.fetchCloudNotices).mockResolvedValue([]);
  vi.mocked(api.fetchPersonalLifecycle).mockResolvedValue(lifecycleFixture);
  vi.mocked(api.fetchCloudContactStatus).mockResolvedValue({ email: null, pendingExpiresAtEpoch: null, enrolmentAvailable: false });
});

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

describe("Cloud encrypted export recovery", () => {
  it.each(["manifest", "first chunk", "later chunk"] as const)(
    "does not download a partial export after a failed %s and allows a fresh retry",
    async (failure) => {
      const createUrl = vi.spyOn(URL, "createObjectURL").mockReturnValue("blob:synthetic-export");
      const revokeUrl = vi.spyOn(URL, "revokeObjectURL").mockImplementation(() => {});
      const download = vi.spyOn(HTMLAnchorElement.prototype, "click").mockImplementation(() => {});
      if (failure === "manifest") {
        vi.mocked(api.startCloudExport).mockRejectedValueOnce(new Error("synthetic export failure"));
      } else {
        vi.mocked(api.startCloudExport).mockResolvedValueOnce(manifest);
        if (failure === "later chunk") {
          vi.mocked(api.fetchCloudExportChunk).mockResolvedValueOnce({ ciphertext: "discarded-chunk" });
        }
        vi.mocked(api.fetchCloudExportChunk).mockRejectedValueOnce(new Error("synthetic export failure"));
      }

      render(<CloudAccountPanel />);
      const button = await screen.findByRole("button", { name: "Download encrypted export" });
      expect(api.startCloudExport).not.toHaveBeenCalled();
      fireEvent.click(button);

      expect(await screen.findByRole("alert")).toHaveTextContent("synthetic export failure");
      expect(button).toBeEnabled();
      expect(api.startCloudExport).toHaveBeenCalledOnce();
      expect(api.fetchCloudExportChunk).toHaveBeenCalledTimes(
        failure === "manifest" ? 0 : failure === "first chunk" ? 1 : 2,
      );
      expect(createUrl).not.toHaveBeenCalled();
      expect(download).not.toHaveBeenCalled();
      expect(revokeUrl).not.toHaveBeenCalled();
      expect(screen.queryByRole("status")).not.toBeInTheDocument();

      vi.mocked(api.startCloudExport).mockClear();
      vi.mocked(api.fetchCloudExportChunk).mockClear();
      let resolveManifest!: (value: api.ExportManifest) => void;
      vi.mocked(api.startCloudExport).mockImplementationOnce(
        () => new Promise((resolve) => { resolveManifest = resolve; }),
      );
      const freshManifest = { ...manifest, exportId: "export-retry" };
      const chunks = [{ ciphertext: "fresh-chunk-zero" }, { ciphertext: "fresh-chunk-one" }];
      vi.mocked(api.fetchCloudExportChunk)
        .mockResolvedValueOnce(chunks[0]).mockResolvedValueOnce(chunks[1]);
      fireEvent.click(button);

      expect(button).toBeDisabled();
      expect(screen.queryByRole("alert")).not.toBeInTheDocument();
      fireEvent.click(button);
      expect(api.startCloudExport).toHaveBeenCalledOnce();
      expect(api.fetchCloudExportChunk).not.toHaveBeenCalled();
      expect(download).not.toHaveBeenCalled();

      await act(async () => { resolveManifest(freshManifest); });

      expect(screen.getByRole("status")).toHaveTextContent("Encrypted export downloaded");
      expect(button).toBeEnabled();
      expect(vi.mocked(api.fetchCloudExportChunk).mock.calls).toEqual([["export-retry", 0], ["export-retry", 1]]);
      expect(createUrl).toHaveBeenCalledOnce();
      const blob = createUrl.mock.calls[0][0] as Blob;
      expect(blob.type).toBe("application/json");
      expect(JSON.parse(await blob.text())).toEqual({ manifest: freshManifest, chunks });
      expect(download).toHaveBeenCalledOnce();
      expect((download.mock.contexts[0] as HTMLAnchorElement).download)
        .toBe("sotto-cloud-export-export-retry.json");
      expect(revokeUrl).toHaveBeenCalledExactlyOnceWith("blob:synthetic-export");
      expect(api.createPersonalCheckout).not.toHaveBeenCalled();
    },
  );

  it("shows preparation progress while manifest is pending, incremental chunk progress, and replaces with success notice", async () => {
    vi.spyOn(URL, "createObjectURL").mockReturnValue("blob:synthetic-export");
    vi.spyOn(URL, "revokeObjectURL").mockImplementation(() => {});
    vi.spyOn(HTMLAnchorElement.prototype, "click").mockImplementation(() => {});

    let resolveManifest!: (value: api.ExportManifest) => void;
    vi.mocked(api.startCloudExport).mockImplementationOnce(
      () => new Promise((resolve) => { resolveManifest = resolve; }),
    );

    let resolveChunk0!: (value: { ciphertext: string }) => void;
    let resolveChunk1!: (value: { ciphertext: string }) => void;
    vi.mocked(api.fetchCloudExportChunk)
      .mockImplementationOnce(() => new Promise((resolve) => { resolveChunk0 = resolve; }))
      .mockImplementationOnce(() => new Promise((resolve) => { resolveChunk1 = resolve; }));

    render(<CloudAccountPanel />);
    const button = await screen.findByRole("button", { name: "Download encrypted export" });

    fireEvent.click(button);
    expect(button).toBeDisabled();
    expect(screen.getByRole("status")).toHaveTextContent("Preparing encrypted export…");

    await act(async () => { resolveManifest(manifest); });
    expect(screen.getByRole("status")).toHaveTextContent("Downloading encrypted export: 0 of 2 parts.");
    expect(button).toBeDisabled();

    await act(async () => { resolveChunk0({ ciphertext: "chunk-zero" }); });
    expect(screen.getByRole("status")).toHaveTextContent("Downloading encrypted export: 1 of 2 parts.");
    expect(button).toBeDisabled();

    await act(async () => { resolveChunk1({ ciphertext: "chunk-one" }); });
    expect(screen.getByRole("status")).toHaveTextContent("Encrypted export downloaded. Keep it with your recovery materials.");
    expect(button).toBeEnabled();
  });

  it("removes progress on failure, preserves error, and restarts progress on retry", async () => {
    vi.spyOn(URL, "createObjectURL").mockReturnValue("blob:synthetic-export");
    vi.spyOn(URL, "revokeObjectURL").mockImplementation(() => {});
    vi.spyOn(HTMLAnchorElement.prototype, "click").mockImplementation(() => {});

    let rejectChunk1!: (reason: unknown) => void;
    vi.mocked(api.startCloudExport).mockResolvedValueOnce(manifest);
    vi.mocked(api.fetchCloudExportChunk)
      .mockResolvedValueOnce({ ciphertext: "chunk-zero" })
      .mockImplementationOnce(() => new Promise((_, reject) => { rejectChunk1 = reject; }));

    render(<CloudAccountPanel />);
    const button = await screen.findByRole("button", { name: "Download encrypted export" });

    fireEvent.click(button);
    expect(await screen.findByText("Downloading encrypted export: 1 of 2 parts.")).toBeInTheDocument();

    await act(async () => { rejectChunk1(new Error("network failure")); });

    expect(screen.queryByText(/Downloading encrypted export/)).not.toBeInTheDocument();
    expect(screen.queryByText(/Preparing encrypted export/)).not.toBeInTheDocument();
    expect(screen.queryByRole("status")).not.toBeInTheDocument();
    expect(screen.getByRole("alert")).toHaveTextContent("network failure");
    expect(button).toBeEnabled();

    let resolveRetryManifest!: (value: api.ExportManifest) => void;
    vi.mocked(api.startCloudExport).mockImplementationOnce(
      () => new Promise((resolve) => { resolveRetryManifest = resolve; }),
    );
    fireEvent.click(button);

    expect(screen.getByRole("status")).toHaveTextContent("Preparing encrypted export…");
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    expect(button).toBeDisabled();

    await act(async () => { resolveRetryManifest({ ...manifest, totalChunks: 0 }); });
  });

  it("handles a zero-chunk manifest without division by zero or stale progress", async () => {
    const createUrl = vi.spyOn(URL, "createObjectURL").mockReturnValue("blob:synthetic-export");
    vi.spyOn(URL, "revokeObjectURL").mockImplementation(() => {});
    const download = vi.spyOn(HTMLAnchorElement.prototype, "click").mockImplementation(() => {});

    const zeroChunkManifest: api.ExportManifest = { ...manifest, totalChunks: 0 };
    vi.mocked(api.startCloudExport).mockResolvedValueOnce(zeroChunkManifest);

    render(<CloudAccountPanel />);
    const button = await screen.findByRole("button", { name: "Download encrypted export" });

    fireEvent.click(button);

    await waitFor(() => {
      expect(screen.getByRole("status")).toHaveTextContent(
        "Encrypted export downloaded. Keep it with your recovery materials.",
      );
    });
    expect(button).toBeEnabled();
    expect(api.fetchCloudExportChunk).not.toHaveBeenCalled();
    expect(createUrl).toHaveBeenCalledOnce();
    const blob = createUrl.mock.calls[0][0] as Blob;
    expect(JSON.parse(await blob.text())).toEqual({ manifest: zeroChunkManifest, chunks: [] });
    expect(download).toHaveBeenCalledOnce();
  });

  it("does not display export progress during unrelated panel actions", async () => {
    let resolvePortal!: (value: api.PersonalLifecycle) => void;
    vi.mocked(api.createPersonalPortal).mockImplementationOnce(
      () => new Promise((resolve) => { resolvePortal = resolve; }),
    );
    vi.mocked(api.fetchEligibility).mockResolvedValueOnce({
      state: "paid", accountInitialized: true, billingAvailable: true,
      paidThroughEpoch: null, recoveryUntilEpoch: null, exportUntilEpoch: null,
      actions: { setup: false, billing: false, export: true, revoke: false },
      payer: "personal", deploymentMode: "cloud",
    });

    render(<CloudAccountPanel />);
    const portalButton = await screen.findByRole("button", { name: "Open billing portal" });
    const exportButton = screen.getByRole("button", { name: "Download encrypted export" });

    fireEvent.click(portalButton);
    expect(portalButton).toBeDisabled();
    expect(exportButton).toBeDisabled();

    expect(screen.queryByText(/Preparing encrypted export/)).not.toBeInTheDocument();
    expect(screen.queryByText(/Downloading encrypted export/)).not.toBeInTheDocument();

    await act(async () => { resolvePortal(lifecycleFixture); });
  });
});
