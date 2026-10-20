//! 协议各方法在 Slack 上的做法（契约 hhh-im client v11 §3、§11，v17 §3、§6、§7；对照 HHH 内置 Slack 的 `im_client.rs`
//! 一个一个搬来，结果直接是协议的统一格式）。
//!
//! Slack 与飞书不同的地方都在这里消化：消息编号是 `ts`、话题要按根消息读、历史里内嵌表情、表情没有编号与时间、
//! 附件上传后没有消息回执、删掉的消息从历史里消失。

use super::emoji::CatalogItem;
use super::translate::{self, SlackMessage, is_slack_id, reaction_of, split_reaction_id};
use super::*;
use hhh_im_protocol::method::{
    ActivityItem, EmojiCatalogResult, FindDirectResult, MessagesListParams, Page, PeopleGetResult,
    RecentActivityResult, SearchPage,
};
use hhh_im_protocol::runtime::{ByteStream, NextChunk, Opened};
use hhh_im_protocol::types::{
    Message, Person, PersonKind, Reaction, ResourceKind, SendOutcome, SendTarget,
};
use std::time::Duration;

/// 一个话题最多读几页回复（每页 200 条）：Slack 按时间从旧到新给回复，只能整段读。
const THREAD_PAGES_MAX: usize = 5;
/// 用户资料在本机记多久。
const USER_TTL: Duration = Duration::from_secs(60 * 60);
/// 人名册读人（契约 client v23 §4）：一批里的人不超过这么多就逐个 `users.info`，多了读一遍工作区名单
/// （一页 1000 人，真实工作区 7147 个账号 8 页约 6.5 秒）更省。
const PEOPLE_USERS_INFO_MAX: usize = 10;
/// 人名册一批里最多读几个机器人（每个一次 `bots.info`）；其余的下次再读。
const PEOPLE_BOTS_MAX: usize = 20;
/// 人名册一批最多多少人（声明给宿主的 `people.batchMax`，同内置的人名册一次查询的上限）：一个刷新间隔里名单只读一遍
/// （契约 client v23 §8；一批 1000 时 1192 人分两批、名单读了两遍，2026-10-02 真实环境实测）。
pub(crate) const PEOPLE_BATCH_MAX: u32 = 2000;
/// 搜索用的会话列表与成员名单记多久。
const CONVERSATIONS_TTL: Duration = Duration::from_secs(5 * 60);
const DIRECTORY_TTL: Duration = Duration::from_secs(60 * 60);
/// 搜人时成员名单最多读几页（每页 1000 人）。
const DIRECTORY_PAGES_MAX: usize = 50;
/// 附件发出后等多久在会话里看到这条消息（契约 client v11 §3.4），每隔多久看一次。
const RECEIPT_WAIT: Duration = Duration::from_secs(30);
const RECEIPT_POLL: Duration = Duration::from_secs(2);
/// 编辑后重读这条暂时失败时，等多久再读一次（Slack 说了要等多久就照它，但至多等这么久）。
const EDIT_REREAD_WAIT: Duration = Duration::from_secs(1);
const EDIT_REREAD_WAIT_MAX: Duration = Duration::from_secs(3);
/// 找附件消息时往回看多久。
const RECEIPT_LOOKBACK_SECS: i64 = 120;
/// 表情图的下载上限，与飞书同一预算。
const EMOJI_IMAGE_MAX_BYTES: usize = 512 * 1024;
/// 搜索关键词最多几个字、每页最多几条（同 HHH 的统一限制）。
const SEARCH_TEXT_MAX_CHARS: usize = 50;
const SEARCH_PAGE_MAX: u16 = 20;
/// 读不到别人点的表情（缺 `reactions:read`）时给用户的话（同内置 Slack 的提示）。
const REACTIONS_TIP: &str = "要显示其他人点的表情，需要 reactions:read 权限：请在设置里重新登录。";

/// 声明给宿主的表情编号写法（同 [`super::mrkdwn::is_emoji_name`]：不以冒号开头或结尾，不含空白与 `_-+':` 以外的
/// ASCII 标点；字母数字含各种文字）。宿主另有泛化校验（非空、64 字节以内、无控制字符）。
pub const EMOJI_NAME_PATTERN: &str = r##"^(?:[^\s:!"#$%&()*,./;<=>?@\[\\\]^`{|}~]|[^\s:!"#$%&()*,./;<=>?@\[\\\]^`{|}~][^\s!"#$%&()*,./;<=>?@\[\\\]^`{|}~]*[^\s:!"#$%&()*,./;<=>?@\[\\\]^`{|}~])$"##;
/// 声明给宿主的头像身份（同 [`avatar_identity`]）：Slack 头像 CDN（`*.slack-edge.com`）上的是路径去掉扩展名与末尾的
/// `_72`、`-72` 这类尺寸；Gravatar 上的是最后一段去掉扩展名。
pub const AVATAR_KEY_PATTERN: &str = r"^(?i:https)://(?:[^/?#]*\.(?i:slack-edge\.com)(?::[0-9]*)?/|(?i:secure\.gravatar\.com)(?::[0-9]*)?/(?:[^/?#]*/)*)([^?#]*?)(?:[_-][0-9]+)?(?:\.[^./?#]*)?(?:[?#]|$)";
/// 声明给宿主的头像变体（尺寸）：Slack 头像 CDN 上是路径末尾的数字，Gravatar 上是查询参数 `s`。
pub const AVATAR_VARIANT_PATTERN: &str = r"^(?i:https)://(?:[^/?#]*\.(?i:slack-edge\.com)(?::[0-9]*)?/[^?#]*?[_-]|(?i:secure\.gravatar\.com)(?::[0-9]*)?/[^#]*?[?&]s=)([0-9]+)(?:\.[^./?#]*)?(?:[?#&]|$)";

/// 头像地址里的身份（契约 client v10 §3）：Slack 的头像在 `avatars.slack-edge.com`、`ca.slack-edge.com`
/// 或 Gravatar 上，文件名末尾的 `_72`、`-72` 或查询里的 `s=72` 是尺寸（变体），其余是编号。
pub(crate) fn avatar_identity(url: &str) -> Option<(String, String)> {
    let url = Url::parse(url).ok()?;
    let host = url.host_str()?;
    if url.scheme() != "https" {
        return None;
    }
    let last = url.path_segments()?.next_back()?.to_owned();
    if host == "secure.gravatar.com" {
        let key = last.split('.').next()?.to_owned();
        let size = url
            .query_pairs()
            .find(|(k, _)| k == "s")
            .map(|(_, v)| v.into_owned())
            .unwrap_or_default();
        return (!key.is_empty()).then_some((key, size));
    }
    if !host.ends_with(".slack-edge.com") {
        return None;
    }
    let path = url.path().trim_start_matches('/');
    let stem = path.rsplit_once('.').map_or(path, |(stem, _)| stem);
    match stem.rsplit_once(['_', '-']) {
        Some((key, size)) if !size.is_empty() && size.bytes().all(|b| b.is_ascii_digit()) => {
            Some((key.to_owned(), size.to_owned()))
        }
        _ => Some((stem.to_owned(), String::new())),
    }
}

