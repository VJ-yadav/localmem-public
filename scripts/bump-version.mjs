#!/usr/bin/env node
// One command to bump the version across every hardcoded location, so a release
// touches a single tool instead of N hand-edited files (the drift the
// website-coherence gate keeps catching). Pairs with check-site-coherence.mjs:
// this APPLIES the version, that VERIFIES the site matches the binary.
//
//   node scripts/bump-version.mjs --list     # show every version location + its current value
//   node scripts/bump-version.mjs 0.3.4      # set every location to 0.3.4
//
// Release-notes in CHANGELOG.md are intentionally NOT auto-written (a human
// writes those); the script prints a reminder instead.

import { readFileSync, writeFileSync, existsSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join, resolve } from 'node:path';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const SEMVER = /\d+\.\d+\.\d+/;

// Each location: a file, a label, and a regex whose group 2 is the version,
// with enough surrounding context (groups 1 and 3) that we never touch an
// unrelated number. `all: true` replaces every match in the file (the landing
// page mentions the version in several spots).
const LOCATIONS = [
  { file: 'core/Cargo.toml',         label: 'Rust crate',        re: /(^version = ")(\d+\.\d+\.\d+)(")/m },
  { file: 'core/Cargo.lock',         label: 'Cargo.lock',        re: /(name = "localmem"\nversion = ")(\d+\.\d+\.\d+)(")/ },
  { file: 'mcp-server/package.json', label: 'npm package',       re: /("version":\s*")(\d+\.\d+\.\d+)(")/ },
  { file: 'README.md',               label: 'README status',     re: /(\*\*Status:\*\* v)(\d+\.\d+\.\d+)()/ },
  { file: 'landing/index.html',      label: 'landing (all v-tags)', re: /(v)(\d+\.\d+\.\d+)()/g, all: true },
];

const arg = process.argv[2];
if (!arg || arg === '-h' || arg === '--help') {
  console.log('usage: node scripts/bump-version.mjs <new-version|--list>');
  process.exit(arg ? 0 : 1);
}

// --list: report current version at every location (the "find all the places" ask).
if (arg === '--list') {
  console.log('version locations:');
  for (const loc of LOCATIONS) {
    const p = join(ROOT, loc.file);
    if (!existsSync(p)) { console.log(`  ${loc.file.padEnd(26)} MISSING`); continue; }
    const txt = readFileSync(p, 'utf8');
    const listRe = new RegExp(loc.re.source, loc.re.flags.includes('g') ? loc.re.flags : loc.re.flags + 'g');
    const found = [...txt.matchAll(listRe)].map((m) => m[2]);
    const uniq = [...new Set(found)];
    console.log(`  ${loc.file.padEnd(26)} ${loc.label.padEnd(20)} ${uniq.join(', ') || '(no match)'}`);
  }
  process.exit(0);
}

const next = arg.replace(/^v/, '');
if (!/^\d+\.\d+\.\d+$/.test(next)) {
  console.error(`not a semver: ${arg}`);
  process.exit(1);
}

let changed = 0;
const report = [];
for (const loc of LOCATIONS) {
  const p = join(ROOT, loc.file);
  if (!existsSync(p)) { report.push(`SKIP  ${loc.file} (missing)`); continue; }
  const before = readFileSync(p, 'utf8');
  let n = 0;
  const after = before.replace(loc.re, (_m, p1, p2, p3) => {
    n++;
    return `${p1}${next}${p3 ?? ''}`;
  });
  if (after !== before) {
    writeFileSync(p, after);
    changed += n;
    report.push(`OK    ${loc.file.padEnd(26)} ${n} occurrence(s) -> ${next}`);
  } else {
    report.push(`--    ${loc.file.padEnd(26)} already ${next} or no match`);
  }
}

console.log(`bump to ${next}`);
for (const r of report) console.log('  ' + r);
console.log(`\n${changed} change(s) written.`);
console.log('reminder: add a CHANGELOG.md entry, then run');
console.log('  node scripts/check-site-coherence.mjs   # verify the site matches the binary');
