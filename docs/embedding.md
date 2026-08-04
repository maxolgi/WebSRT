# Embedding a WebSRT player from Gateway A on Web Server B

This guide covers the realistic case: you have a WebSRT gateway running on host
**A** (`gateway-a.example`), and a separate web site / app on host **B**
(`www.example.com`) that wants to show the player. It explains *why* the obvious
thing doesn't work, and gives two supported approaches.

---

## 1. How the pieces actually fit

Two important facts that shape everything below:

1. **The gateway binary does not serve the web client.** `websrt-gateway` only
   runs:
   - the WebTransport endpoint (default `:4433`, fixed path `/wt`), and
   - a small HTTP health/metrics server (separate port).

   The browser UI (`index.html`, `advanced.html`, `stream.html` + JS/WASM) is a
   static bundle in `web/dist/` (or the Vite dev server on `:5173`) served by
   *some* HTTP(S) host. In production that's usually nginx/caddy in front of, or
   beside, the gateway.

2. **The player derives its WebTransport target from the page's own hostname.**
   In `web/src/shared/viewer.ts` (viewer) and `web/src/stream.tsx` (publisher):

   ```js
   const pageHost = location.hostname || '127.0.0.1';
   const wtHost   = pageHost === 'localhost' ? '127.0.0.1' : pageHost;
   const wtPort   = urlParams.get('port') || '4433';
   ...
   const wtUrl = `https://${wtHost}:${wtPort}/wt?${qp}`;
   ```

   There is **no `?host=` query parameter**. Query params that *are* honored:
   `?port=` (default `4433`), `?stream=` / `?subscribe=` (default `default`),
   `?token=` (auth).

   Net effect: **the page origin must equal the WebTransport origin.** A page
   served from Server B will try to connect to Server B's WT port, not Gateway A.

3. **The cert hash is fetched same-origin.** At runtime the page does
   `fetch('/cert-hash.js')` (`viewer.ts:233`) and parses `window.CERT_HASH` from
   it. That file is written by *the gateway that shares the web root* at startup
   (`crates/websrt-gateway/src/main.rs:156-185`). So the page must be able to
   read Gateway A's hash, or run in a mode where no hash is needed.

---

## 2. Pick an approach

| Approach | Code change? | Page served by | Best when |
|---|---|---|---|
| **A. Iframe from Gateway A** | none | Gateway A's origin (inside the iframe) | You can serve the client from A and just want it visually embedded on B |
| **B. `?host=` query param** | one line per file | Server B | You want the player to live natively on B's page |

Approach C (Server B reverse-proxies `/wt` to Gateway A) is **not recommended**:
WebTransport runs over HTTP/3 datagrams and almost no L7 proxy speaks it. Use a
real origin instead.

---

## Approach A — Iframe from Gateway A (no code change)

The iframe's `location.hostname` becomes Gateway A's host, so the existing code
connects correctly with zero edits.

### Prerequisites

- Gateway A must **also serve the web client** over HTTPS. Since the binary
  doesn't do static files, put a reverse proxy (nginx/caddy) on A that:
  - serves `web/dist/` for `/`, and
  - leaves `/wt` handled by the gateway (WebTransport needs the proxy to pass
    HTTP/3 through; simplest is to expose the gateway's `:4433` directly and let
    the iframe URL reference it, see below).
- The cert presented to the browser must be valid for the hostname in the
  iframe URL (see §4).

### Steps

1. On Gateway A, build the client and serve it:
   ```sh
   ./build.sh web build          # -> web/dist/
   # serve web/dist/ over HTTPS, e.g. with caddy or nginx
   ```
2. Ensure the gateway is running so `web/public/cert-hash.js` exists and is
   copied/served at `/cert-hash.js` on A.
3. On Server B, embed:
   ```html
   <iframe
     src="https://gateway-a.example/?stream=mylive&port=4433"
     allow="autoplay; fullscreen"
     style="width:100%;aspect-ratio:16/9;border:0;">
   </iframe>
   ```
   Pass `?stream=`, `?port=`, `?token=` in the iframe `src` as needed.

### Caveats

- Parent↔iframe scripting is cross-origin: use `postMessage`, not direct DOM
  access.
- The iframe content still needs a valid cert (Chrome's
  `serverCertificateHashes` works inside an iframe, but Firefox ignores it —
  see §4).
- A 0×0 or hidden iframe may throttle timers and hurt the SRT poll loop; keep it
  visible or at least sized.

---

## Approach B — `?host=` query param (one-line code change)

Add a `host` override so a page served from Server B can target Gateway A.
Recommended when you want the player to be a first-class part of B's page.

### Step 1 — edit two files

`web/src/shared/viewer.ts` at line 280:

```js
// before
const wtHost = pageHost === 'localhost' ? '127.0.0.1' : pageHost;
// after
const wtHost = urlParams.get('host') || (pageHost === 'localhost' ? '127.0.0.1' : pageHost);
```

`web/src/stream.tsx` at line 472: identical change.

Rebuild the client (`./build.sh web build`) and serve the new `web/dist/` from
Server B.

### Step 2 — solve the cert-hash problem

This is the part that bites. With the page on Server B, `fetch('/cert-hash.js')`
hits **Server B**, which has no idea what Gateway A's cert hash is. Pick one:

- **(Recommended) Run Gateway A in PKI mode** — `--cert-mode mkcert` with a
  real cert (Let's Encrypt, or mkcert for LAN). Then `cert-hash.js` is
  `window.CERT_HASH = null;` and the browser validates via normal PKI. Works in
  Firefox too. Server B can ship its own `cert-hash.js` containing exactly:
  ```js
  window.CERT_HASH = null;
  ```

- **Self-signed A, copy the hash** — run Gateway A in default (`self`) mode,
  copy its generated `web/public/cert-hash.js` onto Server B's web root. Works
  in Chrome only. **The hash changes every time Gateway A restarts**, so you
  must recopy it on each restart — fragile, hence not recommended for
  production.

- **Proxy `/cert-hash.js` on Server B → Gateway A** — if Server B runs
  nginx/caddy, proxy just that one file through to A. Still Chrome-only for
  self-signed.

### Step 3 — embed

```html
<!-- on Server B's page -->
<video ... />  <!-- or whatever mount point your integration uses -->

