import { describe, expect, it, beforeEach } from "vitest";
import { takeTokenFromLocation } from "./tokenHandoff";

const KEY = "ab12cd34".repeat(8);

beforeEach(() => {
  window.history.replaceState(null, "", "/");
});

describe("takeTokenFromLocation", () => {
  it("returns a well-formed token and removes the fragment from the URL", () => {
    window.history.replaceState(null, "", `/#token=${KEY}`);
    expect(takeTokenFromLocation()).toBe(KEY);
    expect(window.location.hash).toBe("");
    expect(window.location.href).not.toContain(KEY);
  });

  it("keeps path and query when scrubbing", () => {
    window.history.replaceState(null, "", `/sql?x=1#token=${KEY}`);
    takeTokenFromLocation();
    expect(window.location.pathname + window.location.search).toBe("/sql?x=1");
    expect(window.location.hash).toBe("");
  });

  it("replaces the history entry instead of pushing a new one", () => {
    window.history.replaceState(null, "", `/#token=${KEY}`);
    const before = window.history.length;
    takeTokenFromLocation();
    expect(window.history.length).toBe(before);
  });

  it("scrubs but rejects a malformed token", () => {
    for (const bad of ["short", "has space here!!!!!!!", "x".repeat(300), "a%20b".repeat(10), ""]) {
      window.history.replaceState(null, "", `/#token=${bad}`);
      expect(takeTokenFromLocation()).toBeNull();
      expect(window.location.hash).toBe("");
    }
  });

  it("leaves unrelated fragments alone", () => {
    window.history.replaceState(null, "", "/#section-2");
    expect(takeTokenFromLocation()).toBeNull();
    expect(window.location.hash).toBe("#section-2");
  });

  it("returns null with no fragment", () => {
    expect(takeTokenFromLocation()).toBeNull();
  });
});
