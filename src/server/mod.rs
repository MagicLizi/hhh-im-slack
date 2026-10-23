//! 协议服务（HHH-IM-Protocol v1）：并发处理、`$/cancel`、等宿主回话、日志、资源分段、上传攒齐与平台请求计数都用
//! 协议包的插件运行时（`hhh_im_protocol::runtime`）；这里只写每个方法在 Slack 上怎么办：握手 [`declare`]、会话与续期
//! [`session`]、登录 [`login`]（经授权中继 [`relay`]）、读取与动作 [`methods`]。
//!
//! 一个进程只服务一个账号（§1）。

mod credential;
mod declare;
mod login;
mod methods;
mod relay;
mod session;

use std::sync::Arc;
use std::time::Duration;

use hhh_im_protocol::Method;
use hhh_im_protocol::rpc::RpcError;
use hhh_im_protocol::runtime::{self, Call, Host, Plugin, Reply, Writes, params};
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{Mutex, RwLock};

use crate::{Caches, SlackConfig};

/// HHH 的 Slack 应用（契约 friend/im-auth-relay v1，同 Friend 的 `SLACK_CLIENT_ID`）：新登录一律用它，PKCE 下只要
/// Client ID。
pub(crate) const BUILTIN_CLIENT_ID: &str = "2372848536.12176806034562";

/// 插件服务的设置（正式运行用默认值；测试换成本机的假 Slack 与假中继、调短等待）。
#[derive(Debug, Clone)]
pub struct Options {
    /// Slack 的服务地址。
    pub config: SlackConfig,
    /// 授权回调中继的来源：旧宿主（`login/start` 不带方式与地址）登录时用，以及凭据里没记中继的授权续期时用；
    /// 登录框里填了的以填的为准。
    pub relay_url: String,
    /// 打开授权页后多久第一次领取结果（之后按中继建议的间隔）。
    pub relay_first_poll: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            config: SlackConfig::default(),
            relay_url: relay::DEFAULT_RELAY.into(),
            relay_first_poll: relay::RETRY_DEFAULT,
        }
    }
}

/// 一个插件进程里的全部状态。
pub(crate) struct Server {
    /// 宿主那一头：发通知、发请求（登录在后台推步骤、存凭据时用）。
    pub(crate) host: Host,
    pub(crate) options: Options,
    /// 账号的会话。
    pub(crate) state: RwLock<session::State>,
    /// 续期、登录保存串行。
    pub(crate) gate: Mutex<()>,
    pub(crate) login: login::Logins,
    pub(crate) sends: methods::Sends,
    pub(crate) transfers: methods::Transfers,
    /// 本机记着的名单、名字、表情：续期换了客户端也接着用。
    pub(crate) caches: Arc<Caches>,
}

/// 用标准输入、标准输出跑协议服务，直到宿主关掉标准输入；日志交给宿主（`log/write`）。
pub async fn run_stdio() {
    let (host, writes) = Host::new();
    runtime::install_log(&host, "hhh_im_slack");
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "Slack 插件已启动");
    serve_with(
        host,
        writes,
        Options::default(),
        tokio::io::stdin(),
        tokio::io::stdout(),
    )
    .await;
    tracing::info!("宿主关了标准输入，Slack 插件退出");
}

/// 在给定的输入、输出上跑协议服务（测试用进程内的管道），直到输入关掉。
pub async fn serve<R, W>(input: R, output: W, options: Options)
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let (host, writes) = Host::new();
    serve_with(host, writes, options, input, output).await;
}

async fn serve_with<R, W>(host: Host, writes: Writes, options: Options, input: R, output: W)
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let server = Arc::new(Server {
        host: host.clone(),
        options,
        state: RwLock::new(session::State::Closed { saved: None }),
        gate: Mutex::new(()),
        login: login::Logins::default(),
        sends: methods::Sends::default(),
        transfers: methods::Transfers::default(),
        caches: Arc::default(),
    });
    runtime::serve_on(server, host, writes, input, output).await;
}

/// 读懂参数后交给 `then`；读不懂回“参数不对”。
async fn with_params<T, F, Fut>(value: Value, then: F) -> Result<Value, RpcError>
where
    T: serde::de::DeserializeOwned,
    F: FnOnce(T) -> Fut,
    Fut: std::future::Future<Output = Result<Value, RpcError>>,
{
    match params(value) {
        Ok(params) => then(params).await,
        Err(error) => Err(error),
    }
}

