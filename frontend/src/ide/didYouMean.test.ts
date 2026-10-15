import { describe, expect, it } from "vitest";
import { missingKind, suggestNames } from "./didYouMean";

describe("suggestNames", () => {
  it("finds a one-letter slip, case-insensitively", () => {
    expect(suggestNames("usrs", ["users", "orders"])).toEqual(["users"]);
    expect(suggestNames("USERS", ["users"])).toEqual([]); // same name apart from case: not a suggestion
    expect(suggestNames("Analtyics", ["analytics", "app_data"])).toEqual(["analytics"]);
  });

  it("offers a name the word is a prefix of, and ignores far-off names", () => {
    expect(suggestNames("user", ["users", "zebra"])).toEqual(["users"]);
    expect(suggestNames("qqqq", ["users", "orders"])).toEqual([]);
  });

  it("never suggests the missing name itself or duplicates, and caps the list", () => {
    expect(suggestNames("cart", ["cart", "carts", "carts", "card", "care", "cars"], 2)).toHaveLength(2);
    expect(suggestNames("cart", ["cart", "carts", "carts"])).toEqual(["carts"]);
    expect(suggestNames("cart", ["cart"])).toEqual([]);
  });

  it("returns nothing for empty or one-letter input", () => {
    expect(suggestNames("", ["a"])).toEqual([]);
    expect(suggestNames("a", ["ab"])).toEqual([]);
  });

  it("strips surrounding double quotes", () => {
    expect(suggestNames('"usrs"', ["users"])).toEqual(["users"]);
  });
});

describe("missingKind", () => {
  it("reads the kind out of the server's message", () => {
    expect(missingKind("the referenced table was not found")).toBe("table");
    expect(missingKind("the referenced schema was not found")).toBe("schema");
    expect(missingKind("not found")).toBeNull();
  });
});
