# QuickSearch site

The marketing site for QuickSearch: an [Astro](https://astro.build) static
site that ports the app's own visual language ("The Concordance" — paper,
ink, one rubric red) rather than a generic template.

| Command | Purpose |
| --- | --- |
| `pnpm install` | Install dependencies |
| `pnpm dev` | Local dev server with hot reload |
| `pnpm build` | Static build to `dist/` |
| `pnpm preview` | Serve the built `dist/` locally |

Screenshots are copied from `../docs/screenshots/` (regenerate there with
`pnpm screenshots` at the repo root, then re-copy). Astro's image pipeline
handles resizing and WebP conversion at build time.

## Deploying (Cloudflare Pages)

This is a subdirectory of the main repo, so point Cloudflare Pages at it
rather than the repo root:

1. Cloudflare dashboard → Workers & Pages → Create → Pages → connect the
   `etiennedelange/quicksearch` GitHub repo.
2. Build settings:
   - **Root directory:** `site`
   - **Build command:** `pnpm install && pnpm build`
   - **Build output directory:** `dist`
3. Once the project has a real `*.pages.dev` URL (or a custom domain is
   attached), update `site` in `astro.config.mjs` to match — it feeds the
   canonical URL and Open Graph image tag.

No GitHub Actions workflow is needed for this: Cloudflare Pages builds
straight from its own git integration on every push to `main`.
