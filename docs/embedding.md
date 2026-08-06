# Embedding a WebSRT player from Gateway A on Web Server B

You have a WebSRT gateway on host **A** (`gateway-a.example`) and want the player
to appear on a separate site on host **B** (`www.example.com`). This doc explains
the one supported way to do that.

## How it works

The player builds its WebTransport URL from the page's hostname, plus an
optional `?host=` override (`web/src/shared/viewer.ts`, `web/src/stream.tsx`):

```js
const pageHost = location.hostname || '127.0.0.1';
const urlParams = new URLSearchParams(location.search);
const wtHost = urlParams.get('host') || (pageHost === 'localhost' ? '127.0.0.1' : pageHost);
const wtPort = urlParams.get('port') || '4433';
...
const wtUrl = `https://${wtHost}:${wtPort}/wt?${qp}`;
```

Query params: `?host=` (WT host, defaults to the page host), `?port=` (default
`4433`), `?stream=` / `?subscribe=` (default `default`), `?token=` (auth).

WebTransport is not bound by CORS — the browser lets a page on B open a WT
session to A directly. So with the `?host=` param, Server B serves the player
and points it at Gateway A. No reverse proxy, no iframe.

## Setup

### 1. Gateway A — use a real cert

Run Gateway A in PKI mode so the browser validates it via normal trust store:

```sh
websrt-gateway --cert-mode mkcert --wt-port 4433 ...
```

(mkcert for LAN, or a Let's Encrypt cert for public hostnames.) This also adds
Firefox support. Self-signed mode (`--cert-mode self`, the default) does **not**
work cross-origin without also shipping Gateway A's `cert-hash.js` to Server B,
and that hash rotates on every gateway restart — don't bother for production.

### 2. Server B — serve the player bundle, point it at A

Build the web client and host `web/dist/` from Server B's HTTPS origin:

```sh
./build.sh web build      # -> web/dist/
# serve web/dist/ over HTTPS on Server B
```

Open:

```
https://www.example.com/?host=gateway-a.example&port=4433&stream=mylive
```

For the publisher page, use `?publish=` instead of `?stream=`.

## Cert modes at a glance

| Gateway `--cert-mode` | Cross-origin (B → A)? | Firefox? |
|---|---|---|
| `self` (default) | Only if Server B serves A's `cert-hash.js` (rotates each restart) | No |
| `mkcert` / real CA | Yes | Yes |

Use `mkcert` (or a real cert) for cross-origin.

## What does NOT work

- **Reverse-proxying `/wt`** through nginx/caddy on Server B. WebTransport runs
  over HTTP/3 datagrams; essentially no L7 proxy passes it through. Connect to
  Gateway A's origin directly.
- **Loading a self-signed Gateway A in an iframe.** It "works" but triggers the
  browser's cert warning on every load and Firefox blocks it outright. Use
  `?host=` + `mkcert` instead.

## Security

- `?token=` is sent in the WT URL query string — it can appear in browser
  history and gateway logs. For production, use the library's auth callback in
  your own binary rather than the reference binary's query-string scheme.
- On Server B, scope CSP to the gateway origin:
  ```http
  Content-Security-Policy: connect-src 'self' https://gateway-a.example:4433;
  ```
