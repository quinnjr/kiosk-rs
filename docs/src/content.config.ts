import { defineCollection, z } from 'astro:content';
import { glob } from 'astro/loaders';

/**
 * The docs are read from where they already live, not copied here.
 *
 * `base: '..'` points at the repository root, so `README.md`, the test matrix,
 * and the design spec are rendered from their canonical paths. That matters:
 * duplicating them into `src/content/` would mean every future edit has two
 * homes and one of them silently goes stale — which is exactly the drift a
 * documentation audit on this repo has already had to clean up once.
 *
 * None of those files carry frontmatter (they are read directly on GitHub too),
 * so every schema field is optional and the page metadata lives in `nav.ts`.
 */
const docs = defineCollection({
  // Patterns are explicit rather than `docs/**`: this Astro project lives in
  // `docs/` too, so a wildcard would try to render its own source.
  loader: glob({
    base: '..',
    pattern: [
      'README.md',
      'docs/manual-test-matrix.md',
      'docs/superpowers/specs/*.md',
    ],
    // Keep the id equal to the real path, minus the extension. The default
    // slugifies it (`README.md` becomes `readme`), which would break the
    // "edit on GitHub" links — GitHub paths are case-sensitive. URLs are
    // assigned separately in `nav.ts`, so they stay tidy regardless.
    generateId: ({ entry }) => entry.replace(/\.md$/, ''),
  }),
  schema: z.object({
    title: z.string().optional(),
    description: z.string().optional(),
  }),
});

export const collections = { docs };
