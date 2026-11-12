//! A minimal HTTPS client on top of WinHTTP, the HTTP stack built into Windows.
//!
//! Using the system stack keeps the binary small and means TLS, certificate
//! validation and proxy settings are whatever Windows itself uses.

use std::ffi::c_void;
use std::ptr;
use std::time::Instant;

use windows::Win32::Networking::WinHttp::{
    WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_ADDREQ_FLAG_ADD, WINHTTP_DISABLE_AUTHENTICATION,
    WINHTTP_DISABLE_COOKIES, WINHTTP_DISABLE_REDIRECTS, WINHTTP_FLAG_SECURE,
    WINHTTP_OPEN_REQUEST_FLAGS, WINHTTP_OPTION_DISABLE_FEATURE, WINHTTP_QUERY_CONTENT_LENGTH,
    WINHTTP_QUERY_ETAG, WINHTTP_QUERY_FLAG_NUMBER, WINHTTP_QUERY_LAST_MODIFIED,
    WINHTTP_QUERY_LOCATION, WINHTTP_QUERY_STATUS_CODE, WinHttpAddRequestHeaders,
    WinHttpCloseHandle, WinHttpConnect, WinHttpOpen, WinHttpOpenRequest, WinHttpQueryDataAvailable,
    WinHttpQueryHeaders, WinHttpReadData, WinHttpReceiveResponse, WinHttpSendRequest,
    WinHttpSetOption, WinHttpSetTimeouts,
};
use windows::core::PCWSTR;

use crate::config::USER_AGENT;
use crate::win::wide;

const MAX_REDIRECTS: usize = 5;

/// WinHTTP error codes that simply mean "no usable internet connection".
const OFFLINE_CODES: [u32; 4] = [
    12002, // ERROR_WINHTTP_TIMEOUT
    12007, // ERROR_WINHTTP_NAME_NOT_RESOLVED
    12029, // ERROR_WINHTTP_CANNOT_CONNECT
    12030, // ERROR_WINHTTP_CONNECTION_ERROR
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpError {
    pub message: String,
    /// The machine appears to be offline; not worth recording as a failure.
    pub offline: bool,
}

impl HttpError {
    fn other(message: impl Into<String>) -> HttpError {
        HttpError {
            message: message.into(),
            offline: false,
        }
    }

    fn from_win32(what: &str, e: &windows::core::Error) -> HttpError {
        let hresult = e.code().0 as u32;
        // WinHTTP failures arrive as Win32 codes wrapped in an HRESULT.
        let code = if (hresult >> 16) & 0x1FFF == 7 {
            hresult & 0xFFFF
        } else {
            0
        };
        let reason = match code {
            12002 => "the connection timed out".to_string(),
            12007 => "the server name could not be resolved".to_string(),
            12029 => "could not connect to the server".to_string(),
            12030 => "the connection was interrupted".to_string(),
            12175 => "the secure connection could not be verified".to_string(),
            0 => crate::win::describe(e),
            other => format!("network error {other}"),
        };
        HttpError {
            message: format!("{what}: {reason}"),
            offline: OFFLINE_CODES.contains(&code),
        }
    }
}

/// The parts of a URL this client needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Url {
    pub https: bool,
    pub host: String,
    pub port: u16,
    /// Path and query, always starting with `/`.
    pub path: String,
}

fn strip_prefix_ignore_case<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let head = s.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &s[prefix.len()..])
}

impl Url {
    pub fn parse(text: &str) -> Result<Url, String> {
        let invalid = || format!("invalid URL: {text}");
        let text_trimmed = text.trim();
        let (https, rest) = if let Some(rest) = strip_prefix_ignore_case(text_trimmed, "https://") {
            (true, rest)
        } else if let Some(rest) = strip_prefix_ignore_case(text_trimmed, "http://") {
            (false, rest)
        } else {
            return Err(invalid());
        };
        let split = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let (authority, tail) = rest.split_at(split);
        let tail = tail.split('#').next().unwrap_or("");
        let path = if tail.starts_with('/') {
            tail.to_string()
        } else {
            format!("/{tail}")
        };

        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) => (host, port.parse::<u16>().map_err(|_| invalid())?),
            None => (authority, if https { 443 } else { 80 }),
        };
        let host_ok = !host.is_empty()
            && host.len() <= 253
            && host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-');
        let path_ok = path.len() <= 4096 && path.bytes().all(|b| b.is_ascii_graphic());
        if !host_ok || !path_ok || port == 0 {
            return Err(invalid());
        }
        Ok(Url {
            https,
            host: host.to_ascii_lowercase(),
            port,
            path,
        })
    }

    pub fn is_loopback(&self) -> bool {
        self.host == "127.0.0.1" || self.host == "localhost"
    }

    /// Resolves a `Location` header relative to this URL.
    pub fn resolve(&self, location: &str) -> Result<Url, String> {
        let location = location.trim();
        if location.starts_with("//") {
            let scheme = if self.https { "https:" } else { "http:" };
            Url::parse(&format!("{scheme}{location}"))
        } else if location.starts_with('/') {
            let mut next = self.clone();
            next.path = Url::parse(&format!("http://x{location}"))?.path;
            Ok(next)
        } else {
            Url::parse(location)
        }
    }
}

