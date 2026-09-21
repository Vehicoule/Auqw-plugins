// Regression tests for tooling/sign.mjs — manifest id/version grammar is
// enforced before either becomes a path component: a `..` segment in id
// or version would otherwise resolve `join(RELEASES_DIR, id, version)`
// outside releases/ (and verify's derived wasm filename outside the
// release dir).
//
// Run: node --test tooling/sign.test.mjs   (from the repo root)
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { existsSync, mkdirSync, mkdtempSync, readdirSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const SIGN = join(ROOT, 'tooling', 'sign.mjs');
const RELEASES = join(ROOT, 'releases');

const run = (args) => spawnSync(process.execPath, [SIGN, ...args], { encoding: 'utf8' });

const sha256 = (buf) => `sha256:${createHash('sha256').update(buf).digest('hex')}`;

// A scratch plugin dir whose manifest passes everything except what the
// test wants to trip: artifact.digest pins the bytes actually staged in
// dist/, so a grammar-valid manifest reaches the release-dir join.
const makePlugin = (overrides = {}) => {
  const dir = mkdtempSync(join(tmpdir(), 'sign-test-'));
  const manifest = {
    id: 'sign-test',
    version: '0.0.1',
    abi: '0.3.0',
    capabilities: ['playback.resolve'],
    permissions: [],
    artifact: { path: 'dist/sign-test.wasm', digest: '' },
    ...overrides,
  };
  mkdirSync(join(dir, 'dist'), { recursive: true });
  const wasm = Buffer.from(`wasm-bytes-${manifest.id}-${manifest.version}`);
  manifest.artifact.digest = sha256(wasm);
  writeFileSync(join(dir, 'dist', `${manifest.id}.wasm`), wasm);
  writeFileSync(join(dir, 'manifest.json'), `${JSON.stringify(manifest, null, 2)}\n`);
  return dir;
};

const grammarFailures = [
  ['id traversal', { id: '../outside' }, 'manifest.id'],
  ['id uppercase', { id: 'Sign-Test' }, 'manifest.id'],
  ['version traversal', { version: '../../escape' }, 'manifest.version'],
  ['version non-semver', { version: '1.0' }, 'manifest.version'],
];

for (const [name, overrides, field] of grammarFailures) {
  test(`sign rejects ${name}`, () => {
    const dir = makePlugin(overrides);
    try {
      const r = run(['sign', dir, '--key-file', join(dir, 'no-key.json')]);
      assert.equal(r.status, 1, `expected failure, got ${r.stdout}${r.stderr}`);
      assert.match(r.stderr, new RegExp(`${field}.*match|${field}.*semver`));
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  });
}

test('sign + verify round-trip a valid plugin', () => {
  const dir = makePlugin();
  const keyFile = join(dir, 'key.json');
  const releaseDir = join(RELEASES, 'sign-test', '0.0.1');
  try {
    assert.equal(run(['keygen', '--key-file', keyFile]).status, 0);
    const s = run(['sign', dir, '--key-file', keyFile]);
    assert.equal(s.status, 0, s.stderr);
    for (const f of ['sign-test-0.0.1.wasm', 'plugin.manifest.json', 'provenance.json', 'signature']) {
      assert.ok(existsSync(join(releaseDir, f)), `missing ${f}`);
    }
    const v = run(['verify', releaseDir, '--key-file', keyFile]);
    assert.equal(v.status, 0, v.stderr);
  } finally {
    rmSync(dir, { recursive: true, force: true });
    rmSync(releaseDir, { recursive: true, force: true });
    const parent = join(RELEASES, 'sign-test');
    if (existsSync(parent) && readdirSync(parent).length === 0) rmSync(parent, { recursive: true });
  }
});

test('verify rejects traversal provenance before deriving filenames', () => {
  const dir = mkdtempSync(join(tmpdir(), 'sign-verify-'));
  try {
    writeFileSync(join(dir, 'plugin.manifest.json'), '{"id":"x","version":"0.1.0","abi":"0.3.0","artifact":{"path":"x.wasm","digest":"sha256:0000000000000000000000000000000000000000000000000000000000000000"}}\n');
    writeFileSync(
      join(dir, 'provenance.json'),
      `${JSON.stringify({
        plugin: '../evil',
        version: '0.1.0',
        abi_version: '0.3.0',
        wasm_sha256: 'sha256:' + '0'.repeat(64),
        manifest_sha256: 'sha256:' + '0'.repeat(64),
        key_id: '0'.repeat(16),
      })}\n`,
    );
    writeFileSync(join(dir, 'signature'), 'AAAA\n');
    const r = run(['verify', dir]);
    assert.equal(r.status, 1);
    assert.match(r.stderr, /grammar/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
