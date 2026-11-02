#!/bin/sh
# Points the build at another ever-node commit: sets ever-node's rev in Cargo.toml, makes ever_block
# the version that commit uses (the tool passes ever_block types to ever-node, so both must use the
# same ever_block) and updates Cargo.lock. Run it in this folder, then cargo build.
# usage: ./set-ever-node-rev.sh <ever-node commit>
set -eu
[ $# -eq 1 ] || { echo "usage: $0 <ever-node commit>" >&2; exit 2; }
rev=$1
sed -i -E "s/(ever-node = \{[^}]*rev = \")[0-9a-f]+/\1$rev/" Cargo.toml
cargo update -p ever-node
# The line looks like: ever_block v1.11.13 (https://github.com/everx-labs/ever-block.git?tag=1.11.13#a019b2da)
spec=$(cargo tree -p ever-node --depth 1 --prefix none -e normal | sed -n -E \
    -e 's/^ever_block v[^ ]+ \(([^?#]+)\?(tag|rev|branch)=([^#]+)#.*/git = "\1", \2 = "\3"/p' \
    -e 's/^ever_block v[^ ]+ \(([^?#]+)#.*/git = "\1"/p')
[ -n "$spec" ] || { echo "cannot tell which ever_block ever-node $rev uses" >&2; exit 1; }
sed -i -E "s|^ever_block = \{[^}]*\}|ever_block = { $spec }|" Cargo.toml
cargo fetch
grep -E '^(ever_block|ever-node) =' Cargo.toml
