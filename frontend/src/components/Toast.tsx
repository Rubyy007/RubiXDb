import { createContext, useCallback, useContext, useMemo, useState, type ReactNode } from "react";

interface ToastItem {
  id: number;
  message: string;
  tone: "success" | "error";
}

interface ToastContextValue {
  notify: (message: string, tone?: "success" | "error") => void;
}

const ToastContext = createContext<ToastContextValue | null>(null);

let nextId = 1;

export function ToastProvider({ children }: { children: ReactNode }) {
  const [items, setItems] = useState<ToastItem[]>([]);

  const notify = useCallback((message: string, tone: "success" | "error" = "success") => {
    const id = nextId++;
    setItems((prev) => [...prev, { id, message, tone }]);
    window.setTimeout(() => {
      setItems((prev) => prev.filter((t) => t.id !== id));
    }, 5000);
  }, []);

  const value = useMemo(() => ({ notify }), [notify]);

  return (
    <ToastContext.Provider value={value}>
      {children}
      <div className="toast-region" role="region" aria-live="polite" aria-label="Notifications">
        {items.map((item) => (
          <div key={item.id} className={`toast toast-${item.tone}`} role="status">
            {item.message}
          </div>
        ))}
      </div>
    </ToastContext.Provider>
  );
}

export function useToast(): ToastContextValue {
  const ctx = useContext(ToastContext);
  if (!ctx) throw new Error("useToast must be used within a ToastProvider");
  return ctx;
}
