//! Slack 的 Web API 封装（契约 client v11；从 HHH 内置 Slack 原样搬来）：私有响应格式在本模块翻成协议的统一格式。
//!
//! 接入方式是 HHH 的 Slack 应用（PKCE，不要 Client Secret）：读取限速按宽松档（实测一次读 200 条、连读不被限流），
//! 消息只通过本人授权主动读取。
//!
//! `SlackClient` 的自有方法按 Slack 的接口说话；协议各方法的做法在 `methods.rs`（对照内置的 `im_client.rs`）。

pub(crate) mod api;
mod auth;
mod card;
mod emoji;
mod errors;
#[cfg(test)]
mod goldens;
pub(crate) mod methods;
pub(crate) mod mrkdwn;
mod names;
pub(crate) mod parts;
mod rich;
pub(crate) mod translate;

pub use auth::{LoginRequest, SlackAuth, SlackLogin, SlackPendingLogin, missing_scopes};
pub(crate) use emoji::catalog_json;

/// 标准表情目录里每个表情的编号（含肤色版本），测试核对写法用。
#[cfg(test)]
pub(crate) fn standard_catalog_for_tests() -> Vec<String> {
    emoji::catalog()
        .into_iter()
        .map(|item| item.emoji.emoji_type)
        .collect()
}
pub use methods::{
    AVATAR_KEY_PATTERN, AVATAR_VARIANT_PATTERN, EMOJI_NAME_PATTERN, EMOJI_TEXT_PATTERN,
    EMOJI_TEXT_TEMPLATE,
};
pub use translate::SlackMessage;

use crate::{AuthGrant, Error, ErrorKind, Result};
use api::Api;
use hhh_im_protocol::types::{Conversation, ConversationKind, Outgoing};
use reqwest::Url;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;
use translate::is_slack_id;

/// 本版需要的本人权限（契约 client v11 §3.1）。缺哪项由 [`SlackClient::missing_scopes`] 给出，
/// 宿主据此提示“重新授权”。
pub const USER_SCOPES: &[&str] = &[
    "channels:history",
    "groups:history",
    "im:history",
    "mpim:history",
    "channels:read",
    "groups:read",
    "im:read",
    "mpim:read",
    "chat:write",
    "reactions:read",
    "reactions:write",
    "users:read",
    "files:read",
    "files:write",
    "emoji:read",
    "team:read",
    "usergroups:read",
];

/// 新版搜索要的本人权限（契约 client v29 §2）：四种会话各一项。后台会话发现改由宿主逐会话探测、不再声明
/// `conversations/recentActivity`，登录也不再申请这几项（不在 [`USER_SCOPES`] 里，缺了不提示重新授权）。
pub const SEARCH_SCOPES: &[&str] = &[
    "search:read.public",
    "search:read.private",
    "search:read.im",
    "search:read.mpim",
];
/// 新版搜索每页最多 20 条（官方文档）。
const ACTIVITY_PAGE_SIZE: &str = "20";

/// 新版搜索搜到的一条消息：只要会话、时间与作者，不留正文（新版搜索的结果不得留作副本，契约 client v29 §2）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ActivityHit {
    /// 会话编号。
    pub(crate) channel: String,
    /// 消息时间（Unix 毫秒）。
    pub(crate) at_ms: i64,
    /// 作者的用户编号；平台没给时为空。
    pub(crate) author: Option<String>,
}

/// 一页历史的条数上限：Slack 内部应用每次最多给 1000 条，按契约 §3.3 用 200。
pub const HISTORY_PAGE_MAX: u16 = 200;
/// 会话列表一次最多读几页（每页 200 个）；再多的会话本版不列出，并在日志里提示。
const CONVERSATION_PAGES_MAX: usize = 20;
/// 找私聊时最多翻几页私聊列表（每页 200 个）。
const DIRECT_PAGES_MAX: usize = 10;
/// 一条消息文字的上限：Slack 规定 40000 个字符，超过的平台会截断，这里直接拒绝。
pub const SEND_TEXT_MAX_CHARS: usize = 40_000;
/// 上传文件的大小上限：Slack 单个文件最大 1 GB，本版与飞书文件一致取 30 MB，避免整包驻留内存。
pub const UPLOAD_MAX_BYTES: usize = 30 * 1024 * 1024;

