#!/usr/bin/env bash
set -e

echo "Compilando trading-view (release)..."
cargo build --release

mkdir -p bin
cp target/release/trading-view bin/trading-view

echo ""
echo "Listo! Binario actualizado: bin/trading-view"
