#!/usr/bin/env bash
# 编译前准备（lint、测试、编译都用得上）：装 Rust 目标、跑插件配置里这个系统的准备命令，把编译要的环境写进 GITHUB_ENV。
# 本文件由 MagicLizi/hhh-release 统一维护（plugin-files/），插件仓库里不要直接改。
# 环境变量：HHH_TARGET 系统  HHH_RUST_TARGET Rust 目标  HHH_PREPARE 编译前先跑的命令（可空）
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
        # 用 bindgen 的依赖要 libclang：用 Xcode 自带的（插件自己的 .cargo/config.toml 不强制时，环境变量优先）。
        if [ -z "${LIBCLANG_PATH:-}" ]; then
            for candidate in "$(xcode-select -p)/Toolchains/XcodeDefault.xctoolchain/usr/lib" /Library/Developer/CommandLineTools/usr/lib; do
                if [ -f "$candidate/libclang.dylib" ]; then
                    echo "LIBCLANG_PATH=$candidate" >> "$GITHUB_ENV"
                    echo "libclang：$candidate"
                    break
                fi
            done
        fi
        ;;
    wasm32-wasip2)
        # 依赖里有 C 代码（如 ring）要 wasi-sdk 的 clang；tokio 在 WASI 上的网络与文件要 tokio_unstable。
        : "${WASI_SDK_PATH:?没有 WASI_SDK_PATH}"
        {
            echo "CC_wasm32_wasip2=$WASI_SDK_PATH/bin/clang"
            echo "AR_wasm32_wasip2=$WASI_SDK_PATH/bin/llvm-ar"
            echo "CFLAGS_wasm32_wasip2=--sysroot=$WASI_SDK_PATH/share/wasi-sysroot"
            echo "CARGO_TARGET_WASM32_WASIP2_RUSTFLAGS=--cfg tokio_unstable"
        } >> "$GITHUB_ENV"
        ;;
esac
