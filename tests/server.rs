//! 协议服务的端到端测试：测试当宿主，插件在进程内跑，Slack 与授权中继都是本机的假服务。核对握手、开会话与问题行、
//! 交接、登录（经中继领授权码、本机 PKCE 换令牌）、续期（刷新令牌只用一次、先存后用、用发出授权的应用）、借来的凭据、
//! 人名册与平台请求数。

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use common::*;
use hhh_im_protocol::transport::Incoming;
use serde_json::{Value, json};
use wiremock::matchers::{body_string_contains, header, method, path, path_regex};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

/// HHH 的 Slack 应用（新登录与它发出的授权续期都用它）。
const BUILTIN_CLIENT_ID: &str = "2372848536.12176806034562";

/// 握手结果与样例逐项相同（样例拷进 HHH，宿主那边核对平台说明与内置 Slack 一致）；说不出 v1 的宿主被拒绝。
#[tokio::test]
async fn initialize_declares_the_builtin_slack_platform() {
    let slack = MockServer::start().await;
    let host = Host::spawn(options(&slack));
    let result = host.initialize().await;
    let golden = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/slack-protocol/initialize.json");
    if std::env::var_os("HHH_UPDATE_GOLDENS").is_some() {
        std::fs::write(
            &golden,
            serde_json::to_string_pretty(&result).unwrap() + "\n",
        )
        .unwrap();
    }
    let expected: Value = serde_json::from_slice(&std::fs::read(&golden).unwrap()).unwrap();
    assert_eq!(result, expected);
    assert_eq!(result, hhh_im_slack::server::initialize_result());
    let parsed: hhh_im_protocol::method::InitializeResult =
        serde_json::from_value(result.clone()).unwrap();
    parsed
        .capabilities
        .check_consistent(parsed.platform.history)
        .unwrap();
    assert!(parsed.login_methods.is_empty(), "点登录直接开授权页");
    assert!(parsed.capabilities.people.bots, "机器人按编号读");
    let refused = host
        .err(
            "initialize",
            json!({ "protocolVersions": [2], "host": { "name": "HHH", "version": "9" }, "locale": "zh-CN",
                    "tempDir": "", "network": {} }),
        )
        .await;
    assert_eq!(kind_of(&refused), hhh_im_protocol::ErrorKind::Unsupported);
    assert!(host.call("nope/nope", json!({})).await.is_err());
    assert_eq!(host.ok("ping", json!({})).await, json!({}));
}

/// 没有凭据、凭据读不懂时要登录；存着的凭据开会话：账号是 Slack 的用户编号（种类同内置时代）、名字、工作区；
/// 回结果后报问题行：权限齐了是空的，缺了写“缺少权限 …：点重新登录补上授权”、带重新登录。
#[tokio::test]
async fn opening_reports_the_account_the_workspace_and_missing_scopes() {
    let slack = MockServer::start().await;
    let host = Host::start(options(&slack)).await;
    let none = host.ok("session/open", json!({ "settings": {} })).await;
    assert_eq!(
        none,
        json!({ "state": "needsLogin", "reason": "noCredential", "message": "Slack 还没有登录，请先登录。" })
    );
    let garbled = host
        .ok(
            "session/open",
            json!({ "credential": "{}", "settings": {} }),
        )
        .await;
    assert_eq!(garbled["state"], "needsLogin");
    assert_eq!(garbled["reason"], "credentialInvalid");

    mount_auth(&slack).await;
    let open = host
        .ok(
            "session/open",
            json!({ "credential": credential(TOKEN, None, None), "settings": {} }),
        )
        .await;
    assert_eq!(
        open,
        json!({ "state": "online", "account": { "id": ME, "idType": "user_id", "name": "me", "tenant": "测试工作区" } })
    );
    assert_eq!(
        host.expect_notification("session/notice").await,
        json!({ "notices": [] })
    );

    let slack = MockServer::start().await;
    let granted: Vec<&str> = hhh_im_slack::USER_SCOPES
        .iter()
        .copied()
        .filter(|scope| !matches!(*scope, "usergroups:read" | "reactions:read"))
        .collect();
    mount_auth_for(&slack, TOKEN, &granted.join(",")).await;
    let host = Host::start(options(&slack)).await;
    let open = host
        .ok(
            "session/open",
            json!({ "credential": credential(TOKEN, None, None), "settings": {} }),
        )
        .await;
    assert_eq!(open["state"], "online");
    let notices = host.expect_notification("session/notice").await;
    let notice = &notices["notices"][0];
    assert_eq!(notice["id"], "missing-scopes");
    assert_eq!(notice["relogin"], true);
    let text = notice["text"].as_str().unwrap();
    let expected: Vec<&str> = hhh_im_slack::USER_SCOPES
        .iter()
        .copied()
        .filter(|scope| matches!(*scope, "usergroups:read" | "reactions:read"))
        .collect();
    assert_eq!(
        text,
        format!("缺少权限 {}：点重新登录补上授权", expected.join("、"))
    );
}

