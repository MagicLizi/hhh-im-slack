//! 登录（protocol-v1 §6.1）：照 HHH Friend 内置 Slack 的登录（`server/im_login.rs` 的 Slack 一支、契约
//! friend/im-auth-relay v1）。用户不用填任何东西：
//!
//! 1. 用 HHH 的 Slack 应用（只要 Client ID）生成授权页：一次性 state 与 PKCE 暗号只在本机内存；
//! 2. 拿 state 在默认中继登记，登记成功才回 `{flowId}`、推“打开授权页”一步（登记不上就不打开：授权了也回不来）；
//! 3. 按中继建议的间隔领取，领到授权码拼回回调地址，本机用暗号换本人令牌（不要 Client Secret）；
//! 4. 核验身份、读工作区名字，先经 `credential/save` 存好、宿主回了成功，再上线、发 `login/finished`（§6.1：先存后用）。
//!
//! 同一时间只有一次登录：新的 `login/start` 取消没结束的旧的（旧的不再报结局）。说给用户的话同 Friend。

use std::sync::Arc;
use std::time::Duration;

use hhh_im_protocol::method as pm;
use hhh_im_protocol::rpc::{RpcError, RpcId};
use hhh_im_protocol::runtime::Host;
use serde_json::json;
use tokio::sync::watch;

use super::credential::{Credential, KnownAccount};
use super::relay::{self, Claim, Relay};
use super::session::{self, Online, rpc_error};
use super::{BUILTIN_CLIENT_ID, Server};
use crate::{AuthGrant, ErrorKind, LoginRequest, SlackAuth, SlackPendingLogin, USER_SCOPES};

/// 中继暂时连不上时多久再领一次（同 Friend）。
const RELAY_RETRY: Duration = Duration::from_secs(2);
/// “打开授权页”一步里再打开一次的按钮文字（同飞书插件）。
const OPEN_AGAIN: &str = "再打开一次";

const TIMED_OUT: &str = "Slack 登录等待已超时，请重新登录。";
const CANCELLED: &str = "已取消 Slack 登录。";

/// 一次登录为什么停下。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stop {
    /// 用户在宿主里取消（`login/cancel`）。
    Cancelled,
    /// 被新的登录取代、或退出登录：不再报结局。
    Superseded,
}

/// 正在进行的那一次登录。
struct Active {
    flow_id: String,
    stop: watch::Sender<Option<Stop>>,
}

/// 登录的状态：同一时间只有一次。
#[derive(Default)]
pub(crate) struct Logins {
    active: tokio::sync::Mutex<Option<Active>>,
    next: std::sync::atomic::AtomicU64,
}

/// 备好的一次登录：授权页、在中继登记好的领取、换码用的应用。
struct Prepared {
    pending: SlackPendingLogin,
    relay: Relay,
    flow: relay::Flow,
    auth: SlackAuth,
}

/// 登录没完成时给用户的话（平台那边用户拒绝算取消）。
fn failed(error: &crate::Error) -> String {
    if error.kind == ErrorKind::Cancelled {
        CANCELLED.into()
    } else {
        format!("Slack 登录失败：{error}")
    }
}

impl Server {
    /// `login/start`：备好这次登录（先在中继登记成功），回 `{flowId}`，再推“打开授权页”一步、在后台等结果。
    /// 回复由这里直接发出，保证宿主先拿到流程编号、再收到这一步。
    pub(crate) async fn login_start(
        self: &Arc<Self>,
        id: RpcId,
        host: &Host,
        params: pm::LoginStartParams,
    ) {
        let prepared = match self.prepare_login(&params).await {
            Ok(prepared) => prepared,
            Err(error) => return host.respond(id, Err(error)),
        };
        let flow_id = format!(
            "slack-login-{}",
            self.login
                .next
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                + 1
        );
        let (stop, stopped) = watch::channel(None);
        {
            let mut active = self.login.active.lock().await;
            if let Some(old) = active.take() {
                tracing::info!(flow = %old.flow_id, "新的 Slack 登录取代了没结束的旧登录");
                let _ = old.stop.send(Some(Stop::Superseded));
            }
            *active = Some(Active {
                flow_id: flow_id.clone(),
                stop,
            });
        }
        host.respond(id, Ok(json!({ "flowId": flow_id })));
        host.notify(
            hhh_im_protocol::Method::LoginStep.as_str(),
            pm::LoginStepParams {
                flow_id: flow_id.clone(),
                step: pm::LoginStep::OpenUrl {
                    url: prepared.pending.authorization_url().to_owned(),
                    label: OPEN_AGAIN.into(),
                    note: None,
                    links: Vec::new(),
                },
            },
        );
        tracing::info!(flow = %flow_id, "Slack 登录已开始，等待浏览器授权");
        let server = self.clone();
        tokio::spawn(async move { server.run_relay(flow_id, prepared, stopped).await });
    }

    /// 备好授权页并在中继登记；不行时回给用户看的原因（同 Friend 开始登录时就地说）。
    async fn prepare_login(&self, params: &pm::LoginStartParams) -> Result<Prepared, RpcError> {
        // Slack 不声明登录方式，宿主不带方式；带了说明宿主与插件对不上。
        if let Some(method) = params.method.as_deref() {
            return Err(rpc_error(
                ErrorKind::InvalidRequest,
                format!("Slack 没有“{method}”这种登录方式。"),
            ));
        }
        let relay = Relay::new(&self.options.relay_url)
            .map_err(|reason| rpc_error(ErrorKind::InvalidRequest, reason))?;
        let auth = SlackAuth::new(self.options.config.clone(), BUILTIN_CLIENT_ID)
            .map_err(|error| error.rpc())?;
        let pending = auth
            .begin_login(LoginRequest {
                redirect_uri: relay.callback_url(),
                scopes: USER_SCOPES
                    .iter()
                    .map(|scope| (*scope).to_owned())
                    .collect(),
            })
            .map_err(|error| error.rpc())?;
        let flow = relay.register(pending.state()).await.map_err(|error| {
            rpc_error(
                if error.transient {
                    ErrorKind::Disconnected
                } else {
                    ErrorKind::InvalidRequest
                },
                error.message,
            )
        })?;
        Ok(Prepared {
            pending,
            relay,
            flow,
            auth,
        })
    }

