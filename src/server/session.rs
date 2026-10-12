//! 账号的一次会话（protocol-v1 §4.2、§6）：打开（含附录 D 的交接）、续期、授权失效、退出登录。
//!
//! 续期照 HHH Friend 对内置 Slack 的做法（`im/grant.rs`、`im/chat.rs`）：
//! - 用之前看一眼：离过期不足 5 分钟就换新，临时失败时先用旧的；已过期必须换新，临时失败回“暂时连不上”；
//! - Slack 说授权无效：强制续期一次再试一次；续期后仍被拒、或续期被拒，就是登录失效（发 `session/expired`）；
//! - 换新先存后用：新的授权经 `credential/save` 存进宿主、宿主回了成功才用（Slack 的刷新令牌只能用一次）；
//! - 续期用发出这份授权的应用：凭据里记着旧版本自填的 Client ID 时用它，否则是 HHH 的应用；
//! - 同一时间只换一次：续期、登录保存串行，并发的请求共用一次换新；
//! - 借来的凭据（`credentialReadOnly`）只用不续：打开时已经过期，回“暂时连不上”并说下一步（同 Friend 借用的授权），
//!   宿主稍后再开时带上借出方续过的那份；用着用着到期或被拒，发 `session/expired` 并说下一步，宿主据此重读借出方存着的
//!   凭据、变了就重开（契约 provider-protocol v12 缺口 7），这次请求同 Friend 回暂时连不上。
//!
//! 话分两处（同内置）：授权失效的错误（宿主的提示条、弹窗）写 Friend 的 `ReauthReason::message`（[`error_text`]）；
//! `session/expired` 与 `session/open` 的需要登录写重新登录卡片上的那几句（[`card_text`]，内置 Slack 的
//! `reauth_text`）。缺的权限（按 Slack 回的 `x-oauth-scopes` 算）经 `session/notice` 画在卡片下面，说法同内置。
//!
//! 另外存凭据时带 `renewAtMs`（过期前 5 分钟），宿主到时叫 `session/renew`——插件没在跑时也能续上，同 Friend 给
//! 关着的平台照常续期。

use std::future::Future;
use std::sync::Arc;

use hhh_im_protocol::method as pm;
use hhh_im_protocol::rpc::{RpcError, RpcId};
use hhh_im_protocol::runtime::Host;
use hhh_im_protocol::types::Account;
use serde_json::{Value, json};

use super::credential::{self, Credential, Freshness, KnownAccount};
use super::{BUILTIN_CLIENT_ID, Server};
use crate::grant::now_secs;
use crate::{AuthGrant, Error, ErrorKind, ReauthReason, Result, SlackAuth, SlackClient};

/// 一次会话的状态。
pub(crate) enum State {
    /// 没打开，或需要登录：留着宿主还回来的凭据。
    Closed { saved: Option<Credential> },
    /// 在线。
    Online(Online),
    /// 发过 `session/expired`：重新登录之前，之后的请求一律回授权失效（借来的凭据回暂时连不上）。
    Expired {
        saved: Option<Credential>,
        reason: ReauthReason,
        /// 失效的是借来的凭据。
        borrowed: bool,
    },
}

/// 在线时的客户端与它用的凭据。
#[derive(Clone)]
pub(crate) struct Online {
    pub(crate) client: Arc<SlackClient>,
    pub(crate) credential: Credential,
    /// 凭据是借来的：不换新、不存。
    pub(crate) read_only: bool,
}

/// 授权失效的错误里的话：同 Friend 对内置 Slack 的 `ReauthReason::message`（宿主显示在提示条、弹窗里）。
pub(crate) fn error_text(reason: ReauthReason) -> &'static str {
    match reason {
        ReauthReason::Expired | ReauthReason::Revoked => "Slack 登录已失效，请重新登录。",
        ReauthReason::CredentialInvalid => APP_INVALID,
    }
}

/// 重新登录卡片上的话：内置 Slack 按原因写的那几句（`slack/connection.rs` 的 `reauth_text`）。
pub(crate) fn card_text(reason: ReauthReason) -> &'static str {
    match reason {
        ReauthReason::Expired | ReauthReason::Revoked => "重新登录后，就能继续选择频道或联系人。",
        ReauthReason::CredentialInvalid => APP_INVALID,
    }
}

