//! Slack 插件按协议做读取与动作（契约 hhh-im client v11 §3.3–§3.7、§3.10、§11；从 HHH 内置 Slack 的同名测试搬来）：
//! 测试当宿主、经协议调用，用假 Slack（wiremock）核对请求形状与协议的统一格式。响应形状照真实取样（回溯 v11 §4）
//! 与官方文档；没取样过的在注释里标出。

mod common;

use std::collections::HashMap;

use base64::Engine as _;
use common::*;
use hhh_im_protocol::ErrorKind;
use serde_json::{Value, json};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_string_contains, header, method, path},
};

const CHANNEL: &str = "C000000099";

/// 假 Slack 对这个方法一律这样回。
async fn answer(slack: &MockServer, method_name: &str, body: Value) {
    Mock::given(method("POST"))
        .and(path(format!("/api/{method_name}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(slack)
        .await;
}

/// 假 Slack 收到的这个方法的表单（按收到的先后）。
async fn sent(slack: &MockServer, method_name: &str) -> Vec<HashMap<String, String>> {
    requests_to(slack, method_name)
        .await
        .iter()
        .map(form)
        .collect()
}

fn raw(ts: &str, user: &str, text: &str) -> Value {
    json!({ "type": "message", "user": user, "ts": ts, "text": text })
}

/// 一页结果里各项的编号。
fn ids(page: &Value) -> Vec<&str> {
    items(page)
        .iter()
        .map(|item| item["id"].as_str().unwrap())
        .collect()
}

fn items(page: &Value) -> &[Value] {
    page["items"].as_array().unwrap()
}

/// 一页里编号是 `id` 的那项。
fn item<'a>(page: &'a Value, id: &str) -> &'a Value {
    items(page).iter().find(|item| item["id"] == id).unwrap()
}

#[tokio::test]
async fn history_is_read_newest_first_by_time_and_given_old_to_new_with_scoped_cursors() {
    let slack = MockServer::start().await;
    let host = online(&slack).await;
    let mut liked = raw("1759000002.000002", "U000000002", "新的");
    liked["reactions"] = json!([{ "name": "+1", "users": ["U000000003"], "count": 1 }]);
    let mut crowd = raw("1759000001.000001", "U000000002", "旧的");
    // 点的人太多、Slack 只列出一部分：给不出全部表情，留给宿主另外读。
    crowd["reactions"] = json!([{ "name": "tada", "users": ["U000000003"], "count": 30 }]);
    answer(
        &slack,
        "conversations.history",
        json!({ "ok": true, "messages": [liked, crowd], "has_more": true,
                "response_metadata": { "next_cursor": "bmV4dA==" } }),
    )
    .await;
    let query = json!({ "conversationId": CHANNEL, "afterMs": 1_759_000_000_000_i64,
                        "beforeMs": 1_759_000_100_000_i64, "newestFirst": true, "limit": 50 });
    let page = host.ok("messages/list", query.clone()).await;
    assert_eq!(
        ids(&page),
        ["1759000001.000001", "1759000002.000002"],
        "页内从旧到新"
    );
    assert_eq!(
        page["items"][1]["reactions"][0]["id"], "U000000003:+1",
        "历史里内嵌的表情随消息给出，编号是点的人加表情名"
    );
    assert!(page["items"][0].get("reactions").is_none());
    let fields = &sent(&slack, "conversations.history").await[0];
    assert_eq!(fields["oldest"], "1759000000.000000");
    assert_eq!(fields["latest"], "1759000100.999999");
    assert_eq!(fields["inclusive"], "true");
    assert_eq!(fields["limit"], "50");
    // 游标接着原来的条件用。
    let mut next = query.clone();
    next["cursor"] = page["next"].clone();
    host.ok("messages/list", next).await;
    assert_eq!(
        sent(&slack, "conversations.history").await[1]["cursor"],
        "bmV4dA=="
    );
    // 换了条件的游标：协议里由宿主把游标和这次查询的条件绑在一起、不拿去别的查询里用（protocol-v1 §8），
    // 插件不再自己核对，内置时“换了条件就拒绝”的那一条在这里没有对应。
}

#[tokio::test]
async fn threads_are_read_whole_and_a_message_is_found_in_its_thread_when_not_in_the_stream() {
    let slack = MockServer::start().await;
    let host = online(&slack).await;
    let root = "1759000000.000001";
    let mut parent = raw(root, "U000000002", "根");
    parent["thread_ts"] = json!(root);
    let mut reply = raw("1759000005.000005", "U000000003", "回复");
    reply["thread_ts"] = json!(root);
    // 形状照官方文档：conversations.replies 按时间从旧到新给，根消息在最前（多页时每页都带根）。
    Mock::given(method("POST"))
        .and(path("/api/conversations.replies"))
        .and(body_string_contains("cursor=cGFnZTI"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true, "messages": [parent.clone(), reply.clone()], "has_more": false
        })))
        .with_priority(1)
        .mount(&slack)
        .await;
    answer(
        &slack,
        "conversations.replies",
        json!({ "ok": true, "messages": [parent.clone()], "has_more": true,
                "response_metadata": { "next_cursor": "cGFnZTI=" } }),
    )
    .await;
    let page = host
        .ok(
            "messages/list",
            json!({ "conversationId": CHANNEL, "threadId": root, "newestFirst": true, "limit": 20 }),
        )
        .await;
    assert!(page.get("next").is_none(), "整段读完一次交出");
    assert!(ids(&page).contains(&"1759000005.000005"));
    assert_eq!(item(&page, "1759000005.000005")["thread"]["inThread"], true);
    // 按编号读：会话主消息流里没有，就当话题回复找。
    answer(
        &slack,
        "conversations.history",
        json!({ "ok": true, "messages": [] }),
    )
    .await;
    let found = host
        .ok(
            "messages/get",
            json!({ "conversationId": CHANNEL, "messageId": root }),
        )
        .await;
    assert_eq!(found["id"], root);
}