/// Encryption policy: HTTPS everywhere, and never a downgrade. Plain HTTP is
/// accepted only for this machine itself, which is what the test-suite uses.
pub fn check_allowed(from: Option<&Url>, to: &Url) -> Result<(), String> {
    let came_over_https = from.is_some_and(|f| f.https);
    if to.https || (to.is_loopback() && !came_over_https) {
        Ok(())
    } else {
        Err(format!("refusing an unencrypted connection to {}", to.host))
    }
}

/// Validators from an earlier download; the server answers "not modified"
/// instead of sending the file again when they still match.
#[derive(Debug, Default, Clone, Copy)]
pub struct Conditions<'a> {
    pub etag: Option<&'a str>,
    pub last_modified: Option<&'a str>,
}

impl Conditions<'_> {
    pub fn is_empty(&self) -> bool {
        self.etag.is_none() && self.last_modified.is_none()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fetched {
    NotModified,
    Body {
        data: Vec<u8>,
        etag: Option<String>,
        last_modified: Option<String>,
    },
}

/// A header value that is safe to send back or to store.
pub fn header_safe(value: &str) -> bool {
    !value.is_empty() && value.len() <= 512 && value.bytes().all(|b| (0x20..0x7f).contains(&b))
}

struct Handle(*mut c_void);

impl Handle {
    fn new(raw: *mut c_void, what: &str) -> Result<Handle, HttpError> {
        if raw.is_null() {
            Err(HttpError::from_win32(
                what,
                &windows::core::Error::from_thread(),
            ))
        } else {
            Ok(Handle(raw))
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            let _ = WinHttpCloseHandle(self.0);
        }
    }
}

enum Step {
    Redirect(String),
    Done(Fetched),
}

/// Downloads `url`, following redirects, unless `conditions` still match.
pub fn fetch(url: &str, conditions: &Conditions, max_bytes: usize) -> Result<Fetched, HttpError> {
    let mut url = Url::parse(url).map_err(HttpError::other)?;
    check_allowed(None, &url).map_err(HttpError::other)?;

    let agent = wide(USER_AGENT);
    let session = Handle::new(
        unsafe {
            WinHttpOpen(
                PCWSTR(agent.as_ptr()),
                WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
                PCWSTR::null(),
                PCWSTR::null(),
                0,
            )
        },
        "could not start a network session",
    )?;
    // Resolve, connect, send, receive - in milliseconds. These bound each single
    // step; the deadline bounds the whole download, so that a server feeding it
    // a few bytes at a time cannot keep the updater running indefinitely.
    unsafe {
        let _ = WinHttpSetTimeouts(session.0, 15_000, 15_000, 30_000, 30_000);
    }
    let deadline = Instant::now() + crate::config::download_timeout();

    for _ in 0..=MAX_REDIRECTS {
        match request(&session, &url, conditions, max_bytes, deadline)? {
            Step::Done(fetched) => return Ok(fetched),
            Step::Redirect(location) => {
                let next = url.resolve(&location).map_err(HttpError::other)?;
                check_allowed(Some(&url), &next).map_err(HttpError::other)?;
                url = next;
            }
        }
    }
    Err(HttpError::other("too many redirects"))
}

fn request(
    session: &Handle,
    url: &Url,
    conditions: &Conditions,
    max_bytes: usize,
    deadline: Instant,
) -> Result<Step, HttpError> {
    let what = format!("could not reach {}", url.host);
    let fail = |e: windows::core::Error| HttpError::from_win32(&what, &e);
    let too_slow = || HttpError::other("the download took too long and was abandoned");
    if Instant::now() >= deadline {
        return Err(too_slow());
    }
    let host = wide(&url.host);
    let verb = wide("GET");
    let path = wide(&url.path);

    unsafe {
        let connection = Handle::new(
            WinHttpConnect(session.0, PCWSTR(host.as_ptr()), url.port, 0),
            &what,
        )?;
        let flags = if url.https {
            WINHTTP_FLAG_SECURE
        } else {
            WINHTTP_OPEN_REQUEST_FLAGS(0)
        };
        let request = Handle::new(
            WinHttpOpenRequest(
                connection.0,
                PCWSTR(verb.as_ptr()),
                PCWSTR(path.as_ptr()),
                PCWSTR::null(),
                PCWSTR::null(),
                ptr::null(),
                flags,
            ),
            &what,
        )?;

        // Redirects are followed by `fetch` so every hop can be checked; cookies
        // and authentication are simply never wanted.
        let features =
            (WINHTTP_DISABLE_REDIRECTS | WINHTTP_DISABLE_COOKIES | WINHTTP_DISABLE_AUTHENTICATION)
                .to_ne_bytes();
        WinHttpSetOption(
            Some(request.0.cast_const()),
            WINHTTP_OPTION_DISABLE_FEATURE,
            Some(&features),
        )
        .map_err(fail)?;

        let mut headers = String::new();
        if let Some(etag) = conditions.etag.filter(|v| header_safe(v)) {
            headers.push_str(&format!("If-None-Match: {etag}\r\n"));
        }
        if let Some(date) = conditions.last_modified.filter(|v| header_safe(v)) {
            headers.push_str(&format!("If-Modified-Since: {date}\r\n"));
        }
        if !headers.is_empty() {
            let headers: Vec<u16> = headers.encode_utf16().collect();
            WinHttpAddRequestHeaders(request.0, &headers, WINHTTP_ADDREQ_FLAG_ADD).map_err(fail)?;
        }

        WinHttpSendRequest(request.0, None, None, 0, 0, 0).map_err(fail)?;
        WinHttpReceiveResponse(request.0, ptr::null_mut()).map_err(fail)?;

        match status_code(&request).map_err(fail)? {
            304 => Ok(Step::Done(Fetched::NotModified)),
            200 => {
                let expected = header(&request, WINHTTP_QUERY_CONTENT_LENGTH)
                    .and_then(|v| v.parse::<usize>().ok());
                if expected.is_some_and(|n| n > max_bytes) {
                    return Err(HttpError::other("the download is unexpectedly large"));
                }
                let mut data = Vec::with_capacity(expected.unwrap_or(0));
                let mut chunk = vec![0u8; 64 * 1024];
                loop {
                    // Ask what has arrived and take just that. Asking for a full
                    // buffer would block until it is full, however slowly the
                    // server sends, and the deadline below would never be looked at.
                    let mut available = 0u32;
                    WinHttpQueryDataAvailable(request.0, &mut available).map_err(fail)?;
                    if available == 0 {
                        break;
                    }
                    let mut read = 0u32;
                    WinHttpReadData(
                        request.0,
                        chunk.as_mut_ptr().cast::<c_void>(),
                        available.min(chunk.len() as u32),
                        &mut read,
                    )
                    .map_err(fail)?;
                    if read == 0 {
                        break;
                    }
                    if data.len() + read as usize > max_bytes {
                        return Err(HttpError::other("the download is unexpectedly large"));
                    }
                    data.extend_from_slice(&chunk[..read as usize]);
                    if Instant::now() >= deadline {
                        return Err(too_slow());
                    }
                }
                if expected.is_some_and(|n| n != data.len()) {
                    return Err(HttpError::other("the download was cut short"));
                }
                Ok(Step::Done(Fetched::Body {
                    data,
                    etag: header(&request, WINHTTP_QUERY_ETAG).filter(|v| header_safe(v)),
                    last_modified: header(&request, WINHTTP_QUERY_LAST_MODIFIED)
                        .filter(|v| header_safe(v)),
                }))
            }
            301 | 302 | 303 | 307 | 308 => header(&request, WINHTTP_QUERY_LOCATION)
                .map(Step::Redirect)
                .ok_or_else(|| HttpError::other("the server sent a redirect without a target")),
            404 => Err(HttpError::other(format!(
                "{} has no such file (HTTP 404); the release may be being republished",
                url.host
            ))),
            other => Err(HttpError::other(format!(
                "{} answered HTTP {other}",
                url.host
            ))),
        }
    }
}

unsafe fn status_code(request: &Handle) -> windows::core::Result<u32> {
    let mut status = 0u32;
    let mut size = size_of::<u32>() as u32;
    unsafe {
        WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            PCWSTR::null(),
            Some(ptr::from_mut(&mut status).cast::<c_void>()),
            &mut size,
            ptr::null_mut(),
        )?;
    }
    Ok(status)
}

