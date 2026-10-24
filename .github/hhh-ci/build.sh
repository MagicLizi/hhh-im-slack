#!/usr/bin/env bash
# 按一个系统编译插件，把清单、标志与程序放进 dist/stage-<系统>/（之后由 assemble.sh 统一打包）。
# 本文件由 MagicLizi/hhh-release 统一维护（plugin-files/），插件仓库里不要直接改。
# 环境变量（由 hhh-plugin.yml 按 plan 的结果给）：
#   HHH_TARGET 系统  HHH_RUST_TARGET Rust 目标  HHH_EXT 程序扩展名  HHH_BINARY 程序名
#   HHH_LOGO 清单里的标志路径  HHH_CARGO_ARGS cargo build 额外参数  HHH_PREPARE 编译前先跑的命令
set -euo pipefail
target="$HHH_TARGET"
triple="$HHH_RUST_TARGET"

rustup target add "$triple"

if [ -n "${HHH_PREPARE:-}" ]; then
    echo "编译前准备：$HHH_PREPARE"
    bash -c "$HHH_PREPARE"
fi

case "$target" in
    macos-*)
        # 用 bindgen 的依赖要 libclang：用 Xcode 自带的。
        if [ -z "${LIBCLANG_PATH:-}" ]; then
            for candidate in "$(xcode-select -p)/Toolchains/XcodeDefault.xctoolchain/usr/lib" /Library/Developer/CommandLineTools/usr/lib; do
                if [ -f "$candidate/libclang.dylib" ]; then
                    export LIBCLANG_PATH="$candidate"
                    break
                fi
            done
        fi
        ;;
    wasm32-wasip2)
        # 依赖里有 C 代码（如 ring）要 wasi-sdk 的 clang；tokio 在 WASI 上的网络与文件要 tokio_unstable。
        : "${WASI_SDK_PATH:?没有 WASI_SDK_PATH}"
        export CC_wasm32_wasip2="$WASI_SDK_PATH/bin/clang"
        export AR_wasm32_wasip2="$WASI_SDK_PATH/bin/llvm-ar"
        export CFLAGS_wasm32_wasip2="--sysroot=$WASI_SDK_PATH/share/wasi-sysroot"
        export CARGO_TARGET_WASM32_WASIP2_RUSTFLAGS="--cfg tokio_unstable"
        ;;
esac

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
