// Fail on broken internal links in the built site.
//
// The site is served from /acpilot on GitHub Pages, so a link is only correct if
// it resolves with that prefix intact. Checking after the build is the only way
// to catch the relative-path mistake that a base path invites.

import { readdirSync, readFileSync, statSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';

const DIST = new URL('../dist/', import.meta.url).pathname;
const BASE = '/acpilot/';

function walk(dir) {
  return readdirSync(dir).flatMap((entry) => {
    const full = join(dir, entry);
    return statSync(full).isDirectory() ? walk(full) : [full];
  });
}

const pages = walk(DIST).filter((f) => f.endsWith('.html'));
const problems = [];

for (const page of pages) {
  const html = readFileSync(page, 'utf8');
  for (const match of html.matchAll(/href="([^"]+)"/g)) {
    const href = match[1];
    if (/^(https?:|mailto:|#|data:)/.test(href)) continue;

    const path = href.split('#')[0].split('?')[0];
    if (!path) continue;

    let target;
    if (path.startsWith(BASE)) {
      target = join(DIST, path.slice(BASE.length));
    } else if (path.startsWith('/')) {
      problems.push(`${page}: ${href} is missing the ${BASE} base prefix`);
      continue;
    } else {
      target = resolve(dirname(page), path);
    }

    try {
      if (statSync(target).isDirectory()) target = join(target, 'index.html');
    } catch {
      if (!target.endsWith('.html')) target = join(target, 'index.html');
    }

    try {
      statSync(target);
    } catch {
      problems.push(`${page}: ${href} does not resolve (${target})`);
    }
  }
}

if (problems.length > 0) {
  console.error(`broken internal links: ${problems.length}`);
  for (const problem of problems) console.error(' -', problem);
  process.exit(1);
}

console.log(`internal links ok across ${pages.length} pages`);
