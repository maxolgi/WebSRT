# Embedding the WebSRT player

The WebSRT player is a **canvas + WebCodecs SDK** you bundle into your own
application. There is no `<video>`/MSE path — frames go straight from the SRT
receiver (WASM) through a `VideoDecoder` onto a `<canvas>` you own. Your app
owns all the UI (buttons, sliders, overlays); the SDK renders video and emits
events.

There is **one** supported embed model: consume this repo as a git submodule
and import the player at source level. Your bundler produces a same-origin
bundle, so there are no cross-origin fetches and no CORS configuration. See
[Supported embed model](#supported-embed-model).

## Why this exists

Most browser live-streaming stacks (MoQ, fMP4 over HLS/LL-HLS, CMAF) add a
caching layer: encode to fMP4, chunk it, cache the chunks at the edge. That
caching layer exists because long-distance best-effort delivery drops packets
and re-fetching whole segments is too expensive.

SRT takes the other fork: when a packet is dropped, **retransmit just that
packet**, not a segment. No fMP4, no chunking, no cache. The player runs the
*same* `srt-protocol` + `mpeg2ts` Rust crates (compiled to WASM) that the
gateway runs, so ARQ, TSBPD, and the congestion window live in the browser.

That efficiency — retransmitting bytes, not segments — is *why* the player hits
a sub-200 ms glass-to-glass latency floor in a browser. Canvas + WebCodecs is a
deliberate choice to preserve it: MSE chunking would reintroduce exactly the
buffering SRT was designed to avoid.

## Supported embed model

Consume this repo as a **git submodule** and import the player at source level.
Your bundler (Vite or webpack 5) resolves the worker and the WASM.

```sh
# in your app repo
git submodule add https://github.com/maxolgi/WebSRT.git vendor/websrt
cd vendor/websrt
./build.sh wasm        # builds srt-wasm + mpeg2ts-wasm + ts-muxer-wasm
                       # → copies pkg/ into web/wasm/ (gitignored; required)
```

> `web/wasm/` is gitignored — the artifacts are not in the submodule. Anyone
> cloning your app must run `./build.sh wasm` (or the repo's `./build.sh setup`)
> before the player will load. Wire it into your build/bootstrap.

Then import and mount:

```ts
import { mountPlayer } from 'websrt-web/src/player';

const handle = mountPlayer(document.querySelector('canvas')!, {
  host: 'gateway-a.example',
  port: 4433,
  stream: 'mylive',
  latencyMs: 120,
  // certHash: see "Cert modes" below
});
```

**Bundler constraint — module workers.** The SDK spawns its receiver/demuxer on
a worker via `new Worker(new URL('...', import.meta.url), { type: 'module' })`
and loads the WASM as ES modules. Your bundler must support module workers +
WASM URL resolution. **Vite** supports this out of the box. **webpack 5**
supports it (module workers since 5.0; WASM via
`experiments.asyncWebAssembly` or asset modules). Older webpack / legacy
bundler setups will not work — do not try to shim it.

Because the player ships **inside** your app bundle (same origin), there are
**zero** cross-origin fetches at runtime. The "CORS problem" people associate
with cross-origin WebTransport does not apply to this model — see
[The "CORS" question](#the-cors-question).

## The "CORS" question

Short version: **WebTransport is not CORS-restricted.** A page on origin `B`
can open a WebTransport session to gateway `A` with no CORS headers on `A` at
all — only TLS certificate trust matters. CORS governs `fetch`/XHR-style
reads; WebTransport is a raw QUIC stream/datagram transport and is exempt.

What people actually hit and mislabel "CORS" is one of three real, separate
problems:

1. **Cert trust on the LAN (self-signed).** A self-signed gateway presents a
   cert the browser doesn't trust. Over WebTransport there is no click-through
   warning like an HTTPS page — the connection just fails. The fix is the cert
   hash (`serverCertificateHashes`) for Chrome, or a real/mkcert cert for
   everyone. See [Cert modes](#cert-modes). This is a *trust* problem, not
   CORS.
2. **Cross-origin module/worker/WASM loads during development.** Module
   scripts, workers, and `.wasm` are CORS-gated fetches. If you point a page on
   `B` at the player bundle served from the WebSRT **Vite dev server** on `A`,
   the browser will block those fetches without correct CORS headers on the dev
   server. This only ever bites you during *development* against the unbundled
   dev server.
3. **Delivering the self-signed cert hash to a cross-origin page.** This is the
   one that actually bites cross-origin self-signed deployments, and **no
   amount of gateway configuration fixes it** — it is a browser security wall,
   not a CORS setting. See [Delivering the cert hash cross-origin](#delivering-the-cert-hash-cross-origin).

Once the player is **bundled into your app** (the supported embed model), the
player's JS and WASM are same-origin with your page and problem #2 vanishes.
Problem #1 you handle via `certHash` / a real cert. **You do not configure CORS
on the gateway.** Problem #3 has no browser-side fix — see below.

## Player API

### `mountPlayer(canvas, opts) → PlayerHandle`

Creates a player bound to `canvas`, returns a `PlayerHandle` (an `EventTarget`
you can `addEventListener` on).

**`opts`**

| Option | Type | Default | Description |
|---|---|---|---|
| `host` | `string` | page hostname | WebTransport host. `localhost` → `127.0.0.1`. |
| `port` | `number` | `4433` | WebTransport port. |
| `stream` | `string` | `'default'` | Stream name (the gateway's `?stream=`/`?subscribe=`). |
| `token` | `string` | — | Auth token, sent as `?token=` in the WT URL. See [Security](#security). |
| `certHash` | `string \| null` | — | See below. |
| `latencyMs` | `number` | `120` | TSBPD latency floor (the glass-to-glass buffer). |
| `renderPacing` | `boolean` | `true` | Pace canvas draws to video PTS for smooth playback. |
| `decodePacing` | `boolean` | `false` | Pace `VideoDecoder` queue to DTS (helps absorb connect-time bursts). |
| `muted` | `boolean` | — | Start audio muted (browsers block autoplay with sound). |
| `debug` | `boolean` | `false` | Emit verbose `stats`/`drift` events and internal logs. |

**`certHash`** — three behaviors:

| You're connecting to… | Pass |
|---|---|
| A **self-signed** gateway | the DER SHA-256 **hex string** (64 hex chars). Browser pins it via `serverCertificateHashes` (Chrome only). |
| A **real-CA / mkcert** gateway (PKI) | `null`. Browser validates via the normal trust store (Firefox-compatible). |
| The **reference demo pages** in this repo | **omit it** — those pages still read `window.CERT_HASH` from `cert-hash.js` written by the gateway at boot. |

For an embedded app, use a real/mkcert cert and pass `certHash: null`. For the
self-signed + cross-origin case, the hash must arrive out-of-band (your backend
injects it) — see [Delivering the cert hash cross-origin](#delivering-the-cert-hash-cross-origin).

### `PlayerHandle`

`mountPlayer` returns a `PlayerHandle` — an `EventTarget` with the methods
below. `connect()` returns a `Promise` that resolves on the **first decoded
frame**.

| Method | Returns | Description |
|---|---|---|
| `connect()` | `Promise<void>` | Open WebTransport + SRT handshake; resolves on first frame. |
| `disconnect()` | `void` | Tear down the session; suppresses auto-reconnect. |
| `destroy()` | `void` | `disconnect()` + release the worker, WASM, and canvas resources. Handle is dead afterwards. |
| `setMuted(b)` | `void` | Mute / unmute audio. |
| `setLatencyMs(ms)` | `void` | Adjust TSBPD latency live. |
| `setRenderPacing(b)` | `void` | Toggle PTS-paced drawing. |
| `setDecodePacing(b)` | `void` | Toggle DTS-paced decode. |
| Getters | — | `readyState`, `state`, `videoWidth`, `videoHeight`, … |

**Events** (via `handle.addEventListener(type, fn)`):

```
loadstart  connecting  open  canplay  playing  waiting
resize  error  statechange  close  stats  drift
```

Minimal wiring:

```ts
const handle = mountPlayer(canvas, { host: 'gateway-a.example', certHash: null });

handle.addEventListener('open',    () => console.log('connected'));
handle.addEventListener('resize',  () => console.log(`${handle.videoWidth}x${handle.videoHeight}`));
handle.addEventListener('error',   (e) => console.error('player error', e));
handle.addEventListener('stats',   (e) => updateBitrateUI(e.detail));

await handle.connect();
```

> The reference demo pages in this repo (`index.html`, `advanced.html`,
> `stream.html`) additionally accept `?host=`, `?port=`, `?stream=`, and
> `?token=` URL params for quick manual testing. That URL-param layer is a
> convenience of the demos, **not** part of the embed API — embedders pass
> `opts` to `mountPlayer`.

## Cert modes

| Gateway `--cert-mode` | Browser trust | Cross-origin works? | Firefox? | Notes |
|---|---|---|---|---|
| `self` (default) | `serverCertificateHashes` pin (Chrome only) | Only if you ship the hash | No | Hash rotates **on every restart *and* every ≤2 weeks** — see below. |
| `mkcert` / real CA | Normal PKI trust store | Yes | Yes | Recommended for any non-LAN / embedded deployment. |

For embedding, run the gateway with `--cert-mode mkcert` (LAN) or a real
Let's Encrypt cert (public hostname) and pass `certHash: null`.

```sh
websrt-gateway --cert-mode mkcert --wt-port 4433 ...
```

### Why self-signed hashes rotate (and why you can't avoid it)

The self-signed cert is regenerated each boot, but more fundamentally the
WebTransport spec **caps the validity of any cert used with
`serverCertificateHashes` at two weeks** (the browser deems the cert trusted
*iff* the leaf hash matches **and** "the current time is within the validity
period" **and** "the total length of the validity period MUST NOT exceed two
weeks" — WebTransport §6.9 / §14.4). Pinning replaces Web-PKI chain
verification; it does **not** replace the expiry check.

Two consequences worth stating plainly so they aren't re-litigated:

- **Persisting the self-signed cert does not help.** Even a cert saved to disk
  can't have a validity window longer than two weeks, so it must be re-issued
  (new hash) at least that often regardless. The gateway's regenerate-on-boot
  behavior is consistent with the spec cap.
- **The browser-discovery route is closed.** The player page must be HTTPS
  (`WebTransport` is secure-context-only), so a `fetch()`/`<script>` to learn
  the hash from a self-signed gateway is blocked — plain HTTP as **mixed
  content**, self-signed HTTPS by **TLS verification** — and
  `serverCertificateHashes` exists **only** on the `WebTransport` constructor,
  not on `fetch`/XHR/`<script>`. No endpoint on the gateway or on Vite can
  change this.

### Delivering the cert hash cross-origin

Since the browser cannot fetch the hash, it must arrive via a channel the
browser already trusts. Two paths:

**Lab / small environment → `mkcert` with an IP SAN (no hash at all).** mkcert
accepts raw IPs (no DNS required) and its certs are validated by the normal
trust store, so the two-week cap does **not** apply:

```sh
mkcert -install                          # once per browser; installs the local CA
mkcert 192.168.1.10                      # cert valid 2y3m, includes the IP as a SAN
websrt-gateway --cert-mode mkcert \
  --cert-pem ./192.168.1.10.pem \
  --key-pem  ./192.168.1.10-key.pem \
  --wt-port 4433 ...
```

Then `mountPlayer(canvas, { certHash: null })`. No hash juggling, no rotation
for the cert's life, and it works in Firefox too. (The root CA lives ~10
years; leaf certs ~2 years 3 months, per mkcert.)

**Self-signed + cross-origin → your backend injects `certHash`.** This is the
industry-standard "signaling server provisions connection params" pattern
(same shape as WebRTC). Your page is served by your own origin; your backend
fetches the *current* hash **server-side** — no browser in that hop, so none of
the CORS / mixed-content / TLS walls above apply — and renders it into the page
as `certHash`. Fresh on every page load, it absorbs both the per-restart and
the two-week rotation automatically:

```ts
// server-side (Node/etc.): fetch the hash from the gateway, TLS unchecked
const resp = await fetch('https://encoder.lan:5173/cert-hash.js'); // or read a shared file
// inject <script>window.CERT_HASH = "..."</script> into the page, OR
// render the hex string into the mountPlayer({ certHash }) call.
```

### Where the backend gets the hash (no new endpoint needed)

The hash is exposed server-side today, with no gateway change:

- **The gateway boot log** prints `WebTransport cert DER SHA-256: <hex>`
  (`crates/websrt-gateway/src/main.rs`).
- **The `cert-hash.js` file** the gateway writes at boot
  (`web/public/cert-hash.js`), readable on a shared filesystem or fetchable
  from the encoder's existing web server with TLS verification disabled
  server-side (e.g. `curl -k https://encoder.lan:5173/cert-hash.js`).

## What does NOT work

- **Reverse-proxying `/wt` through an L7 proxy** (nginx, caddy, a CDN).
  WebTransport runs over **HTTP/3 datagrams**; essentially no L7 proxy in its
  default configuration passes H3 datagrams through. The browser must connect
  to the gateway's origin **directly**. Connect your page to the gateway, don't
  try to front it.
- **Embedding a self-signed gateway in an `<iframe>`.** It "works" but fires
  the browser's cert warning on every load, and Firefox blocks self-signed
  content in frames outright. Use a real/mkcert cert and connect directly from
  your page instead.

## Security

- **`?token=` (and `opts.token`) travel in the WebTransport URL query string**,
  so they can appear in browser history and gateway logs. The query-string
  scheme is the reference binary's convenience auth. For production, prefer the
  library's auth callback in your own gateway binary over passing tokens from
  the page.
- **Scope CSP to the gateway origin** on the page that embeds the player:
  ```http
  Content-Security-Policy: connect-src 'self' https://gateway-a.example:4433;
  ```
  WebTransport connections count against `connect-src`; the WASM/worker fetches
  are same-origin (covered by `'self'`).
