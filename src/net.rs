//! 网络层：本插件出站连接的唯一出口（从 HHH 的 hhh-net 照搬，一行不改握手配置）。
//!
//! 插件所有向外的连接——Slack Web API、授权中继、文件、头像与表情图下载——都用 [`client_builder`]
//! （下载用 [`download_client_builder`]）建客户端。本模块管“连接怎么建出去”：
//! TLS 用哪套加密实现、握手长什么样，以及网络错误的原因怎么写成一行（[`network_cause`]）；
//! “请求怎么发”——超时、重试、鉴权、流式读取与解析——仍归 Slack 适配器自己。
//!
//! # 为什么要统一
//!
//! 握手请求（ClientHello）长什么样，决定了客户端在网络上“像谁”。主流浏览器与各语言
//! 运行时都已优先抗量子密钥交换 X25519MLKEM768，并同时带传统 X25519：请求约 1.5 KB，
//! 超过一个 TCP 包。只看第一个包的 SNI 过滤设备读不全这种请求，会放行；而老式的小握手
//! （约 240 字节）整包可见，会被单独掐断。2026-09-28 实测飞书就是这样（HHH 仓库的契约
//! `doc/product-contracts/hhh/network-tls/contract-v1.md`）。所以全程序只用一套配置：
//!
//! - 加密后端 aws-lc-rs，默认顺序优先 X25519MLKEM768；服务器不支持时直接用同时带上的
//!   X25519，不多一次往返。
//! - **不记住服务器选过的密钥交换组**。rustls 默认会记住（下次只带那一个组的份额），
//!   对不支持抗量子的服务器，第二次起握手就退回小请求、又被拦；这里换成只缓存会话票据的
//!   会话缓存，每次新建连接都带抗量子份额。
//! - 其余与 reqwest 自带的 rustls 配置一致：TLS 1.3 与 1.2、内置 webpki 根证书、SNI、
//!   ALPN 只报 `http/1.1`（本插件的 reqwest 没开 HTTP/2）。
//!
//! # Examples
//!
//! ```
//! let client = hhh_im_slack::net::client_builder()
//!     .timeout(std::time::Duration::from_secs(30))
//!     .build()
//!     .expect("统一入口建出的客户端总能建成");
//! # drop(client);
//! ```

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use rustls::NamedGroup;
use rustls::client::{
    ClientSessionMemoryCache, ClientSessionStore, Resumption, Tls12ClientSessionValue,
    Tls13ClientSessionValue,
};
use rustls::pki_types::ServerName;

/// 会话缓存记多少个服务器（与 rustls 默认 `Resumption::in_memory_sessions(256)` 相同）。
const SESSION_CACHE_SERVERS: usize = 256;

/// 建一个出站 HTTP 客户端的起点：已装好全进程共用的 TLS 配置。
///
/// 调用方在它上面继续设超时、重定向、请求头等，再 `build()`。不要在它上面再调
/// reqwest 的 TLS 调节方法（最低版本、额外根证书、忽略证书等）：预置配置下它们不生效。
pub fn client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder().use_preconfigured_tls(tls_config().clone())
}

/// 下载用 HTTP 客户端的连接超时：10 秒（与 HHH 的 hhh-download 相同）。
pub const DOWNLOAD_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// 下载用 HTTP 客户端的 TCP 保活间隔：15 秒，让系统及早发现对端已经不在的连接（与 hhh-download 相同）。
pub const DOWNLOAD_TCP_KEEPALIVE: Duration = Duration::from_secs(15);

/// 下载用 HTTP 客户端的起点：统一 TLS（[`client_builder`]）加连接超时与保活（照搬 hhh-download 的
/// `client_builder`，插件只用到这几行，没有搬它的下载引擎）。
///
/// **不设总超时**：大文件慢慢下不会被一刀切断；等首个响应与读停滞的超时归调用方（宿主的下载层）。
/// 调用方在它上面继续设重定向策略等，再 `build()`。
///
/// # Examples
///
/// ```
/// let client = hhh_im_slack::net::download_client_builder()
///     .redirect(reqwest::redirect::Policy::none())
///     .build()
///     .expect("统一入口建出的客户端总能建成");
/// # drop(client);
/// ```
pub fn download_client_builder() -> reqwest::ClientBuilder {
    client_builder()
        .connect_timeout(DOWNLOAD_CONNECT_TIMEOUT)
        .tcp_keepalive(DOWNLOAD_TCP_KEEPALIVE)
}

