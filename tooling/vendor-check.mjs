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

// --- TOML-lite ---------------------------------------------------------------
// The vendored normalized Cargo.toml can never byte-match
// Cargo.toml.orig — cargo rewrites it for maximal compatibility — but
// the sections that drive guest dependency resolution are rewritten
// mechanically, so they can still be compared semantically. This is a
// deliberately small parser for the manifest grammar those sections
// use: [a.b] headers plus `key = value` entries with string/bool/
// number/array/inline-table values (arrays and inline tables may span
// lines). Anything outside that grammar survives as its raw text,
// which still compares correctly.

// Strip a TOML comment (# ...) sitting outside strings.
const stripComment = (line) => {
  let str = null;
  let out = '';
  for (let i = 0; i < line.length; i += 1) {
    const c = line[i];
    if (str !== null) {
      out += c;
      if (str === '"' && c === '\\') {
        i += 1;
        out += line[i] ?? '';
      } else if (c === str) str = null;
    } else if (c === '"' || c === "'") {
      str = c;
      out += c;
    } else if (c === '#') {
      break;
    } else {
      out += c;
    }
  }
  return out;
};

// Split a TOML document into tables: name -> [{key, value}] in order.
const tomlTables = (text) => {
  const bracketDepth = (s) => {
    let d = 0;
    let str = null;
    for (let i = 0; i < s.length; i += 1) {
      const c = s[i];
      if (str !== null) {
        if (str === '"' && c === '\\') i += 1;
        else if (c === str) str = null;
      } else if (c === '"' || c === "'") {
        str = c;
      } else if (c === '{' || c === '[') {
        d += 1;
      } else if (c === '}' || c === ']') {
        d -= 1;
      }
    }
    return d;
  };
  const tables = new Map();
  const lines = text.split('\n').map(stripComment);
  let section = '';
  for (let i = 0; i < lines.length; i += 1) {
    const line = lines[i].trim();
    if (!line) continue;
    const header = line.match(/^\[\[?([^\][]+)\]?\]$/);
    if (header) {
      section = header[1].trim();
      continue;
    }
    // An entry's value may span lines while brackets stay open.
    let stmt = line;
    while (bracketDepth(stmt) > 0 && i + 1 < lines.length) {
      i += 1;
      stmt += ` ${lines[i].trim()}`;
    }
    const m = stmt.match(/^([^=]+?)\s*=\s*(.+)$/s);
    if (!m) continue;
    if (!tables.has(section)) tables.set(section, []);
    tables.get(section).push({ key: m[1].trim(), value: m[2].trim() });
  }
  return tables;
};

// Split on a separator that sits outside strings and nested
// brackets/braces — top-level commas in arrays/inline tables, and the
// dots in dotted keys.
const splitTop = (s, sep) => {
  const parts = [];
  let depth = 0;
  let str = null;
  let start = 0;
  for (let i = 0; i < s.length; i += 1) {
    const c = s[i];
    if (str !== null) {
      if (str === '"' && c === '\\') i += 1;
      else if (c === str) str = null;
      continue;
    }
    if (c === '"' || c === "'") { str = c; continue; }
    if (c === '{' || c === '[') depth += 1;
    else if (c === '}' || c === ']') depth -= 1;
    else if (c === sep && depth === 0) {
      parts.push(s.slice(start, i));
      start = i + 1;
    }
  }
  parts.push(s.slice(start));
  return parts;
};

const tomlKey = (k) => {
  const t = k.trim();
  if (t.length >= 2 && ((t.startsWith('"') && t.endsWith('"')) || (t.startsWith("'") && t.endsWith("'")))) {
    return t.slice(1, -1);
  }
  return t;
};

const keyParts = (key) => splitTop(key, '.').map(tomlKey);

const parseTomlValue = (text) => {
  const s = text.trim();
  if (s.startsWith('{') && s.endsWith('}')) {
    const obj = {};
    const inner = s.slice(1, -1).trim();
    if (inner) {
      for (const part of splitTop(inner, ',')) {
        const m = part.match(/^\s*([^=]+?)\s*=\s*(.*)$/s);
        if (m) obj[tomlKey(m[1])] = parseTomlValue(m[2]);
      }
    }
    return obj;
  }
  if (s.startsWith('[') && s.endsWith(']')) {
    const inner = s.slice(1, -1).trim();
    return inner ? splitTop(inner, ',').map(parseTomlValue) : [];
  }
  if (s.startsWith('"') && s.endsWith('"') && s.length >= 2) {
    try {
      return JSON.parse(s);
    } catch {
      return s.slice(1, -1);
    }
  }
  if (s.startsWith("'") && s.endsWith("'") && s.length >= 2) return s.slice(1, -1);
  if (s === 'true' || s === 'false') return s === 'true';
  const n = Number(s.replaceAll('_', ''));
  return Number.isNaN(n) ? s : n;
};

// Fold a table's `key = value` entries into an object; dotted keys nest.
const foldEntries = (entries) => {
  const out = {};
  for (const { key, value } of entries) {
    const parts = keyParts(key);
    let o = out;
    for (const p of parts.slice(0, -1)) {
      if (typeof o[p] !== 'object' || o[p] === null || Array.isArray(o[p])) o[p] = {};
      o = o[p];
    }
    o[parts[parts.length - 1]] = parseTomlValue(value);
  }
  return out;
};

