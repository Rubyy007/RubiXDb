export function Spinner({ label = "Loading" }: { label?: string }) {
  return (
    <span role="status" className="row">
      <span className="spinner" aria-hidden="true" />
      <span className="visually-hidden">{label}</span>
    </span>
  );
}
