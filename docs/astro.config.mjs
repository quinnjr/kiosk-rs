// @ts-check
import { defineConfig } from 'astro/config';
import tailwindcss from '@tailwindcss/vite';

/** Repo name, so the project site resolves under /kiosk-rs. Used twice below. */
const BASE = '/kiosk-rs';

// Project site, so every URL is prefixed with the repo name. `site` + `base`
// together are what make relative links and the sitemap resolve correctly under
// https://quinnjr.github.io/kiosk-rs/ rather than at a domain root.
export default defineConfig({
  site: 'https://quinnjr.github.io',
  base: BASE,
  trailingSlash: 'ignore',
  // The source Markdown links between files by repo-relative path, which is
  // correct on GitHub and would 404 here. Redirect those paths to the pages that
  // render them, rather than forking the files or rewriting them at build time
  // (a rehype pass needs the legacy unified processor, which drops code blocks).
  // Targets carry BASE explicitly: Astro prefixes the redirect *source* with
  // `base` (via the emitted file path) but not the destination, so a bare '/'
  // would land on the domain root instead of the project site.
  redirects: {
    '/README.md': `${BASE}/`,
    '/docs/manual-test-matrix.md': `${BASE}/testing`,
    '/docs/superpowers/specs': `${BASE}/design`,
    '/docs/superpowers/specs/2026-07-29-kiosk-rs-design.md': `${BASE}/design`,
  },
  markdown: {
    shikiConfig: {
      // Both themes, switched by CSS, so code blocks follow the page theme
      // instead of being locked to one and looking wrong in the other.
      themes: { light: 'github-light', dark: 'github-dark' },
      wrap: false,
    },
  },
  vite: {
    plugins: [tailwindcss()],
  },
});
