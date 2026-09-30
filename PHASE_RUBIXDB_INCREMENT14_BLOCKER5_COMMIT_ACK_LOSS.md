# Increment 14, Blocker 5 — Commit-Acknowledgment Loss

Real scenario, real test (`api/tests/api_commit_ack_loss.rs`): client
sends `COMMIT`, server durably commits, client loses the connection
before consuming the response, client reconnects, real durable state
is inspected.

## 1. Construction

A real server bound to a real OS TCP listener (not `tower::
ServiceExt::oneshot`, which never round-trips actual socket bytes — this
scenario specifically needs a connection the test can sever without
reading). A complete, valid HTTP/1.1 `POST /v1/sql` request for
`COMMIT` is written by hand over a raw `std::net::TcpStream`
(`Content-Length` framed, so the server needs no TCP-level EOF to know
the request is complete), flushed, given a brief real wait, then the
socket is dropped **without ever calling `read()`** — the client never
consumes any byte of the response, deterministically, not by a timing
guess. axum/hyper fully drives the handler future (including the real
`Transaction::commit()` call inside it) to completion before it ever
attempts to write a response, so this reproduces exactly "the server
durably commits before the client would have seen the acknowledgment."

## 2. Real result

```
test commit_durably_succeeds_even_when_the_client_never_reads_the_response ... ok
```

- **Server state**: the row (`INSERT INTO ack_loss_t ... VALUES (1,
  'committed-but-unacked')`) is durably visible via a completely
  separate, brand-new connection/session issued after the abandoned
  one — real evidence, not an assumption from the `write_all` having
  succeeded.
- **Client-visible state**: none. The client that issued `COMMIT`
  received zero bytes of response and has no way to know, from that
  request alone, whether the commit happened.
- **Transaction result**: committed (real durable write survives).
- **Safe retry behavior — documented, not invented**: a naive client
  that assumes "no response means retry" and resends `COMMIT` with the
  same `session_id` gets a clean, typed `SESSION_NOT_FOUND` (HTTP 404)
  rejection — the session was already consumed by the first, real
  commit. This product does **not** fabricate idempotency (no silent
  no-op "success" on replay, and no second real side effect either,
  since the session simply no longer exists to act on). A correct
  client must re-query to discover whether its write already landed,
  rather than trust a blind retry of `COMMIT` itself.

## 3. Why this is safe by construction, not by luck

`handle_commit` (`api/src/routes/sql.rs`) removes the session from the
registry (`sessions.take`) and calls `txn.commit()` synchronously,
*before* constructing any HTTP response — there is no code path in
which a response is sent before the commit is durable, and no code
path in which the session survives a successful commit for a retry to
re-target. This is exactly the "commit-then-respond" ordering the
scenario requires, verified directly by a real client that never sees
that response at all.

## 4. Verdict

**COMMIT ACK LOSS = PASS** — real client-disconnect-before-reading-
response scenario reproduced with a hand-constructed real HTTP
request over a real socket (not simulated), server-side durability
proven via independent reconnection, and retry semantics documented
exactly as they are (a safe, typed rejection) rather than inventing
idempotency that does not exist.