/// 凭据里没记工作区名字时上线读一次（要 team:read），读到了记进凭据、存给宿主；读不到只写账号名。
#[tokio::test]
async fn a_missing_workspace_name_is_read_once_and_saved() {
    let slack = MockServer::start().await;
    mount_auth(&slack).await;
    Mock::given(method("POST"))
        .and(path("/api/team.info"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"ok": true, "team": {"id": TEAM, "name": "示例工作区", "icon": {}}}),
        ))
        .expect(1)
        .mount(&slack)
        .await;
    let host = Arc::new(Host::start(options(&slack)).await);
    let bare = json!({ "version": 1, "accessToken": TOKEN }).to_string();
    let caller = host.clone();
    let open = tokio::spawn(async move {
        caller
            .call(
                "session/open",
                json!({ "credential": bare, "settings": {} }),
            )
            .await
    });
    let saved = host.expect_save(true).await;
    let credential: Value = serde_json::from_str(saved["credential"].as_str().unwrap()).unwrap();
    assert_eq!(credential["workspace"], "示例工作区");
    assert_eq!(credential["account"]["id"], ME);
    let open = open.await.unwrap().unwrap();
    assert_eq!(open["account"]["tenant"], "示例工作区");
}

/// 附录 D：接过内置 Slack 的授权——先存插件自己的凭据、宿主回了成功，才回在线；内置应用发的不记 Client ID，旧版本
/// 自填的记下（续期照旧用它）；工作区名字取自内置的连接设置。用不了的回需要登录。
#[tokio::test]
async fn handover_saves_first_and_keeps_the_app_that_issued_the_grant() {
    let slack = MockServer::start().await;
    mount_auth_for(
        &slack,
        "xoxe.xoxp-builtin",
        &hhh_im_slack::USER_SCOPES.join(","),
    )
    .await;
    let host = Arc::new(Host::start(options(&slack)).await);
    let legacy = json!({ "format": "hhh-builtin-v1",
        "credential": json!({"accessToken":"xoxe.xoxp-builtin","refreshToken":"xoxe-builtin",
            "expiresAt": now_secs() + 7200, "scopes":["chat:write"],"account":{"id":ME,"name":"me"}}).to_string(),
        "settings": { "clientId": BUILTIN_CLIENT_ID, "accountName": "me", "workspaceName": "测试工作区",
            "missingScopes": [] } });
    let caller = host.clone();
    let open = tokio::spawn(async move {
        caller
            .call("session/open", json!({ "settings": {}, "legacy": legacy }))
            .await
    });
    let saved = match host.next().await {
        Incoming::Request { id, method, params } if method == "credential/save" => {
            tokio::time::sleep(Duration::from_millis(200)).await;
            assert!(!open.is_finished(), "宿主回复存好之前不回在线");
            host.conn.respond(id, json!({})).await.unwrap();
            params
        }
        other => panic!("等的是 credential/save，来的是 {other:?}"),
    };
    let credential: Value = serde_json::from_str(saved["credential"].as_str().unwrap()).unwrap();
    assert_eq!(credential["accessToken"], "xoxe.xoxp-builtin");
    assert_eq!(credential["refreshToken"], "xoxe-builtin");
    assert!(credential.get("clientId").is_none(), "内置应用不记");
    assert_eq!(credential["workspace"], "测试工作区");
    assert!(saved["renewAtMs"].as_i64().unwrap() > 0);
    let open = open.await.unwrap().unwrap();
    assert_eq!(open["state"], "online");
    assert_eq!(open["account"]["id"], ME, "账号与内置 Slack 的用户编号相同");
    assert_eq!(open["account"]["tenant"], "测试工作区");
    assert!(open.get("adoptedSettings").is_none());

    // 旧版本自填的 Client ID：记下；交来的授权已过期，续期用它。
    Mock::given(method("POST"))
        .and(path("/api/oauth.v2.access"))
        .and(body_string_contains("grant_type=refresh_token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true, "access_token": "xoxe.xoxp-builtin", "expires_in": 43200,
            "refresh_token": "xoxe-renewed", "token_type": "user", "scope": "chat:write"
        })))
        .mount(&slack)
        .await;
    let host = Host::start(options(&slack)).await;
    let legacy = json!({ "format": "hhh-builtin-v1",
        "credential": json!({"accessToken":"xoxe.xoxp-builtin","refreshToken":"xoxe-old",
            "expiresAt": now_secs() - 10}).to_string(),
        "settings": { "clientId": "123.456" } });
    let call = host.call("session/open", json!({ "settings": {}, "legacy": legacy }));
    let (open, saved) = tokio::join!(call, host.expect_save(true));
    let credential: Value = serde_json::from_str(saved["credential"].as_str().unwrap()).unwrap();
    assert_eq!(credential["clientId"], "123.456");
    assert_eq!(credential["refreshToken"], "xoxe-renewed");
    assert_eq!(open.unwrap()["state"], "online");
    let refresh = requests_to(&slack, "oauth.v2.access").await;
    assert_eq!(form(refresh.last().unwrap())["client_id"], "123.456");

    // 内置那份用不了：回需要登录，不存。
    let host = Host::start(options(&slack)).await;
    let legacy = json!({ "format": "hhh-builtin-v1",
        "credential": json!({"accessToken":"a b"}).to_string(), "settings": {} });
    let open = host
        .ok("session/open", json!({ "settings": {}, "legacy": legacy }))
        .await;
    assert_eq!(open["state"], "needsLogin");
    assert!(host.next_within(Duration::from_millis(200)).await.is_none());
}

