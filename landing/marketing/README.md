# landing/marketing/

Printable marketing assets for localmem.

After deploy these are accessible at:
- `https://localmem.org/marketing/one-pager.html`
- `https://localmem.org/marketing/flyer.html`

## What's here

### `one-pager.html`

US Letter, single-page summary of localmem. Print-styled (`@page` Letter, page-break-avoid on tables and code blocks, no page bleed). Designed to be shared as a PDF.

**Use it for:**
- Cold-DM attachment when introducing localmem
- One-page "what is this" sheet for non-technical reviewers
- Reference for a 5-minute talk
- Page 1 of a fundraiser pitch deck

**To generate a PDF:**

1. Open `one-pager.html` in any browser (Chrome / Safari / Firefox).
2. Cmd-P (macOS) or Ctrl-P (Windows/Linux).
3. **Destination:** Save as PDF.
4. **Layout:** Portrait. **Paper:** US Letter. **Margins:** Default. **Background graphics:** ON.
5. Save.

The print CSS handles the rest. The result is a single-page PDF.

### `flyer.html`

US Letter, **dark-themed**, designed for printing on color and posting on physical surfaces (lab desks, library bulletin boards, hackathon walls, dev meetup signup tables). High-contrast headline, prominent install command, QR code that links to `https://localmem.org`.

**Use it for:**
- Print on color printer, fold in half, tape to a wall
- Hackathon table marker
- Coffee-shop coworking flag

**To generate a PDF or print:**

1. Open `flyer.html` in any browser.
2. Cmd-P (macOS) or Ctrl-P (Windows/Linux).
3. **Destination:** Save as PDF, or your color printer.
4. **Layout:** Portrait. **Paper:** US Letter. **Margins:** None / Minimum.
5. **Background graphics:** ON (so the dark background actually prints).
6. Save / Print.

If your printer is monochrome, the flyer still reads well but loses the accent green.

### QR code dependency

The flyer's QR code is rendered via `api.qrserver.com` — an external service. The QR encodes `https://localmem.org`. Whoever scans it lands on the homepage.

If you'd rather not depend on a third-party service for the printed asset, swap the `<img class="qr">` src for a locally-generated QR. Easiest: use any QR generator (e.g. `qrencode` CLI on macOS via Homebrew) to render an SVG and inline it. Then the printed PDF is self-contained.

```bash
# Optional: generate a local QR PNG if you don't want the api.qrserver.com dependency
brew install qrencode
qrencode -o localmem-qr.png -s 10 -m 1 'https://localmem.org'
# Then replace the <img src=...> in flyer.html with src="localmem-qr.png"
```

## Print color profile note

Both files use a print-safe CSS palette. If your printer's color management is aggressive (some office printers desaturate everything), the dark flyer can look muddy. Test print one before doing a batch. The light one-pager works on any printer.

## Adding more

If you make a new printable asset (poster, slide-deck PDF, conference handout), drop it here and update this README. Keep them all browser-print-to-PDF compatible (no build step, no headless Chrome).
