//! Slack 消息 → 协议的统一消息（契约 client v11 §3.3–§3.9；规则从 HHH 内置 Slack 原样搬来，输出直接是协议的消息）。
//!
//! 取样事实（回溯 v11 §4）：
//! - 编号：消息以 `ts`（`秒.微秒` 的字符串）为编号，只在所在会话里唯一；
//! - 话题：根消息 `thread_ts == ts`，回复 `thread_ts` 指向根；“回复同时发到频道”的回复 `subtype`
//!   是 `thread_broadcast`，既进话题也在主消息流里；
//! - 发送者：人发的有 `user`；自己用本人令牌经应用发的也是 `user` = 本人，同时带 `bot_id` 等应用标记，
//!   判断本人只看 `user`；机器人发的是 `subtype: bot_message`，只有 `bot_id` 与 `bot_profile`；
//! - 正文：人发的都带 `rich_text` 块，`text` 只是兜底；网址预览、分享的消息在 `attachments`；
//!   表情回复内嵌在 `reactions` 里。
//!
//! 编辑过、同时发到频道与内嵌的表情写进协议的消息；发送编号、子类型与回复数这些只有 Slack 有的事实
//! 放在 [`SlackExtras`]，插件自己用。内容先翻成 [`Part`]（读消息时还要补频道与用户组的名字），交给宿主时
//! 再写成协议的内容块（[`SlackMessage::wire`]），本人能编辑的消息同时带上编辑原文。

use super::card;
use super::mrkdwn::{GROUP_ID_TYPE, USER_ID_TYPE, to_rich, tokenize};
use super::parts::Part;
use super::rich::{self, array, str_of};
use crate::{Error, ErrorKind, Result};
use hhh_im_protocol::content::{PersonRef, RichNode};
use hhh_im_protocol::types::{
    Message, Outgoing, Person, PersonKind, Reaction, Resource, ResourceCover, ResourceKind, Sender,
    SenderKind, ThreadActivity, ThreadInfo,
};
use serde_json::Value;

/// 一条消息最多的内容块；与飞书同一预算。
const CONTENT_BLOCKS_MAX: usize = 2048;
/// Slack 编号的长度上限（会话、用户、文件、ts）。
const SLACK_ID_MAX_BYTES: usize = 64;
/// 机器人发言者的编号种类（与内置 Slack 时代相同）。
pub(crate) const BOT_ID_TYPE: &str = "bot_id";

/// 算进“系统消息”的子类型：加入、离开、改名、改话题、置顶等，界面居中显示、不算未读。
const SYSTEM_SUBTYPES: &[&str] = &[
    "channel_join",
    "channel_leave",
    "channel_topic",
    "channel_purpose",
    "channel_name",
    "channel_archive",
    "channel_unarchive",
    "channel_convert_to_private",
    "channel_convert_to_public",
    "group_join",
    "group_leave",
    "group_topic",
    "group_purpose",
    "group_name",
    "group_archive",
    "group_unarchive",
    "pinned_item",
    "unpinned_item",
    "bot_add",
    "bot_remove",
    "reminder_add",
];

/// 翻译好的一条 Slack 消息。
#[derive(Debug, Clone, PartialEq)]
pub struct SlackMessage {
    /// 协议的消息；内容块在 [`SlackMessage::wire`] 里由 `parts` 写进去。
    pub(crate) message: Message,
    /// 内容（写成协议内容块之前）。
    pub(crate) parts: Vec<Part>,
    /// 协议的消息里没有位置的事实。
    pub(crate) extras: SlackExtras,
}

/// 只有 Slack 有的事实（编辑时间、同时发到频道也在这里留一份原样的，协议的消息里另有）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SlackExtras {
    /// 编辑时间（Unix 毫秒）；没编辑过为空。
    pub(crate) edited_at_ms: Option<i64>,
    /// 是话题回复，同时也发到了频道主消息流。
    pub(crate) broadcast: bool,
    /// 内嵌的表情回复：名字、点的人（最多 Slack 给的那些）、总数。
    pub(crate) reactions: Vec<SlackReaction>,
    /// 发送时带的编号（`client_msg_id`）；按它核对“未确认”的发送。
    pub(crate) client_msg_id: Option<String>,
    /// Slack 的子类型原样保留，没有子类型为空。
    pub(crate) subtype: Option<String>,
    /// 话题根消息的回复数。
    pub(crate) reply_count: u32,
}

