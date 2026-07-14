# Certs

The gateway supports two certificate modes via `--cert-mode <self|mkcert>`.

## Mode A: `self` (default)

The gateway generates a fresh self-signed ECDSA P-256 certificate on every
boot with ≤14-day validity (per W3C WebTransport spec). The SHA-256 of the
cert's DER is printed to stdout:

```
INFO gateway: WebTransport cert DER SHA-256: <64 hex chars>
```

Paste that hash into the browser UI's "Cert hash" field. The browser then
connects with `serverCertificateHashes: [{ algorithm: 'sha-256', value: <bytes> }]`.

**Supported by:** Chrome ≥97. Not supported by Firefox (use mkcert mode).

The cert regenerates on every boot, so each restart requires re-pasting the
hash into the browser. This is by design: it keeps the dev workflow zero-config.

## Mode B: `mkcert` (all browsers, including Firefox)

Generate a normal (PKI-trusted) cert+key pair using [mkcert](https://github.com/FiloSottile/mkcert):

```bash
mkcert -install                    # one-time: installs the local CA into your trust store
mkcert localhost 127.0.0.1 ::1    # produces ./localhost+2.pem and ./localhost+2-key.pem
```

Run the gateway pointing at those files:

```bash
cargo run -p gateway -- --cert-mode mkcert \
    --cert-pem ./localhost+2.pem \
    --key-pem  ./localhost+2-key.pem
```

In the browser UI, **leave the cert-hash field blank** — the page detects
mkcert mode and connects without `serverCertificateHashes`. Refresh the page
if you connected before installing the CA.

**Supported by:** All browsers with WebTransport support, once the local CA
is installed.

## Default bind is 127.0.0.1

For security, `--cert-mode self` should only ever bind `127.0.0.1`. Anyone
with the cert hash can connect; on `0.0.0.0` that includes anyone on your
LAN who can read your terminal. The default `--bind 127.0.0.1` enforces
this. Override only if you understand the exposure.
