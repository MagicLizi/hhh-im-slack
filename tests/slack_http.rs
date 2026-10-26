//! Slack 适配器的本机 HTTP 测试（契约 client v11，从 HHH 内置 Slack 的同名测试搬来）：用假 Slack（wiremock）核对请求形状
//! 与错误翻译。读取与动作按协议走（测试当宿主），连接与授权直接用库的公开接口。
//! 真实取样夹具的翻译结果由 `src/slack/goldens.rs` 对拍，这里不再重复。

mod common;

use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use common::*;
use hhh_im_protocol::ErrorKind;
use hhh_im_protocol::rpc::RpcError;
use hhh_im_slack::{AuthGrant, LoginRequest, SlackAuth, SlackClient, USER_SCOPES};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use wiremock::matchers::{body_string_contains, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const CONVERSATION: &str = "C000000099";

/// 令牌拼成的授权（不过期、没有刷新令牌）。
fn grant(token: &str) -> AuthGrant {
    AuthGrant::from_parts(token.into(), None, None, None).unwrap()
}

/// `messages/list` 读会话主消息流的第一页。
fn history_of(conversation: &str) -> Value {
    json!({ "conversationId": conversation, "newestFirst": true, "limit": 20 })
}

/// 按协议上传一份（`uploads/begin` → `uploads/chunk` → `uploads/finish`），交回 `uploads/finish` 的结果。
async fn upload(host: &Host, kind: &str, name: &str, bytes: &[u8]) -> Result<Value, RpcError> {
    let begun = host
        .ok(
            "uploads/begin",
            json!({ "kind": kind, "name": name, "bytes": bytes.len() }),
        )
        .await;
    let id = begun["uploadId"].clone();
    host.ok(
        "uploads/chunk",
        json!({ "uploadId": id, "offset": 0, "data": BASE64.encode(bytes) }),
    )
    .await;
    let sha256: String = Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    host.call(
        "uploads/finish",
        json!({ "uploadId": id, "sha256": sha256 }),
    )
    .await
}

#[tokio::test]
async fn connect_checks_identity_and_reports_missing_scopes() {
    let slack = MockServer::start().await;
    mount_auth_for(&slack, TOKEN, "channels:history,chat:write").await;
    let client = SlackClient::connect(config(&slack), grant(TOKEN), Arc::default())
        .await
        .unwrap();
    assert_eq!(client.account().user_id, ME);
    assert_eq!(client.account().name.as_deref(), Some("me"));
    assert_eq!(client.account().team_id.as_deref(), Some(TEAM));
    let missing = client.missing_scopes();
    assert!(missing.contains(&"im:history".to_string()));
    assert!(!missing.contains(&"chat:write".to_string()));

    // 机器人令牌在联网前就拒绝。
    let bot = SlackClient::connect(config(&slack), grant("xoxb-test"), Arc::default())
        .await
        .unwrap_err();
    assert_eq!(bot.kind, ErrorKind::InvalidRequest);
    assert_eq!(requests_to(&slack, "auth.test").await.len(), 1);
}

#[tokio::test]
async fn platform_errors_and_rate_limits_are_translated() {
    let slack = MockServer::start().await;
    let host = online(&slack).await;
    Mock::given(method("POST"))
        .and(path("/api/conversations.history"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "7"))
        .up_to_n_times(1)
        .mount(&slack)
        .await;
    let limited = host.err("messages/list", history_of(CONVERSATION)).await;
    assert_eq!(kind_of(&limited), ErrorKind::RateLimited);
    assert_eq!(limited.data.as_ref().unwrap().retry_after_secs, Some(7));

    Mock::given(method("POST"))
        .and(path("/api/conversations.history"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"ok": false, "error": "not_in_channel"})),
        )
        .mount(&slack)
        .await;
    let denied = host.err("messages/list", history_of(CONVERSATION)).await;
    assert_eq!(kind_of(&denied), ErrorKind::NotFound);
    assert_eq!(platform_code(&denied).as_deref(), Some("not_in_channel"));
    // 编号不合法时不发请求。
    let asked = requests_to(&slack, "conversations.history").await.len();
    let invalid = host.err("messages/list", history_of("C1/../x")).await;
    assert_eq!(kind_of(&invalid), ErrorKind::InvalidRequest);
    assert_eq!(
        requests_to(&slack, "conversations.history").await.len(),
        asked
    );
}

