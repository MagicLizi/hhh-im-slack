//! Slack IM 插件的库：Slack 适配器本体与协议服务（从 HHH 内置的 Slack 搬来，走 HHH-IM-Protocol v1）。
//!
//! - `slack`：和 Slack 说话、把 Slack 的格式直接翻成协议的统一格式（规则与内置一样）；
//! - `server`：协议的各个方法——握手、登录、续期、交接、读取与动作，协议服务本身（并发、取消、日志、资源分段、
//!   上传、请求计数）用协议包的插件运行时；
//! - `net`：统一的 TLS 握手（同 HHH 的 hhh-net）；
//! - `grant`、`error`：本人授权与错误。
//!
//! 库只消费宿主交来的凭据，不保存凭据（凭据经 `credential/save` 交给宿主存）。

mod error;
mod grant;
// 统一 TLS 握手的客户端起点（照搬 hhh-net）。
pub mod net;
// 协议服务：各方法的做法，以及登录、续期、交接。
pub mod server;
mod slack;

pub use error::{Error, ErrorKind, ReauthReason, Result};
pub use grant::AuthGrant;
pub use slack::{
    AVATAR_KEY_PATTERN, AVATAR_VARIANT_PATTERN, Caches, EMOJI_NAME_PATTERN, EMOJI_TEXT_PATTERN,
    EMOJI_TEXT_TEMPLATE, LoginRequest, SlackAccount, SlackAuth, SlackClient, SlackConfig,
    SlackLogin, SlackPendingLogin, USER_SCOPES, missing_scopes,
};
