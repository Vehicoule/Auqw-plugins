#!/bin/sh
# Release one plugin end-to-end: build -> validate -> sign -> verify ->
# regenerate feed.json. Collapses the manual sequence documented in
# releases/README.md into one command.
#
# Usage: tooling/release.sh <plugin-id> [--force]
#
# Requires: the signing key (AUQW_KEY_FILE or --key-file for sign.mjs),
# a clean plugin tree, and a bumped manifest.version (releases are
# immutable; sign refuses an existing <id>/<version> unless --force).
# Commit the result on a branch per conventions.md (s<slice>/topic).
set -e
cd "$(dirname "$0")/.."

plugin="${1:?usage: tooling/release.sh <plugin-id> [--force]}"
shift || true
force=""
for a in "$@"; do
    [ "$a" = "--force" ] && force="--force"
done

if [ ! -f "plugins/${plugin}/manifest.json" ]; then
    echo "release: no plugins/${plugin}/manifest.json" >&2
    exit 1
fi

version="$(sed -n 's/.*"version": *"\([^"]*\)".*/\1/p' "plugins/${plugin}/manifest.json" | head -1)"
if [ -z "$version" ]; then
    echo "release: cannot read version from plugins/${plugin}/manifest.json" >&2
    exit 1
fi

if [ -d "releases/${plugin}/${version}" ] && [ -z "$force" ]; then
    echo "release: releases/${plugin}/${version} already exists — bump manifest.version or pass --force" >&2
    exit 1
fi

echo "==> release ${plugin} ${version}"
./tooling/build.sh "$plugin"
node tooling/sign.mjs sign "plugins/${plugin}" $force
node tooling/sign.mjs verify "releases/${plugin}/${version}"
node tooling/feed.mjs
echo "==> done — staged releases/${plugin}/${version} + feed.json"
echo "    commit the releases/ tree (and plugins/${plugin}/manifest.json digest pin)"