/// 发出授权的应用已失效时的话（卡片与错误一样）。
const APP_INVALID: &str = "Slack 授权所属的应用已失效，请重新登录。";
/// 借来的授权过期、没人续期时的话（同 Friend 的 `borrowed_grant_expired`）。
pub(crate) const BORROWED_EXPIRED: &str =
    "隔离测试不刷新你的 Slack 授权：请打开你的 Friend 刷新，或在测试里重新授权。";
/// 续期暂时失败、旧授权又已过期时的话（同 Friend）。
const UNAVAILABLE: &str = "暂时无法连接 Slack，请稍后重试。";
/// 还没登录。
const NOT_LOGGED_IN: &str = "Slack 还没有登录，请先登录。";
/// 缺权限那一行的编号（同一个问题前后不变）。
const MISSING_SCOPES_NOTICE: &str = "missing-scopes";

/// 网络中断、限流与读取预算属于临时故障（同 Friend 的 `grant::transient`）。
fn transient(error: &Error) -> bool {
    matches!(
        error.kind,
        ErrorKind::Disconnected | ErrorKind::RateLimited | ErrorKind::LimitExceeded
    )
}

/// 续期失败要用户做什么；临时故障为空（同 Friend 的 `grant::reauth_reason`）。
fn reauth_reason(error: &Error) -> Option<ReauthReason> {
    if transient(error) {
        return None;
    }
    Some(match error.reauth {
        Some(ReauthReason::CredentialInvalid) => ReauthReason::CredentialInvalid,
        Some(ReauthReason::Expired | ReauthReason::Revoked) | None => ReauthReason::Expired,
    })
}

/// 一个给定分类与说明的协议错误。
pub(crate) fn rpc_error(kind: ErrorKind, message: impl Into<String>) -> RpcError {
    Error::new(kind, message).rpc()
}

/// 需要登录的原因。
fn needs_login(reason: ReauthReason, message: &str) -> Value {
    let reason = match reason {
        ReauthReason::CredentialInvalid => pm::NeedsLoginReason::CredentialInvalid,
        ReauthReason::Expired | ReauthReason::Revoked => pm::NeedsLoginReason::Expired,
    };
    serde_json::to_value(pm::SessionOpenResult::NeedsLogin {
        reason,
        message: message.to_owned(),
    })
    .expect("结果总能写成 JSON")
}

/// 线上的账号：编号种类同内置时代（`user_id`），名字是 Slack 的用户名，工作区名字写在账号行（“名字 · 工作区”）。
pub(crate) fn account(client: &SlackClient, credential: &Credential) -> Account {
    let account = client.account();
    Account {
        id: account.user_id.clone(),
        id_type: Some("user_id".into()),
        name: account.name.clone().unwrap_or_default(),
        avatar: None,
        tenant: credential.workspace.clone(),
    }
}

/// 缺的权限那一行（同内置 Slack 的 `missing_scopes_template`，权限之间用顿号）；不缺时没有。
pub(crate) fn notices(client: &SlackClient) -> pm::SessionNoticeParams {
    let missing = client.missing_scopes();
    if missing.is_empty() {
        return pm::SessionNoticeParams::default();
    }
    pm::SessionNoticeParams {
        notices: vec![pm::Notice {
            id: MISSING_SCOPES_NOTICE.into(),
            text: missing_scopes_text(&missing),
            relogin: true,
            action: None,
            prompt: false,
        }],
    }
}

/// “缺少权限 a、b：点重新登录补上授权”；一行写不下（协议限 200 字）时写前面几项再说“等 N 项”。
fn missing_scopes_text(missing: &[String]) -> String {
    const TAIL: &str = "：点重新登录补上授权";
    let all = format!("缺少权限 {}{TAIL}", missing.join("、"));
    if all.chars().count() <= hhh_im_protocol::budget::NOTICE_MAX_CHARS {
        return all;
    }
    let mut shown = missing.len();
    loop {
        shown -= 1;
        let text = format!(
            "缺少权限 {} 等 {} 项{TAIL}",
            missing[..shown].join("、"),
            missing.len()
        );
        if shown <= 1 || text.chars().count() <= hhh_im_protocol::budget::NOTICE_MAX_CHARS {
            return text;
        }
    }
}

