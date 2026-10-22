import "@testing-library/jest-dom/vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import * as api from "../api";
import { CloudContactSettings } from "../CloudContactSettings";

vi.mock("../api", () => ({
  confirmCloudContactVerification: vi.fn(),
  fetchCloudContactStatus: vi.fn(),
  revokeCloudContact: vi.fn(),
  startCloudContactVerification: vi.fn(),
  UnsupportedCloudContactError: class UnsupportedCloudContactError extends Error {},
}));

const emptyStatus: api.CloudContactStatus = {
  email: null,
  latestDelivery: null,
  pendingExpiresAtEpoch: null,
  enrolmentAvailable: true,
};

const acknowledgement = "If this address can be used, a verification message will arrive shortly.";

function deferred() {
  let resolve!: () => void;
  let reject!: (cause: unknown) => void;
  const promise = new Promise<void>((onResolve, onReject) => {
    resolve = onResolve;
    reject = onReject;
  });
  return { promise, resolve, reject };
}

async function enterAddress(address: string) {
  fireEvent.change(await screen.findByLabelText("Email address"), { target: { value: address } });
}

function sendLink() {
  fireEvent.click(screen.getByRole("button", { name: "Send verification link" }));
}

function verificationCalls() {
  return vi.mocked(api.startCloudContactVerification).mock.calls;
}

beforeEach(() => {
  vi.resetAllMocks();
  vi.mocked(api.fetchCloudContactStatus).mockResolvedValue(emptyStatus);
  window.history.replaceState(null, "", "/cloud");
});

afterEach(() => {
  cleanup();
  window.history.replaceState(null, "", "/cloud");
});