#[tokio::test]
async fn send_carries_the_operation_id_and_thread_and_returns_the_users_message() {
    let slack = MockServer::start().await;
    let host = online(&slack).await;
    // 形状取自真实取样：本人令牌经应用发出的消息 user 是本人，同时带 bot_id / app_id。
    Mock::given(method("POST"))
        .and(path("/api/chat.postMessage"))
        .and(body_string_contains("client_msg_id=op-1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true, "channel": CONVERSATION, "ts": "1759000100.000100",
            "message": {
                "user": ME, "type": "message", "ts": "1759000100.000100",
                "bot_id": "B000000009", "app_id": "A000000009", "text": "a &lt; b <@U000000002>",
                "thread_ts": "1759000000.000001", "client_msg_id": "op-1",
                "blocks": [{"type": "rich_text", "block_id": "b1", "elements": [{"type": "rich_text_section", "elements": [
                    {"type": "text", "text": "a < b "}, {"type": "user", "user_id": "U000000002"}
                ]}]}]
            }
        })))
        .mount(&slack)
        .await;
    let sent = host
        .ok(
            "messages/send",
            json!({
                "operationId": "op-1",
                "target": { "type": "reply", "conversationId": CONVERSATION,
                            "messageId": "1759000000.000001", "inThread": true },
                "content": [
                    { "type": "text", "text": "a < b " },
                    { "type": "mention", "userId": "U000000002", "name": "对方" }
                ]
            }),
        )
        .await;
    assert_eq!(sent["state"], "accepted", "{sent}");
    let message = &sent["message"];
    assert_eq!(message["sender"]["id"], ME);
    assert_eq!(message["sender"]["kind"], "user");
    assert_eq!(message["thread"]["rootId"], "1759000000.000001");
    let post = requests_to(&slack, "chat.postMessage").await;
    let fields = form(&post[0]);
    assert_eq!(fields["channel"], CONVERSATION);
    assert_eq!(fields["text"], "a &lt; b <@U000000002>");
    assert_eq!(fields["thread_ts"], "1759000000.000001");
    assert_eq!(fields["client_msg_id"], "op-1");
}

#[tokio::test]
async fn edit_recall_and_reactions_use_the_users_token_and_official_fields() {
    let slack = MockServer::start().await;
    let host = online(&slack).await;
    for method_name in [
        "chat.update",
        "chat.delete",
        "reactions.add",
        "reactions.remove",
    ] {
        Mock::given(method("POST"))
            .and(path(format!("/api/{method_name}")))
            .and(header("authorization", format!("Bearer {TOKEN}").as_str()))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
            .mount(&slack)
            .await;
    }
    let ts = "1759000100.000100";
    // 编辑后插件照 Slack 上的这条重读一次交给宿主（内置是拿本地记着的那条补上）：假 Slack 给出改好的那条。
    Mock::given(method("POST"))
        .and(path("/api/conversations.history"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true, "has_more": false,
            "messages": [{"type": "message", "user": ME, "ts": ts, "text": "改好了",
                "edited": {"user": ME, "ts": "1759000200.000000"}}]
        })))
        .mount(&slack)
        .await;
    let edited = host
        .ok(
            "messages/edit",
            json!({ "conversationId": CONVERSATION, "messageId": ts,
                    "content": [{ "type": "text", "text": "改好了" }] }),
        )
        .await;
    assert_eq!(edited["id"], ts, "{edited}");
    host.ok(
        "messages/recall",
        json!({ "conversationId": CONVERSATION, "messageId": ts }),
    )
    .await;
    let reaction = host
        .ok(
            "reactions/add",
            json!({ "conversationId": CONVERSATION, "messageId": ts, "emojiType": "+1" }),
        )
        .await;
    assert_eq!(reaction["id"], format!("{ME}:+1"));
    host.ok(
        "reactions/remove",
        json!({ "conversationId": CONVERSATION, "messageId": ts,
                "reactionId": format!("{ME}:wave::skin-tone-3") }),
    )
    .await;
    let requests = slack.received_requests().await.unwrap();
    let by_path = |p: &str| form(requests.iter().find(|r| r.url.path() == p).unwrap());
    assert_eq!(by_path("/api/chat.update")["text"], "改好了");
    assert_eq!(by_path("/api/chat.update")["ts"], ts);
    assert_eq!(by_path("/api/chat.delete")["ts"], ts);
    assert_eq!(by_path("/api/reactions.add")["name"], "+1");
    assert_eq!(by_path("/api/reactions.add")["timestamp"], ts);
    assert_eq!(
        by_path("/api/reactions.remove")["name"],
        "wave::skin-tone-3"
    );
    // 编辑、撤回被平台拒绝时原样返回，说明里写清是哪种。
    slack.reset().await;
    Mock::given(method("POST"))
        .and(path("/api/chat.update"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"ok": false, "error": "cant_update_message"})),
        )
        .mount(&slack)
        .await;
    let refused = host
        .err(
            "messages/edit",
            json!({ "conversationId": CONVERSATION, "messageId": ts,
                    "content": [{ "type": "text", "text": "x" }] }),
        )
        .await;
    assert_eq!(kind_of(&refused), ErrorKind::PermissionDenied);
    assert_eq!(
        platform_code(&refused).as_deref(),
        Some("cant_update_message")
    );
    assert!(user_message(&refused).contains("编辑"));
}

