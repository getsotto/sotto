import { act, fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

import { SharePendingGuard } from "../SharePendingGuard";

function shareForm(onSubmit: () => void) {
  return (
    <>
      <SharePendingGuard />
      <form onSubmit={(event) => {
        event.preventDefault();
        onSubmit();
      }}>
        <select aria-label="member" defaultValue="member-a">
          <option value="member-a">member-a</option>
        </select>
        <button type="submit">Share</button>
      </form>
      <p className="notice" />
    </>
  );
}

describe("SharePendingGuard", () => {
  it("prevents overlapping submissions and restores controls after completion", async () => {
    const resolve = vi.fn();
    render(shareForm(resolve));

    const form = screen.getByRole("button", { name: "Share" }).closest("form");
    expect(form).not.toBeNull();
    if (form === null) {
      return;
    }

    await act(async () => {
      fireEvent.submit(form);
    });
    expect(resolve).toHaveBeenCalledTimes(1);
    expect(screen.getByRole("button", { name: "Sharing…" })).toBeDisabled();
    expect(screen.getByRole("combobox", { name: "member" })).toBeDisabled();

    await act(async () => {
      fireEvent.submit(form);
    });
    expect(resolve).toHaveBeenCalledTimes(1);

    const notice = document.querySelector<HTMLElement>(".notice");
    if (notice !== null) {
      notice.textContent = "shared this environment with member-a";
    }

    await act(async () => {
      await Promise.resolve();
    });
    expect(screen.getByRole("button", { name: "Share" })).toBeEnabled();
    expect(screen.getByRole("combobox", { name: "member" })).toBeEnabled();
  });
});