/// 开会话的结局：在线（第二项：这次已经存过凭据），或需要登录（原因与说明）。
enum Opened {
    Online(Online, bool),
    NeedsLogin(ReauthReason, String),
}

impl Server {
    /// `session/open`：回结果后接着报卡片上的问题行（缺的权限），所以自己回复。
    pub(crate) async fn session_open(&self, id: RpcId, host: &Host, params: pm::SessionOpenParams) {
        let result = self.open(params).await;
        let online = matches!(&result, Ok(value) if value["state"] == "online");
        host.respond(id, result);
        if online && let State::Online(online) = &*self.state.read().await {
            host.notify(
                hhh_im_protocol::Method::SessionNotice.as_str(),
                notices(&online.client),
            );
        }
    }

    async fn open(&self, params: pm::SessionOpenParams) -> std::result::Result<Value, RpcError> {
        let read_only = params.credential_read_only;
        if params.credential.is_none()
            && let Some(legacy) = params.legacy.as_ref()
            && legacy.format == pm::LEGACY_FORMAT_BUILTIN_V1
        {
            return self.adopt_legacy(legacy, read_only).await;
        }
        let Some(text) = params.credential.as_deref() else {
            *self.state.write().await = State::Closed { saved: None };
            return Ok(serde_json::to_value(pm::SessionOpenResult::NeedsLogin {
                reason: pm::NeedsLoginReason::NoCredential,
                message: NOT_LOGGED_IN.into(),
            })
            .expect("结果总能写成 JSON"));
        };
        let Some(saved) = Credential::parse(text) else {
            tracing::warn!("宿主还回来的 Slack 凭据读不懂，需要重新登录");
            *self.state.write().await = State::Closed { saved: None };
            return Ok(needs_login(
                ReauthReason::CredentialInvalid,
                card_text(ReauthReason::Expired),
            ));
        };
        match self.establish(saved.clone(), read_only).await? {
            Opened::Online(online, _) => {
                let account = account(&online.client, &online.credential);
                self.go_online(online).await;
                Ok(serde_json::to_value(pm::SessionOpenResult::Online {
                    account,
                    adopted_settings: None,
                })
                .expect("结果总能写成 JSON"))
            }
            Opened::NeedsLogin(reason, message) => {
                *self.state.write().await = State::Closed { saved: Some(saved) };
                Ok(needs_login(reason, &message))
            }
        }
    }

    /// 附录 D：接过 HHH 内置 Slack 的授权。先换成插件的凭据、核验账号，经 `credential/save` 存好、宿主回了成功，
    /// 再回在线；没接成（内置那份用不了）就回需要登录，内置那份宿主留着。Slack 没有要写进插件设置的东西，
    /// 登录方式也只有一种，所以不带 `adoptedSettings`。
    async fn adopt_legacy(
        &self,
        legacy: &pm::LegacyHandover,
        read_only: bool,
    ) -> std::result::Result<Value, RpcError> {
        let credential = match credential::from_legacy(
            &legacy.credential,
            &legacy.settings,
            BUILTIN_CLIENT_ID,
        ) {
            Ok(credential) => credential,
            Err(message) => {
                tracing::warn!(%message, "内置 Slack 交来的授权用不了，不接");
                *self.state.write().await = State::Closed { saved: None };
                return Ok(needs_login(ReauthReason::CredentialInvalid, &message));
            }
        };
        let known = credential.account.clone();
        let (online, saved) = match self.establish(credential, read_only).await? {
            Opened::Online(online, saved) => (online, saved),
            Opened::NeedsLogin(reason, message) => {
                tracing::warn!(?reason, "内置 Slack 交来的授权已经失效，不接");
                *self.state.write().await = State::Closed { saved: None };
                return Ok(needs_login(reason, &message));
            }
        };
        if let Some(known) = &known
            && known.id != online.client.account().user_id
        {
            tracing::warn!("交接来的授权核验出的账号与内置 Slack 记着的不同，按核验出的账号上线");
        }
        // 先存后用（附录 D、§6.3）：续期时已经存过就不再存一遍。
        if !read_only && !saved {
            let credential = online.credential.clone();
            if let Err(reason) = self.save_credential(&credential).await {
                tracing::warn!(%reason, "交接来的 Slack 凭据没存进宿主，这次不算接过");
                return Err(rpc_error(
                    ErrorKind::Disconnected,
                    format!("Slack 登录没交接成：{reason}"),
                ));
            }
        }
        tracing::info!(
            own_app = online.credential.client_id.is_some(),
            "接过了 HHH 内置 Slack 的登录"
        );
        let account = account(&online.client, &online.credential);
        self.go_online(online).await;
        Ok(serde_json::to_value(pm::SessionOpenResult::Online {
            account,
            adopted_settings: None,
        })
        .expect("结果总能写成 JSON"))
    }

