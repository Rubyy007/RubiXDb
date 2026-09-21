type Tone = "healthy" | "pressure" | "danger" | "neutral";

export function Badge({ tone, children }: { tone: Tone; children: string }) {
  // Color is never the only signal -- the text label is always
  // rendered alongside the tone's color (PHASE_FRONTEND_ARCHITECTURE.md
  // §4).
  return <span className={`badge badge-${tone}`}>{children}</span>;
}

export function storageStateTone(state: string): Tone {
  switch (state) {
    case "Healthy":
      return "healthy";
    case "StoragePressure":
      return "pressure";
    case "StorageFull":
      return "danger";
    default:
      return "neutral";
  }
}
