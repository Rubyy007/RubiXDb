import { afterEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { RecentItems, QuickActions, Templates } from "./HomeSections";
import { clearSessionActivity, recordQuery } from "../utils/sessionActivity";

let role: "admin" | "reader" = "admin";

vi.mock("../context/SessionContext", () => ({
  useSession: () => ({ session: { role, principalName: "p" } }),
}));
vi.mock("../api/queries", () => ({
  useSnapshotsQuery: () => ({ data: [], isLoading: false }),
  useBackupsQuery: () => ({ data: { backups: [] }, isError: false }),
  useAdminStatusQuery: () => ({ data: { backups: { configured: true } } }),
  useCreateSnapshotMutation: () => ({ isPending: false, mutate: vi.fn() }),
}));

afterEach(() => {
  cleanup();
  clearSessionActivity();
  role = "admin";
});

describe("Recent items", () => {
  it("renders a hostile query title as inert text, never as DOM", () => {
    const payload = "<img src=x onerror=alert(1)>";
    recordQuery(payload, false);
    const { container } = render(
      <MemoryRouter>
        <RecentItems />
      </MemoryRouter>,
    );
    fireEvent.click(screen.getByRole("tab", { name: "Queries" }));
    expect(screen.getByRole("cell", { name: payload })).toBeInTheDocument();
    expect(container.querySelector("img")).toBeNull();
    expect(container.querySelector("[onerror]")).toBeNull();
    expect(screen.getByText("Query (failed)")).toBeInTheDocument();
  });

  it("shows honest empty states per tab and never invents rows", () => {
    render(
      <MemoryRouter>
        <RecentItems />
      </MemoryRouter>,
    );
    expect(screen.getByText("Nothing here yet")).toBeInTheDocument();
    fireEvent.click(screen.getByRole("tab", { name: "Queries" }));
    expect(screen.getByText("No queries yet")).toBeInTheDocument();
    fireEvent.click(screen.getByRole("tab", { name: "Snapshots" }));
    expect(screen.getByText("No snapshots held")).toBeInTheDocument();
    fireEvent.click(screen.getByRole("tab", { name: "Backups" }));
    expect(screen.getByText("No backups")).toBeInTheDocument();
    expect(screen.queryAllByRole("row")).toHaveLength(0);
  });

  it("a reader is told backups are admin-only instead of seeing an error", () => {
    role = "reader";
    render(
      <MemoryRouter>
        <RecentItems />
      </MemoryRouter>,
    );
    fireEvent.click(screen.getByRole("tab", { name: "Backups" }));
    expect(screen.getByText("Backups are listed for administrators")).toBeInTheDocument();
  });

  it("updates when a query is recorded", () => {
    render(
      <MemoryRouter>
        <RecentItems />
      </MemoryRouter>,
    );
    act(() => recordQuery("SELECT 1", true));
    expect(within(screen.getByRole("table")).getByText("SELECT 1")).toBeInTheDocument();
  });
});

describe("Quick actions", () => {
  it("offers only real actions, and fewer to a reader", () => {
    const { container, rerender } = render(
      <MemoryRouter>
        <QuickActions />
      </MemoryRouter>,
    );
    expect(container.querySelectorAll(".qa-card")).toHaveLength(3);
    role = "reader";
    rerender(
      <MemoryRouter>
        <QuickActions />
      </MemoryRouter>,
    );
    expect(container.querySelectorAll(".qa-card")).toHaveLength(1);
    expect(container.textContent).not.toMatch(/import|notebook|s3|invite|sentiment/i);
  });
});

describe("Templates", () => {
  it("renders only static, titled templates", () => {
    render(
      <MemoryRouter>
        <Templates />
      </MemoryRouter>,
    );
    expect(screen.getAllByRole("button", { name: /^Load template:/ }).length).toBeGreaterThan(0);
  });
});
