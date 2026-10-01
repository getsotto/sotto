import "@testing-library/jest-dom/vitest";
import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, expect, it, vi } from "vitest";

vi.mock("../api", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../api")>();
  return { ...actual, organisationDeletionEnabled: false };
});

import { OrganisationDeletionPanel } from "../OrganisationDeletionPanel";

afterEach(cleanup);

it("shows the disabled feature message without loading deletion status", () => {
  render(<OrganisationDeletionPanel orgId="org-a" orgName="A" onActiveChange={vi.fn()} />);
  expect(screen.getByText(/not enabled/i)).toBeInTheDocument();
});
