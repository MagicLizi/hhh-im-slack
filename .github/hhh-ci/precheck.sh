#!/usr/bin/env bash
# 发布前先查（在编各系统之前，省得编完才发现）：这个版本还没发过、比上一个正式版本大、notes.md 写了这一版的说明。
# 合包时 assemble.sh 还会再查一遍。
# 本文件由 MagicLizi/hhh-release 统一维护（plugin-files/），插件仓库里不要直接改。
# 环境变量：HHH_VERSION 清单版本  GH_TOKEN / GH_REPO 读本仓库的 Release
set -euo pipefail
gh api "repos/$GH_REPO/releases?per_page=100" > "$RUNNER_TEMP/releases.json"
python3 - "$RUNNER_TEMP/releases.json" "$HHH_VERSION" notes.md <<'PY'
import json, re, sys
releases, version, notes = json.load(open(sys.argv[1], encoding="utf-8")), sys.argv[2], sys.argv[3]

def semver(text):
    found = re.fullmatch(r"v?(\d+)\.(\d+)\.(\d+)", text)
    return tuple(map(int, found.groups())) if found else None

published = [r for r in releases if not r["draft"] and not r["prerelease"] and semver(r["tag_name"])]
problems = []
if any(r["tag_name"] == f"v{version}" for r in published):
    problems.append(f"v{version} 已经发布过了：先在清单与 Cargo.toml 里把版本号往上加，合进 main 再发布。")
latest = max(published, key=lambda r: semver(r["tag_name"]), default=None)
if latest and semver(version) <= semver(latest["tag_name"]) and latest["tag_name"] != f"v{version}":
    problems.append(f"{version} 要比上一个正式版本 {latest['tag_name']} 大。")
text = open(notes, encoding="utf-8").read()
if not re.search(rf"(?m)^#{{1,6}}\s*v?{re.escape(version)}\s*$", text):
    problems.append(f"notes.md 里没有 “### {version}” 这一节：先写这次的更新说明。")
for problem in problems:
    print(f"::error::{problem}")
if problems:
    sys.exit(1)
print(f"可以发布 {version}（上一个正式版本：{latest['tag_name'] if latest else '无'}）。")
PY
