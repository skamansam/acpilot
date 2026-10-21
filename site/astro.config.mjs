import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';
import mermaid from 'astro-mermaid';

// Project page on GitHub Pages, served under the account's custom domain:
// https://rudeboy.dev/acpilot/ (skamansam.github.io redirects there).
export default defineConfig({
  site: 'https://rudeboy.dev',
  base: '/acpilot',
  integrations: [
    // Must be registered before Starlight: the integration installs the markdown
    // transform that turns ```mermaid fences into diagram containers.
    mermaid({
      autoTheme: true, // follows Starlight's data-theme toggle, no extra plumbing
      enableLog: false,
      mermaidConfig: {
        fontFamily: 'ui-sans-serif, system-ui, -apple-system, "Segoe UI", Roboto, sans-serif',
        flowchart: { curve: 'basis', padding: 12, nodeSpacing: 40, rankSpacing: 48 },
        sequence: { actorMargin: 44, messageMargin: 34, useMaxWidth: true },
      },
    }),
    starlight({
      title: 'Acpilot',
      description:
        'A code editor that flies your agents. You keep the controls — which is why the permission prompts exist.',
      logo: { src: './src/assets/logo.svg', alt: 'Acpilot: an aircraft inside an instrument gauge' },
      favicon: '/favicon.svg',
      customCss: ['./src/styles/custom.css'],
      social: [
        { icon: 'github', label: 'GitHub', href: 'https://github.com/skamansam/acpilot' },
      ],
      editLink: {
        baseUrl: 'https://github.com/skamansam/acpilot/edit/main/site/',
      },
      lastUpdated: true,
      pagination: true,
      sidebar: [
        {
          label: 'Ground school',
          items: [
            { label: 'Flight status', link: '/status/' },
            { label: 'Installation', link: '/ground-school/installation/' },
            { label: 'First flight', link: '/ground-school/first-flight/' },
          ],
        },
        {
          label: 'Flight deck',
          items: [
            { label: 'CLI', link: '/flight-deck/cli/' },
            { label: 'TUI', link: '/flight-deck/tui/' },
            { label: 'Desktop', link: '/flight-deck/desktop/' },
            { label: 'Web', link: '/flight-deck/web/' },
            { label: 'HTTP API', link: '/flight-deck/api/' },
          ],
        },
        {
          label: 'Systems',
          items: [
            { label: 'Agents and ACP', link: '/systems/agents/' },
            { label: 'Programs as agents', link: '/systems/program-as-agent/' },
            { label: 'Profiles', link: '/systems/profiles/' },
            { label: 'Language servers', link: '/systems/language-servers/' },
            { label: 'Hosts: SSH and WSL', link: '/systems/hosts/' },
          ],
        },
        {
          label: 'Flight test',
          items: [
            { label: 'The model lab', link: '/flight-test/' },
            { label: 'Providers', link: '/flight-test/providers/' },
            { label: 'The workbench', link: '/flight-test/workbench/' },
            { label: 'Recipes', link: '/flight-test/recipes/' },
            { label: 'From model to agent', link: '/flight-test/model-to-agent/' },
            { label: 'Remote control', link: '/flight-test/remote-control/' },
          ],
        },
        {
          label: 'Mod shop',
          items: [
            { label: 'The mod shop', link: '/mod-shop/' },
            { label: 'The manifest', link: '/mod-shop/manifest/' },
            { label: 'Installing and approving', link: '/mod-shop/registry/' },
            { label: 'The marketplace', link: '/mod-shop/marketplace/' },
            { label: 'Themes', link: '/mod-shop/themes/' },
          ],
        },
        {
          label: 'Safety systems',
          items: [
            { label: 'Permissions', link: '/safety/permissions/' },
            { label: 'Secrets', link: '/safety/secrets/' },
          ],
        },
        {
          label: 'Charts',
          items: [
            { label: 'Configuration', link: '/reference/config/' },
            { label: 'Agent manifest', link: '/reference/manifest/' },
          ],
        },
        {
          label: 'In case of emergency',
          items: [{ label: 'FAQ', link: '/faq/' }],
        },
      ],
    }),
  ],
});