/// 一个表情回复。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SlackReaction {
    /// 表情名，如 `+1`、`wave::skin-tone-3`、工作区自定义表情名。
    pub(crate) name: String,
    /// 点了这个表情的人（Slack 只列出一部分，`count` 是总数）。
    pub(crate) users: Vec<String>,
    /// 总共多少人点了。
    pub(crate) count: u32,
}

impl SlackMessage {
    /// 交给宿主的协议消息：内容块按顺序写出；是本人（`me`）发的、能编辑时带上编辑原文（契约 client v18 §2.4，
    /// 协议 `actions.edit.perMessage`）。
    pub(crate) fn wire(&self, me: Option<&str>) -> Message {
        let mut message = self.message.clone();
        message.content = self.parts.iter().map(Part::block).collect();
        let mine = me.is_some_and(|me| {
            !me.is_empty() && message.sender.kind == SenderKind::User && message.sender.id == me
        });
        if mine && !message.deleted {
            message.edit_draft = edit_draft(&self.parts);
        }
        message
    }

    /// 消息编号（`ts`）。
    pub(crate) fn id(&self) -> &str {
        &self.message.id
    }
}

/// 一条消息的原始 JSON → 协议消息。`conversation` 是消息所在会话的编号。
pub(crate) fn message(conversation: &str, raw: &Value) -> Result<SlackMessage> {
    if !is_slack_id(conversation) {
        return Err(Error::new(
            ErrorKind::InvalidRequest,
            "Slack 会话编号不合法。",
        ));
    }
    let ts = str_of(raw, "ts");
    let created_at_ms = ts_millis(ts)
        .ok_or_else(|| Error::new(ErrorKind::ProtocolError, "Slack 消息缺少有效的时间编号。"))?;
    let subtype = raw.get("subtype").and_then(Value::as_str);
    let system = subtype.is_some_and(|s| SYSTEM_SUBTYPES.contains(&s));
    let deleted = subtype == Some("tombstone");

    let sender = sender_of(raw, system);
    let parts = if deleted {
        Vec::new()
    } else {
        let mut parts = body(raw);
        parts.truncate(CONTENT_BLOCKS_MAX);
        parts
    };
    let incomplete = parts.iter().any(Part::has_unsupported);

    // 话题：根消息 thread_ts == ts；回复的 thread_ts 指向根。
    let thread_ts = raw
        .get("thread_ts")
        .and_then(Value::as_str)
        .filter(|t| ts_millis(t).is_some());
    let root_id = thread_ts.filter(|root| *root != ts).map(str::to_owned);
    let edited_at_ms = raw
        .get("edited")
        .and_then(|e| e.get("ts"))
        .and_then(Value::as_str)
        .and_then(ts_millis);
    let broadcast = subtype == Some("thread_broadcast");
    let embedded = reactions_of(raw);
    // 没有 reactions 就是没人点过；列出的人不全时给不出全部，留空让宿主另外读。
    let reactions = if raw.get("reactions").is_some() {
        model_reactions(conversation, ts, &embedded)
    } else {
        Some(Vec::new())
    };
    let extras = SlackExtras {
        edited_at_ms,
        broadcast,
        reactions: embedded,
        client_msg_id: raw
            .get("client_msg_id")
            .and_then(Value::as_str)
            .map(str::to_owned),
        subtype: subtype.map(str::to_owned),
        reply_count: raw
            .get("reply_count")
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or(0),
    };
    // 话题根消息上的话题现状（契约 client v27 取样）：`latest_reply` 是最后一条回复的 ts，`reply_count` 是回复数；
    // 新回复让两项立即变，删掉最后一条回复让 `latest_reply` 退回上一条、回复数减一。改回复两项都不变。
    let activity = match (thread_ts, raw.get("latest_reply").and_then(Value::as_str)) {
        (Some(root), Some(latest)) if root == ts && !deleted => {
            ts_millis(latest).map(|latest_reply_at_ms| ThreadActivity {
                latest_reply_id: latest.to_owned(),
                latest_reply_at_ms,
                reply_count: extras.reply_count,
            })
        }
        _ => None,
    };
    // Slack 说得清：带 thread_ts 的，等于自己是根（在会话消息流里），指向别人是话题回复。
    let thread = (thread_ts.is_some() || broadcast || activity.is_some()).then(|| ThreadInfo {
        id: thread_ts.map(str::to_owned),
        root_id,
        in_thread: thread_ts.map(|root| root != ts),
        broadcast,
        activity,
        copy: None,
    });
    Ok(SlackMessage {
        message: Message {
            read_position: super::reading::position(ts),
            id: ts.to_owned(),
            conversation_id: conversation.to_owned(),
            sender,
            created_at_ms,
            updated_at_ms: edited_at_ms,
            edited_at_ms: edited_at_ms.filter(|_| !deleted),
            deleted,
            incomplete,
            content: Vec::new(),
            reply_to: None,
            thread,
            reactions: reactions.filter(|_| !deleted),
            raw: None,
            edit_draft: None,
            agent_attribution: None,
        },
        parts,
        extras,
    })
}

