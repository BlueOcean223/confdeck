//! confdeck 自己发出去的请求：测连通、拉模型列表、下 models.dev，以后的余额、同步、更新也从这里走。
//! 设置页选的出站代理在这里生效（decisions「功能范围」：默认跟随系统代理，可以单独设置）。
//! agent 自己发的请求不归这里管，它们照自己的环境变量走。
//!
//! 全是阻塞调用，界面放在后台线程上跑。每次操作现建一个客户端（[`Http::new`]）：「跟随系统」时代理是
//! 建客户端那一刻读的，confdeck 开着的时候在 Clash 里开关系统代理，下一次操作就跟上了。

use std::io;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// 设置页「出站代理」的三种选法。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProxyMode {
    /// 环境变量（`HTTPS_PROXY` 等）优先，没有就用系统设置里的 HTTP / HTTPS 代理。
    #[default]
    System,
    /// 直连。
    Direct,
    /// 用 [`Proxy::url`]。
    Custom,
}

/// 出站代理，记在 `state.json` 里（只属于这台设备：代理多半是本机的端口）。
///
/// 选回「跟随系统」时自定义的地址留着，再选「自定义」不用重填。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Proxy {
    #[serde(default)]
    pub mode: ProxyMode,
    /// `http://`、`https://`、`socks5://`、`socks5h://` 开头，可以带用户名密码；没写协议按 `http://`。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub url: String,
}

impl Proxy {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// 自定义的地址整理成能用的样子：去空白，没写协议补 `http://`，去掉末尾的 `/`。
    pub fn normalized_url(url: &str) -> String {
        let url = url.trim();
        if url.is_empty() {
            return String::new();
        }
        let (scheme, rest) = url.split_once("://").unwrap_or(("http", url));
        format!("{scheme}://{}", rest.trim_end_matches('/'))
    }

    /// 自定义的地址能不能用：协议对、有主机名。空的也不行。
    pub fn check_url(url: &str) -> Result<(), NetError> {
        let url = Self::normalized_url(url);
        let bad = || NetError::BadProxy { url: url.clone() };
        let (scheme, rest) = url.split_once("://").ok_or_else(bad)?;
        if !matches!(
            scheme.to_ascii_lowercase().as_str(),
            "http" | "https" | "socks5" | "socks5h"
        ) {
            return Err(bad());
        }
        let authority = rest.split('/').next().unwrap_or_default();
        let host = authority.rsplit('@').next().unwrap_or_default();
        if host.is_empty() || host.starts_with(':') {
            return Err(bad());
        }
        reqwest::Proxy::all(&url).map(drop).map_err(|_| bad())
    }
}

/// 「跟随系统」现在会走哪个代理（设置页显示用）：和客户端用的是同一套规则（环境变量优先，再看系统设置），
/// 去掉了用户名密码。没有代理是 `None`。
pub fn system_proxy() -> Option<String> {
    let matcher = hyper_util::client::proxy::matcher::Matcher::from_system();
    let target: http::Uri = "https://models.dev/".parse().expect("static uri");
    let intercept = matcher.intercept(&target)?;
    let uri = intercept.uri();
    let authority = uri.authority()?.as_str();
    let host = authority.rsplit('@').next().unwrap_or(authority);
    Some(match uri.scheme_str() {
        Some(scheme) => format!("{scheme}://{host}"),
        None => host.to_string(),
    })
}

/// 连不上的几种原因，界面按它翻译。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unreachable {
    /// 域名解析不了。
    Dns,
    /// 对方拒绝连接（端口没开）。
    Refused,
    /// TLS 握手失败、证书不对。
    Tls,
    /// 代理连不上，或代理拒绝转发。
    Proxy,
    /// 连上了又被断开（握手时、发请求时）。开着 TUN / fake-IP 的代理软件时，不存在的域名也是这样：
    /// 本机的 DNS 什么都解析，连过去再被掐掉。macOS 的 TLS 报 `-9806 connection closed via error`。
    Closed,
    /// 别的，带原话（多半是英文的系统报错）。
    Other(String),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NetError {
    #[error("代理地址不对：{url}")]
    BadProxy { url: String },
    #[error("地址不对：{url}")]
    BadUrl { url: String },
    #[error("超时")]
    Timeout,
    #[error("连不上：{0:?}")]
    Unreachable(Unreachable),
    /// 对方回了 2xx 以外的状态码。`body` 是截短的响应内容，Key 已经抹掉。
    #[error("HTTP {status}：{body}")]
    Status { status: u16, body: String },
    #[error("返回的内容解析不了：{detail}")]
    Parse { detail: String },
}

