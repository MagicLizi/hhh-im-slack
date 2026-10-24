#!/usr/bin/env bash
# 用 dist/release/ 里的包与版本索引建一个草稿 Release（HHH 不读草稿，用户看不到）。
# 转正式（官方插件先签名）只在 MagicLizi/hhh-release 里做，这里的令牌只能建草稿。
# 本文件由 MagicLizi/hhh-release 统一维护（plugin-files/），插件仓库里不要直接改。
# 环境变量：HHH_VERSION 清单版本  HHH_REQUEST 发布请求编号  GH_TOKEN / GH_REPO
set -euo pipefail
tag="v$HHH_VERSION"
# 重跑同一版本时，先删掉上次留下的同名草稿。
for id in $(gh api "repos/$GH_REPO/releases?per_page=100" --jq ".[] | select(.draft == true and .tag_name == \"$tag\") | .id"); do
    echo "删掉旧草稿 $id"
    gh api -X DELETE "repos/$GH_REPO/releases/$id"
done
gh release create "$tag" --draft --target "$GITHUB_SHA" --title "$tag" --notes-file dist/release/notes.md \
    dist/release/*.hhhim dist/release/hhhim-release.json
{
    echo "### 已建草稿 $tag"
    echo
    echo "- 发布请求：${HHH_REQUEST}"
    echo "- 提交：\`$GITHUB_SHA\`"
    echo "- 回到 hhh-release 运行「插件发布 ② 签名并转正式」后，用户才看得到。"
} >> "$GITHUB_STEP_SUMMARY"
