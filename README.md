# Slack（HHH IM 插件）

HHH 的 Slack 插件：在 HHH 里看 Slack 的频道、私聊、群私聊与话题，回消息、发图片和文件、编辑、撤回、点表情回应。

它是从 HHH 内置的 Slack 搬出来的独立插件，从 [HHH IM 插件模板](https://github.com/MagicLizi/hhh-im-plugin-template) 起，走 HHH 的
统一插座 HHH-IM-Protocol v1。界面、登录和各种说明文字都和内置时一样，本机已经存下的聊天记录和头像照样能用。装上后会接过内置
Slack 的登录，不用重新登录。

## 能做什么

- **登录**：只有“通过中继登录”一种：登录框里的中继地址默认填好 HHH 的中继，点“登录”就打开 Slack 的授权页（HHH 的 Slack 应用，
  经授权回调中继领回授权码，本机用 PKCE 换令牌，不要 Client Secret）。也可以改成自己部署的中继：中继上配好自己的 Slack 应用后，
  登录与续期都用那个应用（Slack 的回调只收 https，所以没有“自己的应用 + 本机回调”）。卡片写“名字 · 工作区”；缺权限时卡片
  下面写缺哪几项、点重新登录补上。
- **续期**：登录快过期时自动续期，Slack 的刷新令牌只能用一次，换到的新授权先交给 HHH 存好再用。HHH 没在用这个插件时，也会按时叫它续期。
- **读取**：会话列表、搜频道、搜人、找私聊、成员、消息与话题、表情回应、人员与机器人的资料。不在列表里的会话有了新消息，由 HHH
  按会话列表逐个限频查出来再加进列表（不后台调用新版搜索，登录也不申请搜索权限）。
- **内容**：文字、提及（人、用户组、`@here`、`@channel`）、格式正文（mrkdwn 与 rich_text）、卡片（Block Kit 与旧版附件、链接预览）、
  图片、文件、视频封面、工作区自定义表情。
- **发送与动作**：在话题里回复、同时发到频道、@某人与 @所有人、发图片和文件、编辑、撤回、加表情回应、去掉表情回应。
- **格式发送与编辑**：粗体、斜体、删除线、行内代码、链接、列表、引用和代码块转成原生 `rich_text`；表情与提及保留结构，编辑草稿保留可完整写回的格式。Slack 不声明下划线。
- **资源**：图片、文件、头像、表情图都分段交给 HHH。
- **本人阅读位置**：按会话读取平台给出的精确 `last_read`，缺失时保持未知；本轮不声明写回或独立话题回读。Slack 没有他人阅读回执，不显示“对方已读”。

聊天记录、凭据和配置都由 HHH 保存，插件自己不存。

## 构建

格式发送使用 SDK 的 `Outgoing::Rich` 与 `capabilities.richSend`。阅读扩展尚待 SDK 正式发布及固定标签升级；当前任务通过仓库外的 `sdk-local.toml` 覆盖验证，尚不能只靠清单里的旧公开标签构建。正式构建仍使用已发布的固定标签。读取额度保留省略声明时的兼容路径。

```sh
cargo build --release
cargo test
cargo fmt --check
cargo clippy --all-targets
```

程序是 `target/release/hhh-im-slack`，Windows 上是 `hhh-im-slack.exe`。Windows 上按窗口程序编译，HHH 拉起它时不弹控制台窗口。

协议服务、日志、资源分段、上传与请求计数用协议包 `hhh-im-protocol` 的插件运行时（`runtime` 部件）。

## 自检

装自检和打包工具：

```sh
cargo install --git https://github.com/MagicLizi/hhh-im-sdk --tag v0.14.0 hhh-im-protocol --features transport,package
```

打包后自检，全部通过才算改好：

```sh
powershell -File scripts/pack.ps1
hhh-im-check dist/stage/hhh-im-provider.json
```

## 在 HHH 里试

| 方式 | 什么时候用 |
|---|---|
| 设置 → IM → 添加 IM → 文件夹 | 开发时，选 `dist/stage` |
| 添加 IM → 安装包 | 选 `dist/` 里的 `.hhhim`，和用户装的一样 |
| 卡片「管理 → 从文件更新」 | 已经装过，换成新打的包或文件夹 |

## 发版本

发布在私有中控仓库 `MagicLizi/hhh-release` 里统一做（只有维护者能操作）：

- **让 agent 发**：在 HHH 仓库说“发布Slack插件”，agent 用 `hhh-im-release` skill 读改动、拟版本号和更新说明，你确认后一次发完。
- **自己点**：hhh-release 的 Actions →「插件发布（一键）」，选插件、填版本号和更新说明。

两种方式都会把版本号和说明合进 main，再编 Windows、M 芯片 Mac、Intel Mac 的包、核对、签官方签名、转正式。要出哪些系统写在 `.github/hhh-plugin.json`。
详细规矩见 [hhh-im-sdk 的发版本规范](https://github.com/MagicLizi/hhh-im-sdk/blob/main/docs/release.md)。

## 仓库里有什么

| 位置 | 内容 |
|---|---|
| `src/slack/` | 从 HHH 搬来的 Slack 适配器：Slack 接口、登录与续期、翻成协议的统一格式、各协议方法的做法 |
| `src/server/` | 协议方法：握手声明、登录、续期、交接、读取与动作 |
| `tests/fixtures/slack/` | Slack 响应的取样（人名、编号和内容都是编的） |
| `tests/fixtures/slack-protocol/` | 协议样例：插件发给 HHH 的线上消息与握手结果。这些样例会拷进 HHH，用宿主真正的翻译再核对一遍 |
| `scripts/pack.ps1` | 打包与发布 |

## 许可证

[MIT](LICENSE)

## 图片的预览与原件

Slack 已生成的多档缩略图用于预览，url_private_download/url_private 对应的原件保留原始格式；视频封面只声明预览。

新宿主经 `resources/describe` 选择实际版本，读取时带 `variant`；省略 `variant` 的旧调用仍按原来的方式读取；二进制安装仍需通过宿主的最低 SDK 检查。分段缓存包含实际版本，不把预览当成原件。本版固定使用 IM SDK v0.14.0，宿主需支持该版 SDK。
