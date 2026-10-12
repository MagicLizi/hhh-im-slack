//! 测试共用：测试当宿主（用协议包里宿主那一侧的连接层），插件在进程内跑，Slack 与授权中继都是本机的假服务
//! （wiremock）。各个测试文件 `mod common;` 引进来，用不到的件不算错。
#![allow(dead_code)]

use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use hhh_im_protocol::rpc::{RpcError, RpcMessage, codec};
use hhh_im_protocol::transport::{Connection, Incoming, ProviderTransport};
use hhh_im_slack::SlackConfig;
use hhh_im_slack::server::{self, Options};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, mpsc};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

/// 测试用的本人令牌（假 Slack 只认它）。
pub const TOKEN: &str = "xoxp-test";
/// 本人与工作区的编号（假 Slack 的 `auth.test` 回这些）。
pub const ME: &str = "U000000001";
pub const TEAM: &str = "T000000001";

/// 进程内的一条管道，宿主那一头。
struct Pipe {
    write: Mutex<tokio::io::WriteHalf<tokio::io::DuplexStream>>,
    read: Mutex<tokio::io::Lines<BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>>>,
}

impl ProviderTransport for Pipe {
    fn send(
        &self,
        msg: RpcMessage,
    ) -> Pin<Box<dyn std::future::Future<Output = std::io::Result<()>> + Send + '_>> {
        Box::pin(async move {
            let line = codec::encode_line(&msg).map_err(std::io::Error::other)?;
            let mut write = self.write.lock().await;
            write.write_all(line.as_bytes()).await?;
            write.flush().await
        })
    }

    fn recv(&self) -> Pin<Box<dyn std::future::Future<Output = Option<RpcMessage>> + Send + '_>> {
        Box::pin(async move {
            loop {
                let line = self.read.lock().await.next_line().await.ok()??;
                if line.trim().is_empty() {
                    continue;
                }
                match codec::decode_line(&line) {
                    Ok(message) => return Some(message),
                    Err(error) => panic!("插件写了一行不是 JSON-RPC：{error}：{line}"),
                }
            }
        })
    }
}

/// 测试里的宿主：连接与插件主动发来的消息（`log/write` 不看）。
pub struct Host {
    pub conn: Arc<Connection>,
    incoming: Mutex<mpsc::UnboundedReceiver<Incoming>>,
}

impl Host {
    /// 起一个插件、握好手。
    pub async fn start(options: Options) -> Self {
        let host = Self::spawn(options);
        host.initialize().await;
        host
    }

    /// 起一个插件，还没握手。
    pub fn spawn(options: Options) -> Self {
        let (host_side, plugin_side) = tokio::io::duplex(8 << 20);
        let (plugin_read, plugin_write) = tokio::io::split(plugin_side);
        tokio::spawn(server::serve(plugin_read, plugin_write, options));
        let (host_read, host_write) = tokio::io::split(host_side);
        let conn = Connection::new(Arc::new(Pipe {
            write: Mutex::new(host_write),
            read: Mutex::new(BufReader::new(host_read).lines()),
        }));
        let (tx, rx) = mpsc::unbounded_channel();
        let reader = conn.clone();
        tokio::spawn(async move {
            while let Some(incoming) = reader.next_incoming().await {
                if matches!(&incoming, Incoming::Notification { method, .. } if method == "log/write")
                {
                    continue;
                }
                if tx.send(incoming).is_err() {
                    break;
                }
            }
        });
        Self {
            conn,
            incoming: Mutex::new(rx),
        }
    }

    pub async fn call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        tokio::time::timeout(Duration::from_secs(60), self.conn.call(method, params))
            .await
            .unwrap_or_else(|_| panic!("{method} 没有在 60 秒内回复"))
    }

    /// 调用并要求成功。
    pub async fn ok(&self, method: &str, params: Value) -> Value {
        self.call(method, params)
            .await
            .unwrap_or_else(|error| panic!("{method} 失败：{error:?}"))
    }

    /// 调用并要求失败。
    pub async fn err(&self, method: &str, params: Value) -> RpcError {
        match self.call(method, params).await {
            Ok(value) => panic!("{method} 应当失败，却回了 {value}"),
            Err(error) => error,
        }
    }

    /// 插件主动发来的下一件事。
    pub async fn next(&self) -> Incoming {
        tokio::time::timeout(Duration::from_secs(20), self.incoming.lock().await.recv())
            .await
            .expect("插件 20 秒内没再发东西")
            .expect("连接断了")
    }

    /// 等一小会儿，插件有没有主动发来东西（没有为空）。
    pub async fn next_within(&self, wait: Duration) -> Option<Incoming> {
        tokio::time::timeout(wait, self.incoming.lock().await.recv())
            .await
            .ok()
            .flatten()
    }

    /// 下一件必须是 `credential/save`：给出参数，按 `ok` 回复。
    pub async fn expect_save(&self, ok: bool) -> Value {
        match self.next().await {
            Incoming::Request { id, method, params } if method == "credential/save" => {
                if ok {
                    self.conn.respond(id, json!({})).await.unwrap();
                } else {
                    self.conn
                        .respond_err(
                            id,
                            RpcError::from_kind(
                                hhh_im_protocol::ErrorKind::PermissionDenied,
                                "凭据库写不进",
                            ),
                        )
                        .await
                        .unwrap();
                }
                params
            }
            other => panic!("等的是 credential/save，来的是 {other:?}"),
        }
    }

    /// 下一件必须是某个通知：给出参数。
    pub async fn expect_notification(&self, name: &str) -> Value {
        match self.next().await {
            Incoming::Notification { method, params } if method == name => params,
            other => panic!("等的是 {name}，来的是 {other:?}"),
        }
    }

    pub async fn initialize(&self) -> Value {
        self.ok(
            "initialize",
            json!({ "protocolVersions": [1], "host": { "name": "HHH", "version": "0.9.0" }, "locale": "zh-CN",
                    "tempDir": std::env::temp_dir(), "network": {} }),
        )
        .await
    }

    /// 用一份凭据开会话，要求在线；吃掉随后的问题行通知，交回开会话的结果。
    pub async fn open(&self, credential: &str) -> Value {
        let open = self
            .ok(
                "session/open",
                json!({ "credential": credential, "settings": {} }),
            )
            .await;
        assert_eq!(open["state"], "online", "{open}");
        self.expect_notification("session/notice").await;
        open
    }
}

