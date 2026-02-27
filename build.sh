#!/usr/bin/env bash
set -e

echo "Compilando tradingbot (release)..."
cargo build --release

mkdir -p bin
cp target/release/tradingbot bin/tradingbot

echo ""
echo "Listo! Binario actualizado: bin/tradingbot"
