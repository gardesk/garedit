#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

echo "[1/4] formatting check"
cargo fmt --check

echo "[2/4] core tests"
cargo test -p garedit-core --offline

echo "[3/4] build control utility"
cargo build -p gareditctl --release --offline

echo "[4/4] verify CLI surfaces"
./target/release/gareditctl --help >/dev/null
./target/release/gareditctl open --help >/dev/null

echo "release smoke checks passed"
echo "next: run manual GUI and IPC checks (see README.md)"