// 按话题只取某条之后的回复（契约 hhh-im client v27 §8）：带起始时间时 `conversations.replies` 带上 `oldest`；
// 形状照真实取样：只回根消息与之后的回复，根消息上带着话题现状（`reply_count`、`latest_reply`）。
#[tokio::test]
async fn a_thread_read_after_a_point_asks_slack_for_replies_after_it_and_sees_the_root_activity() {
    let slack = MockServer::start().await;
    let host = online(&slack).await;
    let root = "1759000000.000001";
    let mut parent = raw(root, "U000000002", "根");
    parent["thread_ts"] = json!(root);
    parent["reply_count"] = json!(2);
    parent["latest_reply"] = json!("1759000009.000009");
    let mut reply = raw("1759000009.000009", "U000000003", "新回复");
    reply["thread_ts"] = json!(root);
    answer(
        &slack,
        "conversations.replies",
        json!({ "ok": true, "messages": [parent, reply], "has_more": false }),
    )
    .await;
    let page = host
        .ok(
            "messages/list",
            json!({ "conversationId": CHANNEL, "threadId": root, "afterMs": 1_759_000_005_000_i64,
                    "newestFirst": true, "limit": 20 }),
        )
        .await;
    let asked = sent(&slack, "conversations.replies").await;
    assert_eq!(asked.len(), 1);
    assert_eq!(
        asked[0].get("oldest").map(String::as_str),
        Some("1759000005.000000")
    );
    assert_eq!(
        item(&page, root)["thread"]["activity"],
        json!({ "latestReplyId": "1759000009.000009", "latestReplyAtMs": 1_759_000_009_000_i64,
                "replyCount": 2 })
    );
    assert!(ids(&page).contains(&"1759000009.000009"));
}

