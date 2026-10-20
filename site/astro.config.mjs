import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';

// Project page on GitHub Pages, served under the account's custom domain:
// https://rudeboy.dev/acpilot/ (skamansam.github.io redirects there).
export default defineConfig({
  site: 'https://rudeboy.dev',
  base: '/acpilot',
  integrations: [
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
