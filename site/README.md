# Acpilot field manual

The docs site: Astro + Starlight, with a custom theme in `src/styles/custom.css`.

```bash
cd site
npm install
npm run dev        # http://localhost:4321/acpilot/
npm run build      # static output in dist/
node scripts/check-links.mjs
```

## Deploying

The workflow builds and link-checks on every push. Deployment to GitHub Pages is
gated on the repository variable `DOCS_DEPLOY`:

```bash
gh variable set DOCS_DEPLOY --body true
```

That gate exists because GitHub Pages is not available for private repositories
on every plan, and a workflow that fails on every push for an account setting is
one people learn to ignore.

The site is configured for the project-page path (`/acpilot`). To move it to a
custom domain or a root path, change `site` and `base` in `astro.config.mjs` —
and the link checker's `BASE` constant with them.

## Writing

- Every page carries a status pill: `flying`, `boarding`, or `hangar`, matching
  `TODO.md`. A page that claims more than the code does is a bug.
- Internal links are relative (`../../status/`), not root-absolute: the site lives
  under `/acpilot/`, and root-absolute links silently 404 on Pages. The link
  checker enforces this.
- The tone is deadpan aviation, not sketch comedy. One good joke per section is
  worth three that interrupt the instructions.
