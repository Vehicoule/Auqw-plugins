#!/usr/bin/env node
// Release signing for Auqw plugin artifacts (ed25519, node:crypto only).
//
//   tooling/sign.mjs keygen [--force]      create the signing keypair
//   tooling/sign.mjs sign <plugin-dir>     stage + sign a release
//   tooling/sign.mjs verify <release-dir>  re-verify a signed release
//   tooling/sign.mjs pubkey                print key_id + public PEM
//
// The keypair lives outside every repository (default
// ~/.auqw/keys/auqw-ed25519.json; override with
// --key-file <path> or AUQW_KEY_FILE). The private key is never printed.
//
// `sign` reads <plugin-dir>/manifest.json (or plugin.manifest.json) and
// <plugin-dir>/dist/<id>.wasm, then writes <repo>/releases/<id>/<version>/:
//   <id>-<version>.wasm   the artifact bytes, verbatim
//   plugin.manifest.json  the manifest, verbatim
//   provenance.json       digests + signer identity + toolchain
//   signature             base64 ed25519 over the canonical payload
//
// Canonical payload (UTF-8, LF separators, trailing LF — sign and verify
// MUST build this byte-identically):
//
//   auqw-release-v1\n
//   {plugin}\n{version}\n{abi_version}\n
//   {wasm_sha256}\n{manifest_sha256}\n{key_id}\n
//
// All digests are "sha256:<64 lowercase hex>", the same spelling as
// manifest.artifact.digest. key_id is the first 16 hex chars of the
// sha256 of the signer's DER (SPKI) public key.

import { createHash, generateKeyPairSync, sign as edSign, verify as edVerify, createPublicKey } from 'node:crypto';
import { chmodSync, existsSync, mkdirSync, readFileSync, readdirSync, writeFileSync, copyFileSync } from 'node:fs';
import { homedir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const REPO_ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const DEFAULT_KEY_FILE = join(homedir(), '.auqw', 'keys', 'auqw-ed25519.json');
const RELEASES_DIR = join(REPO_ROOT, 'releases');

const fail = (msg) => {
  console.error(`sign: ${msg}`);
  process.exit(1);
};

const sha256 = (buf) => `sha256:${createHash('sha256').update(buf).digest('hex')}`;

// Manifest id/version grammar, mirroring tooling/validate. Both become
// path components (release dir name, wasm filename), so anything outside
// the grammar is rejected before it can reach join().
const ID_RE = /^[a-z0-9][a-z0-9-]*$/;
const VERSION_RE = /^[0-9]+\.[0-9]+\.[0-9]+$/;

const keyIdOf = (publicPem) =>
  createHash('sha256')
    .update(createPublicKey(publicPem).export({ format: 'der', type: 'spki' }))
    .digest('hex')
    .slice(0, 16);

const canonicalPayload = ({ plugin, version, abi, wasmSha, manifestSha, keyId }) =>
  `auqw-release-v1\n${plugin}\n${version}\n${abi}\n${wasmSha}\n${manifestSha}\n${keyId}\n`;

// --- args -------------------------------------------------------------------

const parseArgs = (argv) => {
  const opts = { force: false, keyFile: process.env.AUQW_KEY_FILE ?? DEFAULT_KEY_FILE, pos: [] };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a === '--force') opts.force = true;
    else if (a === '--key-file') opts.keyFile = argv[++i] ?? fail('--key-file needs a path');
    else if (a.startsWith('--key-file=')) opts.keyFile = a.slice('--key-file='.length);
    else if (a === '-h' || a === '--help') opts.help = true;
    else opts.pos.push(a);
  }
  return opts;
};

// --- key loading -------------------------------------------------------------

// Returns { keyId, publicPem, privatePem? }. Accepts the keypair JSON or a
// bare public-key PEM (verify/pubkey never need the private half).
const loadKeyFile = (path) => {
  let text;
  try {
    text = readFileSync(path, 'utf8');
  } catch {
    fail(`cannot read key file ${path} — run \`tooling/sign.mjs keygen\` first or pass --key-file`);
  }
  let publicPem;
  let privatePem;
  if (text.trimStart().startsWith('{')) {
    let parsed;
    try {
      parsed = JSON.parse(text);
    } catch {
      fail(`key file ${path} is not valid JSON`);
    }
    publicPem = parsed.public_key_pem;
    privatePem = parsed.private_key_pem;
    if (typeof publicPem !== 'string') fail(`key file ${path} has no public_key_pem`);
  } else if (text.includes('BEGIN PUBLIC KEY')) {
    publicPem = text;
  } else {
    fail(`key file ${path} is neither a keypair JSON nor a public PEM`);
  }
  return { keyId: keyIdOf(publicPem), publicPem, privatePem };
};