    /// 用一份凭据上线：按需续期（续了就先存），核验账号，补上工作区名字；Slack 说授权无效时强制续期一次。
    /// 回在线（第二项：这次已经存过凭据）或需要登录；网络这类暂时故障回错误，宿主稍后再开。
    async fn establish(
        &self,
        credential: Credential,
        read_only: bool,
    ) -> std::result::Result<Opened, RpcError> {
        let _turn = self.gate.lock().await;
        let mut credential = credential;
        let mut grant = credential.grant().map_err(|error| error.rpc())?;
        let mut renewed = false;
        let mut saved = false;
        match credential::freshness(&grant, now_secs()) {
            Freshness::Current => {}
            Freshness::RefreshSoon if read_only => {}
            _ if read_only => return Err(rpc_error(ErrorKind::Disconnected, BORROWED_EXPIRED)),
            Freshness::Unrenewable => {
                return Ok(Opened::NeedsLogin(
                    ReauthReason::Expired,
                    card_text(ReauthReason::Expired).into(),
                ));
            }
            need => match self.refresh(&credential, &grant).await {
                Ok(fresh) => {
                    credential = credential.with_grant(&fresh);
                    saved = self.save_or_keep(&credential).await;
                    grant = fresh;
                    renewed = true;
                }
                Err(error) => match reauth_reason(&error) {
                    None if need == Freshness::RefreshSoon => {
                        tracing::warn!(%error, "Slack 授权续期暂时失败，继续使用尚未过期的授权");
                    }
                    None => return Err(rpc_error(ErrorKind::Disconnected, UNAVAILABLE)),
                    Some(reason) => {
                        tracing::warn!(%error, ?reason, "Slack 拒绝续期授权");
                        return Ok(Opened::NeedsLogin(reason, card_text(reason).into()));
                    }
                },
            },
        }
        loop {
            match self.connect(grant.clone()).await {
                Ok(client) => {
                    let account = client.account();
                    credential.account = Some(KnownAccount {
                        id: account.user_id.clone(),
                        name: account.name.clone(),
                    });
                    // 工作区名字登录时就记下了；没记着（读不到 team:read 时）这次再读一次，读到了记进凭据。
                    if credential.workspace.is_none()
                        && let Some(workspace) = workspace_of(&client).await
                    {
                        credential.workspace = Some(workspace);
                        if !read_only {
                            saved = self.save_or_keep(&credential).await;
                        }
                    }
                    return Ok(Opened::Online(
                        Online {
                            client: Arc::new(client),
                            credential,
                            read_only,
                        },
                        saved,
                    ));
                }
                Err(error) if error.kind == ErrorKind::AuthExpired && read_only => {
                    tracing::warn!(%error, "借来的 Slack 授权被拒绝，不替用户续期");
                    return Err(rpc_error(ErrorKind::Disconnected, BORROWED_EXPIRED));
                }
                Err(error) if error.kind == ErrorKind::AuthExpired && !renewed => {
                    tracing::warn!(%error, "Slack 认为授权无效，强制续期一次");
                    renewed = true;
                    if grant.refresh_token().is_none() {
                        return Ok(Opened::NeedsLogin(
                            ReauthReason::Expired,
                            card_text(ReauthReason::Expired).into(),
                        ));
                    }
                    match self.refresh(&credential, &grant).await {
                        Ok(fresh) => {
                            credential = credential.with_grant(&fresh);
                            saved = self.save_or_keep(&credential).await;
                            grant = fresh;
                        }
                        Err(error) => match reauth_reason(&error) {
                            None => return Err(rpc_error(ErrorKind::Disconnected, UNAVAILABLE)),
                            Some(reason) => {
                                return Ok(Opened::NeedsLogin(reason, card_text(reason).into()));
                            }
                        },
                    }
                }
                Err(error) if error.kind == ErrorKind::AuthExpired => {
                    tracing::warn!(%error, "续期后的 Slack 授权仍被拒绝");
                    let reason = error.reauth.unwrap_or(ReauthReason::Expired);
                    return Ok(Opened::NeedsLogin(reason, card_text(reason).into()));
                }
                Err(error) => return Err(error.rpc()),
            }
        }
    }