/// 可注入的 Slack 端点；默认使用官方服务，HTTP 仅允许本机测试地址。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlackConfig {
    /// Web API 根地址。
    pub api_base: String,
    /// 官方授权页面地址。
    pub authorize_url: String,
}
impl Default for SlackConfig {
    fn default() -> Self {
        Self {
            api_base: "https://slack.com/api".into(),
            authorize_url: "https://slack.com/oauth/v2/authorize".into(),
        }
    }
}
impl SlackConfig {
    pub(crate) fn validate(&self) -> Result<()> {
        api::parse_base(&self.api_base)?;
        api::parse_base(&self.authorize_url)?;
        Ok(())
    }
}

/// 历史查询条件。时间用 Slack 的 `ts`（`秒.微秒`）字符串，空着表示不限。
#[derive(Debug, Clone, Default)]
pub(crate) struct SlackHistoryQuery {
    /// 续页位置；第一页为空。
    pub(crate) cursor: Option<String>,
    /// 只要比它新的（不含）。
    pub(crate) oldest: Option<String>,
    /// 只要比它旧的（不含）。
    pub(crate) latest: Option<String>,
    /// 条数，1 至 200；为 0 时用 200。
    pub(crate) limit: u16,
    /// 时间范围含两端（`oldest` 与 `latest` 那一条也要）。
    pub(crate) inclusive: bool,
}

/// 一页消息，按 Slack 给的顺序（新的在前）。
#[derive(Debug, Clone)]
pub(crate) struct SlackPage {
    /// 翻译好的消息；翻译失败的已跳过并记日志。
    pub(crate) messages: Vec<SlackMessage>,
    /// 下一页的位置；没有更多时为空。
    pub(crate) next_cursor: Option<String>,
}

/// 核验过的本人身份。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlackAccount {
    /// 本人编号，`U` 或 `W` 开头。
    pub user_id: String,
    /// 用户名（`auth.test` 的 `user`）。
    pub name: Option<String>,
    /// 工作区编号，`T` 开头。
    pub team_id: Option<String>,
}

/// 工作区里的一个人（名字可能为空）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct User {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) avatar: Option<String>,
}

/// 绑定一个已核验本人账号的 Slack 客户端。
pub struct SlackClient {
    api: Api,
    /// 下载文件、头像与表情图：不设总超时、重定向不跟随（令牌不带去别的主机）。
    download: reqwest::Client,
    grant: AuthGrant,
    account: SlackAccount,
    scopes: Vec<String>,
    caches: Arc<Caches>,
}

/// 读下来的工作区成员名单。`next` 有值时还没读全：上次读到这一页被限流了。
#[derive(Clone)]
struct Directory {
    at: Instant,
    users: Vec<User>,
    next: Option<String>,
}

/// 本机记着的资料：Slack 没有按关键词搜会话与人的接口，只能整份读下来在本机找。续期换了客户端也接着用
/// （同一个账号一份，见 [`Caches::for_account`]）。
#[derive(Default)]
pub struct Caches {
    /// 这份资料是谁的（本人编号）。
    owner: Mutex<Option<String>>,
    /// 用户编号 → 资料与读到的时间。
    users: Mutex<HashMap<String, (Instant, User)>>,
    /// 本人所在的会话与读到的时间（搜群聊用）。
    conversations: Mutex<Option<(Instant, Vec<Conversation>)>>,
    /// 工作区成员（搜人用）：读到的时间、已读的人，没读全时还有下一页的游标（被限流停在这里，下次接着读）。
    directory: Mutex<Option<Directory>>,
    /// 频道与用户组的名字（补消息里只带编号的引用）。
    names: Mutex<names::Names>,
    /// 工作区的自定义表情（名字 → 图片地址或 `alias:原名`）：读目录时记下，宿主按表情要图时用。
    custom_emoji: Mutex<Option<HashMap<String, String>>>,
}

impl Caches {
    /// 给这个账号用的那一份：换了账号就清空重来。
    pub(crate) fn for_account(self: &Arc<Self>, user_id: &str) -> Arc<Self> {
        let mut owner = self.owner.lock().expect("缓存锁");
        if owner.as_deref() != Some(user_id) {
            *owner = Some(user_id.to_owned());
            self.users.lock().expect("缓存锁").clear();
            *self.conversations.lock().expect("缓存锁") = None;
            *self.directory.lock().expect("缓存锁") = None;
            *self.names.lock().expect("缓存锁") = names::Names::default();
            *self.custom_emoji.lock().expect("缓存锁") = None;
        }
        self.clone()
    }
}

impl std::fmt::Debug for SlackClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SlackClient")
            .field("account", &self.account)
            .finish_non_exhaustive()
    }
}

