#!/usr/bin/env node
// Regenerate the OTA indexes the app fetches at runtime. Scans
// releases/<id>/<version>/ for signed release dirs and emits two
// files:
//
//   releases/feed.json    — legacy single-line index: ONE entry per
//                           plugin, the latest release on the 0.1.0
//                           abi line. Released builds reject
//                           duplicate ids wholesale, so a multi-line
//                           index here would freeze them at
//                           last-known-good — and an abi-only upgrade
//                           here would sweep their installed pair.
//   releases/feed-v2.json — multi-line index: the newest release per
//                           (id, abi). New builds read this one and
//                           pick the newest line they serve.
//
//   { "keyId": "<16-hex>",
//     "plugins": [ { "id", "version", "abi",
//                    "wasm_sha256", "manifest_sha256", "signature" } ] }
//
// Artifact URLs stay derivable — the app resolves
// `releases/{id}/{version}/{id}-{version}.wasm` and `plugin.manifest.json`
// relative to the feed URL, so the feed itself carries no paths.
// The per-artifact signature + sha fields authenticate everything the
// app downloads; the index needs no signature of its own.
//
//   tooling/feed.mjs            write both feed files
//   tooling/feed.mjs --check    verify both files match the dirs (CI)

import { createHash } from 'node:crypto';
import {
  existsSync,
  readFileSync,
  readdirSync,
  statSync,
  writeFileSync,
} from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const RELEASES = join(ROOT, 'releases');
const FEED = join(RELEASES, 'feed.json');
const FEED_V2 = join(RELEASES, 'feed-v2.json');

// The abi line the legacy feed serves — released builds pin 0.1.0
// and skip anything else, so feed.json only lists the newest 0.1.0
// release of each plugin (a plugin with no 0.1.0 release is absent).
const LEGACY_ABI = '0.1.0';

const fail = (msg) => {
  console.error(`feed: ${msg}`);
  process.exit(1);
};

const sha256 = (buf) => `sha256:${createHash('sha256').update(buf).digest('hex')}`;

const semverKey = (v) => v.split('.').map((n) => Number.parseInt(n, 10));

const cmpSemver = (a, b) => {
  const [as, bs] = [semverKey(a), semverKey(b)];
  for (let i = 0; i < 3; i += 1) {
    if (as[i] !== bs[i]) return as[i] - bs[i];
  }
  return 0;
};

const build = () => {
  const plugins = [];
  const pluginsV2 = [];
  let keyId;
  for (const id of readdirSync(RELEASES).sort()) {
    const dir = join(RELEASES, id);
    if (!statSync(dir, { throwIfNoEntry: false })?.isDirectory()) continue;
    const versions = readdirSync(dir).filter((v) =>
      /^\d+\.\d+\.\d+$/.test(v),
    );
    if (versions.length === 0) continue;
    // First pass: provenance per version, latest version per abi
    // line. Ascending order means a later set() wins the line.
    const byAbi = new Map();
    for (const version of versions.sort(cmpSemver)) {
      const rel = join(dir, version);
      const provenance = JSON.parse(
        readFileSync(join(rel, 'provenance.json'), 'utf8'),
      );
      if (
        provenance.plugin !== id ||
        provenance.version !== version ||
        !provenance.abi_version ||
        !provenance.wasm_sha256 ||
        !provenance.manifest_sha256 ||
        provenance.key_id === undefined
      ) {
        fail(`${id}/${version}: provenance is incomplete or disagrees with the dir`);
      }
      if (keyId === undefined) keyId = provenance.key_id;
      if (keyId !== provenance.key_id) {
        fail(`${id}/${version}: key_id ${provenance.key_id} mixes with ${keyId}`);
      }
      byAbi.set(provenance.abi_version, version);
    }
    for (const version of [...byAbi.values()].sort(cmpSemver)) {
      const rel = join(dir, version);
      const provenance = JSON.parse(
        readFileSync(join(rel, 'provenance.json'), 'utf8'),
      );
      const signature = readFileSync(join(rel, 'signature'), 'utf8').trim();
      // Re-hash the bytes on disk — the feed must never ship an
      // entry whose artifact or manifest drifted from what was
      // signed. The signature itself is still verified at install
      // time (and by `sign.mjs verify` with the key); this is the
      // keyless integrity check that catches tampering after the
      // release dir was cut.
      const wasmBuf = readFileSync(join(rel, `${id}-${version}.wasm`));
      const wasmSha = sha256(wasmBuf);
      if (wasmSha !== provenance.wasm_sha256) {
        fail(`${id}/${version}: wasm digest drift: ${wasmSha} != provenance ${provenance.wasm_sha256}`);
      }
      const manifestBuf = readFileSync(join(rel, 'plugin.manifest.json'));
      const manifestSha = sha256(manifestBuf);
      if (manifestSha !== provenance.manifest_sha256) {
        fail(`${id}/${version}: manifest digest drift: ${manifestSha} != provenance ${provenance.manifest_sha256}`);
      }
      const manifest = JSON.parse(manifestBuf.toString('utf8'));
      if (manifest.id !== id || manifest.version !== version || manifest.abi !== provenance.abi_version) {
        fail(`${id}/${version}: plugin.manifest.json disagrees with the release dir`);
      }
      if (manifest.artifact?.digest !== wasmSha) {
        fail(`${id}/${version}: manifest artifact.digest does not pin the staged wasm`);
      }
      const entry = {
        id,
        version,
        abi: provenance.abi_version,
        wasm_sha256: provenance.wasm_sha256,
        manifest_sha256: provenance.manifest_sha256,
        signature,
      };
      pluginsV2.push(entry);
      if (entry.abi === LEGACY_ABI) {
        plugins.push(entry);
      }
    }
  }
  return {
    feed: { keyId, plugins },
    feedV2: { keyId, plugins: pluginsV2 },
  };
};

const { feed, feedV2 } = build();
const out = `${JSON.stringify(feed, null, 2)}\n`;
const outV2 = `${JSON.stringify(feedV2, null, 2)}\n`;

if (process.argv.includes('--check')) {
  if (!existsSync(FEED)) fail('releases/feed.json does not exist');
  if (readFileSync(FEED, 'utf8') !== out) {
    fail('feed.json is stale — run tooling/feed.mjs');
  }
  if (!existsSync(FEED_V2)) fail('releases/feed-v2.json does not exist');
  if (readFileSync(FEED_V2, 'utf8') !== outV2) {
    fail('feed-v2.json is stale — run tooling/feed.mjs');
  }
  console.log('feed: ok — both indexes match release dirs');
} else {
  writeFileSync(FEED, out);
  writeFileSync(FEED_V2, outV2);
  console.log(
    `feed: wrote ${feed.plugins.length} legacy + ${feedV2.plugins.length} multi-line plugins, keyId ${feed.keyId}`,
  );
}