    /// 核验授权、建客户端（读一次本人身份）。名单、名字这类本机记着的资料同一个账号接着用。
    pub(crate) async fn connect(&self, grant: AuthGrant) -> Result<SlackClient> {
        SlackClient::connect(self.options.config.clone(), grant, self.caches.clone()).await
    }

    /// 用发出这份授权的应用续期：旧版本自填的 Client ID，或 HHH 的应用。
    async fn refresh(&self, credential: &Credential, grant: &AuthGrant) -> Result<AuthGrant> {
        let client_id = credential.client_id.as_deref().unwrap_or(BUILTIN_CLIENT_ID);
        SlackAuth::new(self.options.config.clone(), client_id)?
            .refresh(grant)
            .await
    }

    /// 把凭据交给宿主存（`credential/save`，带下次续期的时间），等宿主回复。
    pub(crate) async fn save_credential(
        &self,
        credential: &Credential,
    ) -> std::result::Result<(), String> {
        let params = pm::CredentialSaveParams {
            credential: credential.to_text(),
            renew_at_ms: credential.renew_at_ms(),
        };
        self.host
            .request(hhh_im_protocol::Method::CredentialSave.as_str(), params)
            .await
            .map(|_| ())
            .map_err(|error| {
                error
                    .data
                    .and_then(|data| data.user_message)
                    .unwrap_or(error.message)
            })
    }

    /// 续期换到的新授权存进宿主。存不进时照样用新的（刷新令牌只能用一次，旧的已经作废）：本次进程里照常读写，
    /// 进程重启后宿主还回来的旧凭据续不上，会请用户重新登录（同 Friend 续期后保存失败的结局）。
    async fn save_or_keep(&self, credential: &Credential) -> bool {
        match self.save_credential(credential).await {
            Ok(()) => true,
            Err(reason) => {
                tracing::error!(%reason, "Slack 登录已续期，但没存进宿主");
                false
            }
        }
    }

    /// 上线：换上新的客户端与凭据。
    pub(crate) async fn go_online(&self, online: Online) {
        *self.state.write().await = State::Online(online);
    }

    /// 登录失效：记下、发 `session/expired`（卡片上的话）；返回给这次请求的授权失效错误（Friend 的话）。
    async fn expire(&self, reason: ReauthReason) -> RpcError {
        self.mark_expired(reason, false, card_text(reason)).await;
        tracing::warn!(?reason, "Slack 登录已失效，请宿主请用户重新登录");
        expired_error(reason, false)
    }

    /// 借来的凭据用着用着到期或被拒：发 `session/expired` 并说下一步，宿主据此重读借出方存着的凭据、变了就重开；
    /// 这次请求同 Friend 回暂时连不上。
    async fn expire_borrowed(&self) -> RpcError {
        self.mark_expired(ReauthReason::Expired, true, BORROWED_EXPIRED)
            .await;
        tracing::warn!("借来的 Slack 授权已不可用，不替用户续期，请宿主重读借出方的凭据");
        expired_error(ReauthReason::Expired, true)
    }