#[tokio::test]
async fn reactions_list_everyone_and_only_your_own_can_be_removed() {
    let slack = MockServer::start().await;
    let host = online(&slack).await;
    let message = "1759000001.000001";
    answer(
        &slack,
        "reactions.get",
        json!({ "ok": true, "type": "message", "message": { "ts": message, "reactions": [
            { "name": "+1", "users": ["U000000002", "U000000001"], "count": 2 },
            { "name": "wave::skin-tone-3", "users": ["U000000003"], "count": 1 }
        ]}}),
    )
    .await;
    for name in ["reactions.add", "reactions.remove"] {
        answer(&slack, name, json!({ "ok": true })).await;
    }
    let page = host
        .ok(
            "reactions/list",
            json!({ "conversationId": CHANNEL, "messageId": message, "limit": 50 }),
        )
        .await;
    assert_eq!(
        ids(&page),
        [
            "U000000002:+1",
            "U000000001:+1",
            "U000000003:wave::skin-tone-3"
        ]
    );
    assert!(
        items(&page).iter().all(|r| r.get("atMs").is_none()),
        "Slack 不给时间"
    );
    assert_eq!(sent(&slack, "reactions.get").await[0]["full"], "true");
    let mine = host
        .ok(
            "reactions/add",
            json!({ "conversationId": CHANNEL, "messageId": message, "emojiType": "+1" }),
        )
        .await;
    assert_eq!(mine["id"], "U000000001:+1");
    host.ok(
        "reactions/remove",
        json!({ "conversationId": CHANNEL, "messageId": message, "reactionId": mine["id"] }),
    )
    .await;
    assert_eq!(sent(&slack, "reactions.remove").await[0]["name"], "+1");
    let theirs = host
        .err(
            "reactions/remove",
            json!({ "conversationId": CHANNEL, "messageId": message, "reactionId": "U000000002:+1" }),
        )
        .await;
    assert_eq!(kind_of(&theirs), ErrorKind::PermissionDenied);
}

#[tokio::test]
async fn an_attachment_is_shared_then_its_message_in_the_conversation_is_the_receipt() {
    let slack = MockServer::start().await;
    let host = online(&slack).await;
    answer(
        &slack,
        "files.completeUploadExternal",
        json!({ "ok": true, "files": [{ "id": "F000000001" }] }),
    )
    .await;
    let mut shared = raw("1759000200.000200", "U000000001", "");
    shared["files"] =
        json!([{ "id": "F000000001", "name": "周报.pdf", "mimetype": "application/pdf" }]);
    answer(
        &slack,
        "conversations.history",
        json!({ "ok": true, "messages": [raw("1759000199.000199", "U000000002", "别人的"), shared] }),
    )
    .await;
    // 资源键就是 Slack 的文件编号（`uploads/finish` 回的就是它），这里直接拿来发。
    let outcome = host
        .ok(
            "messages/send",
            json!({ "operationId": "op-2",
                    "target": { "type": "conversation", "conversationId": CHANNEL },
                    "content": [{ "type": "file", "key": "F000000001" }] }),
        )
        .await;
    assert_eq!(
        outcome["state"], "accepted",
        "找到了带着这个文件的消息，应算已发出"
    );
    assert_eq!(outcome["message"]["id"], "1759000200.000200");
    let fields = &sent(&slack, "files.completeUploadExternal").await[0];
    assert_eq!(fields["channel_id"], CHANNEL);
    assert!(fields["files"].contains("F000000001"));
    // Slack 没有引用回复；进话题时发到话题的根。
    answer(
        &slack,
        "chat.postMessage",
        json!({ "ok": true, "message": raw("1759000300.000300", "U000000001", "好") }),
    )
    .await;
    let reply = |operation: &str, in_thread: bool, also: bool| {
        let mut target = json!({ "type": "reply", "conversationId": CHANNEL,
                                 "messageId": "1759000005.000005" });
        if in_thread {
            target["inThread"] = json!(true);
            target["threadRoot"] = json!("1759000000.000001");
        }
        if also {
            target["alsoToConversation"] = json!(true);
        }
        json!({ "operationId": operation, "target": target,
                "content": [{ "type": "text", "text": "好" }] })
    };
    let quote = host.err("messages/send", reply("op-3", false, false)).await;
    assert_eq!(kind_of(&quote), ErrorKind::Unsupported);
    host.ok("messages/send", reply("op-3", true, false)).await;
    assert_eq!(
        sent(&slack, "chat.postMessage").await[0]["thread_ts"],
        "1759000000.000001"
    );
    // 同时发送到频道：平台原生，同一次请求带 reply_broadcast（契约 client v18 §2.1）；不带时不发这个参数。
    host.ok("messages/send", reply("op-4", true, true)).await;
    let posts = sent(&slack, "chat.postMessage").await;
    assert_eq!(posts.len(), 2, "只发一次");
    assert_eq!(posts[1]["reply_broadcast"], "true");
    assert!(!posts[0].contains_key("reply_broadcast"));
}