/// 表情编号：点的人 + 表情名（契约 client v11 §11 第 4 项）。Slack 的表情没有编号，同一个人对同一条消息
/// 同一个表情只能点一次，所以这样拼出来是稳定的；用户编号里没有冒号，按第一个冒号拆得开。
pub(crate) fn reaction_id(user: &str, name: &str) -> String {
    format!("{user}:{name}")
}

/// 拆开 [`reaction_id`]：点的人与表情名；不是这个形状时为空。
pub(crate) fn split_reaction_id(id: &str) -> Option<(&str, &str)> {
    let (user, name) = id.split_once(':')?;
    (is_slack_id(user) && super::mrkdwn::is_emoji_name(name)).then_some((user, name))
}

/// 一条 Slack 表情 → 协议的表情（没有时间）。
pub(crate) fn reaction_of(conversation: &str, message: &str, user: &str, name: &str) -> Reaction {
    Reaction {
        id: reaction_id(user, name),
        conversation_id: conversation.to_owned(),
        message_id: message.to_owned(),
        operator: Person {
            id: user.to_owned(),
            id_type: Some(USER_ID_TYPE.into()),
            kind: PersonKind::User,
            name: None,
            avatar: None,
        },
        emoji_type: name.to_owned(),
        at_ms: None,
    }
}

/// 内嵌的表情 → 这条消息现在的全部表情。Slack 只列出一部分点的人时（总数比列出的多）给不出全部，为空，
/// 由宿主另外按条读。
fn model_reactions(
    conversation: &str,
    message: &str,
    embedded: &[SlackReaction],
) -> Option<Vec<Reaction>> {
    if embedded
        .iter()
        .any(|r| usize::try_from(r.count).unwrap_or(usize::MAX) > r.users.len())
    {
        return None;
    }
    Some(
        embedded
            .iter()
            .flat_map(|r| {
                r.users
                    .iter()
                    .map(|user| reaction_of(conversation, message, user, &r.name))
            })
            .collect(),
    )
}

/// 发送者：人（`user`）、机器人（`bot_message`，只有 `bot_id`）或系统消息（没有身份，编号种类也空着，同内置）。
fn sender_of(raw: &Value, system: bool) -> Sender {
    if system {
        return Sender {
            id: String::new(),
            id_type: Some(String::new()),
            kind: SenderKind::System,
            name: None,
        };
    }
    let user = str_of(raw, "user");
    let bot_message = str_of(raw, "subtype") == "bot_message";
    if !user.is_empty() && !bot_message {
        let profile = raw.get("user_profile");
        let name = profile
            .and_then(|p| {
                ["display_name", "real_name", "name"]
                    .iter()
                    .map(|key| str_of(p, key))
                    .find(|n| !n.is_empty())
            })
            .map(str::to_owned);
        return Sender {
            id: user.to_owned(),
            id_type: Some(USER_ID_TYPE.into()),
            kind: SenderKind::User,
            name,
        };
    }
    let bot = raw.get("bot_profile");
    let name = bot
        .map(|b| str_of(b, "name"))
        .filter(|n| !n.is_empty())
        .or_else(|| Some(str_of(raw, "username")).filter(|n| !n.is_empty()))
        .map(str::to_owned);
    Sender {
        id: str_of(raw, "bot_id").to_owned(),
        id_type: Some(BOT_ID_TYPE.into()),
        kind: SenderKind::Bot,
        name,
    }
}

/// 正文：先按块的顺序翻（连续的 `rich_text` 合成一段格式正文，连续的其他块合成一张卡片），
/// 没有块时解析 `text`；再接文件，最后是附件。
fn body(raw: &Value) -> Vec<Part> {
    body_at(raw, 0, &mut 0)
}

