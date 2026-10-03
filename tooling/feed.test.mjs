// Regression tests for tooling/feed.mjs — the OTA index must never
// ship an entry whose bytes drifted from what was signed: the feed
// builder re-hashes the staged wasm and manifest and refuses digest
// drift, a manifest that disagrees with the release dir, or a
// manifest whose artifact.digest no longer pins the staged bytes.
//
// Run: node --test tooling/feed.test.mjs   (from the repo root)
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { cpSync, mkdtempSync, readFileSync, readdirSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');

// feed.mjs resolves its repo root from its own location, so a drift
// case stages a scratch copy of the repo (minus the heavy target/
// and .git/ trees) and mutates the release bytes inside it.
const repoCopy = () => {
  const dest = mkdtempSync(join(tmpdir(), 'feed-repo-'));
  cpSync(ROOT, dest, {
    recursive: true,
    filter: (src) => !src.includes(`${dirname(src)}.git`) && !src.endsWith('/target') && !src.endsWith('/.git'),
  });
  return dest;
};

const run = (cwd) =>
  spawnSync(process.execPath, [join(cwd, 'tooling', 'feed.mjs'), '--check'], {
    encoding: 'utf8',
  });

// The feed inspects each plugin's highest-semver release dir, so drift
// cases mutate that dir — a hardcoded version stops being covered the
// moment a newer release lands.
const latestRelease = (id) => {
  const versions = readdirSync(join(ROOT, 'releases', id)).filter((v) =>
    /^\d+\.\d+\.\d+$/.test(v),
  );
  const key = (v) => v.split('.').map(Number);
  versions.sort((a, b) => {
    const [ka, kb] = [key(a), key(b)];
    for (let i = 0; i < 3; i += 1) if (ka[i] !== kb[i]) return ka[i] - kb[i];
    return 0;
  });
  return versions.at(-1);
};

test('feed check passes on the intact tree', () => {
  const r = run(ROOT);
  assert.equal(r.status, 0, r.stderr);
});

test('feed check refuses wasm digest drift', () => {
  const repo = repoCopy();
  try {
    const v = latestRelease('deezer');
    const wasmPath = join(repo, 'releases', 'deezer', v, `deezer-${v}.wasm`);
    const bytes = readFileSync(wasmPath);
    bytes[bytes.length - 1] ^= 0xff;
    writeFileSync(wasmPath, bytes);
    const r = run(repo);
    assert.notEqual(r.status, 0, 'feed check must fail on wasm drift');
    assert.match(r.stderr, /digest drift/);
  } finally {
    rmSync(repo, { recursive: true, force: true });
  }
});

test('feed check refuses manifest digest drift', () => {
  const repo = repoCopy();
  try {
    const manifestPath = join(repo, 'releases', 'deezer', latestRelease('deezer'), 'plugin.manifest.json');
    const manifest = JSON.parse(readFileSync(manifestPath, 'utf8'));
    manifest.description = 'tampered after signing';
    writeFileSync(manifestPath, `${JSON.stringify(manifest, null, 2)}\n`);
    const r = run(repo);
    assert.notEqual(r.status, 0, 'feed check must fail on manifest drift');
    assert.match(r.stderr, /manifest digest drift/);
  } finally {
    rmSync(repo, { recursive: true, force: true });
  }
});