/// 中继登记：不要代换令牌，照请求里的 state 回。
struct Registered {
    base: String,
}

impl Respond for Registered {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(body["platform"], "slack");
        assert!(body.get("exchange").is_none(), "Slack 不要中继代换令牌");
        ResponseTemplate::new(201).set_body_json(json!({
            "state": body["state"], "claimToken": "claim-secret",
            "callbackUrl": format!("{}/hhh-friend-im/slack/callback", self.base),
            "expiresIn": 300
        }))
    }
}

/// 领取：第一次还在等，之后照路径里的 state 回授权码（或平台的错误）。
struct Claimed {
    calls: AtomicUsize,
    result: Value,
}

impl Respond for Claimed {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        assert_eq!(
            request.headers.get("authorization").unwrap(),
            "Bearer claim-secret"
        );
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            return ResponseTemplate::new(200)
                .set_body_json(json!({ "status": "pending", "retryAfterMs": 500 }));
        }
        let state = request.url.path().split('/').nth(3).unwrap().to_owned();
        ResponseTemplate::new(200).set_body_json(
            json!({ "status": "completed", "state": state, "result": self.result.clone() }),
        )
    }
}

async fn mount_relay(slack: &MockServer, result: Value) {
    Mock::given(method("POST"))
        .and(path("/v1/flows"))
        .respond_with(Registered {
            base: relay_base(slack),
        })
        .mount(slack)
        .await;
    Mock::given(method("POST"))
        .and(path_regex(r"^/v1/flows/[0-9a-f]+/claim$"))
        .respond_with(Claimed {
            calls: AtomicUsize::new(0),
            result,
        })
        .mount(slack)
        .await;
}

