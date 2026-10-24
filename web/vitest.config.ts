import { mergeConfig } from 'vite';
import { defineConfig } from 'vitest/config';
import viteConfig from './vite.config';

// Tests share the build's plugins (so `.svelte` compiles the same way) and
// differ only in where they run: happy-dom for a DOM, and an explicit include
// so `vite build` can never pick a test up as an entry point.
//
// This is a separate file rather than a `test` key in `vite.config.ts` because
// that config is typed by Vite, which does not know the key — and a build
// config that imports the test runner is a build that needs the test runner.
export default mergeConfig(
  viteConfig,
  defineConfig({
    // Without this, Node's resolution picks Svelte's server build and `mount`
    // is the function that throws "not available on the server". The browser
    // condition is what the page itself gets from Vite.
    resolve: {
      conditions: ['browser'],
    },
    test: {
      environment: 'happy-dom',
      include: ['src/**/*.test.ts'],
      restoreMocks: true,
    },
  }),
);