/// 把网络错误连同底层原因拼成一行，例如
/// `error sending request ← client error (Connect) ← 远程主机强迫关闭了一个现有的连接。 (os error 10054)`。
///
/// 只打最外层常常是句废话（“发送失败”），真正的原因（连接被重置、握手失败、系统错误码）
/// 埋在 `source` 链深处，排障要全链。先去掉请求地址：地址里可能带会话编号、分页游标、
/// 签名参数；令牌只在请求头里，不会出现。
///
/// # Examples
///
/// ```
/// let error = hhh_im_slack::net::client_builder()
///     .build()
///     .unwrap()
///     .get("http://exa mple.com/?page_token=secret")
///     .build()
///     .unwrap_err();
/// let cause = hhh_im_slack::net::network_cause(error);
/// assert!(cause.starts_with("builder error ← "), "{cause}");
/// assert!(!cause.contains("secret"));
/// ```
pub fn network_cause(error: reqwest::Error) -> String {
    use std::error::Error as _;
    let error = error.without_url();
    let mut parts = vec![error.to_string()];
    let mut source = error.source();
    while let Some(cause) = source {
        parts.push(cause.to_string());
        source = cause.source();
    }
    parts.join(" ← ")
}

/// 全进程共用的 TLS 配置；第一次用时建好，之后每个客户端拿它的一份拷贝。
///
/// 拷贝共享同一个会话缓存（内部是 `Arc`），不同客户端连同一服务器也能复用票据。
fn tls_config() -> &'static rustls::ClientConfig {
    static CONFIG: OnceLock<rustls::ClientConfig> = OnceLock::new();
    CONFIG.get_or_init(|| {
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        config_with(
            roots,
            Arc::new(TicketsOnly(ClientSessionMemoryCache::new(
                SESSION_CACHE_SERVERS,
            ))),
        )
    })
}

/// 按给定根证书与会话缓存组装配置；生产配置与测试走同一条路。
fn config_with(
    roots: rustls::RootCertStore,
    sessions: Arc<dyn ClientSessionStore>,
) -> rustls::ClientConfig {
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let mut config = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(rustls::ALL_VERSIONS)
        // 不变量：aws-lc-rs 默认 provider 自带 TLS 1.2 与 1.3 的密码套件，这一步只会在
        // 依赖本身坏掉时失败——属于“程序自身已坏”，由单测 `production_config_*` 锁住。
        .expect("aws-lc-rs 默认 provider 应支持 TLS 1.2 与 1.3")
        .with_root_certificates(roots)
        .with_no_client_auth();
    // 与 reqwest 在未开 HTTP/2 时的做法一致。
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    config.resumption = Resumption::store(sessions);
    config
}

/// 只缓存会话票据、不记住服务器选过的密钥交换组的会话缓存。
///
/// rustls 用“记住的组”决定下次握手的初始密钥份额；不记，就总按默认顺序先带抗量子份额。
#[derive(Debug)]
struct TicketsOnly(ClientSessionMemoryCache);

