# HHH IM 插件模板

给 HHH 写一个 IM 插件的起点：清单、标志、能跑的插件骨架、打包脚本、给 agent 的规范，都在这里。

HHH 的 IM 层只做统一的界面和一个统一的插座（HHH-IM-Protocol）。插件是一个子进程：和自己的平台说话、把平台的格式翻成统一格式、
声明自己能做什么和要用户填什么；聊天记录、凭据、配置都交给 HHH。

## 五分钟起一个插件

1. 用这个模板建仓库（GitHub 上点 “Use this template”），克隆下来。
2. 装工具（自检与打包）：

   ```sh
   cargo install --git https://github.com/MagicLizi/hhh-im-sdk --tag v0.4.0 hhh-im-protocol --features transport,package
   ```

3. 改清单 `hhh-im-provider.json`：`id`（发布后不能改）、`name`、`publisher`、`source`（换成你的仓库）。
4. 打包并自检：

   ```sh
   powershell -File scripts/pack.ps1     # Windows
   scripts/pack.sh                       # macOS / Linux
   hhh-im-check dist/stage/hhh-im-provider.json
   ```

5. 在 HHH 里试：设置 → IM → 添加 IM → 文件夹，选 `dist/stage`。会多一个“示例 IM”，里面一个示例群，发什么它回“收到「…」”。

然后照 `src/main.rs` 的注释和 `AGENTS.md` 换成你的平台。

## 在 HHH 里怎么试

| 方式 | 什么时候用 |
|---|---|
| 添加 IM → 文件夹 | 开发时：选 `dist/stage` 或你的插件文件夹 |
| 添加 IM → 安装包 | 选 `dist/` 里的 `.hhhim`，和用户装的一样 |
| 添加 IM → 仓库 | 发布后：填你的仓库地址，HHH 读最新版本、下载这台电脑的包 |
| 卡片「管理 → 从文件更新」 | 已经装过，换成新打的包或文件夹 |

## 发版本

1. 改清单与 `Cargo.toml` 的 `version`，写 `notes.md`（更新说明，Markdown）。
2. 提交，打标签 `v<版本>` 推上去。
3. 令牌放环境变量 `HHH_IM_PUBLISH_TOKEN`，跑 `scripts/pack.ps1 -Publish`（或 `scripts/pack.sh --publish`）。

用户打开 HHH 的设置 → IM 时会看到“有新版本”，点一下就更新。详细规矩见 [hhh-im-sdk 的发版本规范](https://github.com/MagicLizi/hhh-im-sdk/blob/main/docs/release.md)。

## 给 agent 的规范

用任何 agent 打开这个仓库都按同一套规矩写插件：

- `AGENTS.md`：规则正本（先读协议、清单怎么填、插件只做什么、改完必须自检并在 HHH 里试、怎么发版本）。
- `CLAUDE.md`：只有一行，指到 `AGENTS.md`。
- skill「开发插件」与「发版本」：`.agents/skills/` 与 `.claude/skills/`（内容相同）。

## 本仓库的版本

这个模板本身也按规范发布示例插件“示例 IM”的版本，可以在 HHH 里用「添加 IM → 仓库」填 `https://github.com/MagicLizi/hhh-im-plugin-template` 试装与更新。

## 许可证

[MIT](LICENSE)。用这个模板起的插件，许可证由你自己定（改掉 `LICENSE` 与 `Cargo.toml` 的 `license`）。
