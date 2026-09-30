# Increment 14, Blocker 3 — GUI Endurance + Browser Memory

Real production frontend build, real Chromium (via Playwright), real
`rubixdb gui` product path (`playwright.gui.config.ts` — one real
release binary hosting both API and frontend on the same origin, the
same config `gui_performance.spec.ts` already uses).
`frontend/e2e-gui/gui_endurance.spec.ts`.

## 1. Real sustained workload

50 real cycles, each driving the actual SQL Console UI through real
`Execute`/`Clear` clicks (never simulated/mocked):

1. `SELECT` (filtered point/range read)
2. `JOIN` (`endur_a` ⋈ `endur_b`)
3. `GROUP BY` / `HAVING`
4. `INSERT`
5. `UPDATE`
6. `DELETE`
7. An explicit **transaction**: `BEGIN` → verified "transaction open"
   badge → `INSERT` (same session) → `COMMIT` → verified "autocommit"
   badge again — three separate real `Execute` clicks sharing one
   `session_id`, exactly how this console's UI expresses a
   transaction (it sends one statement per click; there is no
   client-side `;`-splitting the way the CLI has).

Each of the 9 statements per cycle is followed by a real `Clear`
click. 450 total real statement executions, 50 cycles, 1.4 minutes
real wall-clock.

## 2. Real measurement technique (and a real correction made mid-pass)

First attempt used the JS-exposed `performance.memory.usedJSHeapSize`
— every single sample came back as exactly `10000000` (10MB), a real,
verified Chrome behavior (this API is deliberately coarsened/
bucketized for fingerprinting resistance), which would have made any
conclusion from it worthless — named and fixed, not silently kept as
fake evidence.

**Switched to real Chrome DevTools Protocol** (`Performance.getMetrics`
via `page.context().newCDPSession(page)`, after the required
`Performance.enable`) for three real, non-quantized signals:
- `JSHeapUsedSize` — real (noisy, GC-timing-dependent) V8 heap size.
- `Nodes` — live DOM node count (not GC-noisy at all: reflects exactly
  what is currently attached to the document).
- `JSEventListeners` — live JS listener count (same property).

`Nodes`/`JSEventListeners` are the real, precise "no detached
component leak" evidence this blocker asks for: a React component that
fails to clean up its effects/listeners on unmount would show these
two climbing cycle over cycle. Real server-process RSS/handles/threads
(`Get-Process rubixdb`) sampled in parallel.

## 3. Real results (10 samples, every 5th cycle)

```
CDP (heapBytes/domNodes/jsListeners):
5635068/1511/185 | 7000060/6875/226 | 6476864/2933/216 | 14797432/2236/216
17891488/1257/216 | 11482824/3158/216 | 13381084/2264/216 | 30002784/3349/216
5200260/1482/216 | 18090540/1632/216

server (rss_kb/handles/threads):
11428/127/18 | 11540/127/18 | 11588/127/18 | 11540/127/15 | 11668/127/15
11616/127/15 | 11656/127/15 | 11712/127/15 | 11636/126/15 | 11628/126/15
```

Front-half vs. back-half averages:

| Signal | Front-half avg | Back-half avg | Trend |
|---|---|---|---|
| JS heap (bytes) | 10,360,182 | 15,631,498 | Noisy, GC-timing-dependent, no sustained climb (individual samples range 5.2-30MB with no directional pattern) |
| DOM nodes | 2,962 | 2,377 | **Decreasing** |
| JS listeners | 212 | 216 | Flat (+1.9%) |
| Server RSS (KB) | ~11,452 avg | ~11,650 avg | Flat |
| Server handles | 127 | 126 | Flat/decreasing |
| Server threads | 17 avg | 15.4 avg | Flat/decreasing |

## 4. Explicit checks made (not just eyeballed)

- **No result retention**: after the run's final `Clear`, zero
  `Result (...)` cards remain in the DOM — asserted directly.
- **No history leak**: `SqlConsolePage`'s own `HISTORY_LIMIT = 50` is
  a real, pre-existing bound (not added for this test) — asserted the
  rendered history list never exceeds 50 `<li>` entries despite 450
  real statement executions.
- **No detached component leak**: DOM node count and JS listener count
  back-half average must stay under 1.5x the front-half average plus
  a 200-unit slack — both real signals passed with room to spare
  (nodes actually *decreased*).
- **No monotonic browser memory growth**: JS heap back-half average
  must stay under 3x the front-half average plus 5MB slack (loose,
  because GC timing is real noise this harness cannot control without
  a special `--js-flags=--expose-gc` Chromium launch flag this config
  does not set) — passed.

## 5. Verdict

**GUI ENDURANCE = PASS.** **GUI MEMORY = PASS** — real CDP-level DOM
node and JS listener counts (the precise leak signal) show no growth
across 50 real sustained execute/display/clear cycles spanning every
requested statement kind including an explicit transaction; JS heap
size stays bounded within GC-noise tolerance; server-side RSS/handles/
threads stay flat. The initial `performance.memory` measurement
attempt is documented as a real, discovered dead end (quantized to
uselessness) rather than quietly discarded.