/// 点“登录”：先在中继登记，回流程编号、推“打开授权页”（HHH 的应用、全部权限、回调在中继）；领到授权码后本机用 PKCE
/// 暗号换令牌（不带 Client Secret），核验身份、读工作区名字，先存凭据、宿主回了成功，再报登录完成与问题行。
#[tokio::test]
async fn logging_in_claims_the_code_from_the_relay_and_exchanges_it_with_pkce() {
    let slack = MockServer::start().await;
    mount_relay(&slack, json!({ "type": "code", "code": "c-1" })).await;
    Mock::given(method("POST"))
        .and(path("/api/oauth.v2.access"))
        .and(body_string_contains("code_verifier="))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true, "app_id": "A01",
            "authed_user": {"id": ME, "scope": hhh_im_slack::USER_SCOPES.join(","),
                "access_token": "xoxe.xoxp-new", "token_type": "user",
                "refresh_token": "xoxe-refresh", "expires_in": 43200},
            "team": {"id": TEAM}
        })))
        .expect(1)
        .mount(&slack)
        .await;
    mount_auth_for(
        &slack,
        "xoxe.xoxp-new",
        &hhh_im_slack::USER_SCOPES.join(","),
    )
    .await;
    Mock::given(method("POST"))
        .and(path("/api/team.info"))
        .and(header("authorization", "Bearer xoxe.xoxp-new"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"ok": true, "team": {"id": TEAM, "name": "测试工作区", "icon": {}}}),
        ))
        .mount(&slack)
        .await;
    let host = Host::start(options(&slack)).await;
    let started = host.ok("login/start", json!({})).await;
    let flow = started["flowId"].as_str().unwrap().to_owned();
    let step = host.expect_notification("login/step").await;
    assert_eq!(step["flowId"], flow);
    assert_eq!(step["step"]["kind"], "openUrl");
    let url = reqwest::Url::parse(step["step"]["url"].as_str().unwrap()).unwrap();
    let query: std::collections::HashMap<String, String> = url.query_pairs().into_owned().collect();
    assert_eq!(query["client_id"], BUILTIN_CLIENT_ID);
    assert_eq!(
        query["redirect_uri"],
        format!("{}/hhh-friend-im/slack/callback", relay_base(&slack))
    );
    assert_eq!(query["user_scope"], hhh_im_slack::USER_SCOPES.join(","));
    assert_eq!(query["code_challenge_method"], "S256");

    let saved = host.expect_save(true).await;
    let credential: Value = serde_json::from_str(saved["credential"].as_str().unwrap()).unwrap();
    assert_eq!(credential["accessToken"], "xoxe.xoxp-new");
    assert_eq!(credential["refreshToken"], "xoxe-refresh");
    assert!(credential.get("clientId").is_none());
    assert_eq!(credential["workspace"], "测试工作区");
    let finished = host.expect_notification("login/finished").await;
    assert_eq!(finished["flowId"], flow);
    assert_eq!(
        finished["account"],
        json!({ "id": ME, "idType": "user_id", "name": "me", "tenant": "测试工作区" })
    );
    assert_eq!(
        host.expect_notification("session/notice").await,
        json!({ "notices": [] })
    );
    let exchange = requests_to(&slack, "oauth.v2.access").await;
    let fields = form(&exchange[0]);
    assert_eq!(fields["client_id"], BUILTIN_CLIENT_ID);
    assert_eq!(fields["code"], "c-1");
    assert_eq!(fields["redirect_uri"], query["redirect_uri"]);
    assert!(
        !fields.contains_key("client_secret"),
        "PKCE 不带 Client Secret"
    );
    // 登录后就能用。
    Mock::given(method("POST"))
        .and(path("/api/users.conversations"))
        .and(header("authorization", "Bearer xoxe.xoxp-new"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"ok": true, "channels": [], "response_metadata": {"next_cursor": ""}}),
        ))
        .mount(&slack)
        .await;
    let page = host.ok("conversations/list", json!({ "limit": 20 })).await;
    assert_eq!(page["items"], json!([]));
}

