//! The one URL check shared by command intake and manifest parsing.

/// Whether `url` is a plain-HTTP URL with a non-empty host: `http://` (scheme matched ASCII case-insensitively) followed by an authority whose host part is not empty.
///
/// The authority ends at the first `/`, `?` or `#`; the host is what remains after the last `@` (userinfo) and before a port `:`; an IPv6 literal in `[...]` counts as a host.
/// Both OTA tiers download over plain HTTP only (ADR 011), so anything else (`https://`, `ftp://`, no scheme, empty) would otherwise fail much later as `server_unreachable` or `manifest_fetch`.
/// Only the shape is checked; the host is not resolved and the path is not inspected.
pub(crate) fn is_plain_http_url(url: &str) -> bool {
    const SCHEME: &str = "http://";
    let Some(prefix) = url.get(..SCHEME.len()) else {
        return false;
    };
    if !prefix.eq_ignore_ascii_case(SCHEME) {
        return false;
    }
    let rest = &url[SCHEME.len()..];
    if rest
        .bytes()
        .any(|b| b.is_ascii_whitespace() || b.is_ascii_control())
    {
        return false;
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host_and_port = authority.rsplit('@').next().unwrap_or("");
    let (host, port) = if let Some(bracketed) = host_and_port.strip_prefix('[') {
        let Some(close) = bracketed.find(']') else {
            return false;
        };
        let after = &bracketed[close + 1..];
        let port = match after.strip_prefix(':') {
            Some(p) => Some(p),
            None if after.is_empty() => None,
            None => return false,
        };
        (&bracketed[..close], port)
    } else {
        match host_and_port.split_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (host_and_port, None),
        }
    };
    if host.is_empty() {
        return false;
    }
    match port {
        Some(p) => {
            !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) && p.parse::<u16>().is_ok()
        }
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::is_plain_http_url;

    #[test]
    fn accepted() {
        for url in [
            "http://h/x",
            "HTTP://h/x",
            "HtTp://h",
            "http://h",
            "http://h:8080/fw.bin",
            "http://h:65535/x",
            "http://user:pw@h/x",
            "http://[::1]:8080/x",
            "http://192.168.1.1/fw.bin?a=b",
            "http://h?q",
            "http://h#f",
        ] {
            assert!(is_plain_http_url(url), "{url}");
        }
    }

    #[test]
    fn rejected() {
        for url in [
            "",
            " ",
            "   \t\n",
            "https://h/x",
            "HTTPS://h/x",
            "http://",
            "http:///x",
            "http://?x",
            "http://#x",
            "http://:80/x",
            "http://user@/x",
            "http://user:pw@:80/x",
            "http://h:abc/x",
            "http://h:99999/x",
            "http://h:/x",
            "http://   /x",
            "http://h/x y",
            "http://h/x\r\nX-Evil: 1",
            "http://h/x\n",
            "http://h\0/x",
            "http://[",
            "http://[]",
            "http://[]:80/x",
            "http://[::1",
            "http://[::1]x/x",
            "http://[::1]:abc/x",
            "ftp://h/x",
            "h/x",
            "http:/h/x",
            "http:h",
            " http://h/x",
            "http",
            "ht",
            "héllo://h",
        ] {
            assert!(!is_plain_http_url(url), "{url}");
        }
    }

    #[test]
    fn multibyte_input_never_panics() {
        for url in ["ééééééé", "http:/é", "日本語日本語", "http://日本語/x"] {
            let _ = is_plain_http_url(url);
        }
    }
}