/// 读取出口与响应共用一个进程内单调时钟，日志转发延迟不影响两端配对。
fn read_clock_us() -> u64 {
    static READ_CLOCK: OnceLock<Instant> = OnceLock::new();
    READ_CLOCK.get_or_init(Instant::now).elapsed().as_micros() as u64
}

impl SlackClient {
    /// 用本人令牌核验身份（`auth.test`）；不接受机器人令牌冒充个人连接。
    pub async fn connect(
        config: SlackConfig,
        grant: AuthGrant,
        caches: Arc<Caches>,
    ) -> Result<Self> {
        config.validate()?;
        grant.ensure_current()?;
        let token = grant.access_token();
        if !(token.starts_with("xoxp-") || token.starts_with("xoxe.xoxp-")) {
            return Err(Error::new(
                ErrorKind::InvalidRequest,
                "Slack 令牌不是本人令牌（应以 xoxp- 或 xoxe.xoxp- 开头）。",
            ));
        }
        let api = Api::new(&config.api_base)?;
        let reply = api
            .call(Some(token), "auth.test", &[], Some(api::QUERY_TIMEOUT))
            .await?;
        let value = &reply.value;
        let user_id = text(value, "user_id")
            .filter(|id| is_slack_id(id))
            .ok_or_else(|| Error::new(ErrorKind::ProtocolError, "Slack 身份响应缺少本人编号。"))?;
        if value.get("bot_id").is_some() {
            return Err(Error::new(
                ErrorKind::InvalidRequest,
                "这是机器人令牌，不能当作本人连接。",
            ));
        }
        let account = SlackAccount {
            name: text(value, "user"),
            team_id: text(value, "team_id"),
            user_id,
        };
        let scopes = reply
            .scopes
            .or_else(|| grant.scopes().map(<[String]>::to_vec))
            .unwrap_or_default();
        tracing::info!(scopes = scopes.len(), "Slack 本人身份核验完成");
        let caches = caches.for_account(&account.user_id);
        Ok(Self {
            api,
            download: api::download_client()?,
            grant,
            account,
            scopes,
            caches,
        })
    }

    /// 已核验的本人身份。
    pub fn account(&self) -> &SlackAccount {
        &self.account
    }

    /// 这个令牌实际拿到的权限。
    pub fn scopes(&self) -> &[String] {
        &self.scopes
    }

    /// 相对本版需要的权限（[`USER_SCOPES`]）还缺哪些；不为空时宿主提示重新授权。
    pub fn missing_scopes(&self) -> Vec<String> {
        missing_scopes(&self.scopes, USER_SCOPES)
    }

    /// 读：带本人令牌、10 秒超时，算一次平台请求（宿主按它记读取额度）。
    async fn query(&self, method: &str, form: &[(&str, &str)]) -> Result<Value> {
        self.grant.ensure_current()?;
        hhh_im_protocol::runtime::metering::read_request().await?;
        // 在插件侧记录单调时间，宿主延迟收到日志时仍能核对真实的读取出口间隔。
        let read_started_us = read_clock_us();
        tracing::debug!(method, read_started_us, "向 Slack 读取平台数据");
        Ok(self
            .api
            .call_read(
                Some(self.grant.access_token()),
                method,
                form,
                Some(api::QUERY_TIMEOUT),
                read_started_us,
            )
            .await?
            .value)
    }

    /// 写（发送、编辑、删除、表情、上传）：不算读取额度（同内置），沿用 30 秒总超时。
    async fn write(&self, method: &str, form: &[(&str, &str)]) -> Result<Value> {
        self.grant.ensure_current()?;
        Ok(self
            .api
            .call(Some(self.grant.access_token()), method, form, None)
            .await?
            .value)
    }

    /// 本人所在的频道、私有频道、群私聊与私聊（`users.conversations`），不含已归档的。
    /// 私聊的名字暂时是对方的用户编号，由上层按编号取资料显示。
    pub(crate) async fn conversations(&self) -> Result<Vec<Conversation>> {
        let mut out = Vec::new();
        let mut cursor = String::new();
        for _ in 0..CONVERSATION_PAGES_MAX {
            let value = self
                .query(
                    "users.conversations",
                    &[
                        ("types", "public_channel,private_channel,mpim,im"),
                        ("exclude_archived", "true"),
                        ("limit", "200"),
                        ("cursor", &cursor),
                    ],
                )
                .await?;
            for channel in rich::array(&value, "channels") {
                if let Some(conversation) = conversation_of(channel) {
                    out.push(conversation);
                }
            }
            match next_cursor(&value) {
                Some(next) => cursor = next,
                None => return Ok(out),
            }
        }
        tracing::warn!(count = out.len(), "Slack 会话太多，只列出了前一部分");
        Ok(out)
    }

