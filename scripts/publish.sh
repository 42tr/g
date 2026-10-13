#!/bin/sh
# Check both packages by default. --publish uploads them in dependency order.
set -eu

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
cd "$script_dir/.."

if [ "${1-}" = "--publish" ]; then
    shift
    exec cargo publish --workspace --all-features --locked --registry crates-io "$@"
fi

exec cargo publish --workspace --dry-run --all-features --locked --registry crates-io "$@"
