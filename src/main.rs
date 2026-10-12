//! 示例 IM：HHH IM 插件的最小可用骨架（HHH-IM-Protocol v1，规范见 hhh-im-sdk 的 docs/protocol-v1.md）。
//!
//! 插件是一个子进程：HHH 从标准输入一行一条发 JSON-RPC 请求，插件从标准输出一行一条回。和平台无关、每个插件都要的
//! 那一半交给协议包的插件运行时（`hhh_im_protocol::runtime`）：读写标准输入输出、宿主的请求各开一个任务并发处理、
//! `$/cancel`、向宿主发请求并等回复、tracing 日志转成 `log/write`、资源分段读、上传攒齐与核对、平台请求计数。插件
//! 只写“收到某个方法怎么办”。这个示例：
//! - `initialize`：报协议版本、插件信息、平台说明与能力（只推一次的平台：`history = none`，消息全靠推送）；
//! - `session/open`：不用登录，直接在线，账号是“示例账号”；在线后推一个“示例群”和一条欢迎消息；
//! - `messages/send`：你在 HHH 里发的消息照收，再以“示例机器人”引用原话回一句“收到「…」”；
//! - 别的方法回“不支持”。
//!
//! 改成真的 IM 时：把这些换成和你的平台说话、把平台的格式翻成协议的统一格式；聊天记录、凭据、配置都交给 HHH，
//! 插件自己不存（见 AGENTS.md）。

// Windows：按窗口程序编，桌面宿主拉起时不弹控制台窗口；标准输入输出照样走宿主接好的管道。
#![cfg_attr(windows, windows_subsystem = "windows")]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use hhh_im_protocol::runtime::{self, Call, Host, Plugin, Reply};
use hhh_im_protocol::types::{
    Block, Conversation, ConversationKind, Event, Message, SendOutcome, Sender, SenderKind,
};
use hhh_im_protocol::{ErrorKind, RpcError};
use serde_json::{Value, json};

/// 示例群的会话编号。
const DEMO_CHAT: &str = "demo-chat";
/// 示例账号（你自己）与示例机器人的编号。
const ME: &str = "demo-user";
const BOT: &str = "demo-bot";

struct Demo {
    /// 宿主那一头：推送用（回复由运行时替你写）。
    host: Host,
    /// 同步游标：只推一次的平台每批推送都带上，宿主落库后才确认（这里就是推送的批次数）。
    batches: AtomicU64,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn text_message(id: String, from: &str, name: &str, kind: SenderKind, text: &str) -> Message {
    Message {
        id,
        conversation_id: DEMO_CHAT.into(),
        sender: Sender {
            id: from.into(),
            id_type: None,
            kind,
            name: Some(name.into()),
        },
        created_at_ms: now_ms(),
        updated_at_ms: None,
        edited_at_ms: None,
        deleted: false,
        incomplete: false,
        content: vec![Block::Text { text: text.into() }],
        reply_to: None,
        thread: None,
        reactions: None,
        edit_draft: None,
        raw: None,
    }
}

impl Demo {
    /// 推一批事件给宿主（`events/push`），带上同步游标，等宿主落库后回复。
    async fn push(&self, events: Vec<Event>) {
        let batch = self.batches.fetch_add(1, Ordering::SeqCst) + 1;
        let params = json!({ "events": events, "syncState": batch.to_string() });
        if let Err(error) = self.host.request("events/push", params).await {
            tracing::warn!(message = %error.message, "宿主没收下这批推送");
        }
    }

    /// 在线后：推示例群与一条欢迎消息。
    async fn welcome(&self) {
        let chat = Conversation {
            id: DEMO_CHAT.into(),
            kind: ConversationKind::Group,
            name: "示例群".into(),
            avatar: None,
            peer: None,
            private: false,
        };
        let hello = text_message(
            format!("welcome-{}", now_ms()),
            BOT,
            "示例机器人",
            SenderKind::Bot,
            &format!("你好，这是示例 IM {}。在这里发一句话试试。", env!("CARGO_PKG_VERSION")),
        );
        self.push(vec![
            Event::Conversation { conversation: chat },
            Event::Messages {
                conversation_id: DEMO_CHAT.into(),
                messages: vec![hello],
            },
        ])
        .await;
    }

    /// 平台说明与能力。
    fn initialize() -> Value {
        json!({
            "protocolVersion": 1,
            "provider": { "id": "demo-im", "name": "示例 IM", "version": env!("CARGO_PKG_VERSION") },
            "platform": {
                "avatarShape": "rounded",
                "history": "none",
                "actions": { "reply": false, "thread": false, "react": false, "recall": null, "edit": null }
            },
            "capabilities": {
                "conversations": { "list": false },
                "messages": { "list": false, "get": false },
                "events": { "messages": true, "conversations": true, "people": false },
                "resources": { "read": false, "avatars": false }
            },
            "settings": []
        })
    }
}

impl Plugin for Demo {
    async fn request(self: Arc<Self>, call: Call) -> Reply {
        match call.method.as_str() {
            "initialize" => Reply::from(Ok(Demo::initialize())),
            // 先回“在线”，再推示例群：宿主收到推送时这个会话已经在线了。
            "session/open" => {
                call.host.respond(
                    call.id,
                    Ok(json!({ "state": "online", "account": { "id": ME, "name": "示例账号" } })),
                );
                self.welcome().await;
                Reply::Responded
            }
            "ping" | "session/logout" | "shutdown" => Reply::from(Ok(json!({}))),
            "messages/send" => {
                let text = call
                    .params
                    .pointer("/content/0/text")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                let sent = text_message(format!("sent-{}", now_ms()), ME, "示例账号", SenderKind::User, &text);
                let accepted = serde_json::to_value(SendOutcome::Accepted { message: Box::new(sent) })
                    .map_err(|_| RpcError::from_kind(ErrorKind::ProtocolError, "发送结果写不出"));
                call.host.respond(call.id, accepted);
                // 回过“发出去了”，示例机器人再回一句。
                let reply = text_message(format!("reply-{}", now_ms()), BOT, "示例机器人", SenderKind::Bot, &format!("收到「{text}」"));
                self.push(vec![Event::Messages {
                    conversation_id: DEMO_CHAT.into(),
                    messages: vec![reply],
                }])
                .await;
                Reply::Responded
            }
            other => Reply::from(Err(RpcError::from_kind(
                ErrorKind::Unsupported,
                format!("示例 IM 不支持 {other}"),
            ))),
        }
    }
}

fn main() {
    let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("示例 IM 的运行时建不起来：{error}");
            std::process::exit(1);
        }
    };
    runtime.block_on(async {
        let (host, writes) = Host::new();
        // 日志交给宿主（写进 HHH 的日志），不打到标准输出。
        runtime::install_log(&host, "demo_im");
        let demo = Arc::new(Demo {
            host: host.clone(),
            batches: AtomicU64::new(0),
        });
        runtime::serve_on(demo, host, writes, tokio::io::stdin(), tokio::io::stdout()).await;
    });
}
