---
title: Planned extensions
description: The shelf — component systems that render, a blog that stays files, and what each one needs before it can be built.
---

<span class="pill pill--hangar">hangar</span> &nbsp; Mod shop phase (P4.6) &nbsp;·&nbsp; none of these install today

The [marketplace](../marketplace/) lists what you can install. This page lists what we
intend to build, and what each one needs first. It exists because "Svelte support"
sounds like one feature and is really three, and because the missing one — a way to
render a component inside the editor — is a product decision, not a preset.

**Nothing here is installable.** An entry moves off this page and into
`marketplace/index.toml` when it has a public repository, a licence, a manifest that
loads, and every asset it names.

## What "support" means

Three separate things, and today's extensions can carry only the first two:

| Layer | What it gives you | Available now |
| --- | --- | --- |
| Grammar | Syntax highlighting for the file | Yes — tree-sitter grammar + queries (`grammars`) |
| Language server | Completions, diagnostics, go-to-definition | Yes — a preset (`language_servers`) |
| Preview | The component rendered, with its props editable | **No** — needs a new contribution kind and a sandboxed surface |

A pack that shipped only the first two would be honest but underwhelming, which is
why the preview surface comes before the packs that depend on it.

## Component systems

One extension per framework rather than one "web frameworks" pack: the grammar
differs, the server differs, and above all the compiler that turns the file into
something renderable differs. A single pack would have to bundle seven compilers
and then pick one at runtime — that is a build system, not an extension.

| Extension | Grammar and server | How the preview would render |
| --- | --- | --- |
| `acpilot.web-components` | TS/JS grammar, `custom-elements.json` for completion | The element itself, with attributes, slots and fired events shown beside it. No compiler at all, which is why it is first. |
| `acpilot.svelte` | `tree-sitter-svelte`, `svelte-language-server` | The Svelte compiler, in the sandbox, with props as a form. One compiler, one output — the cheapest real preview. |
| `acpilot.vue` | `tree-sitter-vue`, Volar (`vue-language-server`) | `@vue/compiler-sfc`, props and slots as a form, emitted markup shown under the result. |
| `acpilot.lit` | TS grammar, the TypeScript server with the Lit plugin | Custom elements again, so it reuses the web-components renderer and only adds decorators and reactive properties. |
| `acpilot.solid` | TSX grammar, `solid-language-server` | The Solid compiler plus a bundler step, mounted with a props panel. |
| `acpilot.react` | TSX grammar, the TypeScript server with the React plugin | A bundle built in the sandbox (`esbuild-wasm`), then mounted; the props/state inspector is the same panel. |
| `acpilot.angular` | Angular language service | The Angular compiler and its runtime, which is the heaviest sandbox and the one most likely to ship as "attach to the dev server" instead of an in-editor render. |

Order matters and it is not alphabetical: each row reuses the renderer the row
above it proved. Web components need nothing, Svelte needs one compiler, and Angular
needs a build pipeline before it shows a single `<h1>`.

**The preview is a core surface, not an extension API.** A panel that loads
third-party compiler output is a place where a compromised extension could run
arbitrary code inside the app. So the panel lives in the product (the editor phase
and the desktop phase), the sandbox has no network and no filesystem, and a pack
contributes only a `preview` block: which compiler, which entry, which props schema.
The extension decides *what* to render; the product decides *where* it may run.

For the terminal there is no preview, and the TUI says so rather than showing a
blank pane: it gets the file, the grammar and the server, and the preview belongs to
the surfaces that can draw it.

## The blog extension: `acpilot.blog`

A blogging platform built the way [Front
Matter](https://frontmatter.codes/) builds one — the posts stay Markdown files with
front matter in a git repository, and the extension is the editing surface, not a
CMS:

- **Front-matter forms from a schema.** `title`, `date`, `tags`, `draft`, `slug`,
  `canonical`, `cover` — declared once in the profile, rendered as fields, written
  back as YAML without reformatting the rest of the file.
- **Content rules, not a database.** A directory and a glob (`content/blog/**/*.md`),
  a slug formed from the title with collisions reported instead of silently
  overwriting, and drafts that are visible in the list but never in the build.
- **A rendered preview** beside the editor, using the site's own layout when one is
  configured and a neutral one otherwise — the same markdown pipeline the transcript
  already uses, so a table looks identical in both places.
- **Publishing as a commit.** "Publish" writes the post and hands it to the commit
  writer from the version-control phase: a message, a diff to review, one commit. No
  push button that skips the review.
- **Images and links that survive a move.** Dropping an image writes it under
  `public/`, inserts a relative link, and adds the alt text field; moving a post
  rewrites the links to it or fails with the list of files that would break.

What it is not: no hosted CMS, no scheduled publishing daemon, no content database.
The source of truth is the repository, which is also the backup and the diff history.
That is the whole reason to build it here instead of recommending a CMS.

## What each one waits on

| Needs | Unblocks | Phase that owns it |
| --- | --- | --- |
| A `preview` contribution kind in the manifest schema | Every pack above | Mod shop phase (P4.6), after the studio work settles D18 |
| A sandboxed preview panel (process isolation, no network) | The previews | Desktop phase (P10) panels, web phase (P11) |
| A form surface generated from a schema | Blog front matter, component props | Editor phase (P6) |
| The commit writer | Blog publish | Version-control phase (P6.5) |
| Markdown rendering | Blog preview | Already in the daemon (`pulldown-cmark`) |

Which is the honest summary: the component packs are waiting on one core panel, and
the blog is waiting mostly on things that are already planned for other reasons.

<Aside type="caution" title="What will not happen">
Importing a VS Code extension, running an arbitrary Node runtime, or letting a
preview reach the network. The first needs a code-bearing format this design refuses
([the manifest](../manifest/) says why); the second is a plugin API in disguise; the
third turns a preview of your component into a preview of someone's tracking script.
</Aside>
