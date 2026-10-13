import { beforeEach, describe, expect, it } from "vitest";
import {
  clearSessionActivity,
  queryTitle,
  recordQuery,
  setPendingSql,
  takePendingSql,
} from "./sessionActivity";
import { deriveProcessMemory, deriveRam, ramLevel } from "./ram";

describe("deriveProcessMemory", () => {
  it("reads admin resources.rss_bytes", () => {
    expect(deriveProcessMemory({ resources: { rss_bytes: 123456 } })).toBe(123456);
    expect(deriveProcessMemory({ resources: { rss_bytes: 0 } })).toBe(0);
  });
  it("is null for anything that is not a finite non-negative number", () => {
    expect(deriveProcessMemory(undefined)).toBeNull();
    expect(deriveProcessMemory(null)).toBeNull();
    expect(deriveProcessMemory({})).toBeNull();
    expect(deriveProcessMemory({ resources: null })).toBeNull();
    expect(deriveProcessMemory({ resources: { rss_bytes: "5" } })).toBeNull();
    expect(deriveProcessMemory({ resources: { rss_bytes: -1 } })).toBeNull();
    expect(deriveProcessMemory({ resources: { rss_bytes: NaN } })).toBeNull();
    expect(deriveProcessMemory({ resources: { rss_bytes: Infinity } })).toBeNull();
  });
});

describe("queryTitle", () => {
  it("uses the first non-empty line, whitespace-collapsed", () => {
    expect(queryTitle("\n\n  SELECT   *\n FROM t")).toBe("SELECT *");
  });
  it("clips long statements", () => {
    const t = queryTitle("x".repeat(500), 80);
    expect(t.length).toBe(80);
    expect(t.endsWith("…")).toBe(true);
  });
  it("returns hostile text unchanged (rendering is React's job, it is never HTML)", () => {
    expect(queryTitle("<img src=x onerror=alert(1)>")).toBe("<img src=x onerror=alert(1)>");
  });
});

describe("pending template hand-off", () => {
  beforeEach(() => clearSessionActivity());
  it("is consumed exactly once", () => {
    setPendingSql("SELECT 1");
    expect(takePendingSql()).toBe("SELECT 1");
    expect(takePendingSql()).toBe("");
  });
  it("is dropped by clearSessionActivity and ignores oversized input", () => {
    setPendingSql("SELECT 1");
    clearSessionActivity();
    expect(takePendingSql()).toBe("");
    setPendingSql("x".repeat(5000));
    expect(takePendingSql()).toBe("");
  });
  it("never writes to web storage", () => {
    recordQuery("SELECT secret", true);
    setPendingSql("SELECT secret");
    expect(JSON.stringify([window.sessionStorage, window.localStorage])).not.toContain("secret");
  });
});

describe("deriveRam", () => {
  it("is null (not available) when the status carries no memory fields", () => {
    expect(deriveRam({ storage_state: "Healthy", uptime_secs: 5 })).toBeNull();
    expect(deriveRam(undefined)).toBeNull();
    expect(deriveRam(null)).toBeNull();
  });
  it("is null for partial, non-numeric, non-finite or nonsensical fields", () => {
    expect(deriveRam({ memory_used_bytes: 1 })).toBeNull();
    expect(deriveRam({ memory_used_bytes: "1", memory_total_bytes: "2" })).toBeNull();
    expect(deriveRam({ memory_used_bytes: 1, memory_total_bytes: 0 })).toBeNull();
    expect(deriveRam({ memory_used_bytes: -1, memory_total_bytes: 10 })).toBeNull();
    expect(deriveRam({ memory_used_bytes: NaN, memory_total_bytes: 10 })).toBeNull();
  });
  it("derives the percentage from the reported figures only", () => {
    expect(deriveRam({ memory_used_bytes: 2, memory_total_bytes: 8 })?.percent).toBe(25);
    expect(deriveRam({ memory_used_bytes: 20, memory_total_bytes: 8 })?.percent).toBe(100);
  });
  it("maps percentages to pill levels", () => {
    expect(ramLevel(10)).toBe("healthy");
    expect(ramLevel(75)).toBe("warning");
    expect(ramLevel(90)).toBe("critical");
  });
});
