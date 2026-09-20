#!/bin/sh
# Vendor the packaged auqw-guest-sdk into vendor/ so guest crates build
# without a sibling ../auqw checkout. Explicit local-maintainer command —
# run it when the authoritative SDK in ../auqw/sdk/rust changes.
# Usage: tooling/vendor-sdk.sh   (run from the repo root)
set -eu

sdk_manifest="../auqw/sdk/rust/Cargo.toml"
license_file="../auqw/LICENSE"
vendor_dir="vendor/auqw-guest-sdk-0.2.0"

if [ ! -f "$sdk_manifest" ]; then
    echo "vendor-sdk: sibling SDK source missing: $sdk_manifest" >&2
    exit 1
fi
if [ ! -f "$license_file" ]; then
    echo "vendor-sdk: license file missing: $license_file" >&2
    exit 1
fi

name="$(sed -n 's/^name = "\(.*\)"$/\1/p' "$sdk_manifest" | head -1)"
version="$(sed -n 's/^version = "\(.*\)"$/\1/p' "$sdk_manifest" | head -1)"
if [ "$name" != "auqw-guest-sdk" ] || [ "$version" != "0.2.0" ]; then
    echo "vendor-sdk: expected auqw-guest-sdk 0.2.0, found ${name} ${version}" >&2
    exit 1
fi

cargo package --allow-dirty --manifest-path "$sdk_manifest"

crate_file="../auqw/target/package/auqw-guest-sdk-0.2.0.crate"
if [ ! -f "$crate_file" ]; then
    echo "vendor-sdk: package artifact missing: $crate_file" >&2
    exit 1
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
tar -xzf "$crate_file" -C "$tmp"
src="$tmp/auqw-guest-sdk-0.2.0"

rm -rf "$vendor_dir"
mkdir -p "$vendor_dir"
for f in Cargo.toml Cargo.lock; do
    if [ -f "$src/$f" ]; then
        cp "$src/$f" "$vendor_dir/$f"
    fi
done
cp "$license_file" "$vendor_dir/LICENSE"
cp -R "$src/src" "$vendor_dir/src"

echo "vendor-sdk: vendored ${name} ${version} -> ${vendor_dir}"
