import { useId, type InputHTMLAttributes, type ReactNode, type SelectHTMLAttributes, type TextareaHTMLAttributes } from "react";

interface FieldWrapProps {
  label: string;
  hint?: string;
  error?: string;
  children: (id: string, describedBy: string | undefined) => ReactNode;
}

function FieldWrap({ label, hint, error, children }: FieldWrapProps) {
  const id = useId();
  const hintId = hint ? `${id}-hint` : undefined;
  const errorId = error ? `${id}-error` : undefined;
  const describedBy = [hintId, errorId].filter(Boolean).join(" ") || undefined;
  return (
    <div className="field">
      <label className="field-label" htmlFor={id}>
        {label}
      </label>
      {children(id, describedBy)}
      {hint && (
        <span id={hintId} className="field-hint">
          {hint}
        </span>
      )}
      {error && (
        <span id={errorId} className="field-error" role="alert">
          {error}
        </span>
      )}
    </div>
  );
}

type InputProps = InputHTMLAttributes<HTMLInputElement> & {
  label: string;
  hint?: string;
  error?: string;
  mono?: boolean;
};

export function Input({ label, hint, error, mono, className, ...rest }: InputProps) {
  return (
    <FieldWrap label={label} hint={hint} error={error}>
      {(id, describedBy) => (
        <input
          id={id}
          className={`input ${mono ? "input-mono" : ""} ${className ?? ""}`.trim()}
          aria-describedby={describedBy}
          aria-invalid={error ? true : undefined}
          {...rest}
        />
      )}
    </FieldWrap>
  );
}

type TextAreaProps = TextareaHTMLAttributes<HTMLTextAreaElement> & {
  label: string;
  hint?: string;
  error?: string;
  mono?: boolean;
};

export function TextArea({ label, hint, error, mono, className, ...rest }: TextAreaProps) {
  return (
    <FieldWrap label={label} hint={hint} error={error}>
      {(id, describedBy) => (
        <textarea
          id={id}
          className={`textarea ${mono ? "textarea-mono" : ""} ${className ?? ""}`.trim()}
          aria-describedby={describedBy}
          aria-invalid={error ? true : undefined}
          {...rest}
        />
      )}
    </FieldWrap>
  );
}

type SelectProps = SelectHTMLAttributes<HTMLSelectElement> & {
  label: string;
  hint?: string;
  children: ReactNode;
};

export function Select({ label, hint, className, children, ...rest }: SelectProps) {
  return (
    <FieldWrap label={label} hint={hint}>
      {(id, describedBy) => (
        <select
          id={id}
          className={`select ${className ?? ""}`.trim()}
          aria-describedby={describedBy}
          {...rest}
        >
          {children}
        </select>
      )}
    </FieldWrap>
  );
}
