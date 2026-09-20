# Releases

Signed, immutable plugin artifacts. The app pins a release by digest in
`providers.lock.json`; the signature is what makes the digest an
authenticity claim, not just an integrity check ([plugin-system.md §7]).

## Layout

```text
releases/<id>/<version>/
├── <id>-<version>.wasm    # the artifact bytes, verbatim from plugins/<id>/dist/
├── plugin.manifest.json   # the manifest, verbatim
├── provenance.json        # digests + signer identity + toolchain
└── signature              # base64 ed25519 signature (64 bytes decoded)
```

`provenance.json`:

```json
{
  "plugin": "itunes",
  "version": "0.1.0",
  "abi_version": "0.2.0",
  "wasm_sha256": "sha256:<64 hex>",
  "manifest_sha256": "sha256:<64 hex>",
  "signed_at": "<RFC 3339 / ISO 8601 UTC>",
  "key_id": "<16 hex>",
  "toolchain": { "node": "v…", "platform": "<os>/<arch>" }
}
```

All digests are spelled `sha256:<64 lowercase hex>` — the same format as
`manifest.artifact.digest`.

## Signature payload

The signature covers this exact UTF-8 byte string (LF separators, trailing
LF; `sign` and `verify` build it identically — change nothing):

```text
auqw-release-v1\n
{plugin}\n{version}\n{abi_version}\n
{wasm_sha256}\n{manifest_sha256}\n{key_id}\n
```

`key_id` is the first 16 hex characters of the sha256 of the signer's
DER-encoded (SPKI) public key.

## Producing a release

```sh
./tooling/build.sh <id>                       # builds dist/<id>.wasm + pins the digest
node tooling/sign.mjs sign plugins/<id>       # stages + signs releases/<id>/<version>/
node tooling/sign.mjs verify releases/<id>/<version>
```

`sign` refuses to overwrite an existing `<id>/<version>` directory —
releases are immutable; bump `manifest.version` for a new artifact.
`verify` recomputes both digests, cross-checks the manifest's pinned
`artifact.digest`, and verifies the ed25519 signature; any drift, missing
file, or bad signature exits non-zero.

## Key custody

Signing keys live at **`/Users/btw/Documents/Repo/keys/`** — outside all
repositories, never committed, never copied into a checkout
([AGENTS.md](../AGENTS.md): no secrets in code). The keypair file
`auqw-ed25519.json` is created by:

```sh
node tooling/sign.mjs keygen     # mode 600; refuses to overwrite without --force
node tooling/sign.mjs pubkey     # key_id + PEM public key for providers.lock.json
```

`sign` needs the private half; `verify` and `pubkey` need only the public
half — `--key-file <path>` (or `AUQW_KEY_FILE`) accepts either the keypair
JSON or a bare public PEM, so CI and the app repo can verify releases
without holding the private key. The private key is never printed or
logged by the tooling.
