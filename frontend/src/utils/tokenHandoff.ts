// Phase 7 D-3 / O-2: `rubixdb gui` opens the console at `/#token=<key>`.
// The key travels in the URL *fragment*: browsers never send it to a server
// or in `Referer`. It is read once at boot, removed from the address bar
// immediately (so it cannot be copied, bookmarked or shown by reload), and
// kept only in that tab's sessionStorage by SessionProvider.

// What a locally generated key looks like (64 hex) plus headroom for keys an
// operator configured by hand; anything else is ignored, never stored.
const TOKEN_PATTERN = /^[A-Za-z0-9_-]{16,256}$/;

/**
 * Returns a well-formed token from `location.hash`, or null. Whenever a
 * `token` parameter is present -- valid or not -- the fragment is removed
 * from the URL (history entry replaced, not pushed), so a malformed or stale
 * token is not left sitting in the address bar either.
 */
export function takeTokenFromLocation(win: Window = window): string | null {
  const hash = win.location.hash;
  if (!hash || hash.length < 2) return null;
  const params = new URLSearchParams(hash.slice(1));
  if (!params.has("token")) return null;
  const token = params.get("token") ?? "";
  win.history.replaceState(null, "", win.location.pathname + win.location.search);
  return TOKEN_PATTERN.test(token) ? token : null;
}