pub(super) fn body_at(raw: &Value, depth: usize, used: &mut usize) -> Vec<Part> {
    let mut out = Vec::new();
    let blocks = array(raw, "blocks");
    if blocks.is_empty() {
        let text = str_of(raw, "text");
        if !text.is_empty() {
            out.extend(from_rich_nodes(to_rich(&tokenize(text))));
        }
    } else {
        let mut group: Vec<&Value> = Vec::new();
        let mut group_is_rich = false;
        for block in blocks {
            let is_rich = str_of(block, "type") == "rich_text";
            if !group.is_empty() && is_rich != group_is_rich {
                flush_group(&mut group, group_is_rich, &mut out);
            }
            group_is_rich = is_rich;
            group.push(block);
        }
        flush_group(&mut group, group_is_rich, &mut out);
    }
    for file in array(raw, "files") {
        out.push(file_content(file));
    }
    for attachment in array(raw, "attachments") {
        if let Some(record) = super::forwarded::attachment(attachment, depth, used) {
            out.push(record);
        } else {
            out.extend(card::from_attachment(attachment));
        }
    }
    out
}

fn flush_group(group: &mut Vec<&Value>, rich_group: bool, out: &mut Vec<Part>) {
    if group.is_empty() {
        return;
    }
    if rich_group {
        match rich::blocks(group) {
            Ok(nodes) => out.extend(from_rich_nodes(nodes)),
            Err(_) => out.push(Part::Unsupported {
                kind: "rich_text".into(),
                label: Some("太长的格式正文".into()),
            }),
        }
    } else {
        out.extend(card::from_blocks(group));
    }
    group.clear();
}

/// 格式正文 → 内容块。只有文字、提及与行内换行时拆成纯文字与提及（与飞书普通消息一致），
/// 有格式、链接、表情或多种块时整条用格式正文。
fn from_rich_nodes(nodes: Vec<RichNode>) -> Vec<Part> {
    if nodes.is_empty() {
        return Vec::new();
    }
    let plain = nodes.iter().all(|node| match node {
        RichNode::Paragraph { children } => children.iter().all(|c| {
            matches!(
                c,
                RichNode::Text { .. } | RichNode::Mention { .. } | RichNode::Break
            )
        }),
        _ => false,
    });
    if !plain {
        return vec![Part::Rich(nodes)];
    }
    fn push_text(out: &mut Vec<Part>, value: &str) {
        if let Some(Part::Text(text)) = out.last_mut() {
            text.push_str(value);
        } else {
            out.push(Part::Text(value.to_owned()));
        }
    }
    let mut out: Vec<Part> = Vec::new();
    for (index, node) in nodes.into_iter().enumerate() {
        if index > 0 {
            push_text(&mut out, "\n");
        }
        if let RichNode::Paragraph { children } = node {
            for child in children {
                match child {
                    RichNode::Text { value } => push_text(&mut out, &value),
                    RichNode::Break => push_text(&mut out, "\n"),
                    RichNode::Mention { person, everyone } => {
                        out.push(Part::Mention { person, everyone })
                    }
                    _ => {}
                }
            }
        }
    }
    out
}

/// 一个文件 → 资源块。图片按图片取，其余按文件；已删除或看不到的文件记为暂不支持。
fn file_content(file: &Value) -> Part {
    let id = str_of(file, "id");
    let mode = str_of(file, "mode");
    if !is_slack_id(id) || matches!(mode, "tombstone" | "hidden_by_limit") {
        return Part::Unsupported {
            kind: "file".into(),
            label: Some("文件不可见".into()),
        };
    }
    if mode == "external" {
        return Part::Unsupported {
            kind: "external_file".into(),
            label: Some("外部文件".into()),
        };
    }
    let mime = str_of(file, "mimetype");
    let kind = if mime.starts_with("image/") {
        ResourceKind::Image
    } else {
        ResourceKind::File
    };
    let name = ["name", "title"]
        .iter()
        .map(|key| str_of(file, key))
        .find(|n| !n.is_empty())
        .map(str::to_owned);
    let mut resource = Resource {
        voice: false,
        key: id.to_owned(),
        kind,
        mime: (!mime.is_empty()).then(|| mime.to_owned()),
        bytes: None,
        duration_ms: None,
        cover: None,
    };
    // 图片的宽高不给（同内置：显示宽高由宿主下载后量）；视频给画面宽高。
    let (mut width, mut height) = (None, None);
    if kind == ResourceKind::File && is_video_mime(mime) {
        let video = video_of(file, id);
        resource.duration_ms = video.duration_ms;
        resource.cover = video.cover;
        (width, height) = video.size.unzip();
    }
    Part::Resource {
        resource,
        name,
        width,
        height,
    }
}