/// Slack 的 `ts`：Unix 秒 → 这一秒的开头或结尾（查询含两端）。
fn ts_at(secs: i64, end: bool) -> String {
    if end {
        format!("{secs}.999999")
    } else {
        format!("{secs}.000000")
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

fn now_secs() -> i64 {
    now_ms() / 1000
}

/// 名字里含不含关键词：不分大小写，关键词去掉首尾空白（同 HHH 的本机搜索）。
fn contains(haystack: &str, needle: &str) -> bool {
    haystack
        .to_lowercase()
        .contains(&needle.trim().to_lowercase())
}

/// 关键词 1–50 个字、不含控制字符，每页 1–20 条；不合规的在本地拒绝（同 HHH 的本机搜索）。
fn check_search(text: &str, limit: u16) -> Result<()> {
    if text.trim().is_empty()
        || text.chars().count() > SEARCH_TEXT_MAX_CHARS
        || text.chars().any(char::is_control)
        || !(1..=SEARCH_PAGE_MAX).contains(&limit)
    {
        return Err(Error::new(
            ErrorKind::InvalidRequest,
            "搜索关键词须为 1–50 个字符且不含控制字符，每页数量须为 1–20。",
        ));
    }
    Ok(())
}

/// 在本机的列表里按关键词找，分页用序号当游标；游标里带着查询范围（种类、关键词、每页几条），换了条件的游标不能接着用。
fn search_page<T: Clone>(
    all: &[T],
    scope: &str,
    limit: u16,
    cursor: Option<&str>,
    matches: impl Fn(&T) -> bool,
) -> Result<SearchPage<T>> {
    let start = match cursor {
        Some(cursor) => {
            let (offset, of) = cursor
                .split_once('|')
                .ok_or_else(|| Error::new(ErrorKind::InvalidRequest, "分页游标不正确。"))?;
            if of != scope {
                return Err(Error::new(
                    ErrorKind::InvalidRequest,
                    "分页游标不属于当前账号、会话或查询条件。",
                ));
            }
            offset
                .parse::<usize>()
                .map_err(|_| Error::new(ErrorKind::InvalidRequest, "分页游标不正确。"))?
        }
        None => 0,
    };
    let found: Vec<&T> = all.iter().filter(|item| matches(item)).collect();
    let end = (start + usize::from(limit)).min(found.len());
    let items = found
        .get(start..end)
        .unwrap_or_default()
        .iter()
        .map(|item| (*item).clone())
        .collect();
    Ok(SearchPage {
        items,
        next: (end < found.len()).then(|| format!("{end}|{scope}")),
        notice: None,
    })
}

impl User {
    /// 写成协议的人员（名字为空时不写；同内置把读到的名字原样交出，宿主的人名册再去掉首尾空白）。
    pub(crate) fn person(&self, kind: PersonKind) -> Person {
        Person {
            id: self.id.clone(),
            id_type: Some(
                match kind {
                    PersonKind::User => super::mrkdwn::USER_ID_TYPE,
                    PersonKind::Bot => translate::BOT_ID_TYPE,
                }
                .into(),
            ),
            kind,
            name: (!self.name.is_empty()).then(|| self.name.clone()),
            avatar: self.avatar.clone(),
        }
    }
}

/// 一个文件的下载读流（Slack 的响应体按块读）。
struct Download(reqwest::Response);

impl ByteStream for Download {
    fn next_chunk(&mut self) -> NextChunk<'_> {
        Box::pin(async move {
            self.0
                .chunk()
                .await
                .map(|chunk| chunk.map(|bytes| bytes.to_vec()))
                .map_err(|error| Error::network(error, "Slack 文件下载中断，请稍后重试。").rpc())
        })
    }
}

impl SlackClient {
    fn me(&self) -> &str {
        &self.account.user_id
    }

    /// 翻好的消息交给宿主：写成协议消息，本人发的带编辑原文。
    fn wire_all(&self, messages: &[SlackMessage]) -> Vec<Message> {
        messages.iter().map(|m| m.wire(Some(self.me()))).collect()
    }

    /// 读一页消息（契约 client v11 §3.3）。会话主消息流按时间段、新的在前读，页内从旧到新；
    /// 话题按根消息读（Slack 从旧到新给，读至多 [`THREAD_PAGES_MAX`] 页），一次交出、没有下一页；
    /// 带了起始时间时只要那之后的回复（根消息总在里面，契约 client v27 §8）。
    /// 历史里内嵌的表情随消息给出；频道引用与用户组提及补上名字。
    pub(crate) async fn list_messages(&self, query: &MessagesListParams) -> Result<Page<Message>> {
        check_id(&query.conversation_id)?;
        // 宿主按秒给时间（毫秒是秒乘 1000），Slack 的条件也按秒。
        let start = query.after_ms.map(|ms| ms.div_euclid(1000));
        let end = query.before_ms.map(|ms| ms.div_euclid(1000));
        if query.limit == 0
            || query.limit > HISTORY_PAGE_MAX
            || start.is_some_and(|t| t < 0)
            || end.is_some_and(|t| t < 0)
            || start.zip(end).is_some_and(|(a, b)| a > b)
            || !query.newest_first
        {
            return Err(Error::new(
                ErrorKind::InvalidRequest,
                "消息每页数量应在 1 至 200 之间、从新的往前读，时间范围必须有效。",
            ));
        }
        let channel = query.conversation_id.as_str();
        if let Some(root) = &query.thread_id {
            check_ts(root)?;
            if query.cursor.is_some() {
                return Ok(Page {
                    items: Vec::new(),
                    next: None,
                });
            }
            let mut items = Vec::new();
            let mut cursor = None;
            for _ in 0..THREAD_PAGES_MAX {
                // 带了时间条件就只要那之后的回复（契约 client v27 §8，取样：`oldest` 只回根消息与之后的回复）。
                let page = self
                    .replies(
                        channel,
                        root,
                        &SlackHistoryQuery {
                            cursor: cursor.take(),
                            oldest: start.map(|t| ts_at(t, false)),
                            limit: HISTORY_PAGE_MAX,
                            ..SlackHistoryQuery::default()
                        },
                    )
                    .await?;
                items.extend(page.messages);
                match page.next_cursor {
                    Some(next) => cursor = Some(next),
                    None => break,
                }
            }
            if cursor.is_some() {
                tracing::warn!(replies = items.len(), "Slack 话题太长，只读了前一部分回复");
            }
            items.sort_by_key(|m| m.message.created_at_ms);
            self.name_references(&mut items).await;
            return Ok(Page {
                items: self.wire_all(&items),
                next: None,
            });
        }
        let page = self
            .history(
                channel,
                &SlackHistoryQuery {
                    cursor: query.cursor.clone(),
                    oldest: start.map(|t| ts_at(t, false)),
                    latest: end.map(|t| ts_at(t, true)),
                    limit: query.limit,
                    inclusive: true,
                },
            )
            .await?;
        let mut items = page.messages;
        items.sort_by_key(|m| m.message.created_at_ms);
        self.name_references(&mut items).await;
        Ok(Page {
            items: self.wire_all(&items),
            next: page.next_cursor,
        })
    }

    /// 按编号读一条消息：先在会话主消息流里找，找不到再当话题回复找。
    async fn find_message(&self, conversation: &str, id: &str) -> Result<SlackMessage> {
        check_id(conversation)?;
        check_ts(id)?;
        let exact = SlackHistoryQuery {
            oldest: Some(id.to_owned()),
            latest: Some(id.to_owned()),
            limit: 1,
            inclusive: true,
            ..SlackHistoryQuery::default()
        };
        let found = self.history(conversation, &exact).await?;
        let pick = |page: SlackPage| page.messages.into_iter().find(|m| m.id() == id);
        if let Some(message) = pick(found) {
            return Ok(message);
        }
        let thread = SlackHistoryQuery { limit: 2, ..exact };
        match self.replies(conversation, id, &thread).await {
            Ok(page) => pick(page),
            Err(error) if error.platform_code.as_deref() == Some("thread_not_found") => None,
            Err(error) => return Err(error),
        }
        .ok_or_else(|| {
            Error::new(
                ErrorKind::NotFound,
                "Slack 上找不到这条消息（可能已删除）。",
            )
        })
    }

    /// `messages/get`：读一条消息，补上频道与用户组的名字。
    pub(crate) async fn get_message(&self, conversation: &str, id: &str) -> Result<Message> {
        let mut found = vec![self.find_message(conversation, id).await?];
        self.name_references(&mut found).await;
        Ok(found.remove(0).wire(Some(self.me())))
    }

    /// 一条消息现在的全部表情（`reactions.get`，带全部点的人）；Slack 一次给完，没有下一页。
    /// 缺 `reactions:read` 时给用户的话照内置 Slack 的提示（协议 v12：`reactions/list` 缺权限时的 `userMessage`）。
    pub(crate) async fn list_reactions(
        &self,
        conversation: &str,
        message: &str,
    ) -> Result<Page<Reaction>> {
        check_id(conversation)?;
        check_ts(message)?;
        let value = self
            .query(
                "reactions.get",
                &[
                    ("channel", conversation),
                    ("timestamp", message),
                    ("full", "true"),
                ],
            )
            .await
            .map_err(|error| {
                if error.platform_code.as_deref() == Some("missing_scope") {
                    Error {
                        message: REACTIONS_TIP.into(),
                        ..error
                    }
                } else {
                    error
                }
            })?;
        let raw = value.get("message").cloned().unwrap_or_default();
        let items = rich::array(&raw, "reactions")
            .iter()
            .flat_map(|reaction| {
                let name = rich::str_of(reaction, "name").to_owned();
                rich::array(reaction, "users")
                    .iter()
                    .filter_map(Value::as_str)
                    .filter(|user| is_slack_id(user))
                    .map(|user| reaction_of(conversation, message, user, &name))
                    .collect::<Vec<_>>()
            })
            .filter(|r| mrkdwn::is_emoji_name(&r.emoji_type))
            .collect();
        Ok(Page { items, next: None })
    }

    /// 会话列表的一页。
    pub(crate) async fn list_conversations(
        &self,
        limit: u16,
        cursor: Option<&str>,
    ) -> Result<Page<Conversation>> {
        if limit == 0 || limit > HISTORY_PAGE_MAX {
            return Err(Error::new(
                ErrorKind::InvalidRequest,
                "会话每页数量应在 1 至 200 之间。",
            ));
        }
        let limit_text = limit.to_string();
        let value = self
            .query(
                "users.conversations",
                &[
                    ("types", "public_channel,private_channel,mpim,im"),
                    ("exclude_archived", "true"),
                    ("limit", &limit_text),
                    ("cursor", cursor.unwrap_or_default()),
                ],
            )
            .await?;
        let items = rich::array(&value, "channels")
            .iter()
            .filter_map(conversation_of)
            .collect();
        Ok(Page {
            items,
            next: next_cursor(&value),
        })
    }

    /// 会话发现（契约 client v29 §2）：新版搜索列出的消息按会话归并（最早、最新一条），会话类别、名字与私聊对方
    /// 取本机记着的会话列表；列表里没有的（刚开的私聊）按会话读一次资料，读不到的跳过。群与频道的资料随这一项交给
    /// 宿主（协议 v15 的 `conversation`），宿主不再为它读一次。
    pub(crate) async fn recent_activity(
        &self,
        since_ms: i64,
        pages_max: u16,
    ) -> Result<RecentActivityResult> {
        if since_ms < 0 || pages_max == 0 {
            return Err(Error::new(
                ErrorKind::InvalidRequest,
                "起始时间不能为负数，至少翻一页。",
            ));
        }
        let (hits, complete) = self
            .search_activity(since_ms.div_euclid(1000), usize::from(pages_max))
            .await?;
        // 按会话归并，保持“最新一条从新到旧”的先后（搜索结果本身从新到旧）。
        let mut order: Vec<String> = Vec::new();
        let mut spans: HashMap<String, (i64, i64)> = HashMap::new();
        for hit in hits.into_iter().filter(|hit| hit.at_ms >= since_ms) {
            let span = spans.entry(hit.channel.clone()).or_insert_with(|| {
                order.push(hit.channel.clone());
                (hit.at_ms, hit.at_ms)
            });
            span.0 = span.0.min(hit.at_ms);
            span.1 = span.1.max(hit.at_ms);
        }
        if order.is_empty() {
            return Ok(RecentActivityResult {
                items: Vec::new(),
                complete,
            });
        }
        let known = self.cached_conversations().await?;
        let mut items = Vec::with_capacity(order.len());
        for id in order {
            let (first_ms, latest_ms) = spans[&id];
            let profile = match known.iter().find(|c| c.id == id) {
                Some(found) => found.clone(),
                None => match self.get_conversation(&id).await {
                    Ok(found) => found,
                    Err(error)
                        if matches!(
                            error.kind,
                            ErrorKind::NotFound | ErrorKind::PermissionDenied
                        ) =>
                    {
                        tracing::info!(conversation = %id, %error, "搜到的 Slack 会话读不到资料，跳过");
                        continue;
                    }
                    Err(error) => return Err(error),
                },
            };
            items.push(ActivityItem {
                conversation_id: id,
                kind: profile.kind,
                first_ms,
                latest_ms,
                peer_id: profile.peer.clone(),
                // 私聊的名字在会话列表里是对方的编号，不当资料用，由宿主读对方本人。
                conversation: (profile.kind != ConversationKind::Direct).then_some(profile),
            });
        }
        items.sort_by_key(|item| std::cmp::Reverse(item.latest_ms));
        Ok(RecentActivityResult { items, complete })
    }

    /// 搜群聊用的会话列表：本机记 5 分钟，过期再整份读（契约 client v11 §3.10）。
    async fn cached_conversations(&self) -> Result<Vec<Conversation>> {
        if let Some((at, list)) = self
            .caches
            .conversations
            .lock()
            .expect("会话缓存锁")
            .clone()
            && at.elapsed() < CONVERSATIONS_TTL
        {
            return Ok(list);
        }
        let list = self.conversations().await?;
        *self.caches.conversations.lock().expect("会话缓存锁") =
            Some((Instant::now(), list.clone()));
        Ok(list)
    }

    /// 搜人用的成员名单：工作区成员（不含本人、机器人与停用的）。读全了一小时内直接用；没读全（上次被限流）就从
    /// 断点接着读。读到一半又被限流时，记下已读的与断点，这次按已读的搜（不报错）；一个人都还没读到才把限流报给宿主
    /// （契约 friend/im-watch v65 验收）。
    async fn cached_directory(&self) -> Result<Vec<User>> {
        let saved = self
            .caches
            .directory
            .lock()
            .expect("成员缓存锁")
            .clone()
            .filter(|d| d.at.elapsed() < DIRECTORY_TTL);
        let (mut all, mut cursor) = match saved {
            Some(d) if d.next.is_none() => return Ok(d.users),
            Some(d) => (d.users, d.next),
            None => (Vec::new(), None),
        };
        let mut finished = false;
        let mut limited = false;
        for _ in 0..DIRECTORY_PAGES_MAX {
            match self.users(cursor.as_deref()).await {
                Ok((users, next)) => {
                    all.extend(users);
                    match next {
                        Some(next) => cursor = Some(next),
                        None => {
                            cursor = None;
                            finished = true;
                            break;
                        }
                    }
                }
                Err(error) if error.kind == ErrorKind::RateLimited && !all.is_empty() => {
                    tracing::warn!(count = all.len(), %error, "Slack 成员名单读到一半被限流：先按已读的搜，下次接着读");
                    limited = true;
                    break;
                }
                Err(error) => return Err(error),
            }
        }
        if !finished && !limited {
            tracing::warn!(count = all.len(), "Slack 工作区成员太多，只读了前一部分");
            cursor = None;
        }
        {
            let mut known = self.caches.users.lock().expect("用户缓存锁");
            for user in &all {
                known.insert(user.id.clone(), (Instant::now(), user.clone()));
            }
        }
        *self.caches.directory.lock().expect("成员缓存锁") = Some(Directory {
            at: Instant::now(),
            users: all.clone(),
            next: cursor,
        });
        Ok(all)
    }

    /// 按编号读一个人的资料：本机记着的（一小时内）直接用，否则读一次；读不到为空。
    async fn cached_user(&self, id: &str) -> Result<Option<User>> {
        if !is_slack_id(id) {
            return Ok(None);
        }
        if let Some((at, person)) = self.caches.users.lock().expect("用户缓存锁").get(id)
            && at.elapsed() < USER_TTL
        {
            return Ok(Some(person.clone()));
        }
        match self.user(id).await {
            Ok(person) => {
                self.caches
                    .users
                    .lock()
                    .expect("用户缓存锁")
                    .insert(id.to_owned(), (Instant::now(), person.clone()));
                Ok(Some(person))
            }
            Err(error) if error.kind == ErrorKind::NotFound => {
                tracing::debug!("Slack 没有这个人的资料，跳过");
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    /// 人名册按编号读一批（`people/get`，契约 client v23 §4，2026-10-02 真实取样）：人少逐个 `users.info`，人多先读
    /// 工作区名单（含停用的人与机器人用户，读到一页里全齐了就停），名单里没有的（外部工作区的人）再逐个读；机器人逐个
    /// `bots.info`。`user_not_found`、`bot_not_found` 算没有；`user_not_visible` 只跳过这个人，不标成不存在。
    /// 这次没读到的两边都不放（宿主下次再读）；不走本机的用户资料
    /// 缓存（人名册自己按刷新间隔判断过期），读到的人顺手更新缓存。
    pub(crate) async fn look_up_people(
        &self,
        ids: &[String],
        bots: &[String],
    ) -> Result<PeopleGetResult> {
        let mut result = PeopleGetResult {
            found: Vec::new(),
            missing: Vec::new(),
        };
        let mut users: Vec<&str> = ids
            .iter()
            .map(String::as_str)
            .filter(|id| is_slack_id(id))
            .collect();
        users.sort_unstable();
        users.dedup();
        let mut read: Vec<User> = Vec::new();
        if users.len() > PEOPLE_USERS_INFO_MAX {
            let mut cursor: Option<String> = None;
            for _ in 0..DIRECTORY_PAGES_MAX {
                let (page, next) = self.users_page(cursor.as_deref(), true).await?;
                for user in page {
                    if let Ok(at) = users.binary_search(&user.id.as_str()) {
                        users.remove(at);
                        read.push(user);
                    }
                }
                match next {
                    Some(next) if !users.is_empty() => cursor = Some(next),
                    _ => break,
                }
            }
        }
        for id in users.into_iter().take(PEOPLE_USERS_INFO_MAX) {
            match self.user(id).await {
                Ok(user) => read.push(user),
                Err(error) if error.platform_code.as_deref() == Some("user_not_visible") => {
                    tracing::debug!("Slack 用户对当前账号不可见，这次跳过，保留其他人的资料");
                }
                Err(error) if error.kind == ErrorKind::NotFound => {
                    result.missing.push(id.to_owned())
                }
                Err(error) => return Err(error),
            }
        }
        {
            let mut known = self.caches.users.lock().expect("用户缓存锁");
            for user in &read {
                known.insert(user.id.clone(), (Instant::now(), user.clone()));
            }
        }
        result
            .found
            .extend(read.iter().map(|user| user.person(PersonKind::User)));
        for id in bots.iter().take(PEOPLE_BOTS_MAX) {
            if !is_slack_id(id) {
                result.missing.push(id.clone());
                continue;
            }
            match self.bot(id).await {
                Ok(bot) => result.found.push(bot.person(PersonKind::Bot)),
                Err(error) if error.kind == ErrorKind::NotFound => result.missing.push(id.clone()),
                Err(error) => return Err(error),
            }
        }
        Ok(result)
    }

    /// 一个会话的资料；私聊的名字与头像是对方的。
    pub(crate) async fn get_conversation(&self, conversation: &str) -> Result<Conversation> {
        check_id(conversation)?;
        let value = self
            .query("conversations.info", &[("channel", conversation)])
            .await?;
        let mut found = value
            .get("channel")
            .and_then(conversation_of)
            .ok_or_else(|| Error::new(ErrorKind::ProtocolError, "Slack 会话资料缺少编号。"))?;
        if let Some(peer) = found.peer.clone()
            && let Some(person) = self.cached_user(&peer).await?
        {
            found.name = person.name;
            found.avatar = person.avatar;
        }
        Ok(found)
    }

    /// 搜群聊与频道：在本人所在的会话里按名字找（Slack 没有按关键词搜会话的接口）。
    pub(crate) async fn search_conversations(
        &self,
        text: &str,
        limit: u16,
        cursor: Option<&str>,
    ) -> Result<SearchPage<Conversation>> {
        check_search(text, limit)?;
        let scope = serde_json::to_string(&("search-groups", text, limit))
            .map_err(|_| Error::new(ErrorKind::InvalidRequest, "无法编码查询范围。"))?;
        let all = self.cached_conversations().await?;
        search_page(&all, &scope, limit, cursor, |c| {
            c.kind != ConversationKind::Direct && contains(&c.name, text)
        })
    }

    /// 搜人：在工作区成员名单里按名字找。
    pub(crate) async fn search_people(
        &self,
        text: &str,
        limit: u16,
        cursor: Option<&str>,
    ) -> Result<SearchPage<Person>> {
        check_search(text, limit)?;
        let scope = serde_json::to_string(&("search-users", text, limit))
            .map_err(|_| Error::new(ErrorKind::InvalidRequest, "无法编码查询范围。"))?;
        let all = self.cached_directory().await?;
        let page = search_page(&all, &scope, limit, cursor, |u| contains(&u.name, text))?;
        Ok(SearchPage {
            items: page
                .items
                .iter()
                .map(|user| user.person(PersonKind::User))
                .collect(),
            next: page.next,
            notice: page.notice,
        })
    }

    /// 与某人的既有私聊（只读，不新建）。
    pub(crate) async fn find_direct(&self, user_id: &str) -> Result<FindDirectResult> {
        Ok(FindDirectResult {
            conversation_id: self.direct_conversation(user_id).await?,
            notice: None,
        })
    }

    /// 群成员的一页（`conversations.members` 只给编号）：名字与头像不在这里读，由人名册按编号读（契约 client v23 §1）。
    pub(crate) async fn list_members(
        &self,
        conversation: &str,
        limit: u16,
        cursor: Option<&str>,
    ) -> Result<Page<Person>> {
        check_id(conversation)?;
        let limit_text = limit.clamp(1, 1000).to_string();
        let value = self
            .query(
                "conversations.members",
                &[
                    ("channel", conversation),
                    ("limit", &limit_text),
                    ("cursor", cursor.unwrap_or_default()),
                ],
            )
            .await?;
        let items = rich::array(&value, "members")
            .iter()
            .filter_map(Value::as_str)
            .filter(|id| is_slack_id(id) && *id != self.me())
            .map(|id| Person {
                id: id.to_owned(),
                id_type: Some(super::mrkdwn::USER_ID_TYPE.into()),
                kind: PersonKind::User,
                name: None,
                avatar: None,
            })
            .collect();
        Ok(Page {
            items,
            next: next_cursor(&value),
        })
    }

    /// 发送（契约 client v11 §3.4）：文字与提及直接发；图片与文件是先传好的，这里分享到会话，
    /// 再在会话里找到带着它的那条消息当回执，30 秒内没找到算“未确认”。Slack 没有引用回复。
    pub(crate) async fn send_message(
        &self,
        operation_id: &str,
        target: &SendTarget,
        content: &[Outgoing],
    ) -> Result<SendOutcome> {
        let (conversation, thread_ts, broadcast) = match target {
            SendTarget::Conversation { conversation_id } => (conversation_id, None, false),
            SendTarget::Reply {
                conversation_id,
                message_id,
                in_thread: true,
                thread_root,
                also_to_conversation,
            } => (
                conversation_id,
                Some(thread_root.clone().unwrap_or_else(|| message_id.clone())),
                *also_to_conversation,
            ),
            SendTarget::Reply { .. } => {
                return Err(Error::new(
                    ErrorKind::Unsupported,
                    "Slack 没有引用回复，请在话题里回复。",
                ));
            }
        };
        check_id(conversation)?;
        let attachment = content.iter().find_map(|item| match item {
            Outgoing::Image { key } | Outgoing::File { key } => Some(key.clone()),
            _ => None,
        });
        let Some(file_id) = attachment else {
            let mut sent = vec![
                self.send(
                    conversation,
                    thread_ts.as_deref(),
                    broadcast,
                    content,
                    operation_id,
                )
                .await?,
            ];
            self.name_references(&mut sent).await;
            return Ok(SendOutcome::Accepted {
                message: Box::new(sent.remove(0).wire(Some(self.me()))),
            });
        };
        if content.len() != 1 {
            return Err(Error::new(
                ErrorKind::InvalidRequest,
                "图片与文件要单独成一条消息发送。",
            ));
        }
        if broadcast {
            // 平台说明里写了带附件不能同时发送（threads.broadcast.attachments），宿主已在登记时拒绝；这里兜底。
            return Err(Error::new(
                ErrorKind::Unsupported,
                "Slack 带图片或文件时不能同时发送到频道。",
            ));
        }
        check_id(&file_id)?;
        let started = now_secs();
        let files = serde_json::json!([{ "id": file_id }]).to_string();
        let mut form = vec![
            ("files", files.as_str()),
            ("channel_id", conversation.as_str()),
        ];
        push_optional(&mut form, "thread_ts", thread_ts.as_deref());
        self.write("files.completeUploadExternal", &form).await?;
        tracing::info!("Slack 附件已分享到会话，等它出现在会话里");
        match self
            .find_attachment(conversation, thread_ts.as_deref(), &file_id, started)
            .await
        {
            Some(message) => {
                let mut found = vec![message];
                self.name_references(&mut found).await;
                Ok(SendOutcome::Accepted {
                    message: Box::new(found.remove(0).wire(Some(self.me()))),
                })
            }
            None => {
                tracing::warn!("Slack 附件发出后 30 秒内还没在会话里看到，记为未确认");
                Ok(SendOutcome::Unknown {
                    reason: "附件已交给 Slack，但 30 秒内还没在会话里看到这条消息。".into(),
                })
            }
        }
    }

    /// 在会话（或话题）最近的消息里找带着这个文件的那条；读不到或没找到时为空。
    async fn find_attachment(
        &self,
        conversation: &str,
        thread_ts: Option<&str>,
        file_id: &str,
        started: i64,
    ) -> Option<SlackMessage> {
        let deadline = Instant::now() + RECEIPT_WAIT;
        let query = SlackHistoryQuery {
            oldest: Some(ts_at(started - RECEIPT_LOOKBACK_SECS, false)),
            limit: 50,
            ..SlackHistoryQuery::default()
        };
        loop {
            let page = match thread_ts {
                Some(root) => self.replies(conversation, root, &query).await,
                None => self.history(conversation, &query).await,
            };
            match page {
                Ok(page) => {
                    let found = page.messages.into_iter().find(|m| {
                        m.message.sender.id == self.me()
                            && m.parts.iter().any(|part| {
                                matches!(part, parts::Part::Resource { resource, .. } if resource.key == file_id)
                            })
                    });
                    if found.is_some() {
                        return found;
                    }
                }
                Err(error) => tracing::debug!(%error, "找附件消息时读取失败，稍后再看"),
            }
            if Instant::now() + RECEIPT_POLL > deadline {
                return None;
            }
            tokio::time::sleep(RECEIPT_POLL).await;
        }
    }

    /// 编辑本人的一条消息（契约 client v11 §3.6）：平台接受后，用平台记下的这条消息回给宿主。
    /// 内置是拿本地记着的那条补上编辑回执里的内容；插件手边没有本地的那条（协议只给编号），所以照 Slack 的消息重读一次
    /// （话题归属、表情都是平台上的）；重读不到时退回编辑回执里的内容（补上编号与本人）。
    pub(crate) async fn edit_message(
        &self,
        conversation: &str,
        id: &str,
        content: &[Outgoing],
    ) -> Result<Message> {
        check_id(conversation)?;
        check_ts(id)?;
        let text = outgoing_text(content)?;
        let value = self
            .write(
                "chat.update",
                &[("channel", conversation), ("ts", id), ("text", &text)],
            )
            .await?;
        tracing::info!("Slack 消息已编辑");
        let mut read = self.find_message(conversation, id).await;
        // 重读暂时失败（网络、限流）时等一下再读一次：编辑回执里没有话题归属，宿主拿它换掉本地那条时，话题回复会
        // 暂时跑到频道里，直到下次读这个话题。
        if let Err(error) = &read
            && matches!(error.kind, ErrorKind::Disconnected | ErrorKind::RateLimited)
        {
            let wait = error
                .retry_after_secs
                .map_or(EDIT_REREAD_WAIT, Duration::from_secs)
                .min(EDIT_REREAD_WAIT_MAX);
            tokio::time::sleep(wait).await;
            read = self.find_message(conversation, id).await;
        }
        let mut fresh = match read {
            Ok(found) => found,
            Err(error) => {
                tracing::warn!(%error, "Slack 消息已编辑，重读这条失败，用编辑回执里的内容");
                let mut raw = value.get("message").cloned().unwrap_or_default();
                if let Some(fields) = raw.as_object_mut() {
                    fields
                        .entry("ts")
                        .or_insert_with(|| Value::String(id.to_owned()));
                    fields
                        .entry("user")
                        .or_insert_with(|| Value::String(self.me().to_owned()));
                }
                let mut message = translate::message(conversation, &raw)?;
                let now = now_ms();
                message.message.edited_at_ms = message.message.edited_at_ms.or(Some(now));
                message.message.updated_at_ms = Some(now);
                // 表情没随编辑回执给出：本地的不动。
                message.message.reactions = None;
                message
            }
        };
        let mut found = vec![fresh];
        self.name_references(&mut found).await;
        fresh = found.remove(0);
        Ok(fresh.wire(Some(self.me())))
    }

    /// 撤回（删除）一条本人消息。
    pub(crate) async fn recall_message(&self, conversation: &str, id: &str) -> Result<()> {
        self.recall(conversation, id).await
    }

    /// 点表情：成功后给出本人点的这一个。
    pub(crate) async fn add_message_reaction(
        &self,
        conversation: &str,
        id: &str,
        name: &str,
    ) -> Result<Reaction> {
        self.add_reaction(conversation, id, name).await?;
        Ok(reaction_of(conversation, id, self.me(), name))
    }

    /// 取消表情：只能取消自己点的。
    pub(crate) async fn remove_message_reaction(
        &self,
        conversation: &str,
        id: &str,
        reaction_id: &str,
    ) -> Result<()> {
        check_id(conversation)?;
        check_ts(id)?;
        let (user, name) = split_reaction_id(reaction_id)
            .ok_or_else(|| Error::new(ErrorKind::InvalidRequest, "表情编号不是 Slack 的形状。"))?;
        if user != self.me() {
            return Err(Error::new(
                ErrorKind::PermissionDenied,
                "只能取消自己点的表情。",
            ));
        }
        self.remove_reaction(conversation, id, name).await
    }

    /// 表情目录（契约 client v11 §3.7、v17 §7）：随包的标准表情加这个工作区的自定义表情；
    /// 别名指向标准表情的用它的字符，指向自定义表情的用它的图。Slack 不给目录的更新时间：写 0，宿主按内容比较是否变了。
    pub(crate) async fn emoji_catalog(&self) -> Result<EmojiCatalogResult> {
        let custom = self.custom_emoji().await?;
        *self.caches.custom_emoji.lock().expect("表情缓存锁") =
            Some(custom.iter().cloned().collect());
        let mut items = emoji::catalog();
        items.extend(custom_items(&custom));
        Ok(EmojiCatalogResult {
            emojis: items.into_iter().map(|item| item.emoji).collect(),
            categories: emoji::categories(),
            tones: emoji::tones(),
            updated_at_ms: 0,
        })
    }

    /// 一个表情的图在哪：工作区自定义的（别名照目录的规则找到原图），否则标准表情的官方图；都没有为空。
    async fn emoji_image_url(&self, emoji_type: &str) -> Result<Option<String>> {
        let cached = self.caches.custom_emoji.lock().expect("表情缓存锁").clone();
        let custom = match cached {
            Some(custom) => custom,
            None => {
                let custom: HashMap<String, String> =
                    self.custom_emoji().await?.into_iter().collect();
                *self.caches.custom_emoji.lock().expect("表情缓存锁") = Some(custom.clone());
                custom
            }
        };
        if let Some(target) = custom.get(emoji_type) {
            let list: Vec<(String, String)> = vec![(emoji_type.to_owned(), target.clone())];
            let mut all: Vec<(String, String)> =
                custom.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            all.extend(list);
            return Ok(custom_items(&all)
                .into_iter()
                .find(|item| item.emoji.emoji_type == emoji_type)
                .map(|item| item.image_url)
                .filter(|url| !url.is_empty()));
        }
        Ok(emoji::standard_info(emoji_type)
            .map(|(_, url)| url)
            .filter(|url| !url.is_empty()))
    }

    /// 下载一个表情的图（自定义表情与标准表情的官方图，只从 Slack 的 CDN，不带令牌）。
    pub(crate) async fn emoji_image(&self, emoji_type: &str) -> Result<Vec<u8>> {
        let address = self
            .emoji_image_url(emoji_type)
            .await?
            .ok_or_else(|| Error::new(ErrorKind::NotFound, "这个表情没有 Slack 上的图。"))?;
        let url = Url::parse(&address)
            .ok()
            .filter(|url| {
                (url.scheme() == "https"
                    && url
                        .host_str()
                        .is_some_and(|h| h.ends_with(".slack-edge.com")))
                    || self.api.same_origin(url)
            })
            .ok_or_else(|| Error::new(ErrorKind::NotFound, "这个表情没有 Slack 上的图。"))?;
        let response = self
            .download
            .get(url)
            .send()
            .await
            .map_err(|error| Error::network(error, "表情图下载失败，请稍后重试。"))?;
        if !response.status().is_success() {
            return Err(errors::from_slack(
                "file_not_found",
                response.status().as_u16(),
                None,
            ));
        }
        let bytes = api::read_limited(response, EMOJI_IMAGE_MAX_BYTES).await?;
        if !is_image(&bytes) {
            return Err(Error::new(
                ErrorKind::ProtocolError,
                "下载到的表情图不是图片。",
            ));
        }
        Ok(bytes)
    }

    /// 打开一个消息里的文件（契约 client v10 §9）：先查文件资料拿到本人才能下载的地址，再带本人令牌从 `offset` 起下载
    /// （Slack 按段给时从这里起，整份给时从头起，运行时替宿主跳到要的位置）。视频按图片要的是它的封面。
    pub(crate) async fn open_resource(
        &self,
        key: &str,
        kind: ResourceKind,
        offset: u64,
    ) -> Result<Opened> {
        check_id(key)?;
        if kind == ResourceKind::Sticker {
            return Err(Error::new(ErrorKind::Unsupported, "Slack 没有表情包资源。"));
        }
        let value = self.query("files.info", &[("file", key)]).await?;
        let file = value.get("file").cloned().unwrap_or_default();
        // 视频文件按图片要的是它的封面：视频缩略图 `thumb_video`（契约 client v35 §1.4）。
        let cover = kind == ResourceKind::Image
            && translate::is_video_mime(rich::str_of(&file, "mimetype"));
        let total = if cover {
            None
        } else {
            file.get("size").and_then(Value::as_u64)
        };
        let addresses: &[&str] = if cover {
            &["thumb_video"]
        } else {
            &["url_private_download", "url_private"]
        };
        let address = addresses
            .iter()
            .map(|key| rich::str_of(&file, key))
            .find(|url| !url.is_empty())
            .ok_or_else(|| Error::new(ErrorKind::NotFound, "Slack 没有给出这个文件的下载地址。"))?;
        let url = Url::parse(address)
            .ok()
            .filter(|url| {
                (url.scheme() == "https"
                    && url
                        .host_str()
                        .is_some_and(|h| h == "files.slack.com" || h.ends_with(".slack.com")))
                    || self.api.same_origin(url)
            })
            .ok_or_else(|| {
                Error::new(
                    ErrorKind::ProtocolError,
                    "Slack 给的文件地址不在 Slack 的域名下。",
                )
            })?;
        self.grant.ensure_current()?;
        let mut request = self
            .download
            .get(url)
            .bearer_auth(self.grant.access_token());
        if offset > 0 {
            request = request.header("Range", format!("bytes={offset}-"));
        }
        let response = request
            .send()
            .await
            .map_err(|error| Error::network(error, "Slack 文件下载失败，请稍后重试。"))?;
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            let name = match status {
                401 | 403 => "not_authed",
                404 | 410 => "file_not_found",
                _ => "service_unavailable",
            };
            return Err(errors::from_slack(name, status, None));
        }
        // 206 是从要的位置起给的；200 是整份（没认 Range），运行时从头跳过去。
        let start = if status == 206 { offset } else { 0 };
        Ok(Opened {
            stream: Box::new(Download(response)),
            start,
            total,
            sha256: None,
        })
    }

    /// 打开一个头像：只接受 Slack 头像 CDN 与 Gravatar 的 HTTPS 地址，不带令牌、重定向不跟随。
    pub(crate) async fn open_avatar(&self, url: &str) -> Result<Opened> {
        let parsed = Url::parse(url)
            .ok()
            .filter(|u| avatar_identity(u.as_str()).is_some() || self.api.same_origin(u))
            .ok_or_else(|| Error::new(ErrorKind::InvalidRequest, "不是 Slack 的头像地址。"))?;
        let response = self
            .download
            .get(parsed)
            .send()
            .await
            .map_err(|error| Error::network(error, "头像读取失败，请稍后重试。"))?;
        if !response.status().is_success() {
            return Err(errors::from_slack(
                "file_not_found",
                response.status().as_u16(),
                None,
            ));
        }
        let total = response.content_length();
        Ok(Opened {
            stream: Box::new(Download(response)),
            start: 0,
            total,
            sha256: None,
        })
    }

    /// 消息里只带编号的频道引用与用户组提及补上名字（契约 client v18 §2.5）：有要补的才按需刷新名字表，
    /// 读不到时照原样交出（频道留着编号、用户组写“用户组”）。
    async fn name_references(&self, messages: &mut [SlackMessage]) {
        if !names::needs_names(messages) {
            return;
        }
        let stale = self.caches.names.lock().expect("Slack 名字表锁").stale();
        if stale {
            self.refresh_names().await;
        }
        let names = self.caches.names.lock().expect("Slack 名字表锁");
        names::fill(&names, messages);
    }

    /// 重读频道（本人所在的，会话缓存）与用户组的名字；读不到的一项保留上次的。
    async fn refresh_names(&self) {
        self.caches.names.lock().expect("Slack 名字表锁").tried_at = Some(Instant::now());
        let channels = match self.cached_conversations().await {
            Ok(list) => Some(
                list.into_iter()
                    .filter(|c| c.kind == ConversationKind::Channel && !c.name.is_empty())
                    .map(|c| (c.id, c.name))
                    .collect(),
            ),
            Err(error) => {
                tracing::warn!(%error, "读本人所在的频道失败，频道引用先留着编号");
                None
            }
        };
        let groups = match self.query("usergroups.list", &[]).await {
            Ok(value) => Some(
                rich::array(&value, "usergroups")
                    .iter()
                    .filter_map(|group| {
                        let id = group.get("id")?.as_str()?;
                        let name = group
                            .get("handle")
                            .and_then(Value::as_str)
                            .filter(|h| !h.is_empty())
                            .or_else(|| group.get("name")?.as_str())?;
                        is_slack_id(id).then(|| (id.to_owned(), name.to_owned()))
                    })
                    .collect(),
            ),
            Err(error) => {
                // 没有 usergroups:read 时平台回 missing_scope：用户组写“用户组”。
                tracing::info!(
                    code = error.platform_code.as_deref().unwrap_or(""),
                    "读不到工作区的用户组，用户组提及写“用户组”"
                );
                None
            }
        };
        let mut names = self.caches.names.lock().expect("Slack 名字表锁");
        if let Some(channels) = channels {
            names.channels = channels;
            names.loaded_at = Some(Instant::now());
        }
        if let Some(groups) = groups {
            names.groups = groups;
        }
    }
}

/// 工作区自定义表情 → 目录项（排在标准表情后面，按名字排序）：别名指向别的自定义表情的用它的图，指向标准表情的
/// 用它的字符与官方图；目标找不到的别名不列。
fn custom_items(custom: &[(String, String)]) -> Vec<CatalogItem> {
    let images: HashMap<&str, &str> = custom
        .iter()
        .filter(|(_, target)| target.starts_with("https://"))
        .map(|(name, target)| (name.as_str(), target.as_str()))
        .collect();
    let mut extra: Vec<CatalogItem> = custom
        .iter()
        .filter_map(|(name, target)| {
            let custom = |mut item: CatalogItem| {
                item.emoji.category = Some(emoji::CUSTOM.into());
                item
            };
            if let Some(original) = target.strip_prefix("alias:") {
                if let Some(url) = images.get(original) {
                    return Some(custom(CatalogItem::new(name, *url)));
                }
                return emoji::standard_info(original).map(|(unicode, url)| {
                    let mut item = CatalogItem::new(name, url);
                    item.emoji.unicode = Some(unicode);
                    custom(item)
                });
            }
            Some(custom(CatalogItem::new(name, target)))
        })
        .collect();
    extra.sort_by(|a, b| a.emoji.emoji_type.cmp(&b.emoji.emoji_type));
    extra
}

/// 字节开头是不是常见图片（PNG、GIF、JPEG、WebP；同 HHH 的判断）。
fn is_image(bytes: &[u8]) -> bool {
    bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a])
        || bytes.starts_with(b"GIF8")
        || bytes.starts_with(&[0xff, 0xd8, 0xff])
        || (bytes.len() > 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn avatar_identities_ignore_the_size_and_other_hosts_are_refused() {
        assert_eq!(
            avatar_identity("https://avatars.slack-edge.com/2023-03-01/4889393_d9b7c6e0f0_72.jpg"),
            Some(("2023-03-01/4889393_d9b7c6e0f0".into(), "72".into()))
        );
        assert_eq!(
            avatar_identity("https://ca.slack-edge.com/T01-U01-abcdef-512"),
            Some(("T01-U01-abcdef".into(), "512".into()))
        );
        assert_eq!(
            avatar_identity("https://secure.gravatar.com/avatar/abc123.jpg?s=72&d=x"),
            Some(("abc123".into(), "72".into()))
        );
        assert_eq!(avatar_identity("https://evil.example.com/a_72.jpg"), None);
        assert_eq!(
            avatar_identity("http://avatars.slack-edge.com/a_72.jpg"),
            None
        );
    }

    /// 宿主按声明认头像身份（同 HHH 的 `Platform::avatar_identity_of`）：编号对不上时整个键就是身份，变体对不上时没有变体。
    fn declared_identity(key: &str) -> (String, String) {
        let first = |pattern: &str| {
            regex_lite::Regex::new(pattern)
                .expect("宿主编得出这个写法")
                .captures(key)
                .and_then(|captures| captures.get(1))
                .map(|found| found.as_str().to_owned())
        };
        (
            first(AVATAR_KEY_PATTERN).unwrap_or_else(|| key.to_owned()),
            first(AVATAR_VARIANT_PATTERN).unwrap_or_default(),
        )
    }

    // 声明给宿主的写法与内置的认法一致：Slack CDN 的各种主机与尺寸、Gravatar 的尺寸参数（地址是编的，形状真实）。
    #[test]
    fn declared_avatar_identity_matches_the_builtin() {
        for url in [
            "https://avatars.slack-edge.com/2023-03-01/4889393_d9b7c6e0f0_72.jpg",
            "https://avatars.slack-edge.com/2023-03-01/4889393_d9b7c6e0f0_192.jpg",
            "https://avatars.slack-edge.com/2026-01-02/1111_aaaa-48.png",
            "https://ca.slack-edge.com/T01-U01-abcdef-512",
            "https://ca.slack-edge.com/T01-U01-abcdef-72",
            "https://ca.slack-edge.com/T01-U01-abcdef",
            "https://a.slack-edge.com/80588/img/avatars/ava_0001-72.png",
            "https://AVATARS.SLACK-EDGE.COM/x/y_24.jpg",
            "https://avatars.slack-edge.com/x/plain.jpg?v=3",
            "https://secure.gravatar.com/avatar/abc123.jpg?s=72&d=https%3A%2F%2Fa.slack-edge.com%2Fdf10d%2Fimg%2Favatars%2Fava_0024-72.png",
            "https://secure.gravatar.com/avatar/abc123.jpg?d=x&s=192",
            "https://secure.gravatar.com/avatar/abc123",
        ] {
            assert_eq!(Some(declared_identity(url)), avatar_identity(url), "{url}");
        }
        // 内置认不出的（不是 https、别的主机）：声明的写法对不上编号，整个键就是身份，不会认错成别的头像。
        for url in [
            "http://avatars.slack-edge.com/a_72.jpg",
            "https://evil.example.com/a_72.jpg",
            "https://slack-edge.com.example.com/a_72.jpg",
        ] {
            assert_eq!(avatar_identity(url), None, "{url}");
            assert_eq!(declared_identity(url).0, url, "{url}");
        }
    }

    // 声明给宿主的表情编号写法与内置的认法一致（宿主另有 64 字节以内、无控制字符的泛化校验）。
    #[test]
    fn declared_emoji_names_match_the_builtin() {
        let rule = regex_lite::Regex::new(EMOJI_NAME_PATTERN).expect("宿主编得出这个写法");
        for name in [
            "+1",
            "thumbsup",
            "wave::skin-tone-3",
            "white_check_mark",
            "its-ok",
            "don't",
            "x",
            "火炬",
            "日本語_emoji",
            "a1",
            "",
            ":smile",
            "smile:",
            ":",
            "a b",
            "a.b",
            "a/b",
            "<x>",
            "a,b",
            "a!b",
            "a@b",
            "a#b",
            "a~b",
            "a=b",
        ] {
            assert_eq!(rule.is_match(name), mrkdwn::is_emoji_name(name), "{name:?}");
        }
    }

    // 编辑用原文（契约 client v18 §2.4）：文字与提及分段、表情写成 :名字:、段落写成换行；不能编辑的没有原文。
    #[test]
    fn edit_drafts_split_text_and_mentions_and_round_trip_to_mrkdwn() {
        use hhh_im_protocol::content::{PersonRef, RichNode};
        use parts::Part;
        let person = PersonRef {
            user_id: "U000000002".into(),
            id_type: Some("user_id".into()),
            kind: None,
            name: Some("小王".into()),
        };
        let rich = vec![Part::Rich(vec![
            RichNode::Paragraph {
                children: vec![
                    RichNode::Text {
                        value: "好的 ".into(),
                    },
                    RichNode::Mention {
                        person: Some(person),
                        everyone: false,
                    },
                    RichNode::Emoji {
                        emoji_type: "+1".into(),
                    },
                ],
            },
            RichNode::Paragraph {
                children: vec![
                    mrkdwn::everyone_mention("channel"),
                    RichNode::Link {
                        url: "https://example.com".into(),
                        children: vec![RichNode::Text {
                            value: "https://example.com".into(),
                        }],
                    },
                ],
            },
        ])];
        let draft = translate::edit_draft(&rich).unwrap();
        assert_eq!(
            draft,
            vec![
                Outgoing::Text {
                    text: "好的 ".into()
                },
                Outgoing::Mention {
                    user_id: "U000000002".into(),
                    name: "小王".into()
                },
                Outgoing::Text {
                    text: ":+1:\n".into()
                },
                Outgoing::MentionAll,
                Outgoing::Text {
                    text: "https://example.com".into()
                },
            ]
        );
        assert_eq!(
            outgoing_text(&draft).unwrap(),
            "好的 <@U000000002>:+1:\n<!channel>https://example.com"
        );
        let code = vec![Part::Rich(vec![RichNode::Code {
            lang: None,
            value: "x".into(),
        }])];
        assert_eq!(translate::edit_draft(&code), None);
        // @here、用户组的提及发送时写不出来，编辑后会变样：不算能编辑。
        for mention in [
            mrkdwn::everyone_mention("here"),
            mrkdwn::group_mention("S03", Some("dev")),
        ] {
            let odd = vec![Part::Rich(vec![RichNode::Paragraph {
                children: vec![mention],
            }])];
            assert_eq!(translate::edit_draft(&odd), None);
        }
        let channel = vec![Part::Rich(vec![RichNode::Paragraph {
            children: vec![mrkdwn::everyone_mention("channel")],
        }])];
        assert_eq!(
            translate::edit_draft(&channel),
            Some(vec![Outgoing::MentionAll])
        );
        assert_eq!(
            translate::edit_draft(&[Part::Text("hi".into())]),
            Some(vec![Outgoing::Text { text: "hi".into() }])
        );
        // 带文字说明的链接（编辑后说明会丢）、带文件的不能编辑。
        let labelled = vec![Part::Rich(vec![RichNode::Paragraph {
            children: vec![RichNode::Link {
                url: "https://example.com".into(),
                children: vec![RichNode::Text {
                    value: "链接".into(),
                }],
            }],
        }])];
        assert_eq!(translate::edit_draft(&labelled), None);
    }

    #[test]
    fn local_search_pages_by_offset_and_keeps_the_scope() {
        let all: Vec<String> = (0..5).map(|i| format!("dev-{i}")).collect();
        let first = search_page(&all, "s", 2, None, |s| contains(s, "DEV")).unwrap();
        assert_eq!(first.items, ["dev-0", "dev-1"]);
        let second =
            search_page(&all, "s", 2, first.next.as_deref(), |s| contains(s, "DEV")).unwrap();
        assert_eq!(second.items, ["dev-2", "dev-3"]);
        assert!(search_page(&all, "other", 2, first.next.as_deref(), |_| true).is_err());
        for (text, limit) in [("  ", 5), ("a\u{7}", 5), ("x", 0), ("x", 21)] {
            assert_eq!(
                check_search(text, limit).unwrap_err().kind,
                ErrorKind::InvalidRequest,
                "{text:?} / {limit}"
            );
        }
        assert!(check_search(&"字".repeat(50), 20).is_ok());
        assert!(check_search(&"字".repeat(51), 20).is_err());
    }
}