#[tokio::test]
async fn uploads_ask_for_a_url_send_the_bytes_then_complete_into_the_conversation() {
    let slack = MockServer::start().await;
    let host = online(&slack).await;
    Mock::given(method("POST"))
        .and(path("/api/files.getUploadURLExternal"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true, "upload_url": format!("{}/upload/abc", slack.uri()), "file_id": "F000000077"
        })))
        .mount(&slack)
        .await;
    Mock::given(method("POST"))
        .and(path("/upload/abc"))
        .respond_with(ResponseTemplate::new(200).set_body_string("OK - 5"))
        .mount(&slack)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/files.completeUploadExternal"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true, "files": [{"id": "F000000077", "title": "图.png"}]
        })))
        .mount(&slack)
        .await;
    // 分享到会话后，插件在会话里找带着这个文件的那条当回执。
    Mock::given(method("POST"))
        .and(path("/api/conversations.history"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true, "has_more": false,
            "messages": [{"type": "message", "user": ME, "ts": "1759000300.000100", "text": "",
                "files": [{"id": "F000000077", "mode": "hosted", "mimetype": "image/png", "name": "图.png"}]}]
        })))
        .mount(&slack)
        .await;
    let finished = upload(&host, "image", "图.png", b"hello").await.unwrap();
    assert_eq!(finished["key"], "F000000077");
    let requests = slack.received_requests().await.unwrap();
    let ask = form(
        requests
            .iter()
            .find(|r| r.url.path() == "/api/files.getUploadURLExternal")
            .unwrap(),
    );
    assert_eq!(ask["filename"], "图.png");
    assert_eq!(ask["length"], "5");
    let bytes = requests
        .iter()
        .find(|r| r.url.path() == "/upload/abc")
        .unwrap();
    assert_eq!(bytes.body, b"hello");
    // 传完还没分享到任何会话：发送时才分享。
    assert!(
        requests_to(&slack, "files.completeUploadExternal")
            .await
            .is_empty()
    );
    let sent = host
        .ok(
            "messages/send",
            json!({
                "operationId": "op-2",
                "target": { "type": "conversation", "conversationId": CONVERSATION },
                "content": [{ "type": "image", "key": finished["key"] }]
            }),
        )
        .await;
    assert_eq!(sent["state"], "accepted", "{sent}");
    assert_eq!(
        sent["message"]["content"][0]["resource"]["key"],
        "F000000077"
    );
    let complete = form(&requests_to(&slack, "files.completeUploadExternal").await[0]);
    assert_eq!(complete["channel_id"], CONVERSATION);
    let files: Value = serde_json::from_str(&complete["files"]).unwrap();
    assert_eq!(files[0]["id"], "F000000077");
    // 上传地址不是 Slack 的域名、也不是接口同源时拒绝，不把字节发到别处。
    slack.reset().await;
    Mock::given(method("POST"))
        .and(path("/api/files.getUploadURLExternal"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true, "upload_url": "https://evil.example/upload", "file_id": "F000000078"
        })))
        .mount(&slack)
        .await;
    let refused = upload(&host, "image", "图.png", b"hello")
        .await
        .unwrap_err();
    assert_eq!(kind_of(&refused), ErrorKind::ProtocolError);
}