// --- subcommands -------------------------------------------------------------

const cmdKeygen = (opts) => {
  const keyFile = opts.keyFile;
  if (existsSync(keyFile) && !opts.force) {
    fail(`key file ${keyFile} already exists — pass --force to replace it (old signatures stay valid only if you kept the old key)`);
  }
  const { publicKey, privateKey } = generateKeyPairSync('ed25519', {
    publicKeyEncoding: { format: 'pem', type: 'spki' },
    privateKeyEncoding: { format: 'pem', type: 'pkcs8' },
  });
  const keyId = keyIdOf(publicKey);
  const doc = {
    key_id: keyId,
    public_key_pem: publicKey,
    private_key_pem: privateKey,
    created_at: new Date().toISOString(),
  };
  mkdirSync(dirname(keyFile), { recursive: true, mode: 0o700 });
  // 'wx' makes the no-clobber check atomic; --force swaps to 'w'.
  writeFileSync(keyFile, `${JSON.stringify(doc, null, 2)}\n`, {
    flag: opts.force ? 'w' : 'wx',
    mode: 0o600,
  });
  chmodSync(keyFile, 0o600);
  console.log(`sign: keygen ok — key_id ${keyId}`);
  console.log(`sign: wrote ${keyFile} (mode 600); keep it outside every repo`);
};

const readManifest = (pluginDir) => {
  for (const name of ['manifest.json', 'plugin.manifest.json']) {
    const p = join(pluginDir, name);
    if (existsSync(p)) return { path: p, buf: readFileSync(p) };
  }
  fail(`no manifest.json or plugin.manifest.json in ${pluginDir}`);
};

const cmdSign = (opts) => {
  const pluginDir = resolve(opts.pos[1] ?? fail('usage: tooling/sign.mjs sign <plugin-dir>'));
  const { path: manifestPath, buf: manifestBuf } = readManifest(pluginDir);
  let manifest;
  try {
    manifest = JSON.parse(manifestBuf.toString('utf8'));
  } catch (e) {
    fail(`${manifestPath} is not valid JSON: ${e.message}`);
  }
  const { id, version, abi } = manifest;
  if (!id || !version || !abi) fail(`${manifestPath}: manifest needs id, version, abi`);
  if (typeof id !== 'string' || !ID_RE.test(id)) {
    fail(`${manifestPath}: manifest.id must match ^[a-z0-9][a-z0-9-]*$`);
  }
  if (typeof version !== 'string' || !VERSION_RE.test(version)) {
    fail(`${manifestPath}: manifest.version must be semver x.y.z`);
  }

  const wasmPath = join(pluginDir, 'dist', `${id}.wasm`);
  if (!existsSync(wasmPath)) {
    fail(`artifact ${wasmPath} missing — run tooling/build.sh ${id} first`);
  }
  const wasmBuf = readFileSync(wasmPath);
  const wasmSha = sha256(wasmBuf);
  const manifestSha = sha256(manifestBuf);

  // The manifest pins its artifact; signing a stale pin ships a broken release.
  const pinned = manifest.artifact?.digest;
  if (pinned !== wasmSha) {
    fail(`manifest artifact.digest ${pinned ?? '(missing)'} != ${wasmSha} — rebuild via tooling/build.sh ${id}`);
  }

  const releaseDir = join(RELEASES_DIR, id, version);
  if (existsSync(releaseDir) && !opts.force) {
    fail(`release ${releaseDir} already exists — releases are immutable; bump the version or pass --force`);
  }

  const { keyId, privatePem } = loadKeyFile(opts.keyFile);
  if (!privatePem) fail(`key file ${opts.keyFile} holds no private key — cannot sign`);

  const payload = canonicalPayload({ plugin: id, version, abi, wasmSha, manifestSha, keyId });
  const signature = edSign(null, Buffer.from(payload, 'utf8'), privatePem);

  const provenance = {
    plugin: id,
    version,
    abi_version: abi,
    wasm_sha256: wasmSha,
    manifest_sha256: manifestSha,
    signed_at: new Date().toISOString(),
    key_id: keyId,
    toolchain: { node: process.version, platform: `${process.platform}/${process.arch}` },
  };

  mkdirSync(releaseDir, { recursive: true });
  copyFileSync(wasmPath, join(releaseDir, `${id}-${version}.wasm`));
  copyFileSync(manifestPath, join(releaseDir, 'plugin.manifest.json'));
  writeFileSync(join(releaseDir, 'provenance.json'), `${JSON.stringify(provenance, null, 2)}\n`);
  writeFileSync(join(releaseDir, 'signature'), `${signature.toString('base64')}\n`);
  console.log(`sign: ok — ${id} ${version} -> ${releaseDir}`);
  console.log(`sign: wasm ${wasmSha} key_id ${keyId}`);
};