    /// 与某人的既有私聊的编号；没有时为空。只读，不新建私聊（与飞书规则一致，契约 §3.10）。
    pub(crate) async fn direct_conversation(&self, user_id: &str) -> Result<Option<String>> {
        check_id(user_id)?;
        let mut cursor = String::new();
        for _ in 0..DIRECT_PAGES_MAX {
            let value = self
                .query(
                    "conversations.list",
                    &[("types", "im"), ("limit", "200"), ("cursor", &cursor)],
                )
                .await?;
            let found = rich::array(&value, "channels")
                .iter()
                .find(|c| rich::str_of(c, "user") == user_id)
                .map(|c| rich::str_of(c, "id"))
                .filter(|id| is_slack_id(id));
            if let Some(id) = found {
                return Ok(Some(id.to_owned()));
            }
            match next_cursor(&value) {
                Some(next) => cursor = next,
                None => return Ok(None),
            }
        }
        Ok(None)
    }

    /// 新版搜索（`assistant.search.context`，契约 client v29 §2）列出 `since`（Unix 秒）之后人发的消息：只要会话编号、
    /// 时间与作者，按时间从新到旧，最多翻 `pages` 页。`query` 用 `*`、按时间排时平台走关键词检索、列出全部消息；
    /// `include_bots` 关着也会混进机器人的，这里按 `is_author_bot` 再筛一遍（2026-10-04 取样）。第二项为真表示翻完了。
    /// 本人令牌不需要 `action_token`。
    ///
    /// # Errors
    /// 授权里缺 [`SEARCH_SCOPES`] 时不发请求、返回 [`ErrorKind::PermissionDenied`]；其余同别的查询。
    pub(crate) async fn search_activity(
        &self,
        since: i64,
        pages: usize,
    ) -> Result<(Vec<ActivityHit>, bool)> {
        let missing = missing_scopes(&self.scopes, SEARCH_SCOPES);
        if !self.scopes.is_empty() && !missing.is_empty() {
            return Err(Error::new(
                ErrorKind::PermissionDenied,
                format!(
                    "Slack 授权里没有搜索消息的权限（{}），不能自动发现会话；在应用后台加上后重新登录。",
                    missing.join("、")
                ),
            ));
        }
        let after = since.to_string();
        let mut hits = Vec::new();
        let mut cursor = String::new();
        for _ in 0..pages {
            let mut form: Vec<(&str, &str)> = vec![
                ("query", "*"),
                ("sort", "timestamp"),
                ("sort_dir", "desc"),
                ("after", &after),
                ("channel_types", "public_channel,private_channel,mpim,im"),
                ("content_types", "messages"),
                ("include_bots", "false"),
                ("limit", ACTIVITY_PAGE_SIZE),
            ];
            push_optional(&mut form, "cursor", Some(cursor.as_str()));
            let value = self.query("assistant.search.context", &form).await?;
            let messages = value
                .get("results")
                .map(|results| rich::array(results, "messages"))
                .unwrap_or_default();
            for message in messages {
                if message.get("is_author_bot").and_then(Value::as_bool) == Some(true) {
                    continue;
                }
                let channel = rich::str_of(message, "channel_id");
                let Some(at_ms) = translate::ts_millis(rich::str_of(message, "message_ts")) else {
                    continue;
                };
                if !is_slack_id(channel) {
                    continue;
                }
                let author = rich::str_of(message, "author_user_id");
                hits.push(ActivityHit {
                    channel: channel.to_owned(),
                    at_ms,
                    author: is_slack_id(author).then(|| author.to_owned()),
                });
            }
            match next_cursor(&value) {
                Some(next) => cursor = next,
                None => return Ok((hits, true)),
            }
        }
        Ok((hits, false))
    }

