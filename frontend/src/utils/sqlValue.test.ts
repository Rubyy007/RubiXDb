import { describe, expect, it } from "vitest";
import { formatSqlValue, isNullValue } from "./sqlValue";
import type { SqlValue } from "../api/types";

describe("formatSqlValue", () => {
  it("renders NULL as literal text, distinguishable from empty string", () => {
    expect(formatSqlValue({ type: "null" })).toBe("NULL");
    expect(isNullValue({ type: "null" })).toBe(true);
    expect(isNullValue({ type: "text", value: "" })).toBe(false);
  });

  it("renders boolean/integer/real/double as plain text", () => {
    expect(formatSqlValue({ type: "boolean", value: true })).toBe("true");
    expect(formatSqlValue({ type: "integer", value: 42 })).toBe("42");
    expect(formatSqlValue({ type: "real", value: 1.5 })).toBe("1.5");
    expect(formatSqlValue({ type: "double", value: 2.25 })).toBe("2.25");
  });

  it("preserves bigint precision beyond 2^53 (carried as a string on the wire)", () => {
    const big: SqlValue = { type: "bigint", value: "9223372036854775807" };
    expect(formatSqlValue(big)).toBe("9223372036854775807");
  });

  it("formats decimal from unscaled + scale", () => {
    expect(formatSqlValue({ type: "decimal", unscaled: "12345", scale: 2 })).toBe("123.45");
    expect(formatSqlValue({ type: "decimal", unscaled: "-500", scale: 2 })).toBe("-5.00");
    expect(formatSqlValue({ type: "decimal", unscaled: "5", scale: 3 })).toBe("0.005");
  });

  it("renders text/date/time/timestamp verbatim", () => {
    expect(formatSqlValue({ type: "text", value: "hello" })).toBe("hello");
    expect(formatSqlValue({ type: "date", value: "2026-09-29" })).toBe("2026-09-29");
  });

  it("renders blob as a byte-length summary, never raw bytes", () => {
    // "aGVsbG8=" is base64 for "hello" (5 bytes).
    expect(formatSqlValue({ type: "blob", value_b64: "aGVsbG8=" })).toBe("(5-byte blob)");
  });

  it("adversarial text content (HTML/script) passes through as inert text, never executed", () => {
    const payload = "<script>alert('xss')</script>";
    expect(formatSqlValue({ type: "text", value: payload })).toBe(payload);
  });
});
