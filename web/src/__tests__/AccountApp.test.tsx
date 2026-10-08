import "@testing-library/jest-dom/vitest";
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import * as api from "../api";
import { AccountApp } from "../AccountApp";

vi.mock("../api", () => ({ me: vi.fn(), logout: vi.fn() }));
vi.mock("../CloudAccountPanel", () => ({
  CloudAccountPanel: () => <h1>Cloud account controls</h1>,
}));
vi.mock("../login", () => ({ startLogin: vi.fn() }));

afterEach(cleanup);
beforeEach(() => vi.resetAllMocks());

describe("AccountApp logout recovery", () => {
  it("shows a logout failure and clears it while retrying successfully", async () => {
    vi.mocked(api.me).mockResolvedValue({ userId: "synthetic-user" });
    let resolve!: () => void;
    const retry = new Promise<void>((res) => { resolve = res; });
    vi.mocked(api.logout)
      .mockRejectedValueOnce(new Error("synthetic logout failure"))
      .mockReturnValueOnce(retry);

    render(<AccountApp />);
    expect(await screen.findByRole("heading", { name: "Cloud account controls" })).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Log out" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("synthetic logout failure");
    expect(screen.getByRole("heading", { name: "Cloud account controls" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Log out" })).toBeEnabled();
    expect(api.logout).toHaveBeenCalledOnce();

    fireEvent.click(screen.getByRole("button", { name: "Log out" }));
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    expect(screen.getByRole("heading", { name: "Cloud account controls" })).toBeInTheDocument();
    expect(api.logout).toHaveBeenCalledTimes(2);
    await act(async () => { resolve(); });

    expect(screen.getByRole("button", { name: "Log in with GitHub" })).toBeInTheDocument();
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Log out" })).not.toBeInTheDocument();
    expect(screen.queryByRole("heading", { name: "Cloud account controls" })).not.toBeInTheDocument();
    expect(api.me).toHaveBeenCalledOnce();
  });
});
