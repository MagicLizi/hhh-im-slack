//! `initialize`（protocol-v1 §4.1、§5）：插件信息、平台说明、能力与登录方式，照 HHH 内置 Slack 的平台表（`im_client.rs`
//! 的 `PLATFORM`）一项一项写出来。`tests/fixtures/initialize.json` 锁住结果，HHH 那边拿它与内置的平台表逐项对照。
//!
//! 登录方式只有「通过中继登录」一种（契约 provider-protocol v12 缺口 6 的 `relay`）：中继地址框预填 HHH 的中继
//! （v14 的 `defaultValue`），直接点登录和原来一样；用户可以改成自己部署的中继。Slack 的 OAuth 回调只收 https，所以
//! 没有飞书那种“自己的应用 + 本机回调”。

use hhh_im_protocol::method::{self as pm, LoginKind, LoginMethod};
use hhh_im_protocol::platform::{
    ActionRules, Adjustable, AvatarIdentity, AvatarShape, Broadcast, Capabilities,
    ConversationCaps, ConversationNouns, DiscoveryDesc, EditRule, EmojiRules, HistoryMode, Limits,
    MessageCaps, OwnWithin, PeopleCaps, PlatformDesc, RateBudget, ReactionCaps, ReadIntervalsSecs,
    ResourceCaps, RichFormat, RichSendCaps, SendCaps, SessionCaps, Threads, UnlistedEmoji,
};
use hhh_im_protocol::rpc::RpcError;
use serde_json::Value;

use super::login;
use super::relay::DEFAULT_RELAY;
use crate::slack::{HISTORY_PAGE_MAX, UPLOAD_MAX_BYTES, methods::PEOPLE_BATCH_MAX};
use crate::{
    AVATAR_KEY_PATTERN, AVATAR_VARIANT_PATTERN, EMOJI_NAME_PATTERN, EMOJI_TEXT_PATTERN,
    EMOJI_TEXT_TEMPLATE, ErrorKind,
};

/// 插件编号（同清单 `id`）。
pub(crate) const PROVIDER_ID: &str = "slack";
/// 平台名字。
pub(crate) const NAME: &str = "Slack";
/// 附件文件名上限（Slack 按 255 字节截断）。
const ATTACHMENT_NAME_MAX_BYTES: u32 = 255;