/// 指向假 Slack 与假中继的设置；第一次领取等得很短。
pub fn options(slack: &MockServer) -> Options {
    Options {
        config: config(slack),
        relay_url: relay_base(slack),
        relay_first_poll: Duration::from_millis(20),
    }
}

/// 假中继的来源：同一台假服务，主机名写 `localhost`（Slack 只认这个主机名的 http 回调）。
pub fn relay_base(slack: &MockServer) -> String {
    slack.uri().replace("127.0.0.1", "localhost")
}

pub fn config(slack: &MockServer) -> SlackConfig {
    SlackConfig {
        api_base: format!("{}/api", slack.uri()),
        authorize_url: format!("{}/oauth/v2/authorize", slack.uri()),
    }
}

/// 表单正文 → 键值对，便于逐项断言（借用网址的查询参数解码，规则与表单编码相同）。
pub fn form(request: &Request) -> std::collections::HashMap<String, String> {
    let body = String::from_utf8_lossy(&request.body);
    reqwest::Url::parse(&format!("http://form.invalid/?{body}"))
        .unwrap()
        .query_pairs()
        .into_owned()
        .collect()
}

/// 假 Slack 的本人身份：只认这个令牌，`x-oauth-scopes` 报这些权限。
pub async fn mount_auth_for(slack: &MockServer, token: &str, scopes: &str) {
    Mock::given(method("POST"))
        .and(path("/api/auth.test"))
        .and(header("authorization", format!("Bearer {token}").as_str()))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-oauth-scopes", scopes)
                .set_body_json(json!({
                    "ok": true, "url": "https://example.slack.com/", "team": "测试工作区",
                    "user": "me", "team_id": TEAM, "user_id": ME,
                    "is_enterprise_install": false
                })),
        )
        .mount(slack)
        .await;
}

/// 假 Slack 的本人身份（[`TOKEN`]，全部权限）。
pub async fn mount_auth(slack: &MockServer) {
    mount_auth_for(slack, TOKEN, &hhh_im_slack::USER_SCOPES.join(",")).await;
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// 一份插件凭据：令牌、刷新令牌（可无）、过期时刻（可无）、工作区名字。
pub fn credential(access: &str, refresh: Option<&str>, expires_at: Option<u64>) -> String {
    let mut value = json!({ "version": 1, "accessToken": access, "workspace": "测试工作区" });
    if let Some(refresh) = refresh {
        value["refreshToken"] = refresh.into();
    }
    if let Some(expires_at) = expires_at {
        value["expiresAt"] = expires_at.into();
    }
    value.to_string()
}

/// 起一个插件、握手、用 [`TOKEN`] 开会话（不过期、没有刷新令牌）；假 Slack 已经认得这个令牌。
pub async fn online(slack: &MockServer) -> Host {
    mount_auth(slack).await;
    let host = Host::start(options(slack)).await;
    host.open(&credential(TOKEN, None, None)).await;
    host
}

pub fn kind_of(error: &RpcError) -> hhh_im_protocol::ErrorKind {
    error.data.as_ref().unwrap().kind
}

pub fn user_message(error: &RpcError) -> String {
    error
        .data
        .as_ref()
        .unwrap()
        .user_message
        .clone()
        .unwrap_or_default()
}

pub fn platform_code(error: &RpcError) -> Option<String> {
    error.data.as_ref().unwrap().platform_code.clone()
}

/// 假 Slack 收到的某个方法的请求（按收到的先后）。
pub async fn requests_to(slack: &MockServer, api_method: &str) -> Vec<Request> {
    let target = format!("/api/{api_method}");
    slack
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|request| request.url.path() == target)
        .collect()
}
