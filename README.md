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
- **资源**：图片、文件、头像、表情图都分段交给 HHH。

聊天记录、凭据和配置都由 HHH 保存，插件自己不存。

## 构建

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
cargo install --git https://github.com/MagicLizi/hhh-im-sdk --tag v0.4.0 hhh-im-protocol --features transport,package
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

Slack 是官方插件：HHH 只认官方私钥签过名的版本索引。目前只发 Windows 的包（`windows-x64`）。

1. 改清单 `hhh-im-provider.json` 和 `Cargo.toml` 里的 `version`，写好 `notes.md`（给用户看的更新说明）。
2. 提交，打标签 `v<版本>`，推送提交和标签。
3. 发布令牌放在环境变量 `HHH_IM_PUBLISH_TOKEN` 里。签名私钥在私有的密钥仓库 `hhh-keys` 里，**不进这个仓库**：

   ```sh
   powershell -File scripts/pack.ps1 -SignKey <hhh-keys>/official/hhh-official-signing.key -Publish
   ```

   打包工具写出 `.hhhim`、版本索引 `hhhim-release.json` 和签名 `hhhim-release.json.sig`，三样一起传到 GitHub Release。

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