/// `mime` 是不是视频（同 HHH 的判断：开头是 `video/`，不分大小写）。
pub(crate) fn is_video_mime(mime: &str) -> bool {
    mime.get(..6)
        .is_some_and(|head| head.eq_ignore_ascii_case("video/"))
}

/// Slack 视频文件的封面、时长与画面宽高。
struct Video {
    cover: Option<ResourceCover>,
    duration_ms: Option<u64>,
    size: Option<(u32, u32)>,
}

/// Slack 的视频文件（契约 client v35 §1.4；字段取样见 friend/video-preview 回溯 §3）：封面是这个文件的视频缩略图
/// `thumb_video`（用同一个文件编号按图片取，见读资源），宽高 `thumb_video_w/h`，时长 `duration_ms`。缺的项为空。
fn video_of(file: &Value, id: &str) -> Video {
    let number = |key: &str| {
        file.get(key)
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .filter(|n| *n > 0)
    };
    let size = number("thumb_video_w").zip(number("thumb_video_h"));
    Video {
        cover: (!str_of(file, "thumb_video").is_empty()).then(|| ResourceCover {
            key: id.to_owned(),
            width: size.map(|(w, _)| w),
            height: size.map(|(_, h)| h),
        }),
        duration_ms: file
            .get("duration_ms")
            .and_then(Value::as_u64)
            .filter(|ms| *ms > 0),
        size,
    }
}

fn reactions_of(raw: &Value) -> Vec<SlackReaction> {
    array(raw, "reactions")
        .iter()
        .filter_map(|reaction| {
            let name = str_of(reaction, "name");
            super::mrkdwn::is_emoji_name(name).then(|| SlackReaction {
                name: name.to_owned(),
                users: array(reaction, "users")
                    .iter()
                    .filter_map(Value::as_str)
                    .filter(|u| is_slack_id(u))
                    .map(str::to_owned)
                    .collect(),
                count: reaction
                    .get("count")
                    .and_then(Value::as_u64)
                    .and_then(|n| u32::try_from(n).ok())
                    .unwrap_or(0),
            })
        })
        .collect()
}

/// Slack 编号：字母数字，限长。
pub(crate) fn is_slack_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= SLACK_ID_MAX_BYTES
        && id.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// `秒.微秒` → Unix 毫秒。格式不对时为空。
pub(crate) fn ts_millis(ts: &str) -> Option<i64> {
    let (secs, micros) = ts.split_once('.')?;
    if secs.is_empty() || secs.len() > 12 || micros.len() != 6 {
        return None;
    }
    if !secs
        .bytes()
        .chain(micros.bytes())
        .all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let secs: i64 = secs.parse().ok()?;
    let micros: i64 = micros.parse().ok()?;
    Some(secs * 1000 + micros / 1000)
}

/// 提及能不能原样编辑回去（契约 client v11 §3.6、v18 §2.4，同内置）：发送只会写人与 `@channel`；`@here`、用户组的
/// 提及编辑后会变样，不算能编辑。
pub(crate) fn editable_mention(person: Option<&PersonRef>, everyone: bool) -> bool {
    match (person, everyone) {
        (person, true) => person.is_none_or(|p| p.name.as_deref() == Some("channel")),
        (Some(person), false) => person.id_type.as_deref() == Some(USER_ID_TYPE),
        (None, false) => false,
    }
}