#[tokio::test]
async fn editing_keeps_the_thread_and_marks_the_message_edited() {
    let slack = MockServer::start().await;
    let host = online(&slack).await;
    let root = "1759000000.000001";
    let ts = "1759000005.000005";
    // 形状照官方文档：回执带 channel、ts 与消息（消息可能不带 ts）。未核实：真实回执里消息带不带 edited。
    answer(
        &slack,
        "chat.update",
        json!({ "ok": true, "channel": CHANNEL, "ts": ts, "text": "改好了",
                "message": { "type": "message", "user": "U000000001", "text": "改好了",
                             "edited": { "user": "U000000001", "ts": "1759000060.000000" } } }),
    )
    .await;
    // 内置时拿本地记着的那条补上回执；协议只给编号，插件编辑后照 Slack 重读这条（话题回复：主消息流里没有，按话题找）。
    answer(
        &slack,
        "conversations.history",
        json!({ "ok": true, "messages": [] }),
    )
    .await;
    let mut fresh = raw(ts, "U000000001", "改好了");
    fresh["thread_ts"] = json!(root);
    fresh["edited"] = json!({ "user": "U000000001", "ts": "1759000060.000000" });
    Mock::given(method("POST"))
        .and(path("/api/conversations.replies"))
        .and(body_string_contains(format!("ts={ts}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true, "messages": [fresh], "has_more": false
        })))
        .with_priority(1)
        .mount(&slack)
        .await;
    answer(
        &slack,
        "conversations.replies",
        json!({ "ok": false, "error": "thread_not_found" }),
    )
    .await;
    let edit = |id: &str| {
        json!({ "conversationId": CHANNEL, "messageId": id,
                "content": [{ "type": "text", "text": "改好了" }] })
    };
    let edited = host.ok("messages/edit", edit(ts)).await;
    assert_eq!(
        edited["content"],
        json!([{ "type": "text", "text": "改好了" }])
    );
    assert_eq!(edited["editedAtMs"], 1_759_000_060_000_i64);
    assert_eq!(
        edited["thread"]["id"], root,
        "话题归属照平台上的（插件重读到的）"
    );
    assert_eq!(sent(&slack, "chat.update").await[0]["ts"], ts);
    // 重读不到时用编辑回执里的内容：回执不带表情，不写表情，宿主本地的不动。
    let other = "1759000007.000007";
    let fallback = host.ok("messages/edit", edit(other)).await;
    assert_eq!(fallback["id"], other);
    assert_eq!(
        fallback["content"],
        json!([{ "type": "text", "text": "改好了" }])
    );
    assert_eq!(fallback["editedAtMs"], 1_759_000_060_000_i64);
    assert!(
        fallback.get("reactions").is_none(),
        "编辑回执不带表情：本地的不动"
    );
}