/// 用户在授权页点了“取消”：中继转来平台的错误，算取消；用户在宿主里取消：再领一次还在等，算取消；中继登记不上：
/// 不开授权页，就地说原因。
#[tokio::test]
async fn denied_cancelled_and_unregistered_logins_end_with_a_reason() {
    let slack = MockServer::start().await;
    mount_relay(&slack, json!({ "type": "error", "error": "access_denied" })).await;
    let host = Host::start(options(&slack)).await;
    let flow = host.ok("login/start", json!({})).await["flowId"].clone();
    host.expect_notification("login/step").await;
    let failed = host.expect_notification("login/failed").await;
    assert_eq!(
        failed,
        json!({ "flowId": flow, "message": "已取消 Slack 登录。", "retryable": true })
    );

    let slack = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/flows"))
        .respond_with(Registered {
            base: relay_base(&slack),
        })
        .mount(&slack)
        .await;
    Mock::given(method("POST"))
        .and(path_regex(r"^/v1/flows/[0-9a-f]+/claim$"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "status": "pending", "retryAfterMs": 500 })),
        )
        .mount(&slack)
        .await;
    let host = Host::start(options(&slack)).await;
    let flow = host.ok("login/start", json!({})).await["flowId"].clone();
    host.expect_notification("login/step").await;
    host.conn
        .notify("login/cancel", json!({ "flowId": flow }))
        .await
        .unwrap();
    let failed = host.expect_notification("login/failed").await;
    assert_eq!(failed["message"], "已取消 Slack 登录。");

    let slack = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/flows"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&slack)
        .await;
    let host = Host::start(options(&slack)).await;
    let refused = host.err("login/start", json!({})).await;
    assert_eq!(kind_of(&refused), hhh_im_protocol::ErrorKind::Disconnected);
    assert!(
        user_message(&refused).starts_with("授权回调服务暂时不可用"),
        "{}",
        user_message(&refused)
    );
    assert!(host.next_within(Duration::from_millis(200)).await.is_none());
    let other = host.err("login/start", json!({ "method": "own" })).await;
    assert_eq!(user_message(&other), "Slack 没有“own”这种登录方式。");
}

/// 已过期的授权开会话：先续期（HHH 的应用、刷新令牌只用一次），新的存好才上线；Slack 拒绝续期时请用户重新登录，
/// 卡片上写内置 Slack 的那句话。
#[tokio::test]
async fn an_expired_grant_is_renewed_and_saved_before_use_or_asks_to_log_in_again() {
    let slack = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/oauth.v2.access"))
        .and(body_string_contains("refresh_token=xoxe-old"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true, "access_token": TOKEN, "expires_in": 43200,
            "refresh_token": "xoxe-next", "token_type": "user"
        })))
        .expect(1)
        .mount(&slack)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oauth.v2.access"))
        .and(body_string_contains("refresh_token=xoxe-used"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"ok": false, "error": "invalid_refresh_token"})),
        )
        .mount(&slack)
        .await;
    mount_auth(&slack).await;
    let host = Arc::new(Host::start(options(&slack)).await);
    let caller = host.clone();
    let open = tokio::spawn(async move {
        caller
            .call(
                "session/open",
                json!({ "credential": credential("xoxe.xoxp-stale", Some("xoxe-old"), Some(now_secs() - 5)),
                        "settings": {} }),
            )
            .await
    });
    let saved = host.expect_save(true).await;
    let credential_saved: Value =
        serde_json::from_str(saved["credential"].as_str().unwrap()).unwrap();
    assert_eq!(credential_saved["accessToken"], TOKEN);
    assert_eq!(credential_saved["refreshToken"], "xoxe-next");
    assert_eq!(credential_saved["workspace"], "测试工作区", "工作区照旧");
    let at = saved["renewAtMs"].as_i64().unwrap();
    let expected = ((now_secs() + 43200 - 300) * 1000) as i64;
    assert!((at - expected).abs() < 5_000, "过期前 5 分钟续期");
    assert_eq!(open.await.unwrap().unwrap()["state"], "online");
    let refresh = requests_to(&slack, "oauth.v2.access").await;
    assert_eq!(form(&refresh[0])["client_id"], BUILTIN_CLIENT_ID);

    let host = Host::start(options(&slack)).await;
    let open = host
        .ok(
            "session/open",
            json!({ "credential": credential("xoxe.xoxp-stale", Some("xoxe-used"), Some(now_secs() - 5)),
                    "settings": {} }),
        )
        .await;
    assert_eq!(
        open,
        json!({ "state": "needsLogin", "reason": "expired", "message": "重新登录后，就能继续选择频道或联系人。" })
    );
    // 没有刷新令牌又过期了：直接要登录，不联网。
    let host = Host::start(options(&slack)).await;
    let open = host
        .ok(
            "session/open",
            json!({ "credential": credential("xoxe.xoxp-stale", None, Some(now_secs() - 5)), "settings": {} }),
        )
        .await;
    assert_eq!(open["reason"], "expired");
}