#[tokio::test]
async fn pkce_exchange_and_refresh_never_send_a_client_secret() {
    let slack = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/oauth.v2.access"))
        .and(body_string_contains("code_verifier="))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true, "app_id": "A01",
            "authed_user": {"id": ME, "scope": "channels:history,chat:write",
                "access_token": "xoxe.xoxp-1-a", "token_type": "user",
                "refresh_token": "xoxe-1-b", "expires_in": 43200},
            "team": {"id": TEAM}, "enterprise": null, "is_enterprise_install": false
        })))
        .mount(&slack)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oauth.v2.access"))
        .and(body_string_contains("grant_type=refresh_token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true, "access_token": "xoxe.xoxp-1-c", "expires_in": 43200,
            "refresh_token": "xoxe-1-d", "token_type": "user", "scope": "chat:write",
            "user_id": ME, "team": {"id": TEAM}
        })))
        .mount(&slack)
        .await;
    let auth = SlackAuth::new(config(&slack), "123.456").unwrap();
    let redirect = "http://localhost:29527/hhh-friend-im/slack/callback";
    let pending = auth
        .begin_login(LoginRequest {
            redirect_uri: redirect.into(),
            scopes: USER_SCOPES.iter().map(|s| s.to_string()).collect(),
        })
        .unwrap();
    let state = reqwest::Url::parse(pending.authorization_url())
        .unwrap()
        .query_pairs()
        .find(|(k, _)| k == "state")
        .unwrap()
        .1
        .to_string();
    let login = auth
        .finish_login(pending, &format!("{redirect}?code=c-1&state={state}"))
        .await
        .unwrap();
    assert_eq!(login.user_id, ME);
    assert_eq!(login.team_id, TEAM);
    let refreshed = auth.refresh(&login.grant).await.unwrap();
    assert_eq!(refreshed.access_token(), "xoxe.xoxp-1-c");
    assert_eq!(refreshed.refresh_token(), Some("xoxe-1-d"));
    for request in slack.received_requests().await.unwrap() {
        let fields = form(&request);
        assert!(
            !fields.contains_key("client_secret"),
            "PKCE 不带 Client Secret"
        );
        assert!(request.headers.get("authorization").is_none());
        assert_eq!(fields["client_id"], "123.456");
    }
}

/// 一份经 https 回调登录拿到的授权（本机续期会被要 Client Secret）。
fn relayed_grant() -> AuthGrant {
    AuthGrant::from_parts(
        "xoxe.xoxp-1-old".into(),
        Some("xoxe-1-r1".into()),
        Some(1),
        None,
    )
    .unwrap()
}

// 经授权中继续期（HHH 契约 friend/im-auth-relay v7）：中继转回令牌就用；Slack 拒绝按 Slack 的错误名要求重新登录；
// 中继上的应用凭据不对、没配、连不上 Slack、回执不对都不算这份授权失效；限流照限流。中继只收到刷新令牌一项。
#[tokio::test]
async fn relay_refresh_answers_keep_reauthorization_apart_from_temporary_trouble() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/slack/refresh"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"token": {
            "access_token": "xoxe.xoxp-1-new", "refresh_token": "xoxe-1-r2", "expires_in": 43200,
            "token_type": "user", "scope": "chat:write"
        }})))
        .expect(1)
        .mount(&server)
        .await;
    let auth = SlackAuth::new(config(&server), "123.456").unwrap();
    let renewed = auth
        .refresh_via_relay(&server.uri(), &relayed_grant())
        .await
        .unwrap();
    assert_eq!(renewed.access_token(), "xoxe.xoxp-1-new");
    assert_eq!(renewed.refresh_token(), Some("xoxe-1-r2"));
    let sent: Value =
        serde_json::from_slice(&server.received_requests().await.unwrap()[0].body).unwrap();
    assert_eq!(sent, json!({"refreshToken": "xoxe-1-r1"}));

    let cases = [
        (
            ResponseTemplate::new(400).set_body_json(json!({"error": {
                "code": "slack_refresh_rejected", "slackError": "invalid_refresh_token", "message": "拒绝"}})),
            ErrorKind::AuthExpired,
            Some(hhh_im_slack::ReauthReason::Expired),
        ),
        (
            ResponseTemplate::new(400).set_body_json(json!({"error": {
                "code": "slack_refresh_rejected", "slackError": "bad_client_secret", "message": "拒绝"}})),
            ErrorKind::Disconnected,
            None,
        ),
        (
            ResponseTemplate::new(409).set_body_json(json!({"error": {
                "code": "slack_refresh_unavailable", "message": "这个中继没有配置 Slack 应用。"}})),
            ErrorKind::Disconnected,
            None,
        ),
        (
            ResponseTemplate::new(502).set_body_json(json!({"error": {
                "code": "slack_unreachable", "message": "连不上"}})),
            ErrorKind::Disconnected,
            None,
        ),
        (
            ResponseTemplate::new(200).set_body_json(json!({"unexpected": true})),
            ErrorKind::Disconnected,
            None,
        ),
        (
            ResponseTemplate::new(429).set_body_json(json!({"error": {
                "code": "refresh_rate_limited", "message": "太频繁"}})),
            ErrorKind::RateLimited,
            None,
        ),
    ];
    for (answer, kind, reauth) in cases {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/slack/refresh"))
            .respond_with(answer)
            .mount(&server)
            .await;
        let auth = SlackAuth::new(config(&server), "123.456").unwrap();
        let error = auth
            .refresh_via_relay(&server.uri(), &relayed_grant())
            .await
            .unwrap_err();
        assert_eq!((error.kind, error.reauth), (kind, reauth), "{error:?}");
    }
    let auth = SlackAuth::new(config(&server), "123.456").unwrap();
    assert_eq!(
        auth.refresh_via_relay("http://127.0.0.1:9", &relayed_grant())
            .await
            .unwrap_err()
            .kind,
        ErrorKind::Disconnected,
        "中继连不上：暂时故障"
    );
}

