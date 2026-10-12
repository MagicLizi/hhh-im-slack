//! Slack 的人名册读法（同内置 Slack，契约 hhh-im client v23 §4）：测试当宿主，经 `people/get` 与
//! `conversations/members` 用假 Slack（wiremock）核对按编号读人与机器人。
//! 响应形状照 2026-10-02 真实取样（Friend 回溯 v76 §3）：人的 `users.info` 带 `profile.display_name/real_name/
//! image_72`、`deleted`、`is_bot`；停用的人照样有名字与头像；机器人的 `bots.info` 带 `name` 与 `icons.image_72`；
//! 工作区名单一页 1000 人、带停用的人与机器人用户。编号与名字是假的。

mod common;

use common::*;
use serde_json::{Value, json};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_string_contains, method, path},
};

/// 某个方法、请求里带着某段参数时的回答。
async fn answer(slack: &MockServer, method_name: &str, contains: &str, body: Value) {
    Mock::given(method("POST"))
        .and(path(format!("/api/{method_name}")))
        .and(body_string_contains(contains))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(slack)
        .await;
}

/// 取样形状的人：`profile` 里的名字与 72px 头像，`deleted` / `is_bot` 照平台给。
fn user(id: &str, name: &str, deleted: bool, bot: bool) -> Value {
    json!({
        "id": id, "name": id.to_lowercase(), "deleted": deleted, "is_bot": bot, "is_app_user": false,
        "updated": 1789527480,
        "profile": {
            "display_name": if bot { "" } else { name }, "real_name": name,
            "image_72": format!("https://avatars.slack-edge.com/{id}_72.png"),
        }
    })
}

/// 读到的人：编号、名字、头像，按编号排好。
fn found(result: &Value) -> Vec<(String, Option<String>, Option<String>)> {
    let text = |p: &Value, key: &str| p.get(key).and_then(Value::as_str).map(str::to_owned);
    let mut out: Vec<_> = result["found"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| (text(p, "id").unwrap(), text(p, "name"), text(p, "avatar")))
        .collect();
    out.sort();
    out
}

// 人少逐个 users.info；停用的人照样有名字与头像；机器人 bots.info；读不到的记“没有”（契约 client v23 §4）。
#[tokio::test]
async fn a_few_people_and_bots_are_read_one_by_one_and_not_found_means_missing() {
    let slack = MockServer::start().await;
    let host = online(&slack).await;
    answer(
        &slack,
        "users.info",
        "user=U00000000A",
        json!({ "ok": true, "user": user("U00000000A", "陈言", false, false) }),
    )
    .await;
    answer(
        &slack,
        "users.info",
        "user=U00000DEAD",
        json!({ "ok": true, "user": user("U00000DEAD", "前同事", true, false) }),
    )
    .await;
    answer(
        &slack,
        "users.info",
        "user=U00000GONE",
        json!({ "ok": false, "error": "user_not_found" }),
    )
    .await;
    answer(
        &slack,
        "bots.info",
        "bot=B00000000B",
        json!({ "ok": true, "bot": {
            "id": "B00000000B", "deleted": false, "name": "匿名消息", "updated": 1734954844,
            "app_id": "A000000001", "user_id": "U00000BOTU", "is_workflow_bot": true,
            "icons": {
                "image_36": "https://avatars.slack-edge.com/bot_36.png",
                "image_48": "https://avatars.slack-edge.com/bot_48.png",
                "image_72": "https://avatars.slack-edge.com/bot_72.png"
            }
        }}),
    )
    .await;
    answer(
        &slack,
        "bots.info",
        "bot=B00000NONE",
        json!({ "ok": false, "error": "bot_not_found" }),
    )
    .await;
    let result = host
        .ok(
            "people/get",
            json!({
                "ids": ["U00000000A", "U00000DEAD", "U00000GONE"],
                "bots": ["B00000000B", "B00000NONE"]
            }),
        )
        .await;
    assert_eq!(
        found(&result),
        vec![
            (
                "B00000000B".into(),
                Some("匿名消息".into()),
                Some("https://avatars.slack-edge.com/bot_72.png".into())
            ),
            (
                "U00000000A".into(),
                Some("陈言".into()),
                Some("https://avatars.slack-edge.com/U00000000A_72.png".into())
            ),
            (
                "U00000DEAD".into(),
                Some("前同事".into()),
                Some("https://avatars.slack-edge.com/U00000DEAD_72.png".into())
            ),
        ],
        "停用的人以前发的消息照样显示名字与头像"
    );
    let mut missing: Vec<&str> = result["missing"]
        .as_array()
        .unwrap()
        .iter()
        .map(|id| id.as_str().unwrap())
        .collect();
    missing.sort_unstable();
    assert_eq!(missing, ["B00000NONE", "U00000GONE"]);
    assert_eq!(result["platformRequests"], 5, "每人每个机器人一次");
    assert_eq!(requests_to(&slack, "users.info").await.len(), 3);
    assert_eq!(requests_to(&slack, "bots.info").await.len(), 2);
    assert_eq!(
        requests_to(&slack, "users.list").await.len(),
        0,
        "人少不读整份名单"
    );
}

