#!/usr/bin/env node
// Website-coherence gate: assert the public landing page matches the product it
// advertises. The failure this prevents is the site quietly drifting from the
// shipped binary, a wrong version in the hero, a dead dashboard port, a command
// the CLI no longer has, an oversized or missing demo video, so a new user hits
// a wall the page swore wasn't there.
//
// Truth sources, most authoritative first:
//   1. the built `localmem` binary (`--version`, `--help`)   [behavioral]
//   2. core/Cargo.toml + mcp-server/package.json             [build inputs]
//   3. core/src/config.rs DEFAULT_SERVER_ADDR                [the served port]
//   4. landing/{_redirects,CNAME}, install.sh, landing/media [deploy artifacts]
//
// Emits the same PASS/WARN/FAIL rows as `localmem doctor`, so this is one more
// surface of the single check model, not a parallel one. Exits non-zero on any
// FAIL so release CI can gate the publish on it.
//
//   node scripts/check-site-coherence.mjs           # human table
//   node scripts/check-site-coherence.mjs --json    # machine-readable
//   LOCALMEM_BIN=/path/to/localmem node scripts/...  # pin the binary to query

import { readFileSync, existsSync, statSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { dirname, join, resolve } from 'node:path';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const asJson = process.argv.includes('--json');

// Landing budget: demo videos must stay small enough to autoplay without a
// visible stall on a cold load. Keep in sync with the encode in scripts.
const MAX_VIDEO_BYTES = 5 * 1024 * 1024;

const results = [];
const add = (status, name, detail, fix) =>
  results.push({ name, status, detail, fix: fix ?? null });

const readMaybe = (p) => (existsSync(p) ? readFileSync(p, 'utf8') : null);

// Remove <style>/<script> bodies so CSS/JS numbers (z-index:1000, canvas
// coordinates) can never masquerade as a version or port claim. onclick=
// attributes ride on elements, not in <script>, so copyText() commands survive.
const stripBlocks = (html) =>
  html
    .replace(/<style[\s\S]*?<\/style>/gi, ' ')
    .replace(/<script[\s\S]*?<\/script>/gi, ' ');

// The only contexts where a CLI command is a *claim* and not prose: <code>,
// the hero/step command spans, and the copyText() button payloads.
function codeText(html) {
  const chunks = [];
  for (const m of html.matchAll(/<code[^>]*>([\s\S]*?)<\/code>/gi)) chunks.push(m[1]);
  for (const m of html.matchAll(/<span class="cmd"[^>]*>([\s\S]*?)<\/span>/gi)) chunks.push(m[1]);
  for (const m of html.matchAll(/copyText\(this,\s*'([^']*)'/gi)) chunks.push(m[1]);
  return chunks.map((c) => c.replace(/<[^>]+>/g, '')).join('\n');
}

const uniq = (xs) => [...new Set(xs)];

// ---------------------------------------------------------------- truth sources
const html = readMaybe(join(ROOT, 'landing/index.html'));
if (!html) {
  add('fail', 'landing page present', `landing/index.html missing under ${ROOT}`);
  report();
  process.exit(1);
}
const body = stripBlocks(html);
const code = codeText(html);

const cargoToml = readMaybe(join(ROOT, 'core/Cargo.toml'));
const cargoVersion = cargoToml?.match(/^version\s*=\s*"([^"]+)"/m)?.[1] ?? null;

const pkgRaw = readMaybe(join(ROOT, 'mcp-server/package.json'));
let pkg = null;
try {
  pkg = pkgRaw ? JSON.parse(pkgRaw) : null;
} catch {
  /* surfaced by the version-sources check below */
}

const configRs = readMaybe(join(ROOT, 'core/src/config.rs'));
const configPort =
  configRs?.match(/DEFAULT_SERVER_ADDR\s*:\s*&str\s*=\s*"[^"]*:(\d+)"/)?.[1] ?? null;

// Built binary: env override, then this repo's target dirs, then PATH.
function findBinary() {
  if (process.env.LOCALMEM_BIN && existsSync(process.env.LOCALMEM_BIN)) {
    return process.env.LOCALMEM_BIN;
  }
  for (const rel of ['core/target/release/localmem', 'core/target/debug/localmem']) {
    const p = join(ROOT, rel);
    if (existsSync(p)) return p;
  }
  try {
    const p = execFileSync('which', ['localmem'], { encoding: 'utf8' }).trim();
    return p || null;
  } catch {
    return null;
  }
}
const bin = findBinary();
let binVersion = null;
let subcommands = null; // null => could not determine behaviorally
if (bin) {
  try {
    binVersion = execFileSync(bin, ['--version'], { encoding: 'utf8' })
      .trim()
      .match(/(\d+\.\d+\.\d+)/)?.[1];
  } catch {
    /* surfaced below */
  }
  try {
    const help = execFileSync(bin, ['--help'], { encoding: 'utf8' });
    const cmds = help.slice(
      help.indexOf('Commands:'),
      help.indexOf('Options:') === -1 ? undefined : help.indexOf('Options:'),
    );
    subcommands = new Set(
      [...cmds.matchAll(/^\s{2,}([a-z][a-z-]+)\b/gm)].map((m) => m[1]).filter((c) => c !== 'help'),
    );
  } catch {
    /* surfaced below */
  }
}

const canonicalVersion = cargoVersion ?? binVersion ?? pkg?.version ?? null;

// ---------------------------------------------------------------------- checks

// 1. The version sources the binary is built from must agree with each other.
{
  const parts = [
    ['core/Cargo.toml', cargoVersion],
    ['mcp-server/package.json', pkg?.version ?? null],
    ['binary --version', binVersion],
  ].filter(([, v]) => v != null);
  const seen = uniq(parts.map(([, v]) => v));
  if (!canonicalVersion) {
    add('fail', 'version sources', 'no version found in Cargo.toml, package.json, or the binary');
  } else if (seen.length === 1) {
    add(
      'pass',
      'version sources agree',
      `${canonicalVersion} (${parts.map(([s]) => s.replace(/.*\//, '')).join(', ')})`,
    );
  } else {
    add(
      'fail',
      'version sources agree',
      parts.map(([s, v]) => `${s}=${v}`).join(' / '),
      'bump every source to the same version before release',
    );
  }
}

// 2. Every semver printed on the page must be the canonical version. The
//    negative lookarounds exclude IP octets (127.0.0.1) and other dotted runs.
{
  const found = uniq([...body.matchAll(/(?<![\d.])v?(\d+\.\d+\.\d+)(?![\d.])/g)].map((m) => m[1]));
  const wrong = found.filter((v) => v !== canonicalVersion);
  if (!canonicalVersion) {
    add('warn', 'site version', 'no canonical version to compare against');
  } else if (found.length === 0) {
    add('warn', 'site version', 'page prints no version string');
  } else if (wrong.length === 0) {
    add('pass', 'site version', `all ${found.length} version mentions are ${canonicalVersion}`);
  } else {
    add(
      'fail',
      'site version',
      `page advertises ${wrong.join(', ')} but the product is ${canonicalVersion}`,
      `update landing/index.html version strings to ${canonicalVersion}`,
    );
  }
}

// 3. Every dashboard port the page mentions must be the served default.
{
  const ports = uniq([
    ...[...body.matchAll(/127\.0\.0\.1:(\d+)/g)].map((m) => m[1]),
    ...[...body.matchAll(/(?<![\d.]):(\d{4,5})\b/g)].map((m) => m[1]),
  ]);
  const wrong = ports.filter((p) => p !== configPort);
  if (!configPort) {
    add('warn', 'dashboard port', 'could not read DEFAULT_SERVER_ADDR from core/src/config.rs');
  } else if (ports.length === 0) {
    add('warn', 'dashboard port', 'page mentions no port');
  } else if (wrong.length === 0) {
    add('pass', 'dashboard port', `all port mentions are :${configPort}`);
  } else {
    add(
      'fail',
      'dashboard port',
      `page mentions :${wrong.join(', :')} but the core serves :${configPort}`,
      `update the port on the page to :${configPort}`,
    );
  }
}

// 4. The npm package the page tells people to install must be the one we ship.
{
  const refs = uniq([
    ...[...html.matchAll(/npmjs\.com\/package\/([a-z0-9@/_-]+)/gi)].map((m) => m[1]),
    ...[...html.matchAll(/npm\s+install\s+-g\s+([a-z0-9@/_-]+)/gi)].map((m) => m[1]),
  ]);
  const name = pkg?.name ?? null;
  const wrong = refs.filter((r) => r !== name);
  if (!name) {
    add('warn', 'npm package', 'no name in mcp-server/package.json');
  } else if (refs.length === 0) {
    add('warn', 'npm package', 'page references no npm package');
  } else if (wrong.length === 0) {
    add('pass', 'npm package', `page points at ${name}`);
  } else {
    add(
      'fail',
      'npm package',
      `page references ${wrong.join(', ')} but the package is ${name}`,
      `fix the npm package name on the page to ${name}`,
    );
  }
}

// 5. Every `localmem <sub>` shown in a command context must be a real subcommand.
{
  const claimed = uniq([...code.matchAll(/\blocalmem\s+([a-z][a-z-]+)/g)].map((m) => m[1]));
  if (!subcommands) {
    add(
      'warn',
      'CLI commands',
      bin
        ? `could not parse \`${bin} --help\`; commands unverified: ${claimed.join(', ')}`
        : `no built binary found; commands unverified: ${claimed.join(', ')}`,
      'build the core (cargo build) so commands can be checked behaviorally',
    );
  } else {
    const bad = claimed.filter((c) => !subcommands.has(c));
    if (bad.length === 0) {
      add('pass', 'CLI commands', `all ${claimed.length} commands exist in the binary`);
    } else {
      add(
        'fail',
        'CLI commands',
        `page shows \`localmem ${bad.join('`, `localmem ')}\` which the binary does not have`,
        'remove or rename the stale command(s) on the page',
      );
    }
  }
}

// 6. The one-liner install must resolve to this site and have a redirect behind it.
{
  const cname = readMaybe(join(ROOT, 'landing/CNAME'))?.trim() ?? null;
  const redirects = readMaybe(join(ROOT, 'landing/_redirects')) ?? '';
  const installHosts = uniq(
    [...code.matchAll(/https?:\/\/([^/'"\s]+)\/install\b/g)].map((m) => m[1]),
  );
  const problems = [];
  if (cname && installHosts.length && installHosts.some((h) => h !== cname)) {
    problems.push(`install URL host ${installHosts.join(', ')} != CNAME ${cname}`);
  }
  if (!/^\/install\s+\S+install\.sh/m.test(redirects)) {
    problems.push('landing/_redirects has no /install -> install.sh rule');
  }
  if (!existsSync(join(ROOT, 'install.sh'))) {
    problems.push('install.sh missing at repo root (the asset the redirect serves)');
  }
  if (problems.length === 0) {
    add('pass', 'install one-liner', `curl https://${cname}/install -> install.sh redirect wired`);
  } else {
    add('fail', 'install one-liner', problems.join('; '), 'reconcile the install URL, _redirects, and install.sh');
  }
}

// 7. Every media asset the page embeds must exist and stay within the budget.
{
  const refs = uniq([
    ...[...html.matchAll(/(?:src|poster)="(media\/[^"]+)"/g)].map((m) => m[1]),
  ]);
  const problems = [];
  for (const rel of refs) {
    const p = join(ROOT, 'landing', rel);
    if (!existsSync(p)) {
      problems.push(`${rel} referenced but missing`);
      continue;
    }
    if (rel.endsWith('.mp4')) {
      const sz = statSync(p).size;
      if (sz === 0) problems.push(`${rel} is empty`);
      else if (sz > MAX_VIDEO_BYTES)
        problems.push(`${rel} is ${(sz / 1048576).toFixed(1)}MB (> ${MAX_VIDEO_BYTES / 1048576}MB budget)`);
    }
  }
  if (refs.length === 0) {
    add('warn', 'media assets', 'page embeds no media');
  } else if (problems.length === 0) {
    add('pass', 'media assets', `${refs.length} assets present and within budget`);
  } else {
    add('fail', 'media assets', problems.join('; '), 're-encode/restore the offending media files');
  }
}

// 8. A stat repeated across the page must be the same number everywhere (catches
//    a half-finished copy edit that leaves 75% in one place and 76% in another).
{
  const lme = uniq([...body.matchAll(/(\d{1,3})%\s*(?:answered correctly|on\s+)?[^.]*?LongMemEval/gi)].map((m) => m[1]));
  const lme2 = uniq([...body.matchAll(/(\d{1,3})%\s+LongMemEval/gi)].map((m) => m[1]));
  const allLme = uniq([...lme, ...lme2]);
  if (allLme.length <= 1) {
    add('pass', 'stat consistency', allLme.length ? `LongMemEval cited as ${allLme[0]}% throughout` : 'no repeated stat to cross-check');
  } else {
    add(
      'fail',
      'stat consistency',
      `LongMemEval score given inconsistently: ${allLme.join('%, ')}%`,
      'make every LongMemEval mention use the same number',
    );
  }
}

// ----------------------------------------------------------------- output + exit
function report() {
  if (asJson) {
    const ok = !results.some((r) => r.status === 'fail');
    process.stdout.write(JSON.stringify({ ok, checks: results }, null, 2) + '\n');
    return;
  }
  const glyph = { pass: 'PASS', warn: 'WARN', fail: 'FAIL' };
  console.log('level  check                  detail');
  for (const r of results) {
    console.log(`${glyph[r.status].padEnd(6)} ${r.name.padEnd(22)} ${r.detail}`);
    if (r.fix) console.log(`       fix: ${r.fix}`);
  }
  const n = (s) => results.filter((r) => r.status === s).length;
  console.log(`\nsummary: ${n('pass')} PASS / ${n('warn')} WARN / ${n('fail')} FAIL`);
  console.log(`sources: version=${canonicalVersion ?? '?'} port=${configPort ?? '?'} binary=${bin ?? 'none'}`);
}

report();
process.exit(results.some((r) => r.status === 'fail') ? 1 : 0);
