# Project website

The static project website lives in `docs/index.html` (English) and
`docs/de/index.html` (German), with shared CSS and theme logic in `docs/assets/`.
It follows the project-page structure of Werk1112 and the neutral/teal visual
language of philipbodenbach.de. The existing Markdown documentation remains
linked on GitHub.

Public paths:

- `https://philipbodenbach.github.io/werkWorker/`
- `https://philipbodenbach.github.io/werkWorker/de/`
- `https://philipbodenbach.github.io/werkWorker/experiments/tetris/`

Repository-name casing is intentional. Asset URLs are relative, so English,
German and experiment pages work under the `/werkWorker/` project path.

## GitHub Pages

The repository's Pages source was configured as **GitHub Actions** on 2026-10-26;
HTTPS is enforced. This prepares hosting without publishing the local changes.
The `Project website`
workflow in `.github/workflows/pages.yml` uploads only `docs/`. It deploys on
changes to `docs/**` on `main`, or a manual workflow run on `main`. Write permissions
are scoped to that deployment job; no personal token is required. Existing branch
and release protections remain in effect.

The workflow does not publish the repository root, local runtime directories,
secrets or the website repository's social-media exports. `.nojekyll` preserves
the experiment as plain HTML. No build or dependency installation is necessary.

For local inspection, run a static server with `docs` as its root. When verifying
deployment paths, mount it as `/werkWorker/` beneath a temporary server root.
The Lab report and homepage teasers live in the `philipbodenbach.de` repository.
Publish the product site before the corresponding homepage links go live.

## Preparation checks (2026-10-26)

- English and German product pages checked in Chromium at 390px and 1280px,
  with both light and dark themes, alongside the homepage and Lab (32 page,
  viewport and theme combinations in total).
- Relative product links and assets resolve beneath `/werkWorker/`.
- Theme preference survives page reload and language switching.
- Original Tetris starts, records points and pauses in the browser; SHA-256
  matches the supplied source. This is a smoke check, not full gameplay testing.
- Website build and SEO audit pass for all 10 indexable DE/EN pages.
- Web video plays in Chromium; full video decoding and Faststart were checked.
- No commits, pushes, pull requests or releases were created during preparation.