impl ClientSessionStore for TicketsOnly {
    fn set_kx_hint(&self, _: ServerName<'static>, _: NamedGroup) {}

    fn kx_hint(&self, _: &ServerName<'_>) -> Option<NamedGroup> {
        None
    }

    fn set_tls12_session(&self, server_name: ServerName<'static>, value: Tls12ClientSessionValue) {
        self.0.set_tls12_session(server_name, value);
    }

    fn tls12_session(&self, server_name: &ServerName<'_>) -> Option<Tls12ClientSessionValue> {
        self.0.tls12_session(server_name)
    }

    fn remove_tls12_session(&self, server_name: &ServerName<'static>) {
        self.0.remove_tls12_session(server_name);
    }

    fn insert_tls13_ticket(
        &self,
        server_name: ServerName<'static>,
        value: Tls13ClientSessionValue,
    ) {
        self.0.insert_tls13_ticket(server_name, value);
    }

    fn take_tls13_ticket(
        &self,
        server_name: &ServerName<'static>,
    ) -> Option<Tls13ClientSessionValue> {
        self.0.take_tls13_ticket(server_name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 抗量子份额 1216 字节；带上它的 ClientHello 一定超过这个长度，不带的远小于它。
    const PQ_CLIENT_HELLO_MIN: usize = 1200;

    fn server() -> ServerName<'static> {
        ServerName::try_from("open.feishu.cn").expect("合法域名")
    }

    /// 按给定配置开一个连接，取它发出的第一个握手请求（ClientHello）的字节数。
    fn client_hello_len(config: &rustls::ClientConfig) -> usize {
        let mut connection =
            rustls::ClientConnection::new(Arc::new(config.clone()), server()).expect("配置可用");
        let mut bytes = Vec::new();
        connection.write_tls(&mut bytes).expect("写入内存");
        bytes.len()
    }

    #[test]
    fn production_config_prefers_post_quantum_and_keeps_x25519() {
        let groups: Vec<NamedGroup> = tls_config()
            .crypto_provider()
            .kx_groups
            .iter()
            .map(|group| group.name())
            .collect();
        assert_eq!(groups.first(), Some(&NamedGroup::X25519MLKEM768));
        assert!(groups.contains(&NamedGroup::X25519));
        assert!(client_hello_len(tls_config()) > PQ_CLIENT_HELLO_MIN);
    }

    #[test]
    fn production_config_matches_reqwest_defaults() {
        let config = tls_config();
        assert_eq!(config.alpn_protocols, vec![b"http/1.1".to_vec()]);
        assert!(config.enable_sni);
        assert!(!config.client_auth_cert_resolver.has_certs());
        // 协议版本传的是与 reqwest 相同的 `rustls::ALL_VERSIONS`（TLS 1.3 与 1.2）；
        // rustls 不公开已启用的版本，这里不重复断言。
    }

    /// 对照组：rustls 默认缓存记下服务器选了 X25519 后，下次握手只带 X25519，请求变小——
    /// 这就是 2026-09-28 在飞书上第二次起又被拦的原因。
    #[test]
    fn default_rustls_cache_shrinks_the_next_hello_after_remembering_x25519() {
        let cache = Arc::new(ClientSessionMemoryCache::new(SESSION_CACHE_SERVERS));
        let config = config_with(rustls::RootCertStore::empty(), cache.clone());
        assert!(client_hello_len(&config) > PQ_CLIENT_HELLO_MIN);
        cache.set_kx_hint(server(), NamedGroup::X25519);
        assert!(client_hello_len(&config) < PQ_CLIENT_HELLO_MIN);
    }

    #[test]
    fn tickets_only_cache_keeps_every_hello_post_quantum() {
        let cache = Arc::new(TicketsOnly(ClientSessionMemoryCache::new(
            SESSION_CACHE_SERVERS,
        )));
        let config = config_with(rustls::RootCertStore::empty(), cache.clone());
        cache.set_kx_hint(server(), NamedGroup::X25519);
        assert_eq!(cache.kx_hint(&server()), None);
        for _ in 0..3 {
            assert!(client_hello_len(&config) > PQ_CLIENT_HELLO_MIN);
        }
    }

    #[test]
    fn client_builder_builds_clients_repeatedly() {
        for _ in 0..3 {
            client_builder()
                .build()
                .expect("统一入口建出的客户端总能建成");
        }
    }

    /// 本机一个刚关掉的端口：得到真实的“连接被拒绝”，看原因链是否完整、是否带出地址。
    #[tokio::test]
    async fn network_cause_keeps_the_chain_but_not_the_address() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let error = client_builder()
            .build()
            .unwrap()
            .get(format!(
                "http://127.0.0.1:{port}/open-apis/im/v1/messages?page_token=secret-cursor"
            ))
            .send()
            .await
            .expect_err("端口已关闭");
        let cause = network_cause(error);
        assert!(cause.contains(" ← "), "应带底层原因：{cause}");
        assert!(
            !cause.contains("secret-cursor") && !cause.contains("open-apis"),
            "不应带请求地址：{cause}"
        );
    }
}