/// 链接里只有文字时，它的文字；有别的节点时为空。
fn link_text(children: &[RichNode]) -> Option<String> {
    children
        .iter()
        .map(|node| match node {
            RichNode::Text { value } => Some(value.as_str()),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()
        .map(|parts| parts.concat())
}

/// 能编辑的内容（契约 client v11 §3.6）：只有文字、提及、链接与表情，没有附件、卡片、代码块等格式。
/// 编辑后是纯文字加提及，这样的消息编辑了不会丢掉原来的格式。
fn editable(parts: &[Part]) -> bool {
    fn plain(nodes: &[RichNode]) -> bool {
        nodes.iter().all(|node| match node {
            RichNode::Text { .. } | RichNode::Break | RichNode::Emoji { .. } => true,
            RichNode::Mention { person, everyone } => editable_mention(person.as_ref(), *everyone),
            RichNode::Link { url, children } => {
                link_text(children).is_some_and(|text| text == *url)
            }
            _ => false,
        })
    }
    !parts.is_empty()
        && parts.iter().all(|part| match part {
            Part::Text(_) => true,
            Part::Mention { person, everyone } => editable_mention(person.as_ref(), *everyone),
            Part::Rich(nodes) => nodes.iter().all(|node| match node {
                RichNode::Paragraph { children } => plain(children),
                _ => false,
            }),
            _ => false,
        })
}

/// 编辑用原文（契约 client v18 §2.4）：能编辑的消息拆成文字与提及几段，表情写成 `:名字:`、换行与段落写成换行；
/// 保存时这几段原样交回，内容不丢。不能编辑时为空。
pub(crate) fn edit_draft(parts: &[Part]) -> Option<Vec<Outgoing>> {
    fn push_text(out: &mut Vec<Outgoing>, text: &str) {
        if let Some(Outgoing::Text { text: last }) = out.last_mut() {
            last.push_str(text);
        } else if !text.is_empty() {
            out.push(Outgoing::Text {
                text: text.to_owned(),
            });
        }
    }
    fn mention(out: &mut Vec<Outgoing>, person: Option<&PersonRef>, everyone: bool) {
        match (person, everyone) {
            (_, true) => out.push(Outgoing::MentionAll),
            (Some(person), false) => out.push(Outgoing::Mention {
                user_id: person.user_id.clone(),
                name: person.name.clone().unwrap_or_default(),
            }),
            (None, false) => {}
        }
    }
    fn walk(out: &mut Vec<Outgoing>, nodes: &[RichNode]) {
        for node in nodes {
            match node {
                RichNode::Text { value } => push_text(out, value),
                RichNode::Break => push_text(out, "\n"),
                RichNode::Emoji { emoji_type } => push_text(out, &format!(":{emoji_type}:")),
                RichNode::Link { url, .. } => push_text(out, url),
                RichNode::Mention { person, everyone } => mention(out, person.as_ref(), *everyone),
                _ => {}
            }
        }
    }
    if !editable(parts) {
        return super::rich_send::edit_draft(parts);
    }
    let mut out = Vec::new();
    for part in parts {
        match part {
            Part::Text(text) => push_text(&mut out, text),
            Part::Mention { person, everyone } => mention(&mut out, person.as_ref(), *everyone),
            Part::Rich(nodes) => {
                for (index, paragraph) in nodes.iter().enumerate() {
                    if index > 0 {
                        push_text(&mut out, "\n");
                    }
                    if let RichNode::Paragraph { children } = paragraph {
                        walk(&mut out, children);
                    }
                }
            }
            _ => {}
        }
    }
    Some(out)
}

/// 用户组提及与提及所有人里的人（别处用来认）。
pub(crate) fn is_group(person: &PersonRef) -> bool {
    person.id_type.as_deref() == Some(GROUP_ID_TYPE)
}

/// 提及所有人的原词（`here`、`channel`、`everyone`）。
#[cfg(test)]
pub(crate) fn everyone_word(person: &PersonRef) -> Option<&str> {
    (person.id_type.as_deref() == Some(super::mrkdwn::BROADCAST_ID_TYPE))
        .then_some(person.user_id.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hhh_im_protocol::types::Block;
    use serde_json::json;

    fn translate(raw: Value) -> SlackMessage {
        message("C01", &raw).unwrap()
    }

    fn user(id: &str) -> PersonRef {
        PersonRef {
            user_id: id.into(),
            id_type: Some(USER_ID_TYPE.into()),
            kind: None,
            name: None,
        }
    }

    #[test]
    fn timestamps_are_seconds_and_microseconds() {
        assert_eq!(ts_millis("1759120000.123456"), Some(1_759_120_000_123));
        for bad in ["", "1759120000", "1759120000.12", "abc.123456", "1.1234567"] {
            assert_eq!(ts_millis(bad), None, "{bad}");
        }
    }

    #[test]
    fn plain_rich_text_with_mentions_becomes_text_and_mention_blocks() {
        let msg = translate(json!({
            "type": "message", "user": "U01", "ts": "1759120000.000100", "text": "兜底",
            "client_msg_id": "c-1",
            "blocks": [{"type": "rich_text", "elements": [{"type": "rich_text_section", "elements": [
                {"type": "text", "text": "你好 "}, {"type": "user", "user_id": "U02"}, {"type": "text", "text": "\n第二行"}
            ]}]}]
        }));
        assert_eq!(msg.message.sender.kind, SenderKind::User);
        assert_eq!(msg.message.sender.id, "U01");
        assert_eq!(
            msg.parts,
            vec![
                Part::Text("你好 ".into()),
                Part::Mention {
                    person: Some(user("U02")),
                    everyone: false
                },
                Part::Text("\n第二行".into()),
            ]
        );
        assert_eq!(msg.extras.client_msg_id.as_deref(), Some("c-1"));
        assert!(!msg.message.incomplete);
        let wire = msg.wire(Some("U01"));
        assert_eq!(
            wire.content[1],
            Block::Mention {
                user_id: Some("U02".into()),
                id_type: Some("user_id".into()),
                kind: None,
                name: None,
                everyone: false
            }
        );
        assert_eq!(
            wire.edit_draft,
            Some(vec![
                Outgoing::Text {
                    text: "你好 ".into()
                },
                Outgoing::Mention {
                    user_id: "U02".into(),
                    name: String::new()
                },
                Outgoing::Text {
                    text: "\n第二行".into()
                },
            ]),
            "本人发的能编辑，带编辑原文"
        );
        assert_eq!(msg.wire(Some("U09")).edit_draft, None, "别人的不带");
    }

    #[test]
    fn own_messages_sent_through_the_app_stay_the_users() {
        // 取样事实：用本人令牌经应用发出的消息 user 是本人，同时带 bot_id / app_id / bot_profile。
        let msg = translate(json!({
            "type": "message", "user": "U01", "bot_id": "B01", "app_id": "A01",
            "bot_profile": {"name": "HHH-Friend"}, "ts": "1759120000.000200", "text": "hi"
        }));
        assert_eq!(msg.message.sender.kind, SenderKind::User);
        assert_eq!(msg.message.sender.id, "U01");
    }

    #[test]
    fn bot_messages_system_messages_and_tombstones() {
        let bot = translate(json!({
            "type": "message", "subtype": "bot_message", "bot_id": "B09", "username": "CI",
            "ts": "1759120000.000300", "text": "*构建* 通过"
        }));
        assert_eq!(bot.message.sender.kind, SenderKind::Bot);
        assert_eq!(bot.message.sender.id, "B09");
        assert_eq!(bot.message.sender.id_type.as_deref(), Some(BOT_ID_TYPE));
        assert_eq!(bot.message.sender.name.as_deref(), Some("CI"));
        assert!(matches!(bot.parts[0], Part::Rich(_)));

        let join = translate(json!({
            "type": "message", "subtype": "channel_join", "user": "U03",
            "ts": "1759120000.000400", "text": "<@U03> has joined the channel"
        }));
        assert_eq!(join.message.sender.kind, SenderKind::System);
        assert_eq!(join.message.sender.id_type.as_deref(), Some(""));
        assert!(matches!(join.parts[0], Part::Mention { .. }));
        assert_eq!(join.wire(Some("U03")).edit_draft, None, "系统消息不能编辑");

        let gone = translate(json!({
            "type": "message", "subtype": "tombstone", "ts": "1759120000.000500",
            "thread_ts": "1759120000.000500", "reply_count": 2, "text": "This message was deleted."
        }));
        assert!(gone.message.deleted);
        assert!(gone.parts.is_empty());
        assert_eq!(
            gone.message.thread.as_ref().unwrap().id.as_deref(),
            Some("1759120000.000500")
        );
    }

    #[test]
    fn video_files_carry_their_thumbnail_as_cover_with_size_and_duration() {
        // 契约 client v35 §1.4：mimetype 是视频的文件是视频；封面是同一个文件编号按图片取的视频缩略图。
        let posted = translate(json!({
            "type": "message", "user": "U01", "ts": "1759120001.000100", "text": "",
            "files": [
                {"id": "F01", "mode": "hosted", "mimetype": "video/mp4", "name": "demo.mp4",
                 "thumb_video": "https://files.slack.com/files-tmb/T1-F01-abc/demo_thumb_video.jpg",
                 "thumb_video_w": 1280, "thumb_video_h": 720, "duration_ms": 8000},
                {"id": "F02", "mode": "hosted", "mimetype": "video/quicktime", "name": "clip.mov"},
                {"id": "F03", "mode": "hosted", "mimetype": "audio/mpeg", "name": "voice.mp3"}
            ]
        }));
        let Part::Resource {
            resource,
            width,
            height,
            ..
        } = &posted.parts[0]
        else {
            panic!("不是资源：{:?}", posted.parts)
        };
        assert_eq!(resource.kind, ResourceKind::File);
        assert_eq!(resource.mime.as_deref(), Some("video/mp4"));
        assert_eq!(resource.duration_ms, Some(8000));
        assert_eq!(
            resource.cover,
            Some(ResourceCover {
                key: "F01".into(),
                width: Some(1280),
                height: Some(720)
            })
        );
        assert_eq!((*width, *height), (Some(1280), Some(720)));
        let Part::Resource {
            resource: plain, ..
        } = &posted.parts[1]
        else {
            panic!()
        };
        assert_eq!(
            (plain.cover.as_ref(), plain.duration_ms),
            (None, None),
            "没缩略图的视频没有封面"
        );
        let Part::Resource {
            resource: audio,
            width,
            ..
        } = &posted.parts[2]
        else {
            panic!()
        };
        assert_eq!((audio.cover.as_ref(), *width), (None, None));
    }

    #[test]
    fn threads_broadcasts_edits_reactions_and_files() {
        let reply = translate(json!({
            "type": "message", "subtype": "thread_broadcast", "user": "U01",
            "ts": "1759120001.000100", "thread_ts": "1759120000.000100", "text": "也发到频道",
            "edited": {"user": "U01", "ts": "1759120002.000000"},
            "reactions": [{"name": "+1", "users": ["U02"], "count": 3}, {"name": "bad name", "count": 1}],
            "files": [
                {"id": "F01", "mode": "hosted", "mimetype": "image/png", "name": "a.png"},
                {"id": "F02", "mode": "hosted", "mimetype": "application/pdf", "title": "文档"},
                {"id": "F03", "mode": "tombstone"}
            ]
        }));
        let message = &reply.message;
        let thread = message.thread.as_ref().unwrap();
        assert_eq!(thread.root_id.as_deref(), Some("1759120000.000100"));
        assert_eq!(thread.id.as_deref(), Some("1759120000.000100"));
        assert_eq!(thread.in_thread, Some(true));
        assert!(thread.broadcast);
        assert!(reply.extras.broadcast);
        assert_eq!(reply.extras.edited_at_ms, Some(1_759_120_002_000));
        assert_eq!(message.updated_at_ms, Some(1_759_120_002_000));
        assert_eq!(
            reply.extras.reactions,
            vec![SlackReaction {
                name: "+1".into(),
                users: vec!["U02".into()],
                count: 3
            }]
        );
        assert_eq!(message.reactions, None, "列出的人不全，宿主另读");
        assert!(matches!(
            &reply.parts[1],
            Part::Resource { resource, name, .. } if resource.key == "F01" && resource.kind == ResourceKind::Image && name.as_deref() == Some("a.png")
        ));
        assert!(matches!(
            &reply.parts[2],
            Part::Resource { resource, .. } if resource.kind == ResourceKind::File
        ));
        assert!(matches!(&reply.parts[3], Part::Unsupported { kind, .. } if kind == "file"));
        assert!(message.incomplete);
    }

    #[test]
    fn everyone_and_group_mentions_keep_their_words() {
        let msg = translate(json!({
            "type": "message", "user": "U01", "ts": "1759120000.000700",
            "text": "<!here> <!channel> <!subteam^S01|@dev>"
        }));
        let people: Vec<(Option<&str>, bool, bool)> = msg
            .parts
            .iter()
            .filter_map(|part| match part {
                Part::Mention { person, everyone } => Some((
                    person.as_ref().and_then(everyone_word),
                    *everyone,
                    person.as_ref().is_some_and(is_group),
                )),
                _ => None,
            })
            .collect();
        assert_eq!(
            people,
            vec![
                (Some("here"), true, false),
                (Some("channel"), true, false),
                (None, false, true)
            ]
        );
        // @here、用户组的提及编辑后会变样：不能编辑；只有 @channel 的能。
        assert_eq!(msg.wire(Some("U01")).edit_draft, None);
        let channel = translate(json!({
            "type": "message", "user": "U01", "ts": "1759120000.000800", "text": "<!channel> 看"
        }));
        assert_eq!(
            channel.wire(Some("U01")).edit_draft,
            Some(vec![
                Outgoing::MentionAll,
                Outgoing::Text {
                    text: " 看".into()
                }
            ])
        );
    }

    #[test]
    fn bad_ids_and_timestamps_are_rejected() {
        assert!(message("C/../x", &json!({"ts": "1759120000.000100"})).is_err());
        assert!(message("C01", &json!({"ts": "nope"})).is_err());
    }
}