    /// 经中继领取，直到领到授权码、出错、超时或取消。取消时再领最后一次：已经授权了照样完成（同 Friend）。
    async fn run_relay(
        &self,
        flow_id: String,
        prepared: Prepared,
        mut stopped: watch::Receiver<Option<Stop>>,
    ) {
        let Prepared {
            pending,
            relay,
            flow,
            auth,
        } = prepared;
        let mut wait = self.options.relay_first_poll;
        let claim = loop {
            if flow.expired() {
                return self.fail_login(&flow_id, TIMED_OUT.into()).await;
            }
            let cancelling = tokio::select! {
                () = tokio::time::sleep(wait) => false,
                changed = stopped.changed() => {
                    if changed.is_err() || *stopped.borrow() == Some(Stop::Superseded) {
                        return;
                    }
                    true
                }
            };
            match relay.claim(&flow).await {
                Ok(Claim::Pending { retry_after }) => {
                    if cancelling {
                        return self.fail_login(&flow_id, CANCELLED.into()).await;
                    }
                    wait = retry_after;
                }
                Ok(claim) => break claim,
                // 网络或中继暂时有问题：还在等就下次再领（领取凭证与授权码都还在中继），取消时如实说。
                Err(error) if error.transient && !error.gone && !cancelling => wait = RELAY_RETRY,
                Err(error) => return self.fail_login(&flow_id, error.message).await,
            }
        };
        let callback = relay.callback_with(&flow, &claim);
        match auth.finish_login(pending, &callback).await {
            Ok(login) => self.finish_login(&flow_id, login.grant).await,
            Err(error) => self.fail_login(&flow_id, failed(&error)).await,
        }
    }

    /// 拿到授权：核验身份、读工作区名字，先存凭据、宿主回了成功，再上线、发 `login/finished`，接着报问题行。
    async fn finish_login(&self, flow_id: &str, grant: AuthGrant) {
        let client = match self.connect(grant.clone()).await {
            Ok(client) => client,
            Err(error) => {
                return self
                    .fail_login(flow_id, format!("Slack 登录失败：{error}"))
                    .await;
            }
        };
        let mut credential = Credential::new(&grant, None);
        credential.account = Some(KnownAccount {
            id: client.account().user_id.clone(),
            name: client.account().name.clone(),
        });
        // 工作区名字要 team:read；没有这项权限时卡片只写账号名，缺的权限另有一行。
        credential.workspace = session::workspace_of(&client).await;
        let account = session::account(&client, &credential);
        let notices = session::notices(&client);
        {
            let _turn = self.gate.lock().await;
            if !self.login_is(flow_id).await {
                tracing::info!(flow = flow_id, "Slack 登录已被取代，结果不提交");
                return;
            }
            if let Err(reason) = self.save_credential(&credential).await {
                return self
                    .fail_login(flow_id, format!("Slack 登录未完成：{reason}"))
                    .await;
            }
            self.go_online(Online {
                client: Arc::new(client),
                credential,
                read_only: false,
            })
            .await;
        }
        self.end_login(flow_id).await;
        tracing::info!(flow = flow_id, "Slack 登录完成");
        self.host.notify(
            hhh_im_protocol::Method::LoginFinished.as_str(),
            pm::LoginFinishedParams {
                flow_id: flow_id.to_owned(),
                account,
                title: None,
                message: None,
            },
        );
        self.host
            .notify(hhh_im_protocol::Method::SessionNotice.as_str(), notices);
    }

    /// 登录没完成：发 `login/failed`（被取代的不报）。
    async fn fail_login(&self, flow_id: &str, message: String) {
        if !self.login_is(flow_id).await {
            return;
        }
        self.end_login(flow_id).await;
        tracing::info!(flow = flow_id, reason = %message, "Slack 登录未完成");
        self.host.notify(
            hhh_im_protocol::Method::LoginFailed.as_str(),
            pm::LoginFailedParams {
                flow_id: flow_id.to_owned(),
                message,
                retryable: true,
                title: None,
                retry_label: None,
            },
        );
    }

    async fn login_is(&self, flow_id: &str) -> bool {
        self.login
            .active
            .lock()
            .await
            .as_ref()
            .is_some_and(|active| active.flow_id == flow_id)
    }

    async fn end_login(&self, flow_id: &str) {
        let mut active = self.login.active.lock().await;
        if active.as_ref().is_some_and(|a| a.flow_id == flow_id) {
            *active = None;
        }
    }

    /// `login/cancel`（`flow_id` 为空时是退出登录：不再报结局）。
    pub(crate) async fn cancel_login(&self, flow_id: Option<&str>) {
        let mut active = self.login.active.lock().await;
        match flow_id {
            Some(id) => {
                if let Some(active) = active.as_ref().filter(|active| active.flow_id == id) {
                    let _ = active.stop.send(Some(Stop::Cancelled));
                }
            }
            None => {
                if let Some(active) = active.take() {
                    let _ = active.stop.send(Some(Stop::Superseded));
                }
            }
        }
    }
}
