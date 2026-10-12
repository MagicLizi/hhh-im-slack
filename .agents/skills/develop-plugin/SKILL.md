---
name: develop-plugin
description: 改 Slack 插件：修 bug、加或改协议方法、跟上 HHH 协议或宿主翻译的变化、补取样。用户说“修一下 xx”“支持 xx 消息”“协议加了 xx 字段”“HHH 那边翻译改了”时用。
---

# 改 Slack 插件

先读 `AGENTS.md`（规矩与仓库结构）和协议 `docs/protocol-v1.md`（在 hhh-im-sdk 仓库里）。第一原则是体验和内置 Slack 一样。
协议服务、日志、资源分段、上传与请求计数用协议包的插件运行时，这里只写 Slack 的事。

## 找地方

- Slack 接口、Slack 格式怎么翻成协议的统一格式：`src/slack/`（`translate.rs` 是入口，正文在 `rich.rs`、`mrkdwn.rs`，卡片在 `card.rs`）。
- 各协议方法在 Slack 上怎么做：`src/slack/methods.rs`。
- 协议方法：`src/server/mod.rs` 按方法名分派。握手声明在 `declare.rs`，会话、续期与交接在 `session.rs`，登录在 `login.rs` 和 `relay.rs`，
  参数交给 Slack 适配器在 `methods.rs`。

## 改一处

1. 先写测试再改代码：
   - Slack 那一侧：用 `tests/slack_http.rs`、`tests/slack_unified.rs`、`tests/slack_people.rs` 里的假 Slack（都经协议调）。
   - 协议那一侧：用 `tests/server.rs` 里的进程内宿主（`tests/common/mod.rs`），宿主和插件按协议说话。
   - 翻译：用 `src/slack/goldens.rs`，取样 → 线上消息，样例锁住。
2. 新取样放进 `tests/fixtures/slack/`，人名、编号、内容一律换成编的。
3. 错误用协议的错误分类（`src/error.rs`、`src/slack/errors.rs`），给用户看的原因写人话，和内置时的说法一样。
4. 协议表达不了时，到 hhh-im-sdk 提 issue，不在插件里绕路。

## HHH 那边变了

- **协议加了字段**：重新构建，按编译错误一处处补上；能对齐内置时的就照内置写，再在 `declare.rs` 或 `goldens.rs` 补测试。
- **宿主翻译改了**：重写样例后拷进 HHH 的 `hhh-im/tests/fixtures/slack-protocol/`，跑 HHH 的 `slack_parity_tests`，让宿主翻回来与内置 Slack 重新一字不差。

## 每次改完

1. `cargo test`、`cargo fmt --check`、`cargo clippy --all-targets` 全部通过。
2. 样例要变时，用 `HHH_UPDATE_GOLDENS=1 cargo test` 重写 `tests/fixtures/slack-protocol/`，逐个看差异，在交付说明里列出要拷进 HHH 的文件。
3. 打包并自检：`powershell -File scripts/pack.ps1`，再跑 `hhh-im-check dist/stage/hhh-im-provider.json`。
4. 在 HHH 里试：设置 → IM → 添加 IM → 文件夹，选 `dist/stage`；已经装过就在卡片里点「管理 → 从文件更新」。登录、读、发都走一遍。

## 不要

- 不在插件里存聊天记录、凭据或配置（都交给 HHH）。
- 不往标准输出写协议以外的东西（日志走 `log/write`）。
- 不改清单的 `id`（`slack`）。
- 不把真实的人名、编号、工作区内容写进夹具、测试或注释。