// 人多先读工作区名单（含停用的人与机器人用户），一页里全齐了就不往下读；名单里没有的（外部工作区的人）
// 再逐个读（契约 client v23 §4）。
#[tokio::test]
async fn many_people_are_read_from_the_directory_and_stragglers_one_by_one() {
    let slack = MockServer::start().await;
    let host = online(&slack).await;
    let wanted: Vec<String> = (1..=12).map(|i| format!("U0000001{i:02}")).collect();
    let mut first: Vec<Value> = wanted[..6]
        .iter()
        .map(|id| user(id, &format!("同事{id}"), false, false))
        .collect();
    first.push(user("U00000DEAD", "前同事", true, false));
    first.push(user("U0000BOTUS", "构建机器人", false, true));
    Mock::given(method("POST"))
        .and(path("/api/users.list"))
        .and(body_string_contains("cursor=page2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "ok": true,
            "members": wanted[6..11].iter().map(|id| user(id, &format!("同事{id}"), false, false)).collect::<Vec<_>>(),
            "response_metadata": { "next_cursor": "page3" } })))
        .with_priority(1)
        .mount(&slack)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/users.list"))
        .and(body_string_contains("cursor=page3"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "ok": true, "members": [] })),
        )
        .with_priority(1)
        .mount(&slack)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/users.list"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "ok": true,
            "members": first, "response_metadata": { "next_cursor": "page2" } })))
        .with_priority(2)
        .mount(&slack)
        .await;
    let outsider = &wanted[11];
    answer(
        &slack,
        "users.info",
        &format!("user={outsider}"),
        json!({ "ok": true, "user": user(outsider, "外部同事", false, false) }),
    )
    .await;
    let mut ids = wanted.clone();
    ids.push("U00000DEAD".into());
    ids.push("U0000BOTUS".into());
    let result = host.ok("people/get", json!({ "ids": ids })).await;
    let found = result["found"].as_array().unwrap();
    assert_eq!(
        found.len(),
        14,
        "名单里的、停用的、机器人用户、外部的都读到：{result}"
    );
    assert_eq!(result["missing"], json!([]));
    assert!(
        found
            .iter()
            .any(|p| p["id"] == "U0000BOTUS" && p["name"] == "构建机器人"),
        "机器人用户没有 display_name，用 real_name"
    );
    let directory = requests_to(&slack, "users.list").await;
    assert_eq!(directory.len(), 3, "读到第三页才知道名单里没有外部的人");
    assert_eq!(
        directory
            .iter()
            .map(|r| form(r).get("cursor").cloned())
            .collect::<Vec<_>>(),
        [None, Some("page2".into()), Some("page3".into())]
    );
    let stragglers = requests_to(&slack, "users.info").await;
    assert_eq!(stragglers.len(), 1, "名单里没有的再逐个读");
    assert_eq!(form(&stragglers[0])["user"], *outsider);
    assert_eq!(result["platformRequests"], 4);
}

// 成员名单只给编号：读成员不再逐个读人（原来 744 人要约 5 分钟），名字与头像由人名册读（契约 client v23 §1）。
#[tokio::test]
async fn reading_members_does_not_read_each_person() {
    let slack = MockServer::start().await;
    let host = online(&slack).await;
    Mock::given(method("POST"))
        .and(path("/api/conversations.members"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true, "members": [ME, "U0000000A1", "U0000000A2"],
            "response_metadata": { "next_cursor": "" }
        })))
        .mount(&slack)
        .await;
    let page = host
        .ok(
            "conversations/members",
            json!({ "conversationId": "C000000099", "limit": 100 }),
        )
        .await;
    assert_eq!(
        page["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| (m["id"].as_str().unwrap(), m.get("name")))
            .collect::<Vec<_>>(),
        [("U0000000A1", None), ("U0000000A2", None)],
        "不含本人，名字留空"
    );
    assert_eq!(requests_to(&slack, "users.info").await.len(), 0);
}