    /// 读会话主消息流的一页（`conversations.history`）；话题回复不在这里，要按话题读。
    pub(crate) async fn history(
        &self,
        conversation: &str,
        query: &SlackHistoryQuery,
    ) -> Result<SlackPage> {
        check_id(conversation)?;
        let limit = page_limit(query.limit).to_string();
        let mut form: Vec<(&str, &str)> = vec![("channel", conversation), ("limit", &limit)];
        push_optional(&mut form, "cursor", query.cursor.as_deref());
        push_optional(&mut form, "oldest", query.oldest.as_deref());
        push_optional(&mut form, "latest", query.latest.as_deref());
        if query.inclusive {
            form.push(("inclusive", "true"));
        }
        let value = self.query("conversations.history", &form).await?;
        Ok(page_of(conversation, &value))
    }

    /// 读一个话题：根消息与它的回复（`conversations.replies`），按时间从旧到新。
    pub(crate) async fn replies(
        &self,
        conversation: &str,
        thread_ts: &str,
        query: &SlackHistoryQuery,
    ) -> Result<SlackPage> {
        check_id(conversation)?;
        check_ts(thread_ts)?;
        let limit = page_limit(query.limit).to_string();
        let mut form: Vec<(&str, &str)> = vec![
            ("channel", conversation),
            ("ts", thread_ts),
            ("limit", &limit),
        ];
        push_optional(&mut form, "cursor", query.cursor.as_deref());
        push_optional(&mut form, "oldest", query.oldest.as_deref());
        push_optional(&mut form, "latest", query.latest.as_deref());
        if query.inclusive {
            form.push(("inclusive", "true"));
        }
        let value = self.query("conversations.replies", &form).await?;
        Ok(page_of(conversation, &value))
    }

    /// 一个人的资料（`users.info`）。
    pub(crate) async fn user(&self, user_id: &str) -> Result<User> {
        check_id(user_id)?;
        let value = self.query("users.info", &[("user", user_id)]).await?;
        value
            .get("user")
            .and_then(user_of)
            .ok_or_else(|| Error::new(ErrorKind::ProtocolError, "Slack 用户资料缺少编号。"))
    }

    /// 工作区成员的一页（`users.list`）：排除本人、机器人与已停用的人（契约 §3.10）。
    pub(crate) async fn users(&self, cursor: Option<&str>) -> Result<(Vec<User>, Option<String>)> {
        self.users_page(cursor, false).await
    }

    /// 一个机器人的资料（`bots.info`，契约 client v23 §4，2026-10-02 真实取样）：`name` 与 `icons.image_72`。
    pub(crate) async fn bot(&self, bot_id: &str) -> Result<User> {
        check_id(bot_id)?;
        let value = self.query("bots.info", &[("bot", bot_id)]).await?;
        let bot = value
            .get("bot")
            .ok_or_else(|| Error::new(ErrorKind::ProtocolError, "Slack 机器人资料缺少内容。"))?;
        let name = rich::str_of(bot, "name").trim();
        let avatar = bot
            .get("icons")
            .map(|icons| rich::str_of(icons, "image_72"))
            .filter(|url| url.starts_with("https://"))
            .map(str::to_owned);
        Ok(User {
            id: bot_id.to_owned(),
            name: name.to_owned(),
            avatar,
        })
    }

    /// 工作区成员的一页，`everyone` 为真时含停用的人与机器人用户（人名册用：停用的人以前发的消息照样显示名字，
    /// 契约 client v23 §4）；本人照样不在里面。
    pub(crate) async fn users_page(
        &self,
        cursor: Option<&str>,
        everyone: bool,
    ) -> Result<(Vec<User>, Option<String>)> {
        // 一页要 1000：Slack 一页最多给 1000（2026-10-01 真实取样：要 2000 也只给 1000）。大工作区停用的账号很多
        // （取样的工作区 7147 个账号、在用的 518 人），200 一页要 36 页，超过 users.list 每分钟约 20 次的限额；
        // 1000 一页 8 页、6.5 秒读完、不限流（契约 friend/im-watch v65 验收）。
        let mut form = vec![("limit", "1000")];
        push_optional(&mut form, "cursor", cursor);
        let value = self.query("users.list", &form).await?;
        let users = rich::array(&value, "members")
            .iter()
            .filter(|m| {
                let flag = |key: &str| m.get(key).and_then(Value::as_bool).unwrap_or(false);
                (everyone
                    || (!flag("deleted")
                        && !flag("is_bot")
                        && rich::str_of(m, "id") != "USLACKBOT"))
                    && rich::str_of(m, "id") != self.account.user_id
            })
            .filter_map(user_of)
            .collect();
        Ok((users, next_cursor(&value)))
    }