describe("CloudContactSettings", () => {
  it("asks for a verification link without disclosing whether the address is usable", async () => {
    render(<CloudContactSettings />);
    fireEvent.change(await screen.findByLabelText("Email address"), {
      target: { value: "person@example.test" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Send verification link" }));

    expect(await screen.findByRole("status")).toHaveTextContent(
      "If this address can be used, a verification message will arrive shortly.",
    );
    expect(api.startCloudContactVerification).toHaveBeenCalledWith(
      "person@example.test",
      expect.any(String),
    );
  });

  it("disables the controls and ignores repeated activation while a verification request is pending", async () => {
    const request = deferred();
    vi.mocked(api.startCloudContactVerification).mockReturnValue(request.promise);
    render(<CloudContactSettings />);
    await enterAddress("person@example.test");
    sendLink();

    const input = screen.getByLabelText("Email address");
    const button = screen.getByRole("button", { name: "Send verification link" });
    expect(input).toBeDisabled();
    expect(button).toBeDisabled();
    fireEvent.click(button);
    expect(api.startCloudContactVerification).toHaveBeenCalledOnce();

    request.resolve();
    expect(await screen.findByText(acknowledgement)).toBeInTheDocument();
    expect(api.startCloudContactVerification).toHaveBeenCalledOnce();
  });

  it("shows a failed verification request without retrying it, keeping the address and re-enabling the controls", async () => {
    const request = deferred();
    vi.mocked(api.startCloudContactVerification).mockReturnValue(request.promise);
    render(<CloudContactSettings />);
    await enterAddress("person@example.test");
    sendLink();

    request.reject(new Error("verification service unavailable"));

    expect(await screen.findByRole("alert")).toHaveTextContent("verification service unavailable");
    expect(screen.getByLabelText("Email address")).toHaveValue("person@example.test");
    expect(screen.getByLabelText("Email address")).toBeEnabled();
    expect(screen.getByRole("button", { name: "Send verification link" })).toBeEnabled();
    expect(screen.queryByText(acknowledgement)).not.toBeInTheDocument();
    expect(api.startCloudContactVerification).toHaveBeenCalledOnce();
  });

  it("retries an unchanged address with the same request key and clears the form once it succeeds", async () => {
    vi.mocked(api.startCloudContactVerification)
      .mockRejectedValueOnce(new Error("verification service unavailable"))
      .mockResolvedValueOnce(undefined);
    render(<CloudContactSettings />);
    await enterAddress("person@example.test");
    sendLink();
    expect(await screen.findByRole("alert")).toHaveTextContent("verification service unavailable");

    sendLink();

    expect(await screen.findByText(acknowledgement)).toBeInTheDocument();
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    expect(screen.getByLabelText("Email address")).toHaveValue("");
    const [failed, retried] = verificationCalls();
    expect(retried).toEqual(failed);
    expect(failed).toEqual(["person@example.test", expect.any(String)]);
  });

  it("sends a changed address with a new request key after a failed attempt", async () => {
    vi.mocked(api.startCloudContactVerification)
      .mockRejectedValueOnce(new Error("verification service unavailable"))
      .mockResolvedValueOnce(undefined);
    render(<CloudContactSettings />);
    await enterAddress("person@example.test");
    sendLink();
    expect(await screen.findByRole("alert")).toBeInTheDocument();

    await enterAddress("other@example.test");
    sendLink();

    expect(await screen.findByText(acknowledgement)).toBeInTheDocument();
    const [failed, retried] = verificationCalls();
    expect(failed[0]).toBe("person@example.test");
    expect(retried[0]).toBe("other@example.test");
    expect(retried[1]).not.toBe(failed[1]);
  });

  it("uses a fresh request key for a later submission after a successful request", async () => {
    vi.mocked(api.startCloudContactVerification).mockResolvedValue(undefined);
    render(<CloudContactSettings />);
    await enterAddress("person@example.test");
    sendLink();
    expect(await screen.findByText(acknowledgement)).toBeInTheDocument();
    await waitFor(() => expect(screen.getByLabelText("Email address")).toBeEnabled());

    await enterAddress("person@example.test");
    sendLink();

    await waitFor(() => expect(api.startCloudContactVerification).toHaveBeenCalledTimes(2));
    const [first, second] = verificationCalls();
    expect(second[0]).toBe("person@example.test");
    expect(second[1]).not.toBe(first[1]);
  });

  it("removes the callback token from browser history before posting it", async () => {
    const token = "a".repeat(64);
    window.history.replaceState(null, "", `/cloud#contact_token=${token}`);
    vi.mocked(api.confirmCloudContactVerification).mockResolvedValue(true);

    render(<CloudContactSettings />);

    await waitFor(() => expect(api.confirmCloudContactVerification).toHaveBeenCalledWith(token));
    expect(window.location.hash).toBe("");
    expect(window.location.href).not.toContain(token);
    expect(await screen.findByRole("status")).toHaveTextContent("Email address verified.");
  });

  it("loads contact status after confirmation without a stale request racing it", async () => {
    let resolveConfirmation!: (verified: boolean) => void;
    vi.mocked(api.confirmCloudContactVerification).mockReturnValue(new Promise((resolve) => {
      resolveConfirmation = resolve;
    }));
    const verifiedStatus: api.CloudContactStatus = {
      email: {
        contactId: "contact-verified",
        maskedDestination: "p***@example.test",
        verifiedAtEpoch: 1_800_000_000,
      },
      latestDelivery: null,
      pendingExpiresAtEpoch: null,
      enrolmentAvailable: true,
    };
    vi.mocked(api.fetchCloudContactStatus).mockResolvedValue(verifiedStatus);
    window.history.replaceState(null, "", `/cloud#contact_token=${"b".repeat(64)}`);

    render(<CloudContactSettings />);

    expect(api.fetchCloudContactStatus).not.toHaveBeenCalled();
    resolveConfirmation(true);

    expect(await screen.findByText("p***@example.test")).toBeInTheDocument();
    expect(api.fetchCloudContactStatus).toHaveBeenCalledOnce();
  });

  it("does not leave a failed initial status request looking like it is still loading", async () => {
    vi.mocked(api.fetchCloudContactStatus).mockRejectedValue(new Error("network unavailable"));

    render(<CloudContactSettings />);

    expect(await screen.findByText("Contact settings could not be loaded.")).toBeInTheDocument();
    expect(screen.queryByText("Checking contact settings…")).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Retry" })).toBeInTheDocument();
  });

  it("can revoke an existing verified contact", async () => {
    vi.mocked(api.fetchCloudContactStatus).mockResolvedValue({
      email: {
        contactId: "contact-1",
        maskedDestination: "p***@example.test",
        verifiedAtEpoch: 1_800_000_000,
      },
      latestDelivery: null,
      pendingExpiresAtEpoch: null,
      enrolmentAvailable: true,
    });
    render(<CloudContactSettings />);
    fireEvent.click(await screen.findByRole("button", { name: "Remove verified email" }));
    await waitFor(() => expect(api.revokeCloudContact).toHaveBeenCalledOnce());
  });

  it("explains that older servers do not support contact settings", async () => {
    vi.mocked(api.fetchCloudContactStatus).mockRejectedValue(new api.UnsupportedCloudContactError());
    render(<CloudContactSettings />);

    expect(await screen.findByText("Email contact settings require a newer Sotto server version.")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Send verification link" })).not.toBeInTheDocument();
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
  });

  it("shows only a safe delivery summary without exposing provider identifiers", async () => {
    vi.mocked(api.fetchCloudContactStatus).mockResolvedValue({
      email: null,
      latestDelivery: { state: "suppressed", updatedAtEpoch: 1_800_000_000 },
      pendingExpiresAtEpoch: null,
      enrolmentAvailable: true,
    });
    render(<CloudContactSettings />);

    expect(await screen.findByText(
      "Email delivery is paused because the address could not accept a previous message.",
    )).toBeInTheDocument();
    expect(screen.queryByText(/provider-message|@example\.test/)).not.toBeInTheDocument();
  });

  async function emailForm(address: string) {
    const input = await screen.findByLabelText("Email address");
    fireEvent.change(input, { target: { value: address } });
    const form = input.closest("form");
    expect(form).not.toBeNull();
    return form as HTMLFormElement;
  }

  it("sends the verification link when the email form is submitted, as Enter does", async () => {
    render(<CloudContactSettings />);
    fireEvent.submit(await emailForm("person@example.test"));

    expect(await screen.findByRole("status")).toHaveTextContent(
      "If this address can be used, a verification message will arrive shortly.",
    );
    expect(api.startCloudContactVerification).toHaveBeenCalledOnce();
    expect(api.startCloudContactVerification).toHaveBeenCalledWith("person@example.test", expect.any(String));
  });

  it("sends one request while a verification request is in flight", async () => {
    let finish!: () => void;
    vi.mocked(api.startCloudContactVerification).mockReturnValue(new Promise<void>((resolve) => {
      finish = resolve;
    }));
    render(<CloudContactSettings />);
    const form = await emailForm("person@example.test");

    fireEvent.submit(form);
    await waitFor(() => expect(screen.getByLabelText("Email address")).toBeDisabled());
    fireEvent.submit(form);
    fireEvent.click(screen.getByRole("button", { name: "Send verification link" }));

    expect(api.startCloudContactVerification).toHaveBeenCalledOnce();
    finish();
    expect(await screen.findByRole("status")).toHaveTextContent("a verification message will arrive shortly");
  });

  it("does not submit an empty address", async () => {
    render(<CloudContactSettings />);
    fireEvent.submit(await emailForm("   "));

    expect(screen.getByRole("button", { name: "Send verification link" })).toBeDisabled();
    expect(api.startCloudContactVerification).not.toHaveBeenCalled();
  });

  it("resends through the same form while a verification request is pending, and leaves removal alone", async () => {
    vi.mocked(api.fetchCloudContactStatus).mockResolvedValue({
      email: {
        contactId: "contact-verified",
        maskedDestination: "p***@example.test",
        verifiedAtEpoch: 1_800_000_000,
      },
      latestDelivery: null,
      pendingExpiresAtEpoch: 1_900_000_000,
      enrolmentAvailable: true,
    });
    render(<CloudContactSettings />);
    const form = await emailForm("next@example.test");

    expect(screen.getByRole("button", { name: "Resend verification link" })).toHaveAttribute("type", "submit");
    expect(screen.getByRole("button", { name: "Remove verified email" })).not.toHaveAttribute("type", "submit");
    fireEvent.submit(form);

    await waitFor(() => expect(api.startCloudContactVerification).toHaveBeenCalledWith(
      "next@example.test",
      expect.any(String),
    ));
    expect(api.revokeCloudContact).not.toHaveBeenCalled();
  });
});