/// 用着用着 Slack 说授权无效：强制续期一次、存好再做一次；续期也被拒：发 `session/expired`（卡片上的话），这次回
/// 授权失效（Friend 的话），之后的请求同样。
#[tokio::test]
async fn a_rejected_grant_in_use_is_renewed_once_then_expires() {
    let slack = MockServer::start().await;
    mount_auth(&slack).await;
    mount_auth_for(&slack, "xoxp-renewed", &hhh_im_slack::USER_SCOPES.join(",")).await;
    Mock::given(method("POST"))
        .and(path("/api/users.conversations"))
        .and(header("authorization", format!("Bearer {TOKEN}").as_str()))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"ok": false, "error": "token_expired"})),
        )
        .mount(&slack)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/users.conversations"))
        .and(header("authorization", "Bearer xoxp-renewed"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"ok": true, "channels": [], "response_metadata": {"next_cursor": ""}}),
        ))
        .mount(&slack)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oauth.v2.access"))
        .and(body_string_contains("refresh_token=xoxe-1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true, "access_token": "xoxp-renewed", "expires_in": 43200,
            "refresh_token": "xoxe-2", "token_type": "user"
        })))
        .expect(1)
        .mount(&slack)
        .await;
    let host = Arc::new(Host::start(options(&slack)).await);
    host.open(&credential(TOKEN, Some("xoxe-1"), Some(now_secs() + 7200)))
        .await;
    let caller = host.clone();
    let list = tokio::spawn(async move {
        caller
            .call("conversations/list", json!({ "limit": 20 }))
            .await
    });
    let saved = host.expect_save(true).await;
    assert!(saved["credential"].as_str().unwrap().contains("xoxe-2"));
    assert_eq!(
        host.expect_notification("session/notice").await,
        json!({ "notices": [] })
    );
    assert_eq!(list.await.unwrap().unwrap()["items"], json!([]));

    // 续期也被拒。
    let slack = MockServer::start().await;
    mount_auth(&slack).await;
    Mock::given(method("POST"))
        .and(path("/api/users.conversations"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"ok": false, "error": "token_revoked"})),
        )
        .mount(&slack)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/oauth.v2.access"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"ok": false, "error": "invalid_refresh_token"})),
        )
        .mount(&slack)
        .await;
    let host = Host::start(options(&slack)).await;
    host.open(&credential(TOKEN, Some("xoxe-1"), Some(now_secs() + 7200)))
        .await;
    let error = host.err("conversations/list", json!({ "limit": 20 })).await;
    assert_eq!(kind_of(&error), hhh_im_protocol::ErrorKind::AuthExpired);
    assert_eq!(user_message(&error), "Slack 登录已失效，请重新登录。");
    assert_eq!(
        host.expect_notification("session/expired").await,
        json!({ "reason": "expired", "message": "重新登录后，就能继续选择频道或联系人。" })
    );
    let again = host.err("conversations/list", json!({ "limit": 20 })).await;
    assert_eq!(kind_of(&again), hhh_im_protocol::ErrorKind::AuthExpired);
}

/// `session/renew`：没在线时用宿主存着的凭据续期，先存新的、再回成功；在线时用手里那份。
#[tokio::test]
async fn session_renew_refreshes_the_stored_grant_and_saves_it_first() {
    let slack = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/oauth.v2.access"))
        .and(body_string_contains("refresh_token=xoxe-a"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true, "access_token": "xoxe.xoxp-b", "expires_in": 43200,
            "refresh_token": "xoxe-b", "token_type": "user"
        })))
        .expect(1)
        .mount(&slack)
        .await;
    let host = Arc::new(Host::start(options(&slack)).await);
    let stored = json!({ "version": 1, "accessToken": "xoxe.xoxp-a", "refreshToken": "xoxe-a",
        "expiresAt": now_secs() + 200, "clientId": "123.456" })
    .to_string();
    let caller = host.clone();
    let renew = tokio::spawn(async move {
        caller
            .call("session/renew", json!({ "credential": stored }))
            .await
    });
    let saved = host.expect_save(true).await;
    let credential: Value = serde_json::from_str(saved["credential"].as_str().unwrap()).unwrap();
    assert_eq!(credential["refreshToken"], "xoxe-b");
    assert_eq!(credential["clientId"], "123.456", "续期照旧用那个应用");
    assert_eq!(renew.await.unwrap().unwrap(), json!({}));
    let refresh = requests_to(&slack, "oauth.v2.access").await;
    assert_eq!(form(&refresh[0])["client_id"], "123.456");
}