/// 平台说明。
pub(crate) fn platform() -> PlatformDesc {
    PlatformDesc {
        // Slack 客户端里的头像是圆角方形（契约 client v24）。
        avatar_shape: AvatarShape::Rounded,
        avatar_identity: Some(AvatarIdentity {
            key_pattern: AVATAR_KEY_PATTERN.into(),
            variant_pattern: Some(AVATAR_VARIANT_PATTERN.into()),
        }),
        history: HistoryMode::Server,
        // 同内置的 `actions`：没有引用回复；撤回的、系统消息什么都不能做（宿主统一规则）；其余能回话题、点表情，本人发的
        // 还能撤回；只有带编辑原文的（文字与提及）本人消息能编辑，编辑框用这份原文（契约 client v18 §2.4）。
        actions: ActionRules {
            reply: false,
            thread: true,
            react: true,
            recall: Some(OwnWithin {
                own: true,
                within_secs: None,
            }),
            edit: Some(EditRule {
                own: true,
                within_secs: None,
                per_message: true,
            }),
        },
        // 不带工作区也能打开：Slack 用当前登录的工作区找这个会话。
        chat_link: Some("https://slack.com/app_redirect?channel={conversation}".into()),
        image_name_prefix: Some("Slack 图片".into()),
        // 频道叫“频道”，私聊与群私聊叫“会话”（契约 client v18 §2.1）。
        conversation_nouns: Some(ConversationNouns {
            group: "会话".into(),
            direct: "会话".into(),
            channel: "频道".into(),
            unknown: "会话".into(),
        }),
        // 历史与话题一页最多 200 条（契约 client v11 §3.3）。
        page_max: Some(u32::from(HISTORY_PAGE_MAX)),
        // 每分钟留出余量；逐会话发现会连续读，按 Slack 建议每秒至多一次，避免依赖突发额度。
        reads: Some(RateBudget {
            per_second: 1,
            per_minute: 40,
        }),
        // 每个文件先查一次文件资料（第 4 档，每分钟 100 次以上），再下载。
        downloads: Some(RateBudget {
            per_second: 5,
            per_minute: 80,
        }),
        file_segment_bytes: None,
        limits: Some(Limits {
            image_upload_max_bytes: UPLOAD_MAX_BYTES as u64,
            file_upload_max_bytes: UPLOAD_MAX_BYTES as u64,
            attachment_name_max_bytes: ATTACHMENT_NAME_MAX_BYTES,
        }),
        // 同内置：通用的读取频率，会话发现默认 1 分钟（契约 client v29：新版搜索按用户每分钟约 10 次）。
        read_defaults: Some(ReadIntervalsSecs {
            watching: 2,
            active: 5,
            quiet: 60,
            dormant: 5 * 60,
            stale: 10 * 60,
            profile: 10 * 60,
            people: 10 * 60,
            thread_days: 7,
            discover: Some(60),
        }),
        threads: Some(Threads {
            // 读会话时根消息带 `latest_reply`、`reply_count`，删回复会往回退（契约 client v27 取样）：话题靠线索读。
            activity_on_root: true,
            // `conversations.replies` 的 `oldest` 只回根消息与之后的回复（取样）。
            reads_after: true,
            // 平台原生：话题回复带 `reply_broadcast=true` 就同时出现在频道里，是同一条消息；文件上传没有这个参数。
            broadcast: Some(Broadcast {
                native: true,
                attachments: false,
            }),
        }),
        deleted_messages_vanish: true,
        // 历史与话题里的消息都带着 `reactions`（没人点过就没有这个字段），列出的人不全时那条另读（契约 client v27 §6.3）。
        reactions_in_reads: true,
        emoji_per_account: true,
        emoji: Some(EmojiRules {
            name_pattern: Some(EMOJI_NAME_PATTERN.into()),
            // 目录里没有的表情（自定义表情被删了）不猜图、不画占位图标，写名字。
            unlisted: UnlistedEmoji {
                placeholder_pattern: None,
                placeholder_label: String::new(),
                guess: false,
                guess_pattern: None,
            },
            // 正文里的 `:name:`：读回来的在翻译时已换成表情节点；还没读回来的文字（刚发出、本机先显示的）宿主按它认。
            // Slack 的表情图不是白底。
            text_pattern: Some(EMOJI_TEXT_PATTERN.into()),
            white_background: false,
            // 表情写进文字的写法（协议 v31）：宿主据此给输入框的表情按钮。
            text_template: Some(EMOJI_TEXT_TEMPLATE.into()),
        }),
        // 不声明账号级 `recentActivity`：新版搜索官方只许响应用户操作，不许后台持续查。宿主据此改做逐会话限频发现：
        // `users.conversations` 一页列一批会话，每个同步范围以外的再用一次 `conversations.history` 探测；
        // `pages` / `cold_pages` 是一次读取最多翻几页会话列表，请求走上面的读取额度、按后台一级排队。
        discovery: Some(DiscoveryDesc {
            fastest_secs: 30,
            pages: 5,
            cold_pages: 15,
        }),
        // 参数配置里只开放读取频率，与内置一样（契约 provider-protocol v12 缺口 2）。
        user_adjustable: vec![Adjustable::ReadIntervals],
        // 编辑过的消息 Slack 每次都带着 `edited`（契约 client v11 §3.6），不用按内容比较。
        edits_by_content: Vec::new(),
        retranslate_rules: Vec::new(),
        content_repairs: Vec::new(),
        content_rereads: Vec::new(),
    }
}

/// 能力：内置 Slack 做得到的都声明；不推送（今天的 Slack 靠定时读）。
pub(crate) fn capabilities() -> Capabilities {
    Capabilities {
        receipts: None,
        mark_read: None,
        stickers: Default::default(),
        read_sync: Some(hhh_im_protocol::reading::ReadSyncCaps {
            scopes: hhh_im_protocol::reading::ReadScopes {
                conversations: true,
                threads: false,
            },
            watermark: true,
            individual: false,
        }),
        conversations: ConversationCaps {
            list: true,
            search: true,
            members: true,
            find_direct: true,
            recent_activity: false,
        },
        messages: MessageCaps {
            list: true,
            get: true,
            forwarded: true,
        },
        // 机器人按编号读（`bots.info`）；一批取到人名册一次查询的上限，整份成员名单读一遍就认出一大批人。
        people: PeopleCaps {
            contacts: false,
            get: true,
            search: true,
            bots: true,
            batch_max: Some(PEOPLE_BATCH_MAX),
        },
        send: Some(SendCaps {
            text: true,
            mentions: true,
            mention_all: true,
            image: true,
            file: true,
            lookup: false,
        }),
        recall: true,
        edit: true,
        rich_send: RichSendCaps {
            formats: vec![
                RichFormat::Strong,
                RichFormat::Emphasis,
                RichFormat::Delete,
                RichFormat::InlineCode,
                RichFormat::Link,
                RichFormat::Blockquote,
                RichFormat::List,
                RichFormat::Code,
            ],
            edit: true,
        },
        reactions: ReactionCaps {
            list: true,
            add: true,
            remove: true,
            catalog: true,
        },
        resources: ResourceCaps {
            variants: true,
            read: true,
            avatars: true,
        },
        events: Default::default(),
        retranslate: false,
        message_maintenance: false,
        session: SessionCaps { renew: true },
        // 不跨账号发收伙伴署名、不收宿主头像；性能自报由 SDK 运行时替插件声明（协议 v32）。
        agent_attribution: Default::default(),
        account: Default::default(),
        perf: false,
    }
}

