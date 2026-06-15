# landing/

Static landing page for `localmem.org`.

## Files

- `index.html` — single-file landing page (vanilla HTML + CSS + ~15 lines of JS for the copy-to-clipboard button). Dark/light themed via `prefers-color-scheme`. No build step. No dependencies.
- `_redirects` — Cloudflare Pages / Netlify redirect rules. The important one: `/install` → the latest GitHub Release `install.sh` (so `curl -fsSL https://localmem.org/install | sh` works the same as hitting the GitHub URL directly).
- `CNAME` — used by GitHub Pages to associate the site with `localmem.org` when this directory is the Pages source.

## Deployment options (pick one)

### Option A — Cloudflare Pages (recommended)

Pros: free, edge-deployed, fast, and `_redirects` works natively so `/install` returns a real HTTP 302 (which is what `curl | sh` needs).

1. Sign in at [pages.cloudflare.com](https://pages.cloudflare.com/) → "Create a project" → "Connect to Git".
2. Authorize Cloudflare for the `VJ-yadav/localmem-community` repo.
3. Configure the build:
   - **Production branch:** `main`
   - **Build command:** *(leave blank — pure static)*
   - **Build output directory:** `landing`
4. Click "Save and Deploy". Wait for the first build (~30s).
5. In the project's "Custom domains" tab, add `localmem.org` and `www.localmem.org`. Follow the DNS instructions Cloudflare gives you (it'll set up the records automatically if the domain is managed by Cloudflare DNS, or give you specific records to add at your current registrar).

### Option B — GitHub Pages

Pros: even simpler if you don't want a third-party account. Drawback: GitHub Pages does NOT support `_redirects`-style HTTP redirects, so `https://localmem.org/install` cannot redirect to the GitHub Release `install.sh` for a `curl | sh` flow. Users would need the full GitHub Release URL.

1. Repository → Settings → Pages.
2. Source: "Deploy from a branch" → branch `main`, folder `/landing`.
3. Save. Wait ~1 minute for the first deploy.
4. Add a custom domain: enter `localmem.org`. GitHub will check DNS and issue an HTTPS cert via Let's Encrypt once the DNS records are right.
5. DNS records to add at your registrar (per [GitHub's docs](https://docs.github.com/en/pages/configuring-a-custom-domain-for-your-github-pages-site/managing-a-custom-domain-for-your-github-pages-site#configuring-an-apex-domain)):
   - `A` records for the apex `localmem.org` pointing to GitHub's Pages IPs:
     - `185.199.108.153`
     - `185.199.109.153`
     - `185.199.110.153`
     - `185.199.111.153`
   - `CNAME` for `www.localmem.org` pointing to `vj-yadav.github.io`.

## After deployment — update the install URL in the docs

Once the redirect is live, the canonical install command can become:

```bash
curl -fsSL https://localmem.org/install | sh
```

…and the docs in `README.md` / `docs/INSTALL.md` / `docs/HOW_IT_WORKS.md` can be updated to reflect that. Until then, keep the long GitHub Releases URL as the canonical install command (it's what actually works).

## Local preview

```bash
cd landing
python3 -m http.server 8080
open http://localhost:8080/
```