/// A response header as text, or `None` if the server did not send it.
unsafe fn header(request: &Handle, which: u32) -> Option<String> {
    unsafe {
        let mut size = 0u32;
        let _ = WinHttpQueryHeaders(
            request.0,
            which,
            PCWSTR::null(),
            None,
            &mut size,
            ptr::null_mut(),
        );
        if size == 0 {
            return None;
        }
        let mut buffer = vec![0u16; size as usize / 2 + 1];
        let mut size = (buffer.len() * 2) as u32;
        WinHttpQueryHeaders(
            request.0,
            which,
            PCWSTR::null(),
            Some(buffer.as_mut_ptr().cast::<c_void>()),
            &mut size,
            ptr::null_mut(),
        )
        .ok()?;
        let text = String::from_utf16_lossy(&buffer[..size as usize / 2]);
        Some(text.trim().to_string()).filter(|t| !t.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(https: bool, host: &str, port: u16, path: &str) -> Url {
        Url {
            https,
            host: host.into(),
            port,
            path: path.into(),
        }
    }

    #[test]
    fn parses_the_urls_it_will_meet() {
        assert_eq!(
            Url::parse(
                "https://github.com/MrKraken/StarStrings/releases/download/latest/StarStrings-LIVE.zip"
            ),
            Ok(url(
                true,
                "github.com",
                443,
                "/MrKraken/StarStrings/releases/download/latest/StarStrings-LIVE.zip"
            ))
        );
        assert_eq!(
            Url::parse("HTTPS://Release-Assets.GitHubUserContent.com/a/b?sp=r&sig=x%2Fy#frag"),
            Ok(url(
                true,
                "release-assets.githubusercontent.com",
                443,
                "/a/b?sp=r&sig=x%2Fy"
            ))
        );
        assert_eq!(
            Url::parse("http://127.0.0.1:8080/x.zip"),
            Ok(url(false, "127.0.0.1", 8080, "/x.zip"))
        );
        assert_eq!(
            Url::parse("https://example.com"),
            Ok(url(true, "example.com", 443, "/"))
        );
        assert_eq!(
            Url::parse("https://example.com?q=1"),
            Ok(url(true, "example.com", 443, "/?q=1"))
        );
    }

    #[test]
    fn rejects_urls_it_should_not_touch() {
        for bad in [
            "",
            "github.com/x",
            "ftp://example.com/x",
            "file:///C:/Windows/win.ini",
            "https://",
            "https:///x",
            "https://user:pass@example.com/x",
            "https://exa mple.com/x",
            "https://example.com/a b",
            "https://example.com:0/x",
            "https://example.com:99999/x",
            "https://example.com:abc/x",
            "https://[::1]/x",
            "https://example.com/x\r\nHost: evil",
        ] {
            assert!(Url::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn resolves_redirect_targets() {
        let base = Url::parse("https://github.com/a/b.zip").unwrap();
        assert_eq!(
            base.resolve("https://release-assets.githubusercontent.com/x?sig=1"),
            Ok(url(
                true,
                "release-assets.githubusercontent.com",
                443,
                "/x?sig=1"
            ))
        );
        assert_eq!(
            base.resolve("/other/c.zip"),
            Ok(url(true, "github.com", 443, "/other/c.zip"))
        );
        assert_eq!(
            base.resolve("//cdn.example.com/c.zip"),
            Ok(url(true, "cdn.example.com", 443, "/c.zip"))
        );
        assert!(base.resolve("relative.zip").is_err());
        assert!(base.resolve("javascript:alert(1)").is_err());
    }

    #[test]
    fn only_https_or_this_machine_is_allowed() {
        let https = Url::parse("https://github.com/x").unwrap();
        let plain = Url::parse("http://github.com/x").unwrap();
        let local = Url::parse("http://127.0.0.1:9/x").unwrap();
        let localhost = Url::parse("http://localhost:9/x").unwrap();

        assert!(check_allowed(None, &https).is_ok());
        assert!(check_allowed(None, &plain).is_err());
        assert!(check_allowed(None, &local).is_ok());
        assert!(check_allowed(None, &localhost).is_ok());

        // Redirects may never leave HTTPS, not even towards this machine.
        assert!(check_allowed(Some(&https), &https).is_ok());
        assert!(check_allowed(Some(&https), &plain).is_err());
        assert!(check_allowed(Some(&https), &local).is_err());
        assert!(check_allowed(Some(&local), &local).is_ok());
        assert!(check_allowed(Some(&local), &https).is_ok());
        assert!(check_allowed(Some(&local), &plain).is_err());
    }

    #[test]
    fn header_values_must_be_plain_single_line_ascii() {
        assert!(header_safe("\"0x8DF40E2D5861898\""));
        assert!(header_safe("Thu, 12 Nov 2026 17:31:49 GMT"));
        assert!(!header_safe(""));
        assert!(!header_safe("abc\r\nX-Injected: 1"));
        assert!(!header_safe("caf\u{e9}"));
        assert!(!header_safe(&"a".repeat(513)));
    }

    #[test]
    fn refuses_plain_http_before_touching_the_network() {
        let err = fetch("http://example.com/x.zip", &Conditions::default(), 1024).unwrap_err();
        assert!(err.message.contains("unencrypted"), "{}", err.message);
        assert!(!err.offline);
    }
}