    /// 工作区名字与图标（`team.info`，需要 `team:read`）。
    pub async fn team(&self) -> Result<(String, Option<String>)> {
        let value = self.query("team.info", &[]).await?;
        let team = value.get("team").cloned().unwrap_or_default();
        let icon = team
            .get("icon")
            .map(|icon| rich::str_of(icon, "image_68"))
            .filter(|url| url.starts_with("https://"))
            .map(str::to_owned);
        Ok((rich::str_of(&team, "name").to_owned(), icon))
    }

    /// 工作区的自定义表情（`emoji.list`）：名字 → 图片地址；别名写成 `alias:原名`。
    pub(crate) async fn custom_emoji(&self) -> Result<Vec<(String, String)>> {
        let value = self.query("emoji.list", &[]).await?;
        let Some(map) = value.get("emoji").and_then(Value::as_object) else {
            return Ok(Vec::new());
        };
        Ok(map
            .iter()
            .filter(|(name, _)| mrkdwn::is_emoji_name(name))
            .filter_map(|(name, target)| Some((name.clone(), target.as_str()?.to_owned())))
            .filter(|(_, target)| target.starts_with("https://") || target.starts_with("alias:"))
            .collect())
    }

    /// 以本人身份发文字与提及（`chat.postMessage`）。`thread_ts` 给出时发进那个话题；
    /// `reply_broadcast` 为真时这条话题回复同时出现在会话里（同一条消息，契约 client v18 §2.1）。
    /// `client_msg_id` 是发送记录的编号：同一编号再发，Slack 返回原来那条，不会重复（回溯 v11 §4）。
    pub(crate) async fn send(
        &self,
        conversation: &str,
        thread_ts: Option<&str>,
        reply_broadcast: bool,
        content: &[Outgoing],
        client_msg_id: &str,
    ) -> Result<SlackMessage> {
        check_id(conversation)?;
        if let Some(ts) = thread_ts {
            check_ts(ts)?;
        }
        check_operation_id(client_msg_id)?;
        let text = outgoing_text(content)?;
        let mut form = vec![
            ("channel", conversation),
            ("text", text.as_str()),
            ("client_msg_id", client_msg_id),
        ];
        push_optional(&mut form, "thread_ts", thread_ts);
        if reply_broadcast && thread_ts.is_some() {
            form.push(("reply_broadcast", "true"));
        }
        let value = self.write("chat.postMessage", &form).await?;
        let raw = value
            .get("message")
            .ok_or_else(|| Error::new(ErrorKind::ProtocolError, "Slack 发送回执缺少消息。"))?;
        translate::message(conversation, raw)
    }

    /// 撤回（删除）本人的一条消息（`chat.delete`）。
    pub(crate) async fn recall(&self, conversation: &str, ts: &str) -> Result<()> {
        check_id(conversation)?;
        check_ts(ts)?;
        self.write("chat.delete", &[("channel", conversation), ("ts", ts)])
            .await?;
        tracing::info!("Slack 消息已撤回");
        Ok(())
    }

    /// 点上一个表情（`reactions.add`）。
    pub(crate) async fn add_reaction(
        &self,
        conversation: &str,
        ts: &str,
        name: &str,
    ) -> Result<()> {
        self.reaction("reactions.add", conversation, ts, name).await
    }

    /// 取消自己点的一个表情（`reactions.remove`）。
    pub(crate) async fn remove_reaction(
        &self,
        conversation: &str,
        ts: &str,
        name: &str,
    ) -> Result<()> {
        self.reaction("reactions.remove", conversation, ts, name)
            .await
    }

    async fn reaction(&self, method: &str, conversation: &str, ts: &str, name: &str) -> Result<()> {
        check_id(conversation)?;
        check_ts(ts)?;
        if !mrkdwn::is_emoji_name(name) {
            return Err(Error::new(ErrorKind::InvalidRequest, "表情名不合法。"));
        }
        self.write(
            method,
            &[("channel", conversation), ("timestamp", ts), ("name", name)],
        )
        .await?;
        Ok(())
    }

