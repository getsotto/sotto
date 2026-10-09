import "@testing-library/jest-dom/vitest";
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import * as api from "../api";
import { CloudAccountPanel } from "../CloudAccountPanel";

vi.mock("../api", () => ({
  cancelPersonalBilling: vi.fn(),
  createPersonalCheckout: vi.fn(),
  createPersonalPortal: vi.fn(),
  fetchCloudExportChunk: vi.fn(),
  fetchCloudNotices: vi.fn(),
  fetchEligibility: vi.fn(),
  fetchPersonalQuote: vi.fn(),
  fetchPersonalOperation: vi.fn(),
  requestPersonalRefund: vi.fn(),
  startCloudExport: vi.fn(),
}));

const manifest: api.ExportManifest = {
  version: 1, exportId: "export-first", manifestHash: "synthetic-manifest-hash",
  expiresAt: "2030-01-01T00:00:00Z", totalChunks: 2, complete: true,
  projects: [], environments: [], notSharedEnvironmentIds: [], omittedEnvironmentCount: 0,
};

beforeEach(() => {
  vi.resetAllMocks();
  vi.mocked(api.fetchEligibility).mockResolvedValue({
    state: "export_only", accountInitialized: true, billingAvailable: false,
    paidThroughEpoch: null, recoveryUntilEpoch: null, exportUntilEpoch: null,
    actions: { setup: false, billing: false, export: true, revoke: false },
    payer: null, deploymentMode: "cloud",
  });
  vi.mocked(api.fetchCloudNotices).mockResolvedValue([]);
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
});