    /// 记下失效、发 `session/expired`。
    async fn mark_expired(&self, reason: ReauthReason, borrowed: bool, message: &str) {
        {
            let mut state = self.state.write().await;
            let saved = match &*state {
                State::Online(online) => Some(online.credential.clone()),
                State::Closed { saved } | State::Expired { saved, .. } => saved.clone(),
            };
            *state = State::Expired {
                saved,
                reason,
                borrowed,
            };
        }
        self.host.notify(
            hhh_im_protocol::Method::SessionExpired.as_str(),
            pm::SessionExpiredParams {
                reason,
                message: message.to_owned(),
            },
        );
    }

    /// 现在能用的客户端：用之前看一眼，临近过期就换新（同 Friend 的 `OauthSource::client`）。
    async fn ready_client(&self) -> std::result::Result<Arc<SlackClient>, RpcError> {
        let online = match &*self.state.read().await {
            State::Online(online) => online.clone(),
            State::Expired {
                reason, borrowed, ..
            } => return Err(expired_error(*reason, *borrowed)),
            State::Closed { .. } => {
                return Err(rpc_error(ErrorKind::AuthExpired, NOT_LOGGED_IN));
            }
        };
        let grant = online.credential.grant().map_err(|error| error.rpc())?;
        match credential::freshness(&grant, now_secs()) {
            Freshness::Current => Ok(online.client),
            need => self.renew_in_use(&online, need).await,
        }
    }

    /// 用着用着要换新了（临近过期或已过期）：在门里再看一次（别的请求可能刚换过），然后换新。
    async fn renew_in_use(
        &self,
        stale: &Online,
        need: Freshness,
    ) -> std::result::Result<Arc<SlackClient>, RpcError> {
        let _turn = self.gate.lock().await;
        let current = match &*self.state.read().await {
            State::Online(online) => online.clone(),
            _ => return Err(rpc_error(ErrorKind::AuthExpired, NOT_LOGGED_IN)),
        };
        if current.credential.access_token() != stale.credential.access_token() {
            return Ok(current.client);
        }
        if current.read_only {
            if need == Freshness::RefreshSoon {
                tracing::debug!("借来的 Slack 授权临近过期，交给原 Friend 续期");
                return Ok(current.client);
            }
            tracing::debug!(?need, "借来的 Slack 授权已过期");
            return Err(self.expire_borrowed().await);
        }
        if need == Freshness::Unrenewable {
            tracing::warn!("Slack 授权已过期且没有刷新令牌，需要重新登录");
            return Err(self.expire(ReauthReason::Expired).await);
        }
        let grant = current.credential.grant().map_err(|error| error.rpc())?;
        match self.renew_with(&current, &grant).await {
            Ok(fresh) => Ok(fresh),
            Err(Failure::Transient(error)) if need == Freshness::RefreshSoon => {
                tracing::warn!(%error, "Slack 授权续期暂时失败，继续使用尚未过期的授权");
                Ok(current.client)
            }
            Err(Failure::Transient(error)) => {
                tracing::warn!(%error, "Slack 授权续期暂时失败");
                Err(rpc_error(ErrorKind::Disconnected, UNAVAILABLE))
            }
            Err(Failure::Rejected(reason)) => Err(self.expire(reason).await),
        }
    }

    /// Slack 在使用中说授权无效：强制续期一次（同 Friend 的 `recover`）。别的请求已经换过就直接用新的。
    async fn recover(
        &self,
        rejected: &Arc<SlackClient>,
    ) -> std::result::Result<Arc<SlackClient>, RpcError> {
        let _turn = self.gate.lock().await;
        let current = match &*self.state.read().await {
            State::Online(online) => online.clone(),
            _ => return Err(rpc_error(ErrorKind::AuthExpired, NOT_LOGGED_IN)),
        };
        if !Arc::ptr_eq(&current.client, rejected) {
            return Ok(current.client);
        }
        let grant = current.credential.grant().map_err(|error| error.rpc())?;
        if current.read_only {
            tracing::debug!("借来的 Slack 授权被 Slack 拒绝");
            return Err(self.expire_borrowed().await);
        }
        if grant.refresh_token().is_none() {
            return Err(self.expire(ReauthReason::Expired).await);
        }
        match self.renew_with(&current, &grant).await {
            Ok(fresh) => {
                tracing::info!("Slack 授权已强制续期，恢复读取");
                Ok(fresh)
            }
            Err(Failure::Transient(error)) => {
                tracing::warn!(%error, "Slack 授权暂时续不上");
                Err(rpc_error(ErrorKind::Disconnected, UNAVAILABLE))
            }
            Err(Failure::Rejected(reason)) => Err(self.expire(reason).await),
        }
    }