#[tokio::test]
async fn the_emoji_catalog_has_standard_characters_and_workspace_images_with_aliases() {
    let slack = MockServer::start().await;
    mount_auth(&slack).await;
    let host = Host::spawn(options(&slack));
    let declared = host.initialize().await;
    host.open(&credential(TOKEN, None, None)).await;
    // 未核实：真实工作区的 emoji.list 形状照官方文档（名字 → 图地址或 alias:原名）。
    answer(
        &slack,
        "emoji.list",
        json!({ "ok": true, "emoji": {
            "our-logo": "https://emoji.slack-edge.com/T000000001/our-logo/abc.png",
            "logo-too": "alias:our-logo",
            "yes": "alias:white_check_mark",
            "bad name": "https://emoji.slack-edge.com/x.png"
        }}),
    )
    .await;
    let catalog = host.ok("emoji/catalog", json!({})).await;
    let find = |name: &str| {
        catalog["emojis"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["emojiType"] == name)
    };
    assert_eq!(find("+1").unwrap()["unicode"], "\u{1F44D}");
    // 协议的目录里没有图的地址（宿主按 `emoji/image` 要图），而图在 Slack 的 CDN 上、假 Slack 替不了：这里只核对
    // 自定义表情与指向它的别名都进了目录、用图（不带字符）。
    for name in ["our-logo", "logo-too"] {
        let custom = find(name).unwrap();
        assert_eq!(custom["category"], "custom", "{name}");
        assert!(custom.get("unicode").is_none(), "{name}");
    }
    assert_eq!(find("yes").unwrap()["unicode"], "\u{2705}");
    assert!(find("bad name").is_none(), "名字不合 Slack 规则的不收");
    assert_eq!(
        declared["platform"]["emojiPerAccount"], true,
        "自定义表情按工作区分"
    );
}

#[tokio::test]
async fn message_files_are_opened_with_the_users_token_from_the_file_info() {
    let slack = MockServer::start().await;
    let host = online(&slack).await;
    answer(
        &slack,
        "files.info",
        json!({ "ok": true, "file": { "id": "F000000001", "size": 5,
                "url_private_download": format!("{}/files-pri/T1-F000000001/download/a.txt", slack.uri()) } }),
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/files-pri/T1-F000000001/download/a.txt"))
        .and(header("authorization", "Bearer xoxp-test"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"hello".to_vec()))
        .mount(&slack)
        .await;
    let mut bytes = Vec::new();
    let mut total = None;
    for _ in 0..10 {
        let chunk = host
            .ok(
                "resources/read",
                json!({ "conversationId": CHANNEL, "messageId": "1759000001.000001", "key": "F000000001",
                        "kind": "file", "offset": bytes.len(), "length": 1 << 20 }),
            )
            .await;
        total = chunk["total"].as_u64();
        bytes.extend(
            base64::engine::general_purpose::STANDARD
                .decode(chunk["data"].as_str().unwrap())
                .unwrap(),
        );
        if chunk["eof"] == true {
            break;
        }
    }
    assert_eq!(total, Some(5));
    assert_eq!(bytes, b"hello");
    // 头像只收 Slack 头像 CDN 与 Gravatar。
    let refused = host
        .err(
            "avatars/read",
            json!({ "key": "https://evil.example.com/a_72.png", "offset": 0, "length": 1024 }),
        )
        .await;
    assert_eq!(kind_of(&refused), ErrorKind::InvalidRequest);
}

#[tokio::test]
async fn search_finds_groups_and_people_locally_and_direct_chats_are_only_looked_up() {
    let slack = MockServer::start().await;
    let host = online(&slack).await;
    answer(
        &slack,
        "users.conversations",
        json!({ "ok": true, "channels": [
            { "id": "C000000001", "name": "dev-backend", "is_channel": true },
            { "id": "C000000002", "name": "random", "is_channel": true },
            { "id": "DQ00000001", "is_im": true, "user": "U000000002" }
        ]}),
    )
    .await;
    answer(
        &slack,
        "users.list",
        json!({ "ok": true, "members": [
            { "id": "U000000002", "name": "chen", "profile": { "display_name": "陈言", "image_72": "https://avatars.slack-edge.com/a_72.png" } },
            { "id": "U000000009", "name": "bot", "is_bot": true, "profile": { "display_name": "陈言机器人" } },
            { "id": "U000000001", "name": "me", "profile": { "display_name": "我陈" } }
        ]}),
    )
    .await;
    answer(
        &slack,
        "conversations.list",
        json!({ "ok": true, "channels": [{ "id": "DQ00000001", "user": "U000000002", "is_im": true }] }),
    )
    .await;
    let groups = host
        .ok(
            "conversations/search",
            json!({ "text": "DEV", "limit": 20 }),
        )
        .await;
    let names: Vec<&str> = items(&groups)
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["dev-backend"]);
    let people = host
        .ok("people/search", json!({ "text": "陈", "limit": 20 }))
        .await;
    assert_eq!(ids(&people), ["U000000002"], "不含本人与机器人");
    // 同一个关键词再搜：用本机记着的名单，不再读。
    host.ok("people/search", json!({ "text": "陈", "limit": 20 }))
        .await;
    assert_eq!(sent(&slack, "users.list").await.len(), 1);
    let direct = host
        .ok(
            "conversations/findDirect",
            json!({ "userId": people["items"][0]["id"] }),
        )
        .await;
    assert_eq!(direct["conversationId"], "DQ00000001");
    assert!(
        sent(&slack, "conversations.open").await.is_empty(),
        "不新建私聊"
    );
}

