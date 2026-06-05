# landing/marketing/

Printable summary of localmem, deployed alongside the localmem.org website.

After deploy, accessible at: `https://localmem.org/marketing/one-pager.html`.

## `one-pager.html`

US Letter, single-page summary. Print-styled (`@page` Letter, page-break-avoid on tables and code blocks, no page bleed). Designed to be shared as a PDF.

**Use it for:**
- Cold-DM attachment when introducing localmem
- One-page "what is this" sheet for non-technical reviewers
- Reference for a 5-minute talk
- Page 1 of any decks where you want a self-contained explainer

**To generate a PDF:**

1. Open `one-pager.html` in any browser (Chrome / Safari / Firefox).
2. Cmd-P (macOS) or Ctrl-P (Windows/Linux).
3. **Destination:** Save as PDF.
4. **Layout:** Portrait. **Paper:** US Letter. **Margins:** Default. **Background graphics:** ON.
5. Save.

The print CSS handles the rest. Result is a clean single-page PDF.

## Note on the flyer

A printable flyer (dark-themed, bulletin-board style, with QR code) is kept in the **private repo** at `docs/marketing/flyer.html` rather than here. The flyer is intended for physical printing (lab desks, library boards, hackathon tables), not for download via the website, so it has no reason to live in the public marketing surface.
