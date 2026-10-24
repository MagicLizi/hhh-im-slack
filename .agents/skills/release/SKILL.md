---
name: release
description: 给Slack插件发一个新版本：改版本号与更新说明、合进 main，再由维护者在私有中控仓库 MagicLizi/hhh-release 里编全部系统、建草稿、签名、转正式。用户说“发版本”“发布 x.y.z”“出个包”时用。
---

# 发版本

发布只在私有中控仓库 `MagicLizi/hhh-release` 里做（只有维护者能操作）。本仓库的 `.github/workflows/hhh-plugin.yml` 与 `.github/hhh-ci/` 由 hhh-release 统一维护，只负责编译、核对和建草稿，不在这里直接改；要出哪些系统写在 `.github/hhh-plugin.json`（现在：Windows、M 芯片 Mac、Intel Mac），清单 `os` 要包含对应系统。规矩以 hhh-im-sdk 的 `docs/release.md` 为准。
Slack是官方插件（保留编号 `slack`），HHH 只认官方私钥签过名的版本索引；签名在 hhh-release 的第二步自动做，私钥只在 hhh-release 的 Secrets 里。

1. **定版本号**：比仓库里最新的正式版本大（`主.次.修订`），清单 `hhh-im-provider.json` 与 `Cargo.toml`的 `version` 一起改。
2. **写更新说明** `notes.md`：Markdown，写给用户看，2000 字以内，改进和修复一条一行。
3. **本地核对**：按 `AGENTS.md`「改完必须」跑一遍；在 HHH 里「管理 → 从文件更新」装本机打的包试一遍。
4. **开 PR 合进 main**：PR 上的 `hhh-plugin` 检查要全部通过（要出的系统都会编一遍，并试着合一次包）。不用自己打标签，转正式时标签自动打在这次的提交上。
5. **编译并建草稿**：在 hhh-release 的 Actions 运行「插件发布 ① 编译并建草稿」，插件选 `slack`。它让本仓库在 main 上编全部系统、合成一份版本索引、建草稿 Release（HHH 不读草稿，用户看不到），再逐个核对包的大小与 sha256。看结果汇总：版本、每个系统的包、需要的 IM SDK、更新说明。
6. **签名并转正式**：确认无误后运行「插件发布 ② 签名并转正式」，插件选 `slack`，版本填第 5 步的版本号。它先签官方签名、用 HHH 内置的公钥验过，再转正式。
7. **核对**：Release `v<版本>` 上有每个系统的 `.hhhim` 与 `hhhim-release.json`、`hhhim-release.json.sig`；在 HHH 里打开设置 → IM，卡片写“有新版本 x.y.z”，点更新能装上。

agent 只做 1–4；第 5、6 步由维护者决定什么时候运行，用户明确要求发布时才代为触发。
新版本的系统比上一个正式版本少、版本没往上加、版本已经发过、草稿和编出来的不一致时，工作流会拒绝；照提示改，不要绕过。确实不再出某个系统，写进 `.github/hhh-plugin.json` 的 `dropTargets`。

CI 用不了时的备用办法：维护者在自己电脑上按发版本规范用 `hhh-im-pack --publish` 手动发布（官方插件带 `--sign-key`）；令牌和私钥都不写进任何文件、不贴进对话。
