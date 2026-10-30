import { defineConfig } from 'vite';
import { svelte } from '@sveltejs/vite-plugin-svelte';
import { viteSingleFile } from 'vite-plugin-singlefile';

// The daemon embeds one file, so the build is one file: `viteSingleFile` inlines
// the module script and the stylesheet, and the output lands directly in the
// crate that `include_str!`s it. That keeps the property the hand-written shell
// had — installing the web UI is running the daemon — without keeping the
// hand-written shell.
//
// `{{version}}` in index.html is not a Vite placeholder: the daemon replaces it
// with `acpilot_core::VERSION` when it serves the page, so a browser with no
// token still learns which build is answering. Do not minify it into
// something else; `acpilot-web`'s tests fail if the placeholder survives.
export default defineConfig({
  plugins: [svelte(), viteSingleFile()],
  build: {
    outDir: '../crates/acpilot-web/assets',
    emptyOutDir: true,
    target: 'es2022',
    // A source map would be a second file the daemon does not serve.
    sourcemap: false,
  },
  server: {
    // `npm run dev` talks to a daemon started with
    // `cargo run -p acpilotd -- --addr 127.0.0.1:7777`; same-origin in a build.
    proxy: {
      '/v1': 'http://127.0.0.1:7777',
      // The page's own surface: the stats bar's measured rows and the palette
      // stream. Without this the dev page asks itself for both and gets a 404.
      '/app': 'http://127.0.0.1:7777',
    },
  },
});
