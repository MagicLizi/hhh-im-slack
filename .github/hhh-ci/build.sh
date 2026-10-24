#!/usr/bin/env bash
# 按一个系统编译插件，把清单、标志与程序放进 dist/stage-<系统>/（之后由 assemble.sh 统一打包）。
# 编译要的环境由前一步 prepare.sh 写好。
# 本文件由 MagicLizi/hhh-release 统一维护（plugin-files/），插件仓库里不要直接改。
# 环境变量（由 hhh-plugin.yml 按 plan 的结果给）：
#   HHH_TARGET 系统  HHH_RUST_TARGET Rust 目标  HHH_EXT 程序扩展名  HHH_BINARY 程序名
#   HHH_LOGO 清单里的标志路径  HHH_CARGO_ARGS cargo build 额外参数
set -euo pipefail
target="$HHH_TARGET"
triple="$HHH_RUST_TARGET"

# shellcheck disable=SC2086 # 额外参数按空格分开
cargo build --release --locked --target "$triple" ${HHH_CARGO_ARGS:-}

stage="dist/stage-$target"
rm -rf "$stage"
mkdir -p "$stage"
cp hhh-im-provider.json "$stage/"
if [ -n "${HHH_LOGO:-}" ]; then
    mkdir -p "$stage/$(dirname "$HHH_LOGO")"
    cp "$HHH_LOGO" "$stage/$HHH_LOGO"
fi
cp "target/$triple/release/$HHH_BINARY$HHH_EXT" "$stage/"
echo "已放进 ${stage}："
ls -la "$stage"
