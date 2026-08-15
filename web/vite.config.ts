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
// Multi-page entries: index.html (unified viewer; debug panel lazy-loaded),
// simple.html (minimal viewer), stream.html (publisher).

export default defineConfig({
  // Relocatable dist: worker wasm URLs resolve relative to the worker script
  // instead of an absolute /assets/ prefix baked in at the origin root.
  base: './',
  server: {
    host: '0.0.0.0',
    port: 5173,
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