/// 格式正文走 Slack 原生 blocks；发送和编辑保持一致，不把输入里的 Markdown 或 @ 文本再解释一遍。
#[tokio::test]
async fn rich_send_and_edit_use_native_blocks_and_keep_edit_drafts() {
    let slack = MockServer::start().await;
    let host = online(&slack).await;
    let ts = "1759000100.000100";
    let blocks = json!([{"type":"rich_text","elements":[{"type":"rich_text_section","elements":[
        {"type":"text","text":"粗体","style":{"bold":true}},
        {"type":"emoji","name":"wave"},
        {"type":"text","text":" *原样* @here"}
    ]}]}]);
    let message = json!({"type":"message","user":ME,"ts":ts,"text":"粗体:wave: *原样* @here","blocks":blocks});
    Mock::given(method("POST"))
        .and(path("/api/chat.postMessage"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"ok":true,"channel":CONVERSATION,"ts":ts,"message":message})),
        )
        .expect(1)
        .mount(&slack)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/chat.update"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok":true})))
        .expect(1)
        .mount(&slack)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/conversations.history"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"ok":true,"has_more":false,"messages":[message]})),
        )
        .mount(&slack)
        .await;
    let content = json!([{"type":"rich","root":{"type":"root","children":[{"type":"paragraph","children":[
        {"type":"strong","children":[{"type":"text","value":"粗体"}]},
        {"type":"emoji","emojiType":"wave"},
        {"type":"text","value":" *原样* @here"}
    ]}]}}]);
    let sent = host.ok("messages/send", json!({"operationId":"rich-op-1","target":{"type":"conversation","conversationId":CONVERSATION},"content":content})).await;
    assert_eq!(sent["state"], "accepted");
    assert_eq!(sent["message"]["editDraft"], content);
    let edited = host
        .ok(
            "messages/edit",
            json!({"conversationId":CONVERSATION,"messageId":ts,"content":content}),
        )
        .await;
    assert_eq!(edited["editDraft"], content);
    for name in ["chat.postMessage", "chat.update"] {
        let requests = requests_to(&slack, name).await;
        let fields = form(&requests[0]);
        assert_eq!(
            serde_json::from_str::<Value>(&fields["blocks"]).unwrap(),
            blocks
        );
        assert_eq!(fields["text"], "粗体:wave: *原样* @here");
        if name == "chat.postMessage" {
            assert_eq!(fields["mrkdwn"], "false");
        }
        assert_eq!(fields["parse"], "none");
    }
    assert_eq!(
        form(&requests_to(&slack, "chat.postMessage").await[0])["client_msg_id"],
        "rich-op-1"
    );
}
