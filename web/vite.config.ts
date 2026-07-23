import { defineConfig } from 'vite';
import { resolve } from 'path';
import basicSsl from '@vitejs/plugin-basic-ssl';
import preact from '@preact/preset-vite';

// Vite config. The wasm-pack output (`pkg/`) of each crate is copied into
// `web/wasm/<crate>/` manually — Vite serves it via the dev server.
//
// HTTPS is required so the browser exposes WebTransport (secure-context only)
// when viewing the page from another machine on the LAN.
// The basic-ssl plugin generates a throwaway self-signed cert; click through
// Chrome's "not private" warning to proceed.
//
// Multi-page: index.html is the original simple demo page; advanced.html is
// the full-featured page with the debug panel (Preact + Chart.js + Eruda).

export default defineConfig({
  server: {
    host: '0.0.0.0',
    port: 5173,
    cors: { origin: [/^https?:\/\/(localhost|127\.0\.0\.1):8100$/] },
  },
  plugins: [basicSsl(), preact()],
  build: {
    rollupOptions: {
      input: {
        main: resolve(__dirname, 'index.html'),
        simple: resolve(__dirname, 'simple.html'),
        stream: resolve(__dirname, 'stream.html'),
      },
    },
  },
});
