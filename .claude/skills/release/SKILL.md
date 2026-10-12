---
name: release
description: 给Slack 插件发一个新版本：改版本号、打包、用官方私钥签名、核对、发布到 GitHub。用户说“发版本”“发布 x.y.z”“出个包”时用。
---

# 发版本

规矩以 hhh-im-sdk 的 `docs/release.md` 为准，这里只列步骤。Slack 是官方插件（保留编号 `slack`），HHH 只认官方私钥签过名的版本索引。
目前只发 `windows-x64`。

1. **定版本号**：比仓库里最新的正式版本大（`主.次.修订`）。改清单 `hhh-im-provider.json` 的 `version`，`Cargo.toml` 的 `version` 一起改。
2. **写更新说明** `notes.md`：Markdown，写给用户看，2000 字以内，改进和修复一条一行。
3. **本地核对**：
   - `cargo test`、`cargo fmt --check`、`cargo clippy --all-targets` 全部通过。
   - 跑 `powershell -File scripts/pack.ps1`，再跑 `hhh-im-check dist/stage/hhh-im-provider.json`，全部通过。
   - 在 HHH 里用「管理 → 从文件更新」装 `dist/` 里的 `.hhhim`，试一遍。
4. **提交并打标签**：标签写 `v<版本>`，和清单的版本完全一致。推送提交和标签。
5. **签名并发布**：
   - 发布令牌放环境变量 `HHH_IM_PUBLISH_TOKEN`（GitHub 要 Contents 写权限）。
   - 官方私钥在私有的密钥仓库 `hhh-keys` 里，用的时候直接指过去，**不复制进本仓库、不写进任何文件、不贴进对话**。
   - 运行：

     ```sh
     powershell -File scripts/pack.ps1 -SignKey <hhh-keys>/official/hhh-official-signing.key -Publish
     ```

6. **核对发布结果**：
   - Release 上有 `windows-x64` 的 `.hhhim`、`hhhim-release.json` 和 `hhhim-release.json.sig`。
   - 在 HHH 里打开设置 → IM，卡片显示“有新版本 x.y.z”，点更新能装上。

标签和清单版本对不上、版本没往上加，或者缺签名时，打包工具和 HHH 都会拒绝。照提示改，不要绕过。