    /// 在门里换新：续期、先存、再核验建新客户端、换上；缺的权限可能变了，重报一次问题行。
    async fn renew_with(
        &self,
        current: &Online,
        grant: &AuthGrant,
    ) -> std::result::Result<Arc<SlackClient>, Failure> {
        let fresh = match self.refresh(&current.credential, grant).await {
            Ok(fresh) => fresh,
            Err(error) => {
                return Err(match reauth_reason(&error) {
                    None => Failure::Transient(error),
                    Some(reason) => {
                        tracing::warn!(%error, ?reason, "Slack 拒绝续期授权");
                        Failure::Rejected(reason)
                    }
                });
            }
        };
        let credential = current.credential.with_grant(&fresh);
        self.save_or_keep(&credential).await;
        match self.connect(fresh).await {
            Ok(client) => {
                if client.account().user_id != current.client.account().user_id {
                    tracing::error!("续期后的 Slack 授权属于另一个账号，不换上");
                    return Err(Failure::Rejected(ReauthReason::Expired));
                }
                let client = Arc::new(client);
                *self.state.write().await = State::Online(Online {
                    client: client.clone(),
                    credential,
                    read_only: current.read_only,
                });
                self.host.notify(
                    hhh_im_protocol::Method::SessionNotice.as_str(),
                    notices(&client),
                );
                tracing::info!("Slack 授权已续期");
                Ok(client)
            }
            Err(error) if error.kind == ErrorKind::AuthExpired => {
                tracing::warn!(%error, "续期后的 Slack 授权仍被拒绝");
                Err(Failure::Rejected(
                    error.reauth.unwrap_or(ReauthReason::Expired),
                ))
            }
            Err(error) => Err(Failure::Transient(error)),
        }
    }

