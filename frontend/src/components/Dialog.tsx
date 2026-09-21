import { useEffect, useRef, type ReactNode } from "react";
import { Button } from "./Button";

interface DialogProps {
  open: boolean;
  title: string;
  children: ReactNode;
  confirmLabel?: string;
  onConfirm?: () => void;
  confirmVariant?: "primary" | "danger";
  onClose: () => void;
  busy?: boolean;
}

/** Built on the native <dialog> element specifically for its built-in
 * focus trap and Escape-to-close behavior (PHASE_FRONTEND_
 * ARCHITECTURE.md §4/§6) -- more robust than hand-rolled focus-trap
 * logic, and focus automatically returns to the triggering element
 * when the dialog closes via the browser's own handling of
 * `HTMLDialogElement`. */
export function Dialog({
  open,
  title,
  children,
  confirmLabel,
  onConfirm,
  confirmVariant = "primary",
  onClose,
  busy,
}: DialogProps) {
  const ref = useRef<HTMLDialogElement>(null);

  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    if (open && !el.open) {
      el.showModal();
    } else if (!open && el.open) {
      el.close();
    }
  }, [open]);

  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    const handleCancel = (e: Event) => {
      e.preventDefault();
      onClose();
    };
    el.addEventListener("cancel", handleCancel);
    return () => el.removeEventListener("cancel", handleCancel);
  }, [onClose]);

  return (
    <dialog
      ref={ref}
      className="dialog"
      aria-labelledby="dialog-title"
      onClose={onClose}
      onClick={(e) => {
        if (e.target === ref.current) onClose();
      }}
    >
      <h2 id="dialog-title" className="dialog-title">
        {title}
      </h2>
      <div className="dialog-body">{children}</div>
      <div className="dialog-actions">
        <Button variant="ghost" onClick={onClose} disabled={busy}>
          Cancel
        </Button>
        {confirmLabel && onConfirm && (
          <Button variant={confirmVariant} onClick={onConfirm} disabled={busy}>
            {busy ? "Working…" : confirmLabel}
          </Button>
        )}
      </div>
    </dialog>
  );
}
