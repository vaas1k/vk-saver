use crate::clock::Clock;
use crate::export::GAP_MS;
use reqwest::Url;
use reqwest::blocking::Client;
use reqwest::header::{CONTENT_RANGE, RANGE};
use reqwest::redirect::Policy;
use std::cell::Cell;
use std::io::Read;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_millis(60_000);
const MAX_REDIRECTS: usize = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetError {
    /// The url or a redirect target fails the host rule, or the redirect cap is hit.
    Refused,
    Connect,
    Timeout,
    Other(String),
}

impl From<reqwest::Error> for NetError {
    fn from(e: reqwest::Error) -> NetError {
        if e.is_redirect() {
            NetError::Refused
        } else if e.is_timeout() {
            NetError::Timeout
        } else if e.is_connect() {
            NetError::Connect
        } else {
            // Signed CDN urls must not reach stored error texts.
            NetError::Other(e.without_url().to_string())
        }
    }
}

pub trait Cdn {
    fn get(&self, url: &str, from: u64) -> Result<CdnResponse, NetError>;
}

/// Start-to-start gap of at least `GAP_MS` before every `get`; redirect hops inside
/// `inner` are not separate requests here.
pub struct Paced<'a> {
    pub inner: &'a dyn Cdn,
    pub clock: &'a dyn Clock,
    pub last: Cell<Option<u64>>,
}

impl Cdn for Paced<'_> {
    fn get(&self, url: &str, from: u64) -> Result<CdnResponse, NetError> {
        if let Some(t) = self.last.get() {
            let now = self.clock.now_ms();
            if now < t + GAP_MS {
                self.clock.sleep_ms(t + GAP_MS - now);
            }
        }
        self.last.set(Some(self.clock.now_ms()));
        self.inner.get(url, from)
    }
}

pub struct CdnResponse {
    pub status: u16,
    pub content_length: Option<u64>,
    /// First byte position from Content-Range.
    pub start: Option<u64>,
    pub total: Option<u64>,
    pub body: Box<dyn Read>,
}

/// The only hosts the export links media to (spec:135).
pub fn allowed(url: &str) -> bool {
    Url::parse(url).is_ok_and(|u| url_allowed(&u, false))
}