/// 借来的凭据（隔离测试）只用不续：打开时已过期，回暂时连不上并说下一步；用着被拒，发 `session/expired` 说下一步，
/// 这次回暂时连不上；从不向 Slack 续期、不存凭据。
#[tokio::test]
async fn borrowed_credentials_are_used_but_never_renewed() {
    let slack = MockServer::start().await;
    mount_auth(&slack).await;
    Mock::given(method("POST"))
        .and(path("/api/oauth.v2.access"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&slack)
        .await;
    let borrowed = "隔离测试不刷新你的 Slack 授权：请打开你的 Friend 刷新，或在测试里重新授权。";
    let host = Host::start(options(&slack)).await;
    let expired = host
        .err(
            "session/open",
            json!({ "credential": credential(TOKEN, Some("xoxe-1"), Some(now_secs() - 5)),
                    "settings": {}, "credentialReadOnly": true }),
        )
        .await;
    assert_eq!(kind_of(&expired), hhh_im_protocol::ErrorKind::Disconnected);
    assert_eq!(user_message(&expired), borrowed);

    Mock::given(method("POST"))
        .and(path("/api/users.conversations"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"ok": false, "error": "invalid_auth"})),
        )
        .mount(&slack)
        .await;
    let host = Host::start(options(&slack)).await;
    let open = host
        .ok(
            "session/open",
            json!({ "credential": credential(TOKEN, Some("xoxe-1"), Some(now_secs() + 7200)),
                    "settings": {}, "credentialReadOnly": true }),
        )
        .await;
    assert_eq!(open["state"], "online");
    host.expect_notification("session/notice").await;
    let error = host.err("conversations/list", json!({ "limit": 20 })).await;
    assert_eq!(kind_of(&error), hhh_im_protocol::ErrorKind::Disconnected);
    assert_eq!(user_message(&error), borrowed);
    assert_eq!(
        host.expect_notification("session/expired").await,
        json!({ "reason": "expired", "message": borrowed })
    );
    let renew = host
        .err(
            "session/renew",
            json!({ "credential": credential(TOKEN, Some("xoxe-1"), None) }),
        )
        .await;
    assert_eq!(
        kind_of(&renew),
        hhh_im_protocol::ErrorKind::PermissionDenied
    );
}

