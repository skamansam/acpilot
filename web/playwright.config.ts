import { defineConfig, devices } from '@playwright/test';

// The end-to-end run: the page the daemon serves, in a real browser, against a
// real daemon. `vitest` in happy-dom proves the page's logic and the live test
// proves the palette repaint, but neither can prove that a keystroke reaches
// CodeMirror or that a permission card is clickable: that needs a layout engine
// and a real `fetch`.
//
// The daemon is spawned by the spec, not by this file, because it needs a token
// and a workspace the spec creates and then asserts on. One worker: the tests
// share one daemon and one workspace, and two Chromium instances editing the same
// file would be testing the fixture's race rather than the page.
export default defineConfig({
  testDir: './e2e',
  fullyParallel: false,
  workers: 1,
  retries: process.env.CI ? 1 : 0,
  // A daemon spawn, an agent handshake and a save are in the path; a unit test's
  // five seconds is not this file's budget.
  timeout: 60_000,
  expect: { timeout: 20_000 },
  reporter: process.env.CI ? [['github'], ['list']] : 'list',
  use: {
    trace: 'retain-on-failure',
    screenshot: 'only-on-failure',
    viewport: { width: 1400, height: 900 },
  },
  projects: [{ name: 'chromium', use: { ...devices['Desktop Chrome'] } }],
});