impl Plugin for Server {
    async fn request(self: Arc<Self>, call: Call) -> Reply {
        let Call {
            id,
            method,
            params: value,
            host,
        } = call;
        let Some(known) = Method::from_name(&method) else {
            return Reply::from(Err(runtime::method_not_found(&method)));
        };
        let result = match known {
            Method::Initialize => params(value).and_then(declare::initialize),
            Method::Ping | Method::Shutdown => Ok(json!({})),
            // 回结果后接着报问题行、推登录步骤：自己回复。
            Method::SessionOpen => {
                match params(value) {
                    Ok(params) => self.session_open(id, &host, params).await,
                    Err(error) => host.respond(id, Err(error)),
                }
                return Reply::Responded;
            }
            Method::LoginStart => {
                match params(value) {
                    Ok(params) => self.login_start(id, &host, params).await,
                    Err(error) => host.respond(id, Err(error)),
                }
                return Reply::Responded;
            }
            Method::SessionLogout => self.session_logout().await,
            Method::SessionRenew => with_params(value, |p| self.session_renew(p)).await,
            Method::ConversationsList => with_params(value, |p| self.conversations_list(p)).await,
            Method::ConversationsGet => with_params(value, |p| self.conversations_get(p)).await,
            Method::ConversationsSearch => {
                with_params(value, |p| self.conversations_search(p)).await
            }
            Method::ConversationsMembers => {
                with_params(value, |p| self.conversations_members(p)).await
            }
            Method::ConversationsFindDirect => {
                with_params(value, |p| self.conversations_find_direct(p)).await
            }
            Method::ConversationsRecentActivity => {
                with_params(value, |p| self.conversations_recent_activity(p)).await
            }
            Method::PeopleGet => with_params(value, |p| self.people_get(p)).await,
            Method::PeopleSearch => with_params(value, |p| self.people_search(p)).await,
            Method::MessagesList => with_params(value, |p| self.messages_list(p)).await,
            Method::MessagesGet => with_params(value, |p| self.messages_get(p)).await,
            Method::ReactionsList => with_params(value, |p| self.reactions_list(p)).await,
            Method::EmojiCatalog => self.emoji_catalog().await,
            Method::EmojiImage => with_params(value, |p| self.emoji_image(p)).await,
            Method::ResourcesRead => with_params(value, |p| self.resources_read(p)).await,
            Method::AvatarsRead => with_params(value, |p| self.avatars_read(p)).await,
            Method::UploadsBegin => params(value).and_then(|p| self.transfers.uploads.begin(p)),
            Method::UploadsChunk => params(value).and_then(|p| self.transfers.uploads.chunk(p)),
            Method::UploadsFinish => with_params(value, |p| self.uploads_finish(p)).await,
            Method::UploadsAbort => params(value).map(|p| self.transfers.uploads.abort(p)),
            Method::MessagesSend => with_params(value, |p| self.messages_send(p)).await,
            Method::MessagesEdit => with_params(value, |p| self.messages_edit(p)).await,
            Method::MessagesRecall => with_params(value, |p| self.messages_recall(p)).await,
            Method::ReactionsAdd => with_params(value, |p| self.reactions_add(p)).await,
            Method::ReactionsRemove => with_params(value, |p| self.reactions_remove(p)).await,
            // 能力里没声明：不能先查后发、不重新翻译旧消息；没有插件操作。
            Method::MessagesLookupSend
            | Method::MessagesRetranslate
            | Method::MessagesMaintain
            | Method::ActionsStart
            | Method::StepChoose => Err(Server::unsupported(&method)),
            // Slack 的登录不经宿主在本机收回调、也没有表单；其余是插件发给宿主的方法。
            Method::LoginCallback
            | Method::LoginSubmit
            | Method::LoginCancel
            | Method::LoginStep
            | Method::LoginFinished
            | Method::LoginFailed
            | Method::CredentialSave
            | Method::RequestsAcquire
            | Method::RequestsReceived
            | Method::SessionExpired
            | Method::SessionNotice
            | Method::EventsPush
            | Method::Status
            | Method::LogWrite
            | Method::Cancel
            | Method::ActionStep
            | Method::ActionSubmit
            | Method::ActionCancel
            | Method::ActionFinished
            | Method::ActionFailed => Err(runtime::method_not_found(&method)),
        };
        Reply::from(result)
    }

    async fn notification(self: Arc<Self>, method: String, params: Value, _host: Host) {
        match method.as_str() {
            "login/cancel" => {
                if let Ok(cancel) =
                    serde_json::from_value::<hhh_im_protocol::method::LoginCancelParams>(params)
                {
                    self.cancel_login(Some(&cancel.flow_id)).await;
                }
            }
            // Slack 的登录没有表单这一步。
            "login/submit" => tracing::debug!("Slack 登录没有表单，忽略 login/submit"),
            other => tracing::debug!(method = other, "不认识的通知，忽略"),
        }
    }
}

/// 握手结果（测试与对照文件用）。
#[doc(hidden)]
pub fn initialize_result() -> Value {
    declare::result()
}