const cmdVerify = (opts) => {
  const dir = resolve(opts.pos[1] ?? fail('usage: tooling/sign.mjs verify <release-dir>'));
  const bad = (msg) => fail(`FAIL — ${dir}: ${msg}`);

  const need = (name) => {
    const p = join(dir, name);
    if (!existsSync(p)) bad(`missing ${name}`);
    return readFileSync(p);
  };
  const manifestBuf = need('plugin.manifest.json');
  const provenanceBuf = need('provenance.json');
  const sigBuf = need('signature');

  let provenance;
  let manifest;
  try {
    provenance = JSON.parse(provenanceBuf.toString('utf8'));
    manifest = JSON.parse(manifestBuf.toString('utf8'));
  } catch (e) {
    bad(`release metadata is not valid JSON: ${e.message}`);
  }
  const { plugin, version, abi_version: abi, wasm_sha256, manifest_sha256, key_id } = provenance;
  if (!plugin || !version || !abi || !wasm_sha256 || !manifest_sha256 || !key_id) {
    bad('provenance.json is missing required fields');
  }
  if (!ID_RE.test(plugin) || !VERSION_RE.test(version)) {
    bad('provenance.json plugin/version do not match manifest grammar');
  }

  const wasmBuf = need(`${plugin}-${version}.wasm`);
  if (readdirSync(dir).filter((f) => f.endsWith('.wasm')).length !== 1) {
    bad('release dir must hold exactly one .wasm artifact');
  }

  const wasmSha = sha256(wasmBuf);
  if (wasmSha !== wasm_sha256) bad(`wasm digest drift: ${wasmSha} != provenance ${wasm_sha256}`);
  if (manifest.artifact?.digest !== wasmSha) {
    bad(`manifest artifact.digest ${manifest.artifact?.digest ?? '(missing)'} != wasm ${wasmSha}`);
  }
  const manifestSha = sha256(manifestBuf);
  if (manifestSha !== manifest_sha256) {
    bad(`manifest digest drift: ${manifestSha} != provenance ${manifest_sha256}`);
  }
  if (manifest.id !== plugin || manifest.version !== version || manifest.abi !== abi) {
    bad('plugin.manifest.json id/version/abi disagree with provenance.json');
  }

  const { keyId, publicPem } = loadKeyFile(opts.keyFile);
  if (keyId !== key_id) {
    bad(`release was signed by key ${key_id}; loaded key is ${keyId}`);
  }

  const payload = canonicalPayload({ plugin, version, abi, wasmSha, manifestSha, keyId });
  let ok = false;
  try {
    ok = edVerify(null, Buffer.from(payload, 'utf8'), publicPem, Buffer.from(sigBuf.toString('utf8').trim(), 'base64'));
  } catch (e) {
    bad(`signature is not base64 ed25519: ${e.message}`);
  }
  if (!ok) bad('ed25519 signature does not verify');
  console.log(`sign: verify ok — ${plugin} ${version} (${wasmSha}) signed by ${key_id}`);
};

const cmdPubkey = (opts) => {
  const { keyId, publicPem } = loadKeyFile(opts.keyFile);
  console.log(`key_id: ${keyId}`);
  process.stdout.write(publicPem.endsWith('\n') ? publicPem : `${publicPem}\n`);
};

// --- entry -------------------------------------------------------------------

const usage = `usage: tooling/sign.mjs <keygen|sign|verify|pubkey> [args] [--force] [--key-file <path>]
  keygen                 create the ed25519 keypair (refuses to overwrite; --force replaces)
  sign <plugin-dir>      stage + sign releases/<id>/<version>/ from dist/ + manifest.json
  verify <release-dir>   re-check digests + ed25519 signature
  pubkey                 print key_id + PEM public key
key file: --key-file <path> or AUQW_KEY_FILE (default ${DEFAULT_KEY_FILE})`;

const main = () => {
  const opts = parseArgs(process.argv.slice(2));
  const cmd = opts.pos[0];
  if (opts.help || !cmd) {
    console.log(usage);
    process.exit(opts.help ? 0 : 1);
  }
  switch (cmd) {
    case 'keygen': return cmdKeygen(opts);
    case 'sign': return cmdSign(opts);
    case 'verify': return cmdVerify(opts);
    case 'pubkey': return cmdPubkey(opts);
    default: fail(`unknown subcommand ${cmd}\n${usage}`);
  }
};

main();
