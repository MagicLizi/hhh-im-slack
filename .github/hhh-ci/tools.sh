#!/usr/bin/env bash
# 装与插件锁定的 SDK 同一版本的 hhh-im-pack / hhh-im-check，并加进后面步骤的 PATH。
# 本文件由 MagicLizi/hhh-release 统一维护（plugin-files/），插件仓库里不要直接改。
# 用法：tools.sh <SDK 标签，如 v0.10.0>
set -euo pipefail
tag="$1"
root="$HOME/.hhh-im-tools/$tag"
if [ ! -x "$root/bin/hhh-im-pack" ] && [ ! -x "$root/bin/hhh-im-pack.exe" ]; then
    cargo install --locked --git https://github.com/MagicLizi/hhh-im-sdk --tag "$tag" \
        hhh-im-protocol --features transport,package --root "$root"
fi
bin="$root/bin"
if command -v cygpath >/dev/null 2>&1; then
    bin="$(cygpath -w "$bin")"
fi
echo "$bin" >> "$GITHUB_PATH"
