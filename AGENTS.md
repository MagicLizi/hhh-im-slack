# Slack 插件的规矩（给 agent）

这个仓库是 HHH 的 Slack IM 插件，从 HHH 内置的 Slack 搬出来、从 [IM 插件模板](https://github.com/MagicLizi/hhh-im-plugin-template)
起的，走 HHH-IM-Protocol v1。不管用哪个 agent 打开，都按这里的规矩做；`CLAUDE.md` 只是指到这里。

## 先读

1. 协议：[hhh-im-sdk/docs/protocol-v1.md](https://github.com/MagicLizi/hhh-im-sdk/blob/main/docs/protocol-v1.md)。每个方法的参数和结果都以它为准，
   Rust 类型在 `hhh-im-protocol` 的 `types`、`method`、`platform`、`content` 模块里。
2. 发版本规范：[hhh-im-sdk/docs/release.md](https://github.com/MagicLizi/hhh-im-sdk/blob/main/docs/release.md)。
3. 本仓库的结构：

   | 位置 | 内容 |
   |---|---|
   | `src/slack/` | 从 HHH 搬来的 Slack 适配器：和 Slack 说话（`api.rs`、`errors.rs`）、登录与续期（`auth.rs`）、把 Slack 的格式直接翻成协议的统一格式（`translate.rs`、`rich.rs`、`mrkdwn.rs`、`card.rs`、`emoji.rs`、`names.rs`），各协议方法在 Slack 上的做法（`methods.rs`） |
   | `src/server/` | 协议方法：`mod.rs` 按方法名分派，`declare.rs` 写握手声明，`session.rs` 管会话、续期与交接，`login.rs` 和 `relay.rs` 管登录，`methods.rs` 把参数交给 Slack 适配器，`credential.rs` 是插件自己的凭据 |
   | `src/net.rs`、`src/grant.rs`、`src/error.rs` | 统一的 TLS 握手、本人授权、错误 |
   | `tests/fixtures/slack/` | Slack 响应的取样（人名、编号和内容都是编的） |
   | `tests/fixtures/slack-protocol/` | 协议样例：取样翻成的线上消息与握手结果，会拷进 HHH，用宿主真正的翻译再核对一遍 |

## 每个插件都要的那一半，用运行时

协议服务（并发处理、`$/cancel`、等宿主回话）、日志转 `log/write`、资源分段、上传攒齐与核对、平台请求计数都用协议包的插件运行时
（`hhh_im_protocol::runtime`），这里不另写一份。向 Slack 发读取请求的出口（`SlackClient::query`）每次调一下 `metering::request()`。

## 第一原则：体验和内置 Slack 一样

用户从内置 Slack 换成这个插件时，界面、说明文字、登录、续期和本机记录都不能有区别：

- 翻译规则原样搬自内置 Slack，只是输出直接是协议的类型。线上消息经 HHH 宿主翻回来，要与内置 Slack 翻同一份原始数据一字不差：
  `src/slack/goldens.rs` 锁住插件的输出，HHH 的 `hhh-im/src/provider/slack_parity_tests.rs` 用宿主真正的翻译核对。
- 握手声明（`declare.rs`）照内置 Slack 的平台表逐项写，HHH 那边逐项对照。
- 编号种类（`idType`，如 `user_id`）、头像键（Slack 给的完整地址）都照内置时的写法，本机的旧记录才认得出是同一个人、同一个头像。
- 续期和失效的说法和 Friend 内置时一样，分两处写（`session.rs` 的 `error_text`、`card_text`）：
  - 授权失效的错误写 Friend 的 `ReauthReason::message`；
  - `session/expired` 和 `session/open` 的需要登录写重新登录卡片上的那几句（内置 Slack 的 `reauth_text`）。
- 登录与续期照 Friend 内置时：HHH 的 Slack 应用（只要 Client ID）、PKCE、经授权中继领授权码；刷新令牌只能用一次，换到新的先经
  `credential/save` 存好、HHH 回了成功再用；交接来的旧授权若是用户自填的 Client ID 发的，续期照旧用那个 Client ID。
  1.0.2 起登录框里有“通过中继登录”一种方式（`declare.rs`）：中继地址预填 HHH 的中继、用户可改；中继配了自己的 Slack 应用时
  登记回执带它的 Client ID（中继契约 server/slack-refresh v2），登录、换码与续期都用它，凭据记下应用与中继（HHH 的不记）。
  Slack 的回调只收 https，不做“自己的应用 + 本机回调”。
- 已知的差别（用户看不出）：图片与头像经插件读进 HHH 的文件库再显示，不从 Slack 的 CDN 直接显示（飞书插件同样）。

做不到一样时，不要在插件里绕路或伪造协议没有的东西。到 [hhh-im-sdk](https://github.com/MagicLizi/hhh-im-sdk/issues) 提 issue，
写清楚要什么、为什么所有 IM 都可能用得上。HHH 的 IM 层不认识任何一家 IM，不要求它给 Slack 开特例。

## 插件只做三件事

- **和 Slack 说话**：登录、读消息、发消息、读资源。
- **翻成统一格式**：Slack 的消息、会话、人员、表情都翻成协议的类型，Slack 私有的写法（mrkdwn、Block Kit）不能原样漏给 HHH。
- **如实声明**：`initialize` 里声明平台能做什么，做不到的不声明。

聊天记录、凭据和配置都交给 HHH，插件自己不存：

- 凭据通过 `credential/save` 交给 HHH，等 HHH 回复成功后才用，下次从 `session/open` 拿回来。
- 临时文件只放在 `initialize` 给的 `tempDir` 里。
- 标准输出只写协议消息，日志走 `log/write`。
- `src/main.rs` 开头的 `windows_subsystem` 不要删：HHH 拉起插件时不弹控制台窗口，标准输入输出照样能用。

## 夹具与样例

- **夹具里不放真实数据**：人名、频道名、消息、文件的编号、工作区内容一律用编的。新取样先换成编的，再提交。
- 改了翻译、样例要跟着变时，用 `HHH_UPDATE_GOLDENS=1 cargo test` 重写 `tests/fixtures/slack-protocol/`，再逐个看差异，确认每一处变化都是有意的。
- 样例有变化时，在交付说明里列出要拷进 HHH 的文件。

## 改完必须

1. `cargo build --release`、`cargo test`、`cargo fmt --check`、`cargo clippy --all-targets` 全部通过（零警告）。
2. 打包后自检：`powershell -File scripts/pack.ps1`，再跑 `hhh-im-check dist/stage/hhh-im-provider.json`，全部通过。
   自检工具没装时：`cargo install --git https://github.com/MagicLizi/hhh-im-sdk --tag v0.4.0 hhh-im-protocol --features transport,package`。
3. 在 HHH 里试一遍：设置 → IM → 添加 IM → 文件夹，选 `dist/stage`。登录、读、发、续期都走一遍。

## 清单 `hhh-im-provider.json`

`id` 是 `slack`，**不能改**：HHH 靠它认出这个插件和内置 Slack 的旧记录，而且它是保留编号，只认官方签名。`imType` 也是 `slack`。
`version` 和 `Cargo.toml` 的版本保持一致，`tests/manifest.rs` 会核对。目前 `os` 只写 `windows`，只发 `windows-x64` 的包。

## 发版本

只按发版本规范来，用「发版本」skill：

1. 改 `version`，写 `notes.md`。
2. 打标签 `v<版本>` 并推送。
3. 跑 `scripts/pack.ps1 -SignKey <hhh-keys>/official/hhh-official-signing.key -Publish`。

- 官方私钥在私有的密钥仓库 `hhh-keys` 里，**永远不进这个仓库**：不复制进来，不写进脚本或配置，也不贴进对话。
- 发布令牌只放环境变量 `HHH_IM_PUBLISH_TOKEN`，同样不写进文件、不贴进对话。