/// https, no userinfo, no explicit port, host `^[a-z0-9-]+\.userapi\.com$` (Url lowercases hosts).
/// `loopback` (tests only) also admits `http://127.0.0.1:<port>`.
fn url_allowed(u: &Url, loopback: bool) -> bool {
    if !u.username().is_empty() || u.password().is_some() {
        return false;
    }
    if loopback && u.scheme() == "http" && u.host_str() == Some("127.0.0.1") {
        return u.port().is_some();
    }
    let Some(label) = u.host_str().and_then(|h| h.strip_suffix(".userapi.com")) else {
        return false;
    };
    u.scheme() == "https"
        && u.port().is_none()
        && !label.is_empty()
        && label
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// No cookie store (the `cookies` feature is off), no Referer, only User-Agent added.
pub struct HttpCdn {
    client: Client,
    loopback: bool,
}

impl HttpCdn {
    pub fn new(version: &str) -> HttpCdn {
        HttpCdn::with_policy(version, false)
    }

    #[cfg(test)]
    fn loopback() -> HttpCdn {
        HttpCdn::with_policy("0.0.0-test", true)
    }

    fn with_policy(version: &str, loopback: bool) -> HttpCdn {
        let policy = Policy::custom(move |a| {
            if a.previous().len() > MAX_REDIRECTS {
                a.error("too many redirects")
            } else if !url_allowed(a.url(), loopback) {
                a.error("redirect refused")
            } else {
                a.follow()
            }
        });
        let client = Client::builder()
            .timeout(TIMEOUT)
            .redirect(policy)
            .referer(false)
            .user_agent(format!("vk-saver/{version}"))
            .build()
            .expect("TLS backend unavailable");
        HttpCdn { client, loopback }
    }
}

impl Cdn for HttpCdn {
    fn get(&self, url: &str, from: u64) -> Result<CdnResponse, NetError> {
        if !Url::parse(url).is_ok_and(|u| url_allowed(&u, self.loopback)) {
            return Err(NetError::Refused);
        }
        let mut req = self.client.get(url);
        if from > 0 {
            req = req.header(RANGE, format!("bytes={from}-"));
        }
        let resp = req.send()?;
        let range = resp
            .headers()
            .get(CONTENT_RANGE)
            .and_then(|v| v.to_str().ok());
        let total = range
            .and_then(|v| v.rsplit_once('/'))
            .and_then(|(_, t)| t.trim().parse().ok());
        let start = range
            .and_then(|v| v.trim().strip_prefix("bytes "))
            .and_then(|v| v.split_once('-'))
            .and_then(|(s, _)| s.trim().parse().ok());
        Ok(CdnResponse {
            status: resp.status().as_u16(),
            content_length: resp.content_length(),
            start,
            total,
            body: Box::new(resp),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::download::{DlError, MIN_FREE, Saved, download};
    use crate::testing::{FakeSpace, NoProgress, tmpdir};
    use std::io::Write;
    use std::net::TcpListener;
    use std::sync::atomic::AtomicBool;
    use std::thread::JoinHandle;

    /// Serves one canned response on 127.0.0.1 and returns the raw request (head, body).
    fn serve_once(response: &'static str) -> (String, JoinHandle<(String, String)>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 1024];
            let head_end = loop {
                let n = sock.read(&mut chunk).unwrap();
                assert!(n > 0, "client closed before end of head");
                buf.extend_from_slice(&chunk[..n]);
                if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    break i + 4;
                }
            };
            let head = String::from_utf8(buf[..head_end].to_vec()).unwrap();
            let len = head
                .lines()
                .find_map(|l| {
                    let (k, v) = l.split_once(':')?;
                    k.eq_ignore_ascii_case("content-length")
                        .then(|| v.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            while buf.len() < head_end + len {
                let n = sock.read(&mut chunk).unwrap();
                assert!(n > 0, "client closed before end of body");
                buf.extend_from_slice(&chunk[..n]);
            }
            let body = String::from_utf8(buf[head_end..].to_vec()).unwrap();
            // The client may hang up first when it refuses the reply.
            let _ = sock.write_all(response.as_bytes());
            (head, body)
        });
        (base, handle)
    }

    #[test]
    fn host_rule() {
        for ok in [
            "https://sun9-1.userapi.com/impg/a.jpg?size=1x1",
            "https://psv4.userapi.com/s/v.ogg",
            "https://SUN9-1.USERAPI.COM/x",
        ] {
            assert!(allowed(ok), "{ok}");
        }
        for bad in [
            "http://sun9-1.userapi.com/impg/a.jpg",
            "https://userapi.com/a.jpg",
            "https://a.b.userapi.com/a.jpg",
            "https://sun9-1.userapi.com.evil.invalid/a.jpg",
            "https://x@sun9-1.userapi.com/a.jpg",
            "https://x:y@sun9-1.userapi.com/a.jpg",
            "https://sun9-1.userapi.com:8443/a.jpg",
            "https://sun9-1.userapi.com./a.jpg",
            "https://vk.ru/photo1_2",
            "https://api.vk.com/x",
            "ftp://sun9-1.userapi.com/a.jpg",
            "http://127.0.0.1:1/p.jpg",
            "not a url",
        ] {
            assert!(!allowed(bad), "{bad}");
        }
        let lb = |s: &str| url_allowed(&Url::parse(s).unwrap(), true);
        assert!(lb("http://127.0.0.1:1/p.jpg"));
        assert!(lb("https://sun9-1.userapi.com/a.jpg"));
        assert!(!lb("http://127.0.0.1/p.jpg"));
        assert!(!lb("http://x@127.0.0.1:1/p.jpg"));
        assert!(!lb("http://localhost:1/p.jpg"));
        assert!(!lb("http://sun9-1.userapi.com/a.jpg"));
    }

    #[test]
    fn request_headers_are_minimal() {
        let (base, server) =
            serve_once("HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\nabcd");
        HttpCdn::loopback()
            .get(&format!("{base}/p1.jpg"), 0)
            .unwrap();
        let (head, _) = server.join().unwrap();
        let names: Vec<String> = head
            .lines()
            .skip(1)
            .filter_map(|l| l.split_once(':'))
            .map(|(k, _)| k.to_ascii_lowercase())
            .collect();
        for name in &names {
            assert!(
                ["host", "user-agent", "accept"].contains(&name.as_str()),
                "{head}"
            );
        }
        assert!(
            head.contains("\r\nuser-agent: vk-saver/0.0.0-test\r\n"),
            "{head}"
        );
    }

    #[test]
    fn http_cdn_sends_range() {
        let (base, server) = serve_once(
            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 100-999/1000\r\nContent-Length: 4\r\nConnection: close\r\n\r\nabcd",
        );
        let mut r = HttpCdn::loopback()
            .get(&format!("{base}/p1.jpg"), 100)
            .unwrap();
        let mut body = String::new();
        r.body.read_to_string(&mut body).unwrap();
        let (head, _) = server.join().unwrap();
        assert!(head.starts_with("GET /p1.jpg HTTP/1.1\r\n"), "{head}");
        assert!(
            head.to_ascii_lowercase()
                .contains("\r\nrange: bytes=100-\r\n"),
            "{head}"
        );
        assert_eq!(r.status, 206);
        assert_eq!(r.start, Some(100));
        assert_eq!(r.total, Some(1000));
        assert_eq!(r.content_length, Some(4));
        assert_eq!(body, "abcd");
    }

    #[test]
    fn http_cdn_no_range_from_zero() {
        let (base, server) =
            serve_once("HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\nabcd");
        let r = HttpCdn::loopback()
            .get(&format!("{base}/p1.jpg"), 0)
            .unwrap();
        let (head, _) = server.join().unwrap();
        assert!(!head.to_ascii_lowercase().contains("\r\nrange:"), "{head}");
        assert_eq!(r.status, 200);
        assert_eq!(r.start, None);
        assert_eq!(r.total, None);
        assert_eq!(r.content_length, Some(4));
    }

    fn fetch_into(name: &str, base: &str) -> (Result<Saved, DlError>, std::path::PathBuf) {
        let root = tmpdir(name);
        let stem = root.join("p1");
        let r = download(
            &HttpCdn::loopback(),
            &FakeSpace(Ok(MIN_FREE)),
            &format!("{base}/p1.jpg"),
            &stem,
            &root,
            &AtomicBool::new(false),
            &NoProgress,
        );
        (r, root)
    }

    fn files(root: &std::path::Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(root)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn aborted_chunked_body_not_renamed() {
        let (base, server) = serve_once(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n6\r\nGIF89a\r\n",
        );
        let (r, root) = fetch_into("http-chunked-cut", &base);
        server.join().unwrap();
        assert!(matches!(r, Err(DlError::Io(_))), "{r:?}");
        assert_eq!(files(&root), ["p1.part"]);
    }

    /// Known ceiling: a body delimited by the connection close has no length to check.
    #[test]
    fn close_delimited_body_cannot_be_checked() {
        let (base, server) = serve_once("HTTP/1.1 200 OK\r\nConnection: close\r\n\r\nOggS-cut");
        let (r, root) = fetch_into("http-close-cut", &base);
        server.join().unwrap();
        assert_eq!(r.map(|s| s.bytes), Ok(8));
        assert_eq!(files(&root), ["p1.ogg"]);
    }

    #[test]
    fn content_length_over_cap() {
        let (base, server) =
            serve_once("HTTP/1.1 200 OK\r\nContent-Length: 104857601\r\nConnection: close\r\n\r\n");
        let (r, root) = fetch_into("http-over-cap", &base);
        server.join().unwrap();
        assert_eq!(r, Err(DlError::TooLarge));
        assert!(files(&root).is_empty());
    }

    #[test]
    fn html_200_is_not_media() {
        let (base, server) = serve_once(
            "HTTP/1.1 200 OK\r\nContent-Length: 15\r\nConnection: close\r\n\r\n<!DOCTYPE html>",
        );
        let (r, root) = fetch_into("http-html", &base);
        server.join().unwrap();
        assert_eq!(r, Err(DlError::NotMedia));
        assert!(files(&root).is_empty());
    }

    #[test]
    fn http_cdn_refuses_loopback_in_production() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/p1.jpg", listener.local_addr().unwrap());
        assert!(matches!(
            HttpCdn::new("0.0.0-test").get(&url, 0),
            Err(NetError::Refused)
        ));
        let accepted = listener.accept();
        assert!(
            matches!(&accepted, Err(e) if e.kind() == std::io::ErrorKind::WouldBlock),
            "refused url got a connection"
        );
    }

    fn redirect_to(location: &str) -> &'static str {
        Box::leak(
            format!("HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .into_boxed_str(),
        )
    }

    #[test]
    fn redirect_to_other_host_refused() {
        let (base, server) = serve_once(redirect_to("https://vk.ru/x"));
        let (r, _) = fetch_into("http-redirect-out", &base);
        let (head, _) = server.join().unwrap();
        assert_eq!(r, Err(DlError::Net(NetError::Refused)));
        assert!(head.starts_with("GET /p1.jpg "), "{head}");
    }

    /// `hops` redirects between loopback servers ending at `end`; returns the result and the heads served.
    fn redirect_chain(hops: usize, end: &str) -> (Result<CdnResponse, NetError>, Vec<String>) {
        let mut next = end.to_string();
        let mut servers = Vec::new();
        for _ in 0..hops {
            let (base, h) = serve_once(redirect_to(&next));
            next = format!("{base}/hop");
            servers.push(h);
        }
        let got = HttpCdn::loopback().get(&next, 0);
        let heads = servers.into_iter().map(|h| h.join().unwrap().0).collect();
        (got, heads)
    }

    #[test]
    fn redirect_chain_capped() {
        let (end, server) =
            serve_once("HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        let (got, _) = redirect_chain(3, &format!("{end}/end"));
        assert_eq!(got.map(|r| r.status).ok(), Some(200));
        server.join().unwrap();

        let last = TcpListener::bind("127.0.0.1:0").unwrap();
        last.set_nonblocking(true).unwrap();
        let (got, _) = redirect_chain(4, &format!("http://{}/end", last.local_addr().unwrap()));
        assert!(matches!(got, Err(NetError::Refused)), "{:?}", got.err());
        let accepted = last.accept();
        assert!(
            matches!(&accepted, Err(e) if e.kind() == std::io::ErrorKind::WouldBlock),
            "fourth redirect followed"
        );
    }

    #[test]
    fn redirect_hops_carry_no_credentials() {
        let hop = |to: String| -> &'static str {
            Box::leak(
                format!("HTTP/1.1 302 Found\r\nLocation: {to}\r\nSet-Cookie: s=1; Path=/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .into_boxed_str(),
            )
        };
        let (end, last) =
            serve_once("HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        let (b2, s2) = serve_once(hop(format!("{end}/end")));
        let (b1, s1) = serve_once(hop(format!("{b2}/hop")));
        let got = HttpCdn::loopback().get(&format!("{b1}/p1.jpg"), 0);
        assert_eq!(got.map(|r| r.status).ok(), Some(200));
        for s in [s1, s2, last] {
            let (head, _) = s.join().unwrap();
            for l in head.lines().skip(1) {
                let name = l.split_once(':').map(|(k, _)| k.to_ascii_lowercase());
                assert!(
                    !matches!(
                        name.as_deref(),
                        Some("cookie" | "referer" | "authorization")
                    ),
                    "{head}"
                );
            }
        }
    }

    #[test]
    fn protocol_relative_redirect_refused() {
        let other = TcpListener::bind("127.0.0.1:0").unwrap();
        other.set_nonblocking(true).unwrap();
        let port = other.local_addr().unwrap().port();
        for to in [format!("//localhost:{port}/x"), "//vk.ru/x".to_string()] {
            let (base, server) = serve_once(redirect_to(&to));
            let got = HttpCdn::loopback().get(&format!("{base}/p1.jpg"), 0);
            assert!(matches!(got, Err(NetError::Refused)), "{to}");
            server.join().unwrap();
        }
        let accepted = other.accept();
        assert!(
            matches!(&accepted, Err(e) if e.kind() == std::io::ErrorKind::WouldBlock),
            "protocol-relative redirect followed"
        );
    }
}