/// 一次操作用的客户端，按设置的代理建。
pub struct Http {
    client: reqwest::blocking::Client,
    /// 走了代理：连接被拒绝说明是代理没开（Clash 退了、系统代理还指着 7890 是常见情况）。
    via_proxy: bool,
}

/// 测连通：建连接（DNS、TCP、TLS、代理）的那一次不算，第二次复用连接，量的是往返。
const REACH_TIMEOUT: Duration = Duration::from_secs(8);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(8);
/// 错误信息里留多少响应内容。
const BODY_IN_ERROR: usize = 300;

impl Http {
    pub fn new(proxy: &Proxy) -> Result<Self, NetError> {
        let builder = reqwest::blocking::Client::builder()
            .user_agent(concat!("confdeck/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(Duration::from_secs(60));
        let via_proxy = match proxy.mode {
            ProxyMode::System => system_proxy().is_some(),
            ProxyMode::Direct => false,
            ProxyMode::Custom => true,
        };
        let builder = match proxy.mode {
            // 不设代理时 reqwest 自己按环境变量和系统设置走。
            ProxyMode::System => builder,
            ProxyMode::Direct => builder.no_proxy(),
            ProxyMode::Custom => {
                Proxy::check_url(&proxy.url)?;
                let url = Proxy::normalized_url(&proxy.url);
                let custom = reqwest::Proxy::all(&url)
                    .map_err(|_| NetError::BadProxy { url: url.clone() })?;
                builder.proxy(custom)
            }
        };
        let client = builder
            .build()
            .map_err(|err| NetError::Unreachable(Unreachable::Other(innermost(&err))))?;
        Ok(Self { client, via_proxy })
    }

    fn error(&self, err: reqwest::Error) -> NetError {
        match NetError::from(err) {
            NetError::Unreachable(Unreachable::Refused) if self.via_proxy => {
                NetError::Unreachable(Unreachable::Proxy)
            }
            other => other,
        }
    }

    /// 地址能不能连上、往返多久：收到任何 HTTP 响应（404、401 也算）就是连得上，只有网络层的错误算连不上。
    /// 不发模型请求，不读内容（用 HEAD，连接留着给第二次复用）。
    pub fn reach(&self, url: &str) -> Result<Duration, NetError> {
        let first = Instant::now();
        self.head(url)?;
        let cold = first.elapsed();
        let second = Instant::now();
        Ok(match self.head(url) {
            Ok(()) => second.elapsed(),
            Err(_) => cold,
        })
    }

    fn head(&self, url: &str) -> Result<(), NetError> {
        check_url(url)?;
        self.client
            .head(url)
            .timeout(REACH_TIMEOUT)
            .send()
            .map(drop)
            .map_err(|err| self.error(err))
    }

    /// GET，返回 2xx 的响应内容。其余状态码报 [`NetError::Status`]（内容截短，`secrets` 里的字符串抹掉）。
    pub(crate) fn get(
        &self,
        url: &str,
        headers: &[(&str, &str)],
        timeout: Duration,
        secrets: &[&str],
    ) -> Result<Vec<u8>, NetError> {
        self.send(false, url, headers, None, timeout, secrets)
    }

    /// GET 或 POST（`post`），带上 `body`。返回同 [`Http::get`]。
    pub(crate) fn send(
        &self,
        post: bool,
        url: &str,
        headers: &[(&str, &str)],
        body: Option<&str>,
        timeout: Duration,
        secrets: &[&str],
    ) -> Result<Vec<u8>, NetError> {
        check_url(url)?;
        let mut request = if post {
            self.client.post(url)
        } else {
            self.client.get(url)
        }
        .timeout(timeout);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        if let Some(body) = body {
            request = request.body(body.to_string());
        }
        let response = request.send().map_err(|err| self.error(err))?;
        let status = response.status();
        let body = response.bytes().map_err(|err| self.error(err))?;
        if status.is_success() {
            Ok(body.to_vec())
        } else {
            Err(NetError::Status {
                status: status.as_u16(),
                body: redact(&String::from_utf8_lossy(&body), secrets),
            })
        }
    }
}

fn check_url(url: &str) -> Result<(), NetError> {
    match crate::provider::host(url) {
        Some(host) if !host.is_empty() => Ok(()),
        _ => Err(NetError::BadUrl {
            url: url.to_string(),
        }),
    }
}

/// 抹掉 Key，截短，压成一行。
fn redact(body: &str, secrets: &[&str]) -> String {
    let mut text = body.split_whitespace().collect::<Vec<_>>().join(" ");
    for secret in secrets.iter().filter(|secret| secret.len() >= 4) {
        text = text.replace(secret, "***");
    }
    if text.chars().count() > BODY_IN_ERROR {
        text = text.chars().take(BODY_IN_ERROR).collect::<String>() + "…";
    }
    text
}

impl From<reqwest::Error> for NetError {
    fn from(err: reqwest::Error) -> Self {
        if err.is_timeout() {
            return Self::Timeout;
        }
        if err.is_builder() {
            return Self::BadUrl {
                url: err.url().map(|url| url.to_string()).unwrap_or_default(),
            };
        }
        let detail = innermost(&err);
        let lower = chain(&err).to_ascii_lowercase();
        let kind = if source_kind(&err) == Some(io::ErrorKind::TimedOut) {
            return Self::Timeout;
        } else if lower.contains("tunnel") || lower.contains("proxy") || lower.contains("socks") {
            Unreachable::Proxy
        } else if source_kind(&err) == Some(io::ErrorKind::ConnectionRefused)
            || lower.contains("connection refused")
        {
            Unreachable::Refused
        } else if lower.contains("dns") || lower.contains("lookup") || lower.contains("nodename") {
            Unreachable::Dns
        } else if matches!(
            source_kind(&err),
            Some(
                io::ErrorKind::ConnectionReset
                    | io::ErrorKind::ConnectionAborted
                    | io::ErrorKind::UnexpectedEof
            )
        ) || lower.contains("connection closed")
            || lower.contains("connection reset")
        {
            Unreachable::Closed
        } else if lower.contains("tls")
            || lower.contains("certificate")
            || lower.contains("handshake")
            || lower.contains("ssl")
        {
            Unreachable::Tls
        } else {
            Unreachable::Other(detail)
        };
        Self::Unreachable(kind)
    }
}

/// 错误链最里面那一层的原话。
fn innermost(err: &(dyn std::error::Error + 'static)) -> String {
    let mut current = err;
    while let Some(source) = current.source() {
        current = source;
    }
    current.to_string()
}

/// 整条错误链，用来认是哪一种。
fn chain(err: &(dyn std::error::Error + 'static)) -> String {
    let mut text = err.to_string();
    let mut current = err.source();
    while let Some(source) = current {
        text.push_str(" / ");
        text.push_str(&source.to_string());
        current = source.source();
    }
    text
}

fn source_kind(err: &(dyn std::error::Error + 'static)) -> Option<io::ErrorKind> {
    let mut current = err.source();
    while let Some(source) = current {
        if let Some(io) = source.downcast_ref::<io::Error>() {
            return Some(io.kind());
        }
        current = source.source();
    }
    None
}

#[cfg(test)]
pub(crate) mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::thread;

    use super::*;

    /// 本机上的一次性 HTTP 服务：每个连接读一个请求，记下请求行和头，按 `reply` 回。
    pub(crate) struct Server {
        pub url: String,
        pub requests: mpsc::Receiver<String>,
    }

    pub(crate) fn serve(reply: impl Fn(&str) -> (u16, String) + Send + 'static) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (tx, requests) = mpsc::channel();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let tx = tx.clone();
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                // 读到头结束（GET、HEAD 没有请求体）。一个连接上可能来好几个请求（keep-alive）。
                while let Ok(n @ 1..) = stream.read(&mut chunk) {
                    buf.extend_from_slice(&chunk[..n]);
                    while let Some(end) = find(&buf, b"\r\n\r\n") {
                        // 有请求体的（POST）连着请求体一起交给 `reply`，没收全就等下一段。
                        let head = String::from_utf8_lossy(&buf[..end]).to_string();
                        let length = head
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().ok())?
                            })
                            .unwrap_or(0);
                        if buf.len() < end + 4 + length {
                            break;
                        }
                        let request = String::from_utf8_lossy(&buf[..end + 4 + length]).to_string();
                        buf.drain(..end + 4 + length);
                        let (status, body) = reply(&request);
                        let head = request.starts_with("HEAD ");
                        let response = format!(
                            "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
                            body.len(),
                            if head { "" } else { body.as_str() }
                        );
                        tx.send(request).ok();
                        if stream.write_all(response.as_bytes()).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        Server { url, requests }
    }

    fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack.windows(needle.len()).position(|w| w == needle)
    }

    pub(crate) fn direct() -> Http {
        Http::new(&Proxy {
            mode: ProxyMode::Direct,
            url: String::new(),
        })
        .unwrap()
    }

    #[test]
    fn proxy_urls_get_a_scheme_and_bad_ones_are_rejected() {
        assert_eq!(
            Proxy::normalized_url(" 127.0.0.1:7890/ "),
            "http://127.0.0.1:7890"
        );
        assert_eq!(
            Proxy::normalized_url("socks5://u:p@host:1080"),
            "socks5://u:p@host:1080"
        );
        for good in [
            "127.0.0.1:7890",
            "http://127.0.0.1:7890",
            "socks5h://localhost:1080",
            "https://u:p@proxy.example:443",
        ] {
            assert_eq!(Proxy::check_url(good), Ok(()), "{good}");
        }
        for bad in ["", "ftp://host:21", "http://", "http://:7890"] {
            assert!(Proxy::check_url(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn proxy_setting_round_trips_and_the_default_is_system() {
        let proxy: Proxy = serde_json::from_str("{}").unwrap();
        assert_eq!(proxy.mode, ProxyMode::System);
        assert!(proxy.is_default());
        let custom = Proxy {
            mode: ProxyMode::Custom,
            url: "http://127.0.0.1:7890".into(),
        };
        let text = serde_json::to_string(&custom).unwrap();
        assert_eq!(text, r#"{"mode":"custom","url":"http://127.0.0.1:7890"}"#);
        assert_eq!(serde_json::from_str::<Proxy>(&text).unwrap(), custom);
    }

    #[test]
    fn reach_counts_any_http_response_and_reports_refused_ports() {
        let server = serve(|_| (404, "{}".into()));
        let http = direct();
        assert!(http.reach(&format!("{}/v1", server.url)).is_ok());
        // HEAD，两次。
        let first = server.requests.recv().unwrap();
        assert!(first.starts_with("HEAD /v1 HTTP/1.1"), "{first}");
        assert!(server.requests.recv().unwrap().starts_with("HEAD /v1"));

        // 拿一个刚释放的端口：没人在听。
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        assert_eq!(
            http.reach(&format!("http://127.0.0.1:{port}")),
            Err(NetError::Unreachable(Unreachable::Refused))
        );
        assert!(matches!(
            http.reach("relay.example"),
            Err(NetError::BadUrl { .. })
        ));
    }

    #[test]
    fn a_connection_closed_without_an_answer_is_reported_as_dropped() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                // 读了请求，什么都不回就关掉。
                let mut chunk = [0u8; 1024];
                let _ = stream.read(&mut chunk);
            }
        });
        assert_eq!(
            direct().reach(&url),
            Err(NetError::Unreachable(Unreachable::Closed))
        );
    }

    #[test]
    fn a_dead_custom_proxy_is_a_proxy_error() {
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let http = Http::new(&Proxy {
            mode: ProxyMode::Custom,
            url: format!("127.0.0.1:{port}"),
        })
        .unwrap();
        let err = http.reach("http://relay.example").unwrap_err();
        assert!(
            matches!(err, NetError::Unreachable(Unreachable::Proxy)),
            "{err:?}"
        );
    }

    #[test]
    fn get_returns_the_body_and_hides_the_key_in_errors() {
        let server = serve(|request| {
            if request.contains("authorization: Bearer sk-good") {
                (200, r#"{"ok":true}"#.into())
            } else {
                (401, r#"{"error":"bad key sk-wrong-1234"}"#.into())
            }
        });
        let http = direct();
        let body = http
            .get(
                &server.url,
                &[("authorization", "Bearer sk-good")],
                Duration::from_secs(5),
                &["sk-good"],
            )
            .unwrap();
        assert_eq!(body, br#"{"ok":true}"#);
        assert_eq!(
            http.get(
                &server.url,
                &[("authorization", "Bearer sk-wrong-1234")],
                Duration::from_secs(5),
                &["sk-wrong-1234"],
            ),
            Err(NetError::Status {
                status: 401,
                body: r#"{"error":"bad key ***"}"#.into()
            })
        );
    }
}
