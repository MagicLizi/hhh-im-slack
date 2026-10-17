#!/bin/sh
# 打包（macOS / Linux）：编译、把程序与清单放进 dist/stage、用 hhh-im-pack 打成 .hhhim 并写版本索引。
# 用法：scripts/pack.sh [--sign-key <私钥文件>] [--publish]
# 目标按这台电脑（M 芯片 Mac 出 macos-arm64，Intel Mac 出 macos-x64，Linux 出 linux-x64 / linux-arm64）。
# hhh-im-pack 没装时：cargo install --git https://github.com/MagicLizi/hhh-im-sdk --tag v0.4.0 hhh-im-protocol --features transport,package
set -eu
cd "$(dirname "$0")/.."
cargo build --release
rm -rf dist/stage
mkdir -p dist/stage
cp hhh-im-provider.json logo.svg dist/stage/
cp target/release/hhh-im-slack dist/stage/
hhh-im-pack dist/stage --out dist --notes notes.md "$@"
