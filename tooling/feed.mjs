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
  let keyId;
  for (const id of readdirSync(RELEASES).sort()) {
    const dir = join(RELEASES, id);
    if (!statSync(dir, { throwIfNoEntry: false })?.isDirectory()) continue;
    const versions = readdirSync(dir).filter((v) =>
      /^\d+\.\d+\.\d+$/.test(v),
    );
    if (versions.length === 0) continue;
    // sign.mjs's VERSION_RE admits leading zeros (`0.02.1`); two
    // spellings of the same numbers tie under cmpSemver and make the
    // "latest" pick depend on readdir order, so a version dir must be
    // spelled canonically.
    for (const v of versions) {
      const canonical = semverKey(v).join('.');
      if (v !== canonical) {
        fail(`${id}/${v}: non-canonical version dir (canonical spelling: ${canonical})`);
      }
    }
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
    // The client strictly validates every entry: signature must
    // strict-base64-decode to a 64-byte ed25519 signature and keyId
    // must equal the embedded trust id's 16-hex shape. One malformed
    // entry makes it reject the WHOLE feed, so refuse here — the same
    // way digest drift is refused — instead of publishing it.
    if (typeof provenance.key_id !== 'string' || !/^[0-9a-f]{16}$/.test(provenance.key_id)) {
      fail(`${id}/${version}: key_id is not 16 lowercase hex chars`);
    }
    const sigBytes = Buffer.from(signature, 'base64');
    if (sigBytes.length !== 64 || sigBytes.toString('base64') !== signature) {
      fail(`${id}/${version}: signature is not strict-base64 ed25519`);
    }
    if (keyId === undefined) keyId = provenance.key_id;
    if (keyId !== provenance.key_id) {
      fail(`${id}/${version}: key_id ${provenance.key_id} mixes with ${keyId}`);
    }
    // Re-hash the bytes on disk — the feed must never ship an entry
    // whose artifact or manifest drifted from what was signed. The
    // signature itself is still verified at install time (and by
    // `sign.mjs verify` with the key); this is the keyless integrity
    // check that catches tampering after the release dir was cut.
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
