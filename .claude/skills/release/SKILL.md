---
name: release
description: 给这个 HHH IM 插件发一个新版本：用本机 gh 读改动、拟版本号与更新说明，和用户确认后经 MagicLizi/hhh-release 一次发完（改版本、编全部系统、核对、签名、转正式）。用户说“发版本”“发布 x.y.z”“出个包”时用。
---

# 发版本

本文件由 MagicLizi/hhh-release 统一维护（plugin-files/），不在这里改。

发布步骤正本在私有中控仓库 MagicLizi/hhh-release 的 `RELEASING.md`：先读它再照做——

```sh
gh api repos/MagicLizi/hhh-release/contents/RELEASING.md --jq .content | base64 -d
```

无论正本怎么写，这几条都要守：

- 正式发布前必须让用户确认插件、版本号、要出的系统和更新说明全文；确认前不改任何仓库、不触发工作流。
- 只用本机 `gh` 与 hhh-release 的工作流；不读、不打印 Secrets 与私钥，不在本机签名或用 `hhh-im-pack --publish` 手动发布。
- 失败不自动重试，把原因与当前状态告诉用户。
