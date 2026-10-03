#!/usr/bin/env node
// Regenerate releases/feed.json — the OTA index the app fetches at
// runtime. Scans releases/<id>/<version>/ for signed release dirs and
// emits one entry per plugin at its HIGHEST semver:
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
//   tooling/feed.mjs            write releases/feed.json
//   tooling/feed.mjs --check    verify feed.json matches the dirs (CI)

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

const fail = (msg) => {
  console.error(`feed: ${msg}`);
  process.exit(1);
};

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
  let keyId;
  for (const id of readdirSync(RELEASES).sort()) {
    const dir = join(RELEASES, id);
    if (!statSync(dir, { throwIfNoEntry: false })?.isDirectory()) continue;
    const versions = readdirSync(dir).filter((v) =>
      /^\d+\.\d+\.\d+$/.test(v),
    );
    if (versions.length === 0) continue;
    const version = versions.sort(cmpSemver).at(-1);
    const rel = join(dir, version);
    const provenance = JSON.parse(
      readFileSync(join(rel, 'provenance.json'), 'utf8'),
    );
    const signature = readFileSync(join(rel, 'signature'), 'utf8').trim();
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
    plugins.push({
      id,
      version,
      abi: provenance.abi_version,
      wasm_sha256: provenance.wasm_sha256,
      manifest_sha256: provenance.manifest_sha256,
      signature,
    });
  }
  return { keyId, plugins };
};

const feed = build();
const out = `${JSON.stringify(feed, null, 2)}\n`;

if (process.argv.includes('--check')) {
  if (!existsSync(FEED)) fail('releases/feed.json does not exist');
  if (readFileSync(FEED, 'utf8') !== out) {
    fail('feed.json is stale — run tooling/feed.mjs');
  }
  console.log('feed: ok — matches release dirs');
} else {
  writeFileSync(FEED, out);
  console.log(
    `feed: wrote ${feed.plugins.length} plugins, keyId ${feed.keyId}`,
  );
}
