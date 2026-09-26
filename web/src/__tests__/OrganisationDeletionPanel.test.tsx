import "@testing-library/jest-dom/vitest";
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import * as api from "../api";
import { OrganisationDeletionPanel } from "../OrganisationDeletionPanel";

vi.mock("../api", () => ({
  organisationDeletionEnabled: true,
  fetchOrganisationDeletionStatus: vi.fn(),
  requestOrganisationDeletion: vi.fn(),
  cancelOrganisationDeletion: vi.fn(),
}));

afterEach(cleanup);

const status = {
  state: "requested" as const,
  requestedAt: "2026-09-01T00:00:00Z",
  recoverableUntil: "2026-09-08T00:00:00Z",
  managedBackupExpiryBy: null,
  nextRetryAt: null,
  error: null,
};

describe("OrganisationDeletionPanel", () => {  beforeEach(() => vi.resetAllMocks());

  it("retries an initial status failure and keeps writes frozen", async () => {
    const onActiveChange = vi.fn();
    vi.mocked(api.fetchOrganisationDeletionStatus)
      .mockRejectedValueOnce(new Error("status unavailable"))
      .mockResolvedValueOnce(null);
    render(<OrganisationDeletionPanel orgId="org-a" orgName="A" onActiveChange={onActiveChange} />);

    expect(await screen.findByRole("alert")).toHaveTextContent("status unavailable");
    expect(onActiveChange).toHaveBeenCalledWith(true);
    fireEvent.click(screen.getByRole("button", { name: "Retry status" }));
    expect(await screen.findByRole("button", { name: "Request deletion" })).toBeInTheDocument();
    expect(api.fetchOrganisationDeletionStatus).toHaveBeenCalledTimes(2);
  });

  it("requires exact id and acknowledgement before requesting deletion", async () => {
    vi.mocked(api.fetchOrganisationDeletionStatus).mockResolvedValue(null);
    vi.mocked(api.requestOrganisationDeletion).mockResolvedValue(status);
    render(<OrganisationDeletionPanel orgId="org-a" orgName="A" onActiveChange={vi.fn()} />);
    fireEvent.click(await screen.findByRole("button", { name: "Request deletion" }));
    const confirm = screen.getByRole("button", { name: "Confirm deletion" });
    expect(confirm).toBeDisabled();
    fireEvent.change(screen.getByRole("textbox"), { target: { value: "org-a" } });
    expect(confirm).toBeDisabled();
    fireEvent.click(screen.getByRole("checkbox"));
    expect(confirm).toBeEnabled();
    await act(async () => fireEvent.click(confirm));
    expect(api.requestOrganisationDeletion).toHaveBeenCalledWith("org-a");
  });
});