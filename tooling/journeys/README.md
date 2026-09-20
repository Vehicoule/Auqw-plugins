# Integration journeys

Canned-upstream journey specs consumed by `auqw`'s
`crates/integration-runner`. Each `*.json` names a plugin, one
capability, the verbatim request payload, scripted upstream responses,
and an expectation. `body_file` resolves relative to this directory —
fixture paths stay inside this repo (`../../plugins/<id>/fixtures/`).

Spec format and runner semantics: `auqw/crates/integration-runner/README.md`.

Run (from either checkout root, sibling checkouts assumed):

```sh
cargo run --manifest-path ../auqw/Cargo.toml -p auqw-integration-runner -- \
  --pubkey <release-pubkey.pem> \
  --release releases/<id>/<version> [--release ...] \
  --journeys tooling/journeys
```