// Dependency specs for one table kind (dependencies, dev-dependencies,
// build-dependencies), whichever spelling the manifest uses: source
// manifests write `[kind] name = "req"` or `name = { ... }`, cargo's
// normalized rewrite emits `[kind.name]` tables, and a hand-edit could
// use either. Returns name -> spec object.
const depSpecs = (tables, kind) => {
  const specs = {};
  const merge = (name, spec) => {
    specs[name] = { ...(specs[name] ?? {}), ...spec };
  };
  for (const { key, value } of tables.get(kind) ?? []) {
    const [name, ...rest] = keyParts(key);
    if (rest.length === 0) {
      const v = parseTomlValue(value);
      merge(name, v !== null && typeof v === 'object' && !Array.isArray(v) ? v : { version: v });
    } else {
      // `name.field = v` — fold the field path under the dep name.
      const spec = {};
      let o = spec;
      for (const p of rest.slice(0, -1)) {
        o[p] = {};
        o = o[p];
      }
      o[rest.at(-1)] = parseTomlValue(value);
      merge(name, spec);
    }
  }
  for (const [section, entries] of tables) {
    if (section.startsWith(`${kind}.`)) {
      merge(tomlKey(section.slice(kind.length + 1)), foldEntries(entries));
    }
  }
  return specs;
};

// `prefix` plus `prefix.*` tables folded into Map<suffix, object> —
// used for [features] and [lints] ([lints.clippy], [lints.rust], ...).
const tablesUnder = (tables, prefix) => {
  const m = new Map();
  for (const [section, entries] of tables) {
    if (section === prefix) m.set('', foldEntries(entries));
    else if (section.startsWith(`${prefix}.`)) m.set(section.slice(prefix.length + 1), foldEntries(entries));
  }
  return m;
};

// Fields cargo strips when it rewrites a dep spec for packaging: a
// path dep is pinned to its declared version, workspace-inherited
// fields resolve to concrete values, and VCS sources cannot ship to a
// registry. Everything else — version, package rename, registry,
// features, optional, default-features — survives verbatim and must
// match the vendored spec.
const PACKAGED_DROPS = new Set(['path', 'workspace', 'git', 'branch', 'tag', 'rev']);
const packagedSpec = (spec) =>
  Object.fromEntries(Object.entries(spec).filter(([k]) => !PACKAGED_DROPS.has(k)));

const stable = (v) =>
  Array.isArray(v)
    ? v.map(stable)
    : v !== null && typeof v === 'object'
      ? Object.fromEntries(Object.keys(v).sort().map((k) => [k, stable(v[k])]))
      : v;
const canonical = (v) => JSON.stringify(stable(v));

// Semantically compare the resolution-relevant sections of the source
// manifest (Cargo.toml.orig's contents) against the vendored
// normalized Cargo.toml. Returns a list of drift descriptions.
const manifestDrift = (sourceTables, vendoredTables) => {
  const drifts = [];
  for (const kind of ['dependencies', 'dev-dependencies', 'build-dependencies']) {
    const src = depSpecs(sourceTables, kind);
    const vnd = depSpecs(vendoredTables, kind);
    const names = new Set([...Object.keys(src), ...Object.keys(vnd)]);
    for (const name of [...names].sort()) {
      if (src[name] === undefined) {
        drifts.push(`[${kind}] ${name}: not in Cargo.toml.orig`);
      } else if (vnd[name] === undefined) {
        drifts.push(`[${kind}] ${name}: missing from vendored Cargo.toml`);
      } else {
        const want = canonical(packagedSpec(src[name]));
        const got = canonical(vnd[name]);
        if (want !== got) {
          drifts.push(`[${kind}] ${name}: vendored spec ${got} vs orig ${want}`);
        }
      }
    }
  }
  for (const table of ['features', 'lints']) {
    const src = tablesUnder(sourceTables, table);
    const vnd = tablesUnder(vendoredTables, table);
    const keys = new Set([...src.keys(), ...vnd.keys()]);
    for (const k of [...keys].sort()) {
      const label = k === '' ? `[${table}]` : `[${table}.${k}]`;
      if (!src.has(k)) {
        drifts.push(`${label}: not in Cargo.toml.orig`);
      } else if (!vnd.has(k)) {
        drifts.push(`${label}: missing from vendored Cargo.toml`);
      } else if (canonical(src.get(k)) !== canonical(vnd.get(k))) {
        drifts.push(`${label}: drift vs Cargo.toml.orig`);
      }
    }
  }
  return drifts;
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
  // no raw source counterpart to byte-compare (the normalized
  // Cargo.toml's resolution-relevant sections are parse-compared
  // against Cargo.toml.orig below). Everything else must match its SDK
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
  // The normalized Cargo.toml is exempt from the byte compare above,
  // but its dep specs, features, and lints are mechanical rewrites of
  // Cargo.toml.orig's — compare those sections semantically so a
  // hand-edited dep spec inside vendor/ still trips the drift gate.
  let manifestDrifts;
  if (!existsSync(join(vendor.dir, 'Cargo.toml'))) {
    manifestDrifts = ['normalized Cargo.toml missing from vendor dir'];
  } else if (existsSync(join(vendor.dir, 'Cargo.toml.orig'))) {
    manifestDrifts = manifestDrift(
      tomlTables(readFileSync(join(vendor.dir, 'Cargo.toml.orig'), 'utf8')),
      tomlTables(readFileSync(join(vendor.dir, 'Cargo.toml'), 'utf8')),
    );
  } else {
    // A missing Cargo.toml.orig is already reported as file drift.
    manifestDrifts = [];
  }
  for (const d of manifestDrifts) report(2, `Cargo.toml ${d}`);
  if (clean === comparable.length && newInSdk.length === 0 && manifestDrifts.length === 0) {
    report(0, `vendor ${vendor.version} matches SDK source ${sdkVersion} (${clean} files)`);
  }
}

process.exit(checkOnly && worst > 0 ? 1 : 0);