<!-- load the player pointed at Gateway A -->
<script type="module" src="/assets/main.js"></script>
<!-- then navigate/configure via URL params: -->
<!-- https://www.example.com/player?host=gateway-a.example&port=4433&stream=mylive -->
```

Or for the simplest integration, just link users to:
```
https://www.example.com/?host=gateway-a.example&port=4433&stream=mylive
```

---

## 4. Cert modes and browser support

Gateway `--cert-mode` (see `crates/websrt-gateway/src/main.rs`, and the README's
"Cert modes" section):

| Mode | Browser validation | Works cross-origin (B → A)? | Firefox? |
|---|---|---|---|
| `self` (default) | `serverCertificateHashes` (hash pinning) | Yes, **if** the page knows A's hash | **No** (Firefox ignores `serverCertificateHashes`) |
| `mkcert` | Normal PKI | Yes, if A's cert is publicly trusted | Yes |

Rule of thumb for cross-server embedding: **use `mkcert` (or a real CA cert) on
Gateway A.** It removes the hash-bootstrap problem entirely and adds Firefox
support.

Note: WebTransport itself is a secure-context feature; both A and B must be
served over HTTPS.

---

## 5. Security notes

- `?token=` is the auth query param. It travels in the WT URL (query string) and
  may appear in browser history / server logs. For production, prefer wiring the
  library's auth callback and a header-based scheme in your own binary (the
  library exposes `Gateway::builder().auth_token(...)` and more; see
  `crates/websrt/`).
- Embedding the player on Server B means B's users trust A as a streaming
  source. Apply CSP `frame-src` / `connect-src` rules on B accordingly:
  ```http
  Content-Security-Policy: connect-src 'self' https://gateway-a.example:4433;
  ```
- If A is self-signed and you copy the hash to B, anyone who learns that hash
  can MITM A's WT until the cert rotates. Prefer PKI.

---

## 6. TL;DR decision

- **Want zero code changes and can serve the client from Gateway A?** →
  Approach A (iframe).
- **Want the player to live natively on Server B's page?** → Approach B
  (`?host=` + run Gateway A in `--cert-mode mkcert`).
- **Do not** try to reverse-proxy `/wt` through Server B; WebTransport/HTTP3
  won't survive a typical L7 proxy.
