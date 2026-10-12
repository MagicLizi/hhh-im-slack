# 写 HHH IM 插件的规矩（给 agent）

这个仓库是一个 HHH IM 插件。不管用哪个 agent 打开，都按这里的规矩写；`CLAUDE.md` 只是指到这里。

## 先读

1. 协议：[hhh-im-sdk/docs/protocol-v1.md](https://github.com/MagicLizi/hhh-im-sdk/blob/main/docs/protocol-v1.md)。每个方法的参数与结果都以它为准，
   Rust 类型在 `hhh-im-protocol`（`types`、`method`、`platform`、`content` 模块）。
2. 发版本规范：[hhh-im-sdk/docs/release.md](https://github.com/MagicLizi/hhh-im-sdk/blob/main/docs/release.md)。
3. 本仓库的 `src/main.rs`：最小可用的骨架（不用登录、一个示例群、发什么回什么），从它改起。

## 每个插件都要的那一半，用运行时

协议包的插件运行时（`hhh-im-protocol` 的 `runtime` 部件，`hhh_im_protocol::runtime`）管和平台无关的事，插件不要自己写，
也不要从别的插件抄：

- **协议服务**：`serve_on` / `serve_stdio` 读写标准输入输出，宿主的每个请求各开一个任务并发处理（查询不被下载、登录堵住），
  回复与推送排进同一条队列按顺序写；`$/cancel` 停掉对应的请求。插件实现 `Plugin`：收到某个方法怎么办。
- **向宿主发请求并等回复**：`Host::request`（`credential/save`、`events/push` 先存后用都靠它）；通知用 `Host::notify`。
- **日志**：`install_log` 之后 tracing 的日志转成 `log/write` 交给 HHH；标准输出不写别的东西。
- **资源分段**：`Segments::read` 管 `resources/read`、`avatars/read` 一段一段地读（顺着读复用同一条读流）；插件只提供“从某个位置
  打开一条读流”。整份在内存里的小东西用 `chunk_of_bytes`。
- **上传**：`Uploads` 管 `uploads/begin`、`chunk`、`finish` 攒齐、核对长度与 SHA-256；插件只把整份交给平台。
- **请求计数**：向平台发请求的出口每次调一下 `metering::request()`，运行时随结果报给 HHH（`platformRequests`）。

## 插件只做三件事

- **和平台说话**：登录、收消息、发消息、读资源，都在插件里。
- **翻成统一格式**：平台的消息、会话、人员、表情翻成协议的类型；平台特有的写法不要原样漏给 HHH。
- **声明能力与参数**：`initialize` 里如实声明平台能做什么（`platform`、`capabilities`）和要用户填什么（`settings`）；做不到的不声明。

聊天记录、凭据、配置都交给 HHH，插件自己不存：

- 凭据（登录后的令牌、会话材料）用 `credential/save` 交给 HHH；下次启动从 `session/open` 的 `credential` 拿回来。
- 同步位置随 `events/push` 的 `syncState` 交给 HHH（只推一次的平台必带）；下次从 `session/open` 的 `syncState` 接着推。
- 用户要填的参数在 `settings` 里声明，HHH 生成表单、保存、在 `session/open` 的 `settings` 里给你。
- 临时文件只放 `initialize` 给的 `tempDir`（随时可能被清空）。

Windows 上程序按窗口程序编（`src/main.rs` 开头的 `windows_subsystem`），HHH 拉起时不弹控制台窗口；标准输入输出照样能用，不要删。

## 协议表达不了时

提给协议（在 [hhh-im-sdk](https://github.com/MagicLizi/hhh-im-sdk/issues) 提 issue，写清楚要什么、为什么所有 IM 都可能需要），
不要求 HHH 给自己开特例，也不在插件里绕路伪造协议没有的东西。HHH 的 IM 层不认识任何一家 IM，所有 IM 的界面与交互都一样。

## 清单 `hhh-im-provider.json`

| 字段 | 怎么填 |
|---|---|
| `id` | 插件编号：小写字母开头，只含 `a-z0-9-`，2–32 字符。**发布后不能改**（HHH 靠它认同一个插件，改了聊天记录不跟过去） |
| `name` | 给用户看的名字 |
| `version` | `主.次.修订`，每次发版本往上加 |
| `command` | 程序相对清单的路径；不写扩展名，打 Windows 包时自动指向 `.exe` |
| `args` | 启动参数（可选） |
| `os` | 支持的系统：`windows` / `macos` / `linux` |
| `logo` | 标志文件（svg / png / jpg / webp，96 KB 以内） |
| `publisher`、`homepage` | 发布者、主页 |
| `imType` | 和别的插件是同一种 IM 时写成同一个值（同一种同一时间只用一个）；不写自成一类 |
| `source` | 更新来源（GitHub / GitLab / 网址），HHH 只从这里检查、下载新版本 |
| `minHost` | 需要的最低 HHH 版本（可选） |

## 改完必须

1. `cargo build --release` 通过。
2. 跑自检：`hhh-im-check <插件文件夹>/hhh-im-provider.json`，全部通过（没装：`cargo install --git https://github.com/MagicLizi/hhh-im-sdk --tag v0.4.0 hhh-im-protocol --features transport,package`）。
   开发时清单可以指向 `target/release` 里的程序，或先跑 `scripts/pack` 用 `dist/stage`。
3. 在 HHH 里试一遍：设置 → IM → 添加 IM → 文件夹，选 `dist/stage`（或你的插件文件夹），登录、收发消息都走一遍。

## 发版本

只按发版本规范：改 `version` → 打标签 `v<版本>` 推上去 → `scripts/pack` 带上 `--publish`（`pack.ps1 -Publish`）。
令牌只放环境变量，不写进任何文件，也不要贴进对话或提交。用「发版本」skill。