// 私有频道（契约 client v22 §2）：频道且 `is_private` 才算私有；群私聊的 `is_private` 也是 true，但不是频道。
// 搜索（users.conversations）与单个会话资料（conversations.info）都经同一处翻译。
#[tokio::test]
async fn only_private_channels_are_marked_private() {
    let slack = MockServer::start().await;
    let host = online(&slack).await;
    answer(
        &slack,
        "users.conversations",
        json!({ "ok": true, "channels": [
            { "id": "C000000001", "name": "team-dev", "is_channel": true, "is_private": false },
            { "id": "C000000002", "name": "team-hiring", "is_channel": false, "is_group": true, "is_private": true },
            { "id": "G000000003", "name": "mpdm-team-a--team-b-1", "is_mpim": true, "is_private": true }
        ]}),
    )
    .await;
    let groups = host
        .ok(
            "conversations/search",
            json!({ "text": "team", "limit": 20 }),
        )
        .await;
    // 不是私有的不写 `private`。
    let shapes: Vec<(&str, &str, bool)> = items(&groups)
        .iter()
        .map(|c| {
            (
                c["name"].as_str().unwrap(),
                c["kind"].as_str().unwrap(),
                c["private"].as_bool().unwrap_or(false),
            )
        })
        .collect();
    assert_eq!(
        shapes,
        [
            ("team-dev", "channel", false),
            ("team-hiring", "channel", true),
            ("mpdm-team-a--team-b-1", "group", false),
        ]
    );
    answer(
        &slack,
        "conversations.info",
        json!({ "ok": true, "channel": { "id": "C000000002", "name": "team-hiring", "is_group": true, "is_private": true } }),
    )
    .await;
    let profile = host
        .ok(
            "conversations/get",
            json!({ "conversationId": "C000000002" }),
        )
        .await;
    assert_eq!(profile["private"], true, "单个会话资料也读出私有");
}

