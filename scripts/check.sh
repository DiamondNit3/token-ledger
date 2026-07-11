#!/usr/bin/env sh
set -eu

cd "$(dirname "$0")/.."

rust_version=$(rustc --version | awk '{print $2}')
minimum=1.88.0
first=$(printf '%s\n%s\n' "$minimum" "$rust_version" | sort -V | head -n 1)
if [ "$first" != "$minimum" ]; then
    echo "Rust $minimum or newer is required; active version is $rust_version" >&2
    exit 1
fi

echo '==> Formatting'
cargo fmt --all -- --check

echo '==> Clippy'
cargo clippy --all-targets --all-features --locked -- -D warnings

echo '==> Tests'
cargo test --all-targets --locked

echo '==> Cargo source-package boundary'
entries=$(cargo package --allow-dirty --no-verify --list)
if printf '%s\n' "$entries" | grep -E '^(dist|target|open-source)/|(^|/)test-evidence\.html$|\.(exe|zip|sqlite3|log)$|(^|/)(ledger\.toml|\.env)$' >/dev/null; then
    echo 'Forbidden generated or private file entered the source package.' >&2
    exit 1
fi

echo 'All Token Ledger checks passed.'