/// 人名册一批（同内置 Slack 的人名册）：人按编号读、机器人按编号读（`bots.info`）；Slack 明确说没有的写进 `missing`；
/// 一批里多出来的机器人这次不读，两边都不写（宿主下次再读）；Slack 暂时不可用时整批回错误、宿主稍后重读。结果带着
/// 向 Slack 发了几个请求。
#[tokio::test]
async fn people_get_reads_bots_and_tells_missing_from_not_read() {
    let slack = MockServer::start().await;
    let host = online(&slack).await;
    Mock::given(method("POST"))
        .and(path("/api/users.info"))
        .and(body_string_contains("user=U000000002"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true, "user": {
            "id": "U000000002", "name": "bob", "real_name": "Bob",
            "profile": {"display_name": "鲍勃", "image_72": "https://avatars.slack-edge.com/2024-01-01/1_ab_72.jpg"}}})))
        .mount(&slack)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/users.info"))
        .and(body_string_contains("user=U000000003"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"ok": false, "error": "user_not_found"})),
        )
        .mount(&slack)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/users.info"))
        .and(body_string_contains("user=U000000004"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&slack)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/bots.info"))
        .and(body_string_contains("bot=B000000099"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"ok": false, "error": "bot_not_found"})),
        )
        .mount(&slack)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/bots.info"))
        .respond_with(|request: &Request| {
            let bot = form(request)["bot"].clone();
            ResponseTemplate::new(200).set_body_json(json!({"ok": true, "bot": {
                "id": bot, "name": "部署机器人", "icons": {"image_72": "https://avatars.slack-edge.com/2024-01-01/2_cd_72.png"}}}))
        })
        .mount(&slack)
        .await;
    let result = host
        .ok(
            "people/get",
            json!({ "ids": ["U000000002", "U000000003"], "bots": ["B000000001", "B000000099"] }),
        )
        .await;
    let found = result["found"].as_array().unwrap();
    let ids: Vec<&str> = found.iter().map(|p| p["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["U000000002", "B000000001"], "{result}");
    assert_eq!(found[0]["kind"], "user");
    assert_eq!(found[0]["idType"], "user_id");
    let bot = &found[1];
    assert_eq!(bot["kind"], "bot");
    assert_eq!(bot["name"], "部署机器人");
    assert_eq!(
        result["missing"],
        json!(["U000000003", "B000000099"]),
        "{result}"
    );
    assert_eq!(result["platformRequests"], 4);

    // 一批里的机器人至多读 20 个：第 21 个两边都不写。
    let bots: Vec<String> = (1..=21).map(|n| format!("B{n:09}")).collect();
    let result = host
        .ok("people/get", json!({ "ids": [], "bots": bots }))
        .await;
    assert_eq!(result["found"].as_array().unwrap().len(), 20);
    assert_eq!(result["missing"], json!([]));
    assert!(!result.to_string().contains(&bots[20]));

    let error = host
        .err("people/get", json!({ "ids": ["U000000004"] }))
        .await;
    assert_eq!(kind_of(&error), hhh_im_protocol::ErrorKind::Disconnected);
}

/// 经 https 回调登录拿到的授权到期（HHH 契约 friend/im-auth-relay v7 S1）：本机续期被 Slack 以 `bad_client_secret` 拒绝，
/// 同一个刷新令牌交给登录用的那个中继续上；新凭据先存进宿主、宿主回了成功才回在线。本机那次不带 Secret。
#[tokio::test]
async fn an_expired_grant_that_slack_will_not_renew_locally_is_renewed_through_the_relay() {
    let slack = MockServer::start().await;
    mount_auth_for(
        &slack,
        "xoxe.xoxp-relayed",
        &hhh_im_slack::USER_SCOPES.join(","),
    )
    .await;
    Mock::given(method("POST"))
        .and(path("/api/oauth.v2.access"))
        .and(body_string_contains("grant_type=refresh_token"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"ok": false, "error": "bad_client_secret"})),
        )
        .mount(&slack)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/slack/refresh"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"token": {
            "access_token": "xoxe.xoxp-relayed", "refresh_token": "xoxe-relayed-2", "expires_in": 43200,
            "token_type": "user", "scope": hhh_im_slack::USER_SCOPES.join(",")
        }})))
        .expect(1)
        .mount(&slack)
        .await;
    let host = Host::start(options(&slack)).await;
    let expired = credential(
        "xoxe.xoxp-old",
        Some("xoxe-relayed-1"),
        Some(now_secs() - 10),
    );
    let call = host.call(
        "session/open",
        json!({ "credential": expired, "settings": {} }),
    );
    let (open, saved) = tokio::join!(call, host.expect_save(true));
    let saved: Value = serde_json::from_str(saved["credential"].as_str().unwrap()).unwrap();
    assert_eq!(saved["accessToken"], "xoxe.xoxp-relayed");
    assert_eq!(saved["refreshToken"], "xoxe-relayed-2");
    assert_eq!(open.unwrap()["state"], "online");
    let local = requests_to(&slack, "oauth.v2.access").await;
    assert_eq!(local.len(), 1, "本机只试一次");
    let local = form(&local[0]);
    assert_eq!(local["refresh_token"], "xoxe-relayed-1");
    assert!(!local.contains_key("client_secret"), "本机续期不带 Secret");
    let relayed: Vec<_> = slack
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|request| request.url.path() == "/v1/slack/refresh")
        .collect();
    let body: Value = serde_json::from_slice(&relayed[0].body).unwrap();
    assert_eq!(
        body,
        json!({"refreshToken": "xoxe-relayed-1"}),
        "中继收到同一个刷新令牌"
    );
}