/// 登录方式：只有「通过中继登录」，中继框预填 HHH 的中继、用户可改；叫法与飞书插件的同一种方式一样。
pub(crate) fn login_methods() -> Vec<LoginMethod> {
    vec![LoginMethod {
        id: login::RELAY_METHOD.into(),
        kind: LoginKind::Relay,
        label: login::RELAY_LABEL.into(),
        note: login::RELAY_NOTE.into(),
        placeholder: Some(login::RELAY_PLACEHOLDER.into()),
        default_value: Some(DEFAULT_RELAY.into()),
        id_label: None,
        id_placeholder: None,
        secret_label: None,
        secret_placeholder: None,
        secret_help: None,
        callback_path: None,
        callback_help: None,
        create_link: None,
    }]
}

/// `initialize` 的结果。宿主只说得出别的协议版本时拒绝。
pub(crate) fn initialize(params: pm::InitializeParams) -> Result<Value, RpcError> {
    if !params
        .protocol_versions
        .contains(&hhh_im_protocol::PROTOCOL_VERSION)
    {
        return Err(RpcError::from_kind(
            ErrorKind::Unsupported,
            "Slack 插件只支持 HHH-IM-Protocol v1，请更新 HHH。",
        ));
    }
    Ok(result())
}

/// `initialize` 的结果（不看宿主参数的那部分）。
pub fn result() -> Value {
    serde_json::to_value(pm::InitializeResult {
        protocol_version: hhh_im_protocol::PROTOCOL_VERSION,
        provider: pm::ProviderInfo {
            id: PROVIDER_ID.into(),
            name: NAME.into(),
            version: env!("CARGO_PKG_VERSION").into(),
        },
        platform: platform(),
        capabilities: capabilities(),
        read_request_gating: true,
        settings: Vec::new(),
        login_methods: login_methods(),
        actions: Vec::new(),
    })
    .expect("握手结果总能写成 JSON")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// HHH 契约 provider-protocol v31：写进文字的写法与正文写法对得上——标准目录里每个表情（含肤色版本）按写法写出来，
    /// 正文写法整段认出、认出的就是它的编号（HHH 的自检工具 hhh-im-check 也这样查）；紧贴中文时也一个个认得出。
    #[test]
    fn the_text_template_round_trips_through_the_text_pattern() {
        let declared = result();
        let emoji = &declared["platform"]["emoji"];
        assert_eq!(emoji["textTemplate"], ":{type}:");
        let pattern = regex_lite::Regex::new(emoji["textPattern"].as_str().unwrap()).unwrap();
        let mut checked = 0;
        for item in crate::slack::standard_catalog_for_tests() {
            let text = EMOJI_TEXT_TEMPLATE.replace("{type}", &item);
            let found = pattern.captures(&text).expect("认得出");
            assert_eq!((&found[0], &found[1]), (text.as_str(), item.as_str()));
            checked += 1;
        }
        assert!(checked > 3000, "标准目录与肤色版本都查了：{checked}");
        let found: Vec<&str> = pattern
            .captures_iter("好的:+1:收到:tada:ok:smile: :wave::skin-tone-3::++:")
            .map(|c| c.get(1).unwrap().as_str())
            .collect();
        assert_eq!(found, ["+1", "tada", "smile", "wave::skin-tone-3", "++"]);
    }

    #[test]
    fn the_declaration_is_consistent() {
        capabilities()
            .check_consistent(HistoryMode::Server)
            .expect("能力自洽");
        let value = result();
        assert_eq!(value["provider"]["id"], "slack");
        assert_eq!(value["capabilities"]["people"]["bots"], true);
        assert_eq!(value["capabilities"]["people"]["batchMax"], 2000);
        assert_eq!(value["platform"]["reads"]["perSecond"], 1);
        assert_eq!(value["platform"]["reads"]["perMinute"], 40);
        // 只有「通过中继登录」一种，预填 HHH 的中继（https）；文字在宿主的预算内。
        let methods = value["loginMethods"].as_array().unwrap();
        assert_eq!(methods.len(), 1);
        assert_eq!(methods[0]["kind"], "relay");
        assert_eq!(methods[0]["id"], "relay");
        assert_eq!(methods[0]["defaultValue"], DEFAULT_RELAY);
        assert!(DEFAULT_RELAY.starts_with("https://"));
        for text in [
            login::RELAY_LABEL,
            login::RELAY_NOTE,
            login::RELAY_PLACEHOLDER,
        ] {
            let count = text.chars().count();
            assert!(
                (1..=hhh_im_protocol::budget::LOGIN_TEXT_MAX_CHARS).contains(&count),
                "{text}"
            );
        }
        assert!(methods[0].get("callbackPath").is_none());
    }
}
