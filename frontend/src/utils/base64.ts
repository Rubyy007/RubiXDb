// Standard (padded) base64, matching rubixdb-api's own encoding
// exactly (PHASE_API_ARCHITECTURE.md §2 — `base64::engine::general_
// purpose::STANDARD`). Keys/values are arbitrary bytes; the console
// lets the user enter either UTF-8 text (encoded here) or raw
// base64 directly for binary data.

export function utf8ToB64(text: string): string {
  const bytes = new TextEncoder().encode(text);
  let binary = "";
  bytes.forEach((b) => {
    binary += String.fromCharCode(b);
  });
  return btoa(binary);
}

export function b64ToUtf8(b64: string): string {
  const binary = atob(b64);
  const bytes = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i++) {
    bytes[i] = binary.charCodeAt(i);
  }
  try {
    return new TextDecoder("utf-8", { fatal: true }).decode(bytes);
  } catch {
    // Not valid UTF-8 -- caller should fall back to displaying the
    // base64 form directly rather than mangling binary data.
    throw new Error("value is not valid UTF-8");
  }
}

export function isValidB64(value: string): boolean {
  if (value === "") return true;
  try {
    atob(value);
    return true;
  } catch {
    return false;
  }
}

export function byteLengthOfB64(b64: string): number {
  try {
    return atob(b64).length;
  } catch {
    return 0;
  }
}
