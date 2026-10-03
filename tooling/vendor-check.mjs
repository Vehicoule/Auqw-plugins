#!/usr/bin/env node
// Detect drift between the authoritative auqw-guest-sdk (in a sibling
// ../auqw checkout's sdk/rust) and the vendored copy this repo builds
// guests against. The vendor dir persists only while guests still build
// on it, so drift means: SDK source changed without a vendor-sdk.sh
// refresh, or the crate version moved and a new vendor dir is due.
//
//   node tooling/vendor-check.mjs [--sdk <path>]
//   node tooling/vendor-check.mjs --check    # CI: fail on drift
//
// Compares every packaged file that has a source counterpart: crate
// members (src/**, build.rs, ...) byte-for-byte, the source Cargo.toml
// against the verbatim Cargo.toml.orig, and the LICENSE overlay against
// the SDK repo root. Generated members (normalized Cargo.toml,
// Cargo.lock, .cargo_vcs_info.json) carry no source to compare. Drift
// on the version guests build against is an error; SDK source carrying
// a NEWER version than any vendor dir is a warning (a refresh is due
// but guests still build) — --check fails on both.
import { existsSync, readFileSync, readdirSync, statSync } from 'node:fs';
import { dirname, join, relative, resolve } from 'node:path';
import { execFileSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const arg = (name) => {
  const i = process.argv.indexOf(name);
  return i > 0 && i + 1 < process.argv.length ? process.argv[i + 1] : undefined;
};
const sdkPath = resolve(arg('--sdk') ?? join(ROOT, '..', 'auqw', 'sdk', 'rust'));
const checkOnly = process.argv.includes('--check');

const fail = (msg) => {
  console.error(`vendor-check: ${msg}`);
  process.exit(1);
};

if (!existsSync(sdkPath)) {
  if (checkOnly) fail(`SDK source not found at ${sdkPath} — pass --sdk <path>`);
  console.log('vendor-check: no sibling SDK checkout — nothing to compare');
  process.exit(0);
}

const readVersion = (manifestPath) => {
  const m = readFileSync(manifestPath, 'utf8');
  const name = m.match(/^name = "(.*)"$/m)?.[1];
  const version = m.match(/^version = "(.*)"$/m)?.[1];
  return { name, version };
};

const { name: sdkName, version: sdkVersion } = readVersion(join(sdkPath, 'Cargo.toml'));
if (sdkName !== 'auqw-guest-sdk') fail(`expected auqw-guest-sdk at ${sdkPath}, found ${sdkName}`);

// cargo package builds the package file list from the git index —
// untracked working-tree files never ship. The vendor side keeps
// --others so stray files in vendor/ still surface as drift.
const listFiles = (dir, { untracked = true } = {}) =>
  execFileSync('git', ['-C', dir, 'ls-files', '--cached', ...(untracked ? ['--others', '--exclude-standard'] : [])])
    .toString()
    .split('\n')
    .filter((f) => f && !f.endsWith('.crate'));

const cmpSemver = (a, b) => {
  const [as, bs] = [a, b].map((v) => v.split('.').map(Number));
  for (let i = 0; i < 3; i += 1) if (as[i] !== bs[i]) return as[i] - bs[i];
  return 0;
};

const vendorDirs = readdirSync(join(ROOT, 'vendor'))
  .filter((d) => d.startsWith('auqw-guest-sdk-'))
  .map((d) => ({ dir: join(ROOT, 'vendor', d), version: d.replace('auqw-guest-sdk-', '') }));
if (vendorDirs.length === 0) fail('no vendor/auqw-guest-sdk-<version> dir found');

let worst = 0; // 0 ok, 1 warn, 2 error
const report = (level, msg) => {
  worst = Math.max(worst, level);
  console.log(`${level === 0 ? 'ok' : level === 1 ? 'warn' : 'DRIFT'} — ${msg}`);
};

const matching = vendorDirs.filter((v) => v.version === sdkVersion);
if (matching.length === 0) {
  const newest = vendorDirs.reduce((a, b) => (cmpSemver(a.version, b.version) > 0 ? a : b));
  const cmp = cmpSemver(sdkVersion, newest.version);
  report(cmp > 0 ? 1 : 2, `SDK source is ${sdkVersion}; newest vendor dir is ${newest.version} — run tooling/vendor-sdk.sh`);
} else {
  const vendor = matching[0];
  const sdkTracked = new Set(listFiles(sdkPath, { untracked: false }));
  const vendorFiles = listFiles(vendor.dir);
  // Package members generated at vendor time — the normalized
  // Cargo.toml, generated Cargo.lock, and .cargo_vcs_info.json — have
  // no raw source counterpart. Everything else must match its SDK
  // source: Cargo.toml.orig is the verbatim source manifest, LICENSE
  // is overlaid from the SDK repo root (../.. from sdk/rust).
  const GENERATED = new Set(['Cargo.toml', 'Cargo.lock', '.cargo_vcs_info.json']);
  const counterpart = (f) => {
    if (f === 'Cargo.toml.orig') return 'Cargo.toml';
    if (f === 'LICENSE') return join('..', '..', 'LICENSE');
    return f;
  };
  const comparable = vendorFiles.filter((f) => !GENERATED.has(f));
  if (comparable.length === 0) fail(`no comparable source files under ${vendor.dir}`);
  let clean = 0;
  for (const f of comparable) {
    const sdkRel = counterpart(f);
    const sdkFile = join(sdkPath, sdkRel);
    // Files under the crate root must also be tracked — an untracked
    // counterpart cannot reproduce the vendored member. The LICENSE
    // overlay is copied from the working tree, so existence is enough.
    const packaged = f === 'LICENSE' ? existsSync(sdkFile) : sdkTracked.has(sdkRel) && existsSync(sdkFile);
    if (!packaged) {
      report(2, `${f} has no packaged SDK counterpart (${sdkRel})`);
      continue;
    }
    if (readFileSync(sdkFile).equals(readFileSync(join(vendor.dir, f)))) {
      clean += 1;
      continue;
    }
    report(2, `${f} differs between SDK source and vendor/auqw-guest-sdk-${vendor.version}`);
  }
  // Reverse direction: a tracked SDK file the package would carry but
  // the vendor dir lacks. The source Cargo.toml lands as Cargo.toml.orig.
  const newInSdk = [...sdkTracked].filter((f) => !vendorFiles.includes(f === 'Cargo.toml' ? 'Cargo.toml.orig' : f));
  for (const f of newInSdk) {
    report(2, `${f} exists in SDK source but not in vendor/auqw-guest-sdk-${vendor.version}`);
  }
  if (clean === comparable.length && newInSdk.length === 0) {
    report(0, `vendor ${vendor.version} matches SDK source ${sdkVersion} (${clean} files)`);
  }
}

process.exit(checkOnly && worst > 0 ? 1 : 0);