// 大工作区（im-watch v65 真实环境：7147 个账号、只有 518 个在用的人）：成员名单 1000 一页读（Slack 一页最多给 1000，
// 要 2000 也只给 1000；200 一页要 36 页，超过 users.list 每分钟约 20 次的限额，每次搜都读到一半被限流、从头再来）。
// 读到一半被限流时留下已读的、这次就按它搜，下次从断点接着读；读全了一小时内不再读。
#[tokio::test]
async fn a_large_directory_is_read_a_thousand_at_a_time_and_resumes_after_a_rate_limit() {
    let slack = MockServer::start().await;
    let host = online(&slack).await;
    Mock::given(method("POST"))
        .and(path("/api/users.list"))
        .and(body_string_contains("cursor=page2"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "1"))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&slack)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/users.list"))
        .and(body_string_contains("cursor=page2"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "ok": true, "members": [
                { "id": "U000000003", "name": "chen2", "profile": { "display_name": "陈二" } }
            ]})),
        )
        .with_priority(2)
        .mount(&slack)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/users.list"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "ok": true,
            "members": [{ "id": "U000000002", "name": "chen", "profile": { "display_name": "陈一" } }],
            "response_metadata": { "next_cursor": "page2" } })))
        .with_priority(3)
        .mount(&slack)
        .await;
    let search = json!({ "text": "陈", "limit": 20 });
    let first = host.ok("people/search", search.clone()).await;
    assert_eq!(ids(&first), ["U000000002"], "被限流时按已读的搜，不报错");
    let second = host.ok("people/search", search.clone()).await;
    assert_eq!(
        ids(&second),
        ["U000000002", "U000000003"],
        "下次从断点接着读完"
    );
    host.ok("people/search", search).await;
    let pages = sent(&slack, "users.list").await;
    assert_eq!(
        pages.len(),
        3,
        "第一页一次、第二页限流一次再读一次；读全后不再读"
    );
    assert!(
        pages
            .iter()
            .all(|f| f.get("limit").map(String::as_str) == Some("1000"))
    );
    assert_eq!(
        pages.iter().filter(|f| !f.contains_key("cursor")).count(),
        1,
        "不从头再来"
    );
}

/// 新版搜索的一条结果（形状照 2026-10-04 真实取样的字段，契约 client v29 §2；身份、正文与链接已换掉）。
fn hit(channel: &str, ts: &str, user: &str, bot: bool) -> Value {
    json!({
        "author_name": "某人", "author_email": "someone@example.com", "author_user_id": user,
        "team_id": "T000000001", "channel_id": channel, "channel_name": "",
        "message_ts": ts, "content": "（正文已换掉）", "is_author_bot": bot,
        "permalink": "https://example.slack.com/archives/C0/p1"
    })
}

