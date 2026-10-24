#!/usr/bin/env bash
# 把各系统编好的 dist/stage-<系统>/ 打成 .hhhim，合进同一份版本索引 dist/release/hhhim-release.json，并核对：
#   - 发布时：这个版本还没发过，且比上一个正式版本大；
#   - 系统不比上一个正式版本少（有意不再出的写进配置的 dropTargets）。
# 本文件由 MagicLizi/hhh-release 统一维护（plugin-files/），插件仓库里不要直接改。
# 环境变量：HHH_VERSION 清单版本  HHH_TARGETS 这次的系统（空格分开）  HHH_DROP_TARGETS 有意不再出的系统
#           HHH_RELEASE true 时按发布核对  GH_TOKEN / GH_REPO 读本仓库的 Release
set -euo pipefail
version="$HHH_VERSION"
tag="v$version"
out="dist/release"
rm -rf "$out"
mkdir -p "$out"

# 上一个正式版本（草稿、预发布不算）。
previous="$(gh release view --json tagName --jq .tagName 2>/dev/null || true)"
echo "上一个正式版本：${previous:-无}"

after=()
if [ "${HHH_RELEASE:-false}" = "true" ]; then
    published="$(gh api "repos/$GH_REPO/releases?per_page=100" --jq ".[] | select(.draft == false and .tag_name == \"$tag\") | .tag_name")"
    if [ -n "$published" ]; then
        echo "::error::$tag 已经发布过了：先在清单与 Cargo.toml 里把版本号往上加，合进 main 再发布。"
        exit 1
    fi
    if [ -n "$previous" ]; then
        after=(--after "${previous#v}")
    fi
fi

# 更新说明：notes.md 不超过 2000 字时整份用；超过时从最新的 “### 版本” 小节往下取整节，取到不超过 2000 字为止。
# 发布时必须有这个版本的小节（没写这次的更新说明就不发）。
notes="$out/notes.md"
python3 - notes.md "$notes" "$version" "${HHH_RELEASE:-false}" <<'PY'
import re, sys
source, target, version, release = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4] == "true"
LIMIT = 2000
text = open(source, encoding="utf-8").read().strip() + "\n"
heads = list(re.finditer(r"(?m)^#{1,6}\s*v?(\d+\.\d+\.\d+)\s*$", text))
if not any(h.group(1) == version for h in heads):
    message = f"notes.md 里没有 “### {version}” 这一节：先写这次的更新说明。"
    if release:
        print(f"::error::{message}")
        sys.exit(1)
    print(f"::warning::{message}")
if len(text) > LIMIT:
    if not heads:
        print(f"::error::notes.md 超过 {LIMIT} 字，又没有按 “### 版本” 分节，取不出这次的说明。")
        sys.exit(1)
    sections = [text[h.start():(heads[i + 1].start() if i + 1 < len(heads) else len(text))].strip() for i, h in enumerate(heads)]
    chosen = []
    for section in sections:
        if len("\n\n".join(chosen + [section])) + 1 > LIMIT:
            break
        chosen.append(section)
    if not chosen:
        print(f"::error::notes.md 最新一节就超过 {LIMIT} 字，写短一些。")
        sys.exit(1)
    text = "\n\n".join(chosen) + "\n"
    print(f"notes.md 超过 {LIMIT} 字，取最新的 {len(chosen)} 节（{len(text)} 字）。")
open(target, "w", encoding="utf-8").write(text)
PY

for target in $HHH_TARGETS; do
    stage="dist/stage-$target"
    if [ ! -f "$stage/hhh-im-provider.json" ]; then
        echo "::error::缺少 $target 编好的文件（${stage}）。"
        exit 1
    fi
    hhh-im-pack "$stage" --target "$target" --out "$out" --notes "$notes" --tag "$tag" ${after[@]+"${after[@]}"}
done

# 系统不能比上一个正式版本少。
if [ -n "$previous" ]; then
    previous_index="$RUNNER_TEMP/previous-index.json"
    if gh release view "$previous" --json assets --jq '.assets[].name' | grep -qx 'hhhim-release.json'; then
        gh release download "$previous" -p hhhim-release.json -O "$previous_index" --clobber
        missing="$(python3 - "$previous_index" "$out/hhhim-release.json" "${HHH_DROP_TARGETS:-}" <<'PY'
import json, sys
old = {p["target"] for p in json.load(open(sys.argv[1], encoding="utf-8"))["packages"]}
new = {p["target"] for p in json.load(open(sys.argv[2], encoding="utf-8"))["packages"]}
drop = set(sys.argv[3].split())
print(" ".join(sorted(old - new - drop)))
PY
)"
        if [ -n "$missing" ]; then
            if [ "${HHH_RELEASE:-false}" = "true" ]; then
                echo "::error::上一个正式版本 $previous 有 $missing 的包，这次没有：这些设备上的用户会停在旧版、新用户装不上。确实不再出就写进 .github/hhh-plugin.json 的 dropTargets。"
                exit 1
            fi
            echo "::warning::这次只检查了部分系统，没编 ${missing}（发布时会编全）。"
        fi
    else
        echo "::warning::上一个正式版本 $previous 没有 hhhim-release.json，跳过系统对比。"
    fi
fi

python3 - "$out/hhhim-release.json" "${previous:-}" <<'PY' >> "${GITHUB_STEP_SUMMARY:-/dev/null}"
import json, sys
index = json.load(open(sys.argv[1], encoding="utf-8"))
print(f"### {index.get('name') or index['id']} {index['version']}")
print()
print(f"- 需要的 IM SDK：{index.get('minHost') or '-'}")
print(f"- 上一个正式版本：{sys.argv[2] or '无'}")
print("- 包：")
for p in index["packages"]:
    print(f"  - {p['target']}：`{p['file']}`（{p['size']} 字节）")
PY
echo "版本索引："
cat "$out/hhhim-release.json" | head -c 2000
echo
ls -la "$out"