    /// 用现在能用的客户端做一件事；Slack 说授权无效时强制续期一次、再做一次（同 Friend：平台判定授权无效时续期一次）。
    pub(crate) async fn with_client<T, F, Fut>(&self, work: F) -> std::result::Result<T, RpcError>
    where
        F: Fn(Arc<SlackClient>) -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        let client = self.ready_client().await?;
        match work(client.clone()).await {
            Ok(value) => Ok(value),
            Err(error) if error.kind == ErrorKind::AuthExpired => {
                tracing::warn!(%error, "Slack 认为授权无效，强制续期一次");
                let fresh = self.recover(&client).await?;
                match work(fresh).await {
                    Ok(value) => Ok(value),
                    Err(error) if error.kind == ErrorKind::AuthExpired => {
                        tracing::warn!(%error, "续期后的 Slack 授权仍被拒绝");
                        Err(self
                            .expire(error.reauth.unwrap_or(ReauthReason::Expired))
                            .await)
                    }
                    Err(error) => Err(error.rpc()),
                }
            }
            Err(error) => Err(error.rpc()),
        }
    }

    /// 只用现在能用的客户端做一件事（下载资源、头像这类：不为它强制续期）。
    pub(crate) async fn with_ready_client<T, F, Fut>(
        &self,
        work: F,
    ) -> std::result::Result<T, RpcError>
    where
        F: FnOnce(Arc<SlackClient>) -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        let client = self.ready_client().await?;
        work(client).await.map_err(|error| error.rpc())
    }

    /// `session/renew`：用宿主存着的凭据续期，先存新的、再回成功（§6.6）。插件在线时用手里的那份（最新、宿主也存着）。
    pub(crate) async fn session_renew(
        &self,
        params: pm::SessionRenewParams,
    ) -> std::result::Result<Value, RpcError> {
        let _turn = self.gate.lock().await;
        // 在门里读：别的请求可能刚续过，用它换来的那份（宿主也存着）。
        let online = match &*self.state.read().await {
            State::Online(online) => Some(online.clone()),
            _ => None,
        };
        if let State::Expired { borrowed: true, .. } = &*self.state.read().await {
            return Err(rpc_error(
                ErrorKind::PermissionDenied,
                "这份 Slack 凭据是借来的，不替用户续期。",
            ));
        }
        if let Some(online) = online {
            if online.read_only {
                return Err(rpc_error(
                    ErrorKind::PermissionDenied,
                    "这份 Slack 凭据是借来的，不替用户续期。",
                ));
            }
            let grant = online.credential.grant().map_err(|error| error.rpc())?;
            return match self.renew_with(&online, &grant).await {
                Ok(_) => Ok(json!({})),
                Err(Failure::Transient(error)) => Err(error.rpc()),
                Err(Failure::Rejected(reason)) => Err(self.expire(reason).await),
            };
        }
        let Some(credential) = Credential::parse(&params.credential) else {
            return Err(expired_error(ReauthReason::Expired, false));
        };
        let grant = credential.grant().map_err(|error| error.rpc())?;
        match self.refresh(&credential, &grant).await {
            Ok(fresh) => {
                let credential = credential.with_grant(&fresh);
                self.save_credential(&credential).await.map_err(|reason| {
                    rpc_error(
                        ErrorKind::Disconnected,
                        format!("Slack 登录已续期，但保存失败：{reason}。请重新登录。"),
                    )
                })?;
                if let State::Closed { saved } = &mut *self.state.write().await {
                    *saved = Some(credential);
                }
                tracing::info!("没在线时续上了 Slack 授权");
                Ok(json!({}))
            }
            Err(error) => match reauth_reason(&error) {
                None => Err(error.rpc()),
                Some(reason) => Err(Error {
                    kind: ErrorKind::AuthExpired,
                    message: error_text(reason).into(),
                    platform_code: error.platform_code.clone(),
                    reauth: Some(reason),
                    retry_after_secs: None,
                }
                .rpc()),
            },
        }
    }

    /// `session/logout`：丢掉内存里的会话（同内置 Slack 的退出登录：不向 Slack 撤销授权），宿主随后删凭据。
    pub(crate) async fn session_logout(&self) -> std::result::Result<Value, RpcError> {
        *self.state.write().await = State::Closed { saved: None };
        self.cancel_login(None).await;
        tracing::info!("Slack 已退出登录");
        Ok(json!({}))
    }
}

/// 读工作区名字（要 team:read）；读不到时卡片只写账号名（同 Friend 登录时）。
pub(crate) async fn workspace_of(client: &SlackClient) -> Option<String> {
    match client.team().await {
        Ok((name, _)) => Some(name).filter(|name| !name.trim().is_empty()),
        Err(error) => {
            tracing::info!(%error, "Slack 工作区名字读不到，卡片只写账号名");
            None
        }
    }
}

/// 登录失效后回给请求的错误：Friend 的话；借来的凭据同 Friend 回暂时连不上并说下一步。
fn expired_error(reason: ReauthReason, borrowed: bool) -> RpcError {
    if borrowed {
        return rpc_error(ErrorKind::Disconnected, BORROWED_EXPIRED);
    }
    Error {
        kind: ErrorKind::AuthExpired,
        message: error_text(reason).into(),
        platform_code: None,
        reauth: Some(reason),
        retry_after_secs: None,
    }
    .rpc()
}

/// 在门里换新没成。
enum Failure {
    /// 临时故障：授权状态不变，稍后再试。
    Transient(Error),
    /// 平台拒绝：要用户处理。
    Rejected(ReauthReason),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_long_list_of_missing_scopes_is_cut_to_fit_one_line() {
        let few = vec!["team:read".to_owned(), "usergroups:read".to_owned()];
        assert_eq!(
            missing_scopes_text(&few),
            "缺少权限 team:read、usergroups:read：点重新登录补上授权"
        );
        let many: Vec<String> = crate::USER_SCOPES.iter().map(|s| (*s).to_owned()).collect();
        let text = missing_scopes_text(&many);
        assert!(text.chars().count() <= hhh_im_protocol::budget::NOTICE_MAX_CHARS);
        assert!(text.starts_with("缺少权限 channels:history、"), "{text}");
        assert!(
            text.ends_with(&format!(" 等 {} 项：点重新登录补上授权", many.len())),
            "{text}"
        );
    }
}
