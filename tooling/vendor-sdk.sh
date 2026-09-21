#!/bin/sh
# Vendor the packaged auqw-guest-sdk into vendor/ so guest crates build
# without a sibling ../auqw checkout. Explicit local-maintainer command —
# run it when the authoritative SDK in ../auqw/sdk/rust changes.
# The vendor directory is version-pinned (vendor/auqw-guest-sdk-<v>), so
# multiple ABI revisions can coexist while guests migrate.
# Usage: tooling/vendor-sdk.sh   (run from the repo root)
set -eu

sdk_manifest="../auqw/sdk/rust/Cargo.toml"
license_file="../auqw/LICENSE"

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
if [ "$name" != "auqw-guest-sdk" ]; then
    echo "vendor-sdk: expected auqw-guest-sdk, found ${name} ${version}" >&2
    exit 1
fi
case "$version" in
    *[!0-9.]*|"")
        echo "vendor-sdk: unexpected sdk version '${version}'" >&2
        exit 1
        ;;
esac
vendor_dir="vendor/auqw-guest-sdk-${version}"

cargo package --allow-dirty --manifest-path "$sdk_manifest"

crate_file="../auqw/target/package/auqw-guest-sdk-${version}.crate"
if [ ! -f "$crate_file" ]; then
    echo "vendor-sdk: package artifact missing: $crate_file" >&2
    exit 1
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
tar -xzf "$crate_file" -C "$tmp"
src="$tmp/auqw-guest-sdk-${version}"

rm -rf "$vendor_dir"
mkdir -p "$vendor_dir"
# The packaged crate's file list is authoritative — copy it verbatim so
# new package members are never dropped. The GPL text lives outside the
# package dir (license is an SPDX tag), so it is overlaid separately.
cp -R "$src/." "$vendor_dir/"
cp "$license_file" "$vendor_dir/LICENSE"

echo "vendor-sdk: vendored ${name} ${version} -> ${vendor_dir}"