// 会话发现（契约 client v29 §2）：新版搜索用 `*` 按时间从新到旧列出、四种会话、不含机器人并按 `is_author_bot` 再筛，
// 翻到没有下一页；按会话归并出最早、最新一条；类别与私聊对方取本机会话列表，列表里没有的按会话读一次资料；
// 私聊不带资料（名字是对方编号），由宿主读对方本人。
#[tokio::test]
async fn recent_activity_groups_search_hits_by_conversation_and_skips_bots() {
    let slack = MockServer::start().await;
    let host = online(&slack).await;
    answer(
        &slack,
        "users.conversations",
        json!({ "ok": true, "channels": [
            { "id": "C000000001", "name": "dev-backend", "is_channel": true },
            { "id": "DQ00000001", "is_im": true, "user": "U000000002" }
        ]}),
    )
    .await;
    answer(
        &slack,
        "conversations.info",
        json!({ "ok": true, "channel": { "id": "C000000009", "name": "new-channel", "is_channel": true } }),
    )
    .await;
    Mock::given(method("POST"))
        .and(path("/api/assistant.search.context"))
        .and(body_string_contains("cursor=c2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true,
            "results": { "messages": [hit("C000000001", "1790000100.000100", "U000000003", false)] },
            "response_metadata": { "next_cursor": "" }
        })))
        .mount(&slack)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/assistant.search.context"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true,
            "results": { "messages": [
                hit("C000000001", "1790000300.000100", "U000000002", false),
                hit("C000000005", "1790000260.000100", "B000000001", true),
                hit("DQ00000001", "1790000200.000100", "U000000002", false),
                hit("C000000009", "1790000150.000100", "U000000004", false)
            ] },
            "response_metadata": { "next_cursor": "c2" }
        })))
        .mount(&slack)
        .await;
    let found = host
        .ok(
            "conversations/recentActivity",
            json!({ "sinceMs": 1_790_000_000_000_i64, "pagesMax": 5 }),
        )
        .await;
    assert_eq!(found["complete"], true);
    // 会话、类别、最早、最新、私聊对方、带来的资料名字。
    type Row<'a> = (&'a str, &'a str, i64, i64, Option<&'a str>, Option<&'a str>);
    let summary: Vec<Row> = items(&found)
        .iter()
        .map(|item| {
            (
                item["conversationId"].as_str().unwrap(),
                item["kind"].as_str().unwrap(),
                item["firstMs"].as_i64().unwrap(),
                item["latestMs"].as_i64().unwrap(),
                item["peerId"].as_str(),
                item["conversation"]["name"].as_str(),
            )
        })
        .collect();
    assert_eq!(
        summary,
        vec![
            (
                "C000000001",
                "channel",
                1_790_000_100_000,
                1_790_000_300_000,
                None,
                Some("dev-backend")
            ),
            (
                "DQ00000001",
                "direct",
                1_790_000_200_000,
                1_790_000_200_000,
                Some("U000000002"),
                None
            ),
            (
                "C000000009",
                "channel",
                1_790_000_150_000,
                1_790_000_150_000,
                None,
                Some("new-channel")
            ),
        ],
        "机器人那条（C000000005）不算"
    );
    let pages = sent(&slack, "assistant.search.context").await;
    assert_eq!(pages.len(), 2);
    let first = &pages[0];
    assert_eq!(first["query"], "*");
    assert_eq!(first["sort"], "timestamp");
    assert_eq!(first["sort_dir"], "desc");
    assert_eq!(first["after"], "1790000000");
    assert_eq!(
        first["channel_types"],
        "public_channel,private_channel,mpim,im"
    );
    assert_eq!(first["include_bots"], "false");
    assert_eq!(first["limit"], "20");
    assert!(!first.contains_key("cursor"));
    assert_eq!(pages[1]["cursor"], "c2");
    assert_eq!(
        sent(&slack, "conversations.info").await.len(),
        1,
        "只有会话列表里没有的才按会话读"
    );
}

// 授权里缺新版搜索的权限：在本地拒绝，不发搜索请求（宿主据此说明为什么没在发现）。
#[tokio::test]
async fn recent_activity_without_the_search_scopes_is_refused_before_network() {
    let slack = MockServer::start().await;
    let (search, granted): (Vec<&str>, Vec<&str>) = hhh_im_slack::USER_SCOPES
        .iter()
        .copied()
        .partition(|s| s.starts_with("search:"));
    mount_auth_for(&slack, TOKEN, &granted.join(",")).await;
    let host = Host::start(options(&slack)).await;
    let open = host
        .ok(
            "session/open",
            json!({ "credential": credential(TOKEN, None, None), "settings": {} }),
        )
        .await;
    assert_eq!(open["state"], "online");
    // 缺的正好是新版搜索那几项：开会话后随问题行报给宿主（内置时由 `missing_scopes()` 给出）。
    assert_eq!(
        host.expect_notification("session/notice").await,
        json!({ "notices": [{ "id": "missing-scopes", "relogin": true,
                "text": format!("缺少权限 {}：点重新登录补上授权", search.join("、")) }] })
    );
    let refused = host
        .err(
            "conversations/recentActivity",
            json!({ "sinceMs": 1_790_000_000_000_i64, "pagesMax": 1 }),
        )
        .await;
    assert_eq!(kind_of(&refused), ErrorKind::PermissionDenied);
    assert!(
        user_message(&refused).contains("search:read.public"),
        "{}",
        user_message(&refused)
    );
    assert!(sent(&slack, "assistant.search.context").await.is_empty());
}
