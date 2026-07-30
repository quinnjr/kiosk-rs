/**
 * Sidebar order, labels, and URLs.
 *
 * The source files carry no frontmatter — they are read on GitHub as well as
 * here — so page metadata lives in one place rather than being guessed from an
 * `<h1>`.
 *
 * `id` is the content-collection id, which equals the file's path relative to
 * the repository root (see `generateId` in `content.config.ts`). `slug` is the
 * URL, kept separate so a doc buried at `docs/superpowers/specs/...` does not
 * inherit that as its address.
 */
export type NavEntry = {
  /** Collection id — the real path, so it also builds the GitHub edit link. */
  id: string;
  /** URL segment, relative to the site base. Empty string means the home page. */
  slug: string;
  label: string;
  blurb: string;
};

export const nav: NavEntry[] = [
  {
    id: 'README',
    slug: '',
    label: 'Overview',
    blurb:
      'What kiosk-rs is, how to run it, and what it deliberately does not do.',
  },
  {
    id: 'docs/manual-test-matrix',
    slug: 'testing',
    label: 'Manual test matrix',
    blurb:
      'The 36 hardware cases that gate a release, plus the known limitations to confirm rather than fix.',
  },
  {
    id: 'docs/superpowers/specs/2026-07-29-kiosk-rs-design',
    slug: 'design',
    label: 'Design record',
    blurb:
      'Architecture, the frame-pacing rules, the error-handling tiers, and the rationale behind each.',
  },
];

/** The entry that owns the home page. */
export const HOME_ID = 'README';

/** GitHub URL for the file backing a collection entry, for "edit this page". */
export function sourceUrl(id: string): string {
  return `https://github.com/quinnjr/kiosk-rs/blob/develop/${id}.md`;
}
