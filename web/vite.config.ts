import { defineConfig } from 'vite';
import basicSsl from '@vitejs/plugin-basic-ssl';

// Vite config. The wasm-pack output (`pkg/`) of each crate is copied into
// `web/wasm/<crate>/` manually — Vite serves it via the dev server.
//
// HTTPS is required so the browser exposes WebTransport (secure-context only)
// when viewing the page from another machine on the LAN.
// The basic-ssl plugin generates a throwaway self-signed cert; click through
// Chrome's "not private" warning to proceed.

export default defineConfig({
  server: {
    host: '0.0.0.0',
    port: 5173,
    cors: { origin: [/^https?:\/\/(localhost|127\.0\.0\.1):8100$/] },
  },
  plugins: [basicSsl()],
});
