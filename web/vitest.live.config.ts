import { mergeConfig } from 'vite';
import { defineConfig } from 'vitest/config';
import viteConfig from './vite.config';

// The live shell test, kept apart from `vitest.config.ts` because it needs
// something a unit run must not: a daemon built from this checkout, which it
// spawns as a process and drives over a socket.
//
// Build it with `cargo build -p acpilotd`, then `npm run test:live`. CI does both
// in the `web` job, so the assertion this file makes — that saving a theme
// repaints an open page — is a gate rather than a demonstration.
export default mergeConfig(
  viteConfig,
  defineConfig({
    // As in `vitest.config.ts`: without the browser condition Node resolves
    // Svelte's server build and `mount` is the function that throws.
    resolve: {
      conditions: ['browser'],
    },
    test: {
      environment: 'happy-dom',
      include: ['src/**/*.live.test.ts'],
      restoreMocks: true,
      // A daemon spawn and a 200 ms settings debounce are in the path; five
      // seconds is a unit test's budget, not this one's.
      testTimeout: 60_000,
      hookTimeout: 60_000,
    },
  }),
);