    /// 上传的前两步：要上传地址、传字节；返回文件编号，还没分享到任何会话。
    pub(crate) async fn upload_bytes(&self, filename: &str, bytes: Vec<u8>) -> Result<String> {
        if bytes.is_empty() || bytes.len() > UPLOAD_MAX_BYTES {
            return Err(Error::new(
                ErrorKind::LimitExceeded,
                format!("文件为空或超过 {} MB。", UPLOAD_MAX_BYTES / 1024 / 1024),
            ));
        }
        let name = filename.trim();
        if name.is_empty() || name.len() > 255 || name.chars().any(char::is_control) {
            return Err(Error::new(
                ErrorKind::InvalidRequest,
                "文件名为空、过长或含控制字符。",
            ));
        }
        let length = bytes.len().to_string();
        let ticket = self
            .write(
                "files.getUploadURLExternal",
                &[("filename", name), ("length", &length)],
            )
            .await?;
        let upload_url = rich::str_of(&ticket, "upload_url");
        let file_id = rich::str_of(&ticket, "file_id").to_owned();
        // 只往 Slack 的域名传字节；本机测试时接口根地址在回环地址上，允许同一主机。
        let target = Url::parse(upload_url)
            .ok()
            .filter(|url| {
                let slack = url.scheme() == "https"
                    && url
                        .host_str()
                        .is_some_and(|h| h == "slack.com" || h.ends_with(".slack.com"));
                slack || self.api.same_origin(url)
            })
            .ok_or_else(|| {
                Error::new(
                    ErrorKind::ProtocolError,
                    "Slack 给的上传地址不在 Slack 的域名下。",
                )
            })?;
        if !is_slack_id(&file_id) {
            return Err(Error::new(
                ErrorKind::ProtocolError,
                "Slack 上传回执缺少文件编号。",
            ));
        }
        let response = self
            .api
            .http()
            .post(target)
            .body(bytes)
            .send()
            .await
            .map_err(|error| Error::network(error, "上传文件到 Slack 时连接中断。"))?;
        if !response.status().is_success() {
            return Err(Error::new(
                ErrorKind::Disconnected,
                format!(
                    "Slack 没有收下文件（HTTP {}）。",
                    response.status().as_u16()
                ),
            ));
        }
        tracing::info!(bytes = length, "Slack 文件已上传");
        Ok(file_id)
    }
}

/// 一页消息：翻译不了的跳过并记日志；Slack 说没有更多时不给下一页。
fn page_of(conversation: &str, value: &Value) -> SlackPage {
    let mut messages = Vec::new();
    for raw in rich::array(value, "messages") {
        match translate::message(conversation, raw) {
            Ok(message) => messages.push(message),
            Err(error) => {
                tracing::warn!(reason = %error.message, "跳过一条无法翻译的 Slack 消息")
            }
        }
    }
    let more = value
        .get("has_more")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    SlackPage {
        messages,
        next_cursor: next_cursor(value).filter(|_| more || value.get("has_more").is_none()),
    }
}

/// 一个会话：频道、群私聊、私聊。私聊的名字暂时是对方的编号，对方是 `peer`。
fn conversation_of(channel: &Value) -> Option<Conversation> {
    let id = rich::str_of(channel, "id");
    if !is_slack_id(id) {
        return None;
    }
    let flag = |key: &str| channel.get(key).and_then(Value::as_bool).unwrap_or(false);
    let kind = if flag("is_im") {
        ConversationKind::Direct
    } else if flag("is_mpim") {
        ConversationKind::Group
    } else {
        ConversationKind::Channel
    };
    let name = if kind == ConversationKind::Direct {
        rich::str_of(channel, "user").to_owned()
    } else {
        rich::str_of(channel, "name").to_owned()
    };
    let peer = (kind == ConversationKind::Direct)
        .then(|| rich::str_of(channel, "user"))
        .filter(|user| is_slack_id(user))
        .map(str::to_owned);
    // 群私聊的 `is_private` 也是 true；只有频道才算“私有频道”（契约 client v22 §2）。
    let private = kind == ConversationKind::Channel && flag("is_private");
    Some(Conversation {
        id: id.to_owned(),
        kind,
        name,
        avatar: None,
        peer,
        private,
    })
}

/// 一个人的资料：名字依次用显示名、真名、用户名；头像用 72 像素的（只认 https）。
fn user_of(user: &Value) -> Option<User> {
    let id = rich::str_of(user, "id");
    if !is_slack_id(id) {
        return None;
    }
    let profile = user.get("profile");
    let name = profile
        .and_then(|p| {
            ["display_name", "real_name"]
                .iter()
                .map(|key| rich::str_of(p, key))
                .find(|n| !n.is_empty())
        })
        .unwrap_or(rich::str_of(user, "name"))
        .to_owned();
    let avatar = profile
        .map(|p| rich::str_of(p, "image_72"))
        .filter(|url| url.starts_with("https://"))
        .map(str::to_owned);
    Some(User {
        id: id.to_owned(),
        name,
        avatar,
    })
}

