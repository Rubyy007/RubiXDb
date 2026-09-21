import { forwardRef, type ButtonHTMLAttributes, type ReactNode } from "react";

type Variant = "primary" | "secondary" | "danger" | "ghost";

interface BaseProps extends ButtonHTMLAttributes<HTMLButtonElement> {
  variant?: Variant;
  children: ReactNode;
}

// Icon-only buttons must carry an aria-label -- enforced at the type
// level (PHASE_FRONTEND_ARCHITECTURE.md §4's "accessibility baked into
// the component layer" rule), by making `aria-label` required whenever
// `iconOnly` is set.
type Props =
  | (BaseProps & { iconOnly?: false })
  | (BaseProps & { iconOnly: true; "aria-label": string });

export const Button = forwardRef<HTMLButtonElement, Props>(function Button(
  { variant = "secondary", className, children, iconOnly: _iconOnly, ...rest },
  ref,
) {
  return (
    <button ref={ref} className={`btn btn-${variant} ${className ?? ""}`.trim()} {...rest}>
      {children}
    </button>
  );
});
