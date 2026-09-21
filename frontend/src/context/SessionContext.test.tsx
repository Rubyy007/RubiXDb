import { describe, expect, it, beforeEach } from "vitest";
import { renderHook, act } from "@testing-library/react";
import { SessionProvider, useSession } from "./SessionContext";
import type { ReactNode } from "react";

const wrapper = ({ children }: { children: ReactNode }) => (
  <SessionProvider>{children}</SessionProvider>
);

const sampleSession = {
  baseUrl: "http://localhost:8080",
  apiKey: "test-key",
  role: "admin" as const,
  principalName: "tester",
};

beforeEach(() => {
  window.sessionStorage.clear();
  window.localStorage.clear();
});

describe("SessionProvider", () => {
  it("starts with no session", () => {
    const { result } = renderHook(() => useSession(), { wrapper });
    expect(result.current.session).toBeNull();
  });

  it("defaults to sessionStorage, not localStorage (PHASE_FRONTEND_ARCHITECTURE.md §5)", () => {
    const { result } = renderHook(() => useSession(), { wrapper });
    act(() => result.current.setSession(sampleSession, false));
    expect(result.current.session).toEqual(sampleSession);
    expect(window.sessionStorage.getItem("rubixdb-console-session")).not.toBeNull();
    expect(window.localStorage.getItem("rubixdb-console-session")).toBeNull();
  });

  it("uses localStorage only when the caller explicitly opts in", () => {
    const { result } = renderHook(() => useSession(), { wrapper });
    act(() => result.current.setSession(sampleSession, true));
    expect(window.localStorage.getItem("rubixdb-console-session")).not.toBeNull();
    expect(window.sessionStorage.getItem("rubixdb-console-session")).toBeNull();
  });

  it("clearSession removes the session from both storages and state", () => {
    const { result } = renderHook(() => useSession(), { wrapper });
    act(() => result.current.setSession(sampleSession, true));
    act(() => result.current.clearSession());
    expect(result.current.session).toBeNull();
    expect(window.localStorage.getItem("rubixdb-console-session")).toBeNull();
    expect(window.sessionStorage.getItem("rubixdb-console-session")).toBeNull();
  });

  it("restores a previously stored session on mount", () => {
    window.sessionStorage.setItem("rubixdb-console-session", JSON.stringify(sampleSession));
    const { result } = renderHook(() => useSession(), { wrapper });
    expect(result.current.session).toEqual(sampleSession);
  });
});
