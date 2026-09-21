import { describe, expect, it } from "vitest";
import { render, screen } from "@testing-library/react";
import { Badge, storageStateTone } from "./Badge";

describe("storageStateTone", () => {
  it("maps every StorageState variant to the correct semantic tone", () => {
    expect(storageStateTone("Healthy")).toBe("healthy");
    expect(storageStateTone("StoragePressure")).toBe("pressure");
    expect(storageStateTone("StorageFull")).toBe("danger");
    expect(storageStateTone("SomethingUnexpected")).toBe("neutral");
  });
});

describe("Badge", () => {
  it("always renders a visible text label, never relying on color alone", () => {
    render(<Badge tone="danger">StorageFull</Badge>);
    expect(screen.getByText("StorageFull")).toBeInTheDocument();
  });
});