/// 发送内容 → Slack 文字：普通文字转义 `&`、`<`、`>`，提及写成 `<@编号>`，全体写成 `<!channel>`。
/// 图片与文件要另外上传。
pub(crate) fn outgoing_text(content: &[Outgoing]) -> Result<String> {
    let mut text = String::new();
    for item in content {
        match item {
            Outgoing::Text { text: value } => {
                for c in value.chars() {
                    match c {
                        '&' => text.push_str("&amp;"),
                        '<' => text.push_str("&lt;"),
                        '>' => text.push_str("&gt;"),
                        _ => text.push(c),
                    }
                }
            }
            Outgoing::Mention { user_id, .. } => {
                check_id(user_id)?;
                text.push_str(&format!("<@{user_id}>"));
            }
            Outgoing::MentionAll => text.push_str("<!channel>"),
            _ => {
                return Err(Error::new(
                    ErrorKind::InvalidRequest,
                    "Slack 的图片与文件要单独上传发送。",
                ));
            }
        }
    }
    if text.trim().is_empty() || text.chars().count() > SEND_TEXT_MAX_CHARS {
        return Err(Error::new(
            ErrorKind::InvalidRequest,
            format!("消息为空或超过 {SEND_TEXT_MAX_CHARS} 个字符。"),
        ));
    }
    Ok(text)
}

fn text(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn next_cursor(value: &Value) -> Option<String> {
    value
        .get("response_metadata")
        .and_then(|m| m.get("next_cursor"))
        .and_then(Value::as_str)
        .filter(|c| !c.is_empty())
        .map(str::to_owned)
}

fn push_optional<'a>(form: &mut Vec<(&'a str, &'a str)>, key: &'a str, value: Option<&'a str>) {
    if let Some(value) = value.filter(|v| !v.is_empty()) {
        form.push((key, value));
    }
}

fn page_limit(limit: u16) -> u16 {
    if limit == 0 {
        HISTORY_PAGE_MAX
    } else {
        limit.min(HISTORY_PAGE_MAX)
    }
}

pub(crate) fn check_id(id: &str) -> Result<()> {
    if is_slack_id(id) {
        Ok(())
    } else {
        Err(Error::new(ErrorKind::InvalidRequest, "Slack 编号不合法。"))
    }
}

pub(crate) fn check_ts(ts: &str) -> Result<()> {
    if translate::ts_millis(ts).is_some() {
        Ok(())
    } else {
        Err(Error::new(
            ErrorKind::InvalidRequest,
            "Slack 消息时间编号不合法。",
        ))
    }
}

/// 发送编号：Slack 客户端用的是 UUID；这里收字母数字与连字符，最长 64。
fn check_operation_id(id: &str) -> Result<()> {
    let valid = !id.is_empty()
        && id.len() <= 64
        && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
    if valid {
        Ok(())
    } else {
        Err(Error::new(ErrorKind::InvalidRequest, "发送编号不合法。"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outgoing_text_escapes_and_writes_mentions() {
        let text = outgoing_text(&[
            Outgoing::Text {
                text: "a < b & c ".into(),
            },
            Outgoing::Mention {
                user_id: "U01".into(),
                name: "张三".into(),
            },
            Outgoing::Text { text: " ".into() },
            Outgoing::MentionAll,
        ])
        .unwrap();
        assert_eq!(text, "a &lt; b &amp; c <@U01> <!channel>");
        assert!(outgoing_text(&[Outgoing::Text { text: "  ".into() }]).is_err());
        assert!(outgoing_text(&[Outgoing::Image { key: "k".into() }]).is_err());
        assert!(
            outgoing_text(&[Outgoing::Mention {
                user_id: "<!x>".into(),
                name: "x".into()
            }])
            .is_err()
        );
    }

    #[test]
    fn ids_timestamps_and_operation_ids_are_checked_before_network() {
        assert!(check_id("C0123ABC").is_ok());
        assert!(check_id("C01/../x").is_err());
        assert!(check_ts("1759120000.000100").is_ok());
        assert!(check_ts("1759120000").is_err());
        assert!(check_operation_id("7b0c1f2e-9d1a-4c3b-8e2f-0a1b2c3d4e5f").is_ok());
        assert!(check_operation_id("a b").is_err());
        assert_eq!(page_limit(0), 200);
        assert_eq!(page_limit(500), 200);
        assert_eq!(page_limit(20), 20);
    }
}
