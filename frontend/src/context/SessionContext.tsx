import { createContext, useCallback, useContext, useMemo, useState, type ReactNode } from "react";
import type { Role } from "../api/types";

export interface Session {
  baseUrl: string;
  apiKey: string;
  role: Role;
  principalName: string;
}

interface StoredSession {
  baseUrl: string;
  apiKey: string;
  role: Role;
  principalName: string;
}

const STORAGE_KEY = "rubixdb-console-session";

function readStored(): StoredSession | null {
  for (const storage of [window.sessionStorage, window.localStorage]) {
    try {
      const raw = storage.getItem(STORAGE_KEY);
      if (raw) return JSON.parse(raw) as StoredSession;
    } catch {
      // Corrupted or inaccessible storage -- treat as no session
      // rather than throwing during app startup.
    }
  }
  return null;
}

function clearStored() {
  try {
    window.sessionStorage.removeItem(STORAGE_KEY);
  } catch {
    /* ignore */
  }
  try {
    window.localStorage.removeItem(STORAGE_KEY);
  } catch {
    /* ignore */
  }
}

interface SessionContextValue {
  session: Session | null;
  /** Persists the session; `remember` chooses sessionStorage (default,
   * cleared when the tab closes) vs. localStorage (survives browser
   * restart) -- PHASE_FRONTEND_ARCHITECTURE.md §5's own explicit,
   * user-chosen tradeoff, never a silent default toward the more
   * persistent option. */
  setSession: (session: Session, remember: boolean) => void;
  /** Cleared on logout and automatically on any 401 response
   * (PHASE_FRONTEND_ARCHITECTURE.md §5). */
  clearSession: () => void;
}

const SessionContext = createContext<SessionContextValue | null>(null);

export function SessionProvider({ children }: { children: ReactNode }) {
  const [session, setSessionState] = useState<Session | null>(() => readStored());

  const setSession = useCallback((next: Session, remember: boolean) => {
    setSessionState(next);
    clearStored();
    try {
      const storage = remember ? window.localStorage : window.sessionStorage;
      storage.setItem(STORAGE_KEY, JSON.stringify(next));
    } catch {
      // Storage unavailable (private browsing, quota) -- the session
      // still works in-memory for this page load, just won't survive
      // a reload. Not fatal.
    }
  }, []);

  const clearSession = useCallback(() => {
    setSessionState(null);
    clearStored();
  }, []);

  const value = useMemo(
    () => ({ session, setSession, clearSession }),
    [session, setSession, clearSession],
  );

  return <SessionContext.Provider value={value}>{children}</SessionContext.Provider>;
}

export function useSession(): SessionContextValue {
  const ctx = useContext(SessionContext);
  if (!ctx) throw new Error("useSession must be used within a SessionProvider");
  return ctx;
}
