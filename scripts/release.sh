#!/bin/sh
# Builds a release archive for one target.

set -eu
cd "$(dirname "$0")/.."

target=${1:-$(rustc -vV | sed -n 's/^host: //p')}
name="ats-$target"

cargo build --release --locked --target "$target"
rm -rf "dist/$name"
mkdir -p "dist/$name"
cp "target/$target/release/ats" README.md CHANGELOG.md LICENSE "dist/$name/"
COPYFILE_DISABLE=1 tar --no-xattrs -C dist -czf "dist/$name.tar.gz" "$name"
echo "dist/$name.tar.gz"
