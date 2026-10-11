import { describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";
import { ExplorerPage } from "./ExplorerPage";

const none = { mutate: vi.fn(), isPending: false, isError: false, data: undefined };

vi.mock("../api/queries", () => ({
  useSchemasQuery: () => ({ data: [{ schema_id: 1, database_id: 1, name: "public" }] }),
  useTablesQuery: () => ({ data: [{ table_id: 1, schema_id: 1, name: "orders" }] }),
  useIndexesQuery: () => ({
    data: [{ index_id: 1, table_id: 1, table_name: "orders", name: "orders_status", kind: "secondary", unique: false, state: "ready" }],
  }),
  useGetQuery: () => ({ data: undefined, isFetching: false, isError: false }),
  useExistsQuery: () => ({ data: undefined }),
  useDeleteMutation: () => none,
  useRangeMutation: () => none,
}));
vi.mock("../context/SessionContext", () => ({ useSession: () => ({ session: { role: "admin" } }) }));
vi.mock("../components/Toast", () => ({ useToast: () => ({ notify: vi.fn() }) }));

describe("ExplorerPage (Catalog)", () => {
  it("opens on Objects, listed first, with the other sections after it", () => {
    render(<ExplorerPage />);
    const tabs = screen.getAllByRole("tab").map((t) => t.textContent);
    expect(tabs).toEqual(["Objects", "Point Lookup", "Range Query"]);
    expect(screen.getByRole("tab", { name: "Objects" })).toHaveAttribute("aria-selected", "true");
    expect(screen.getByText("orders_status")).toBeInTheDocument();
  });

  it("shows schemas, tables and indexes without any delete button, even for an admin", () => {
    render(<ExplorerPage />);
    expect(screen.queryByRole("button", { name: /delete/i })).toBeNull();
  });

  it("the other sections are still reachable", () => {
    render(<ExplorerPage />);
    fireEvent.click(screen.getByRole("tab", { name: "Range Query" }));
    expect(screen.getByRole("button", { name: "Run range query" })).toBeInTheDocument();
  });
});
