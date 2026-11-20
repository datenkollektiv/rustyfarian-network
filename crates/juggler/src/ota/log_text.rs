//! `Display` adapters that make a URL or any server-supplied string safe to log.

use core::fmt::{self, Write};

/// Experimental: API may change before 1.0.
///
/// Displays a URL without its userinfo, query and fragment, with control characters escaped.
///
/// The authority ends at the first `/`, `?` or `#`, as in the OTA URL check; the userinfo (`user:password@`) is everything up to the last `@` in it.
/// The query and fragment can carry secrets (a token as `?c=…`), so only the scheme, host, port and path are shown.
/// Input without `://` is not a URL and is shown whole, with control characters escaped.
///
/// # Examples
///
/// ```
/// use juggler::ota::UrlForLog;
///
/// let shown = format!("{}", UrlForLog("http://user:pass@host:8080/fw.bin?c=secret#frag"));
/// assert_eq!(shown, "http://host:8080/fw.bin");
/// ```
#[derive(Debug, Clone, Copy)]
pub struct UrlForLog<'a>(pub &'a str);

impl fmt::Display for UrlForLog<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Some((scheme, rest)) = self.0.split_once("://") else {
            return TextForLog(self.0).fmt(f);
        };
        let (authority, tail) = rest.split_at(rest.find(['/', '?', '#']).unwrap_or(rest.len()));
        let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
        let path = tail.split(['?', '#']).next().unwrap_or("");
        write!(
            f,
            "{}://{}{}",
            TextForLog(scheme),
            TextForLog(host),
            TextForLog(path)
        )
    }
}

/// Experimental: API may change before 1.0.
///
/// Displays any server-supplied string (a link, a file name) with every control character escaped, so it cannot break or forge a log line.
///
/// Control characters (`char::is_control`: C0, DEL and C1) are written as their Rust escape (`\n`, `\r`, `\u{1b}`); everything else is written as-is.
///
/// # Examples
///
/// ```
/// use juggler::ota::TextForLog;
///
/// assert_eq!(format!("{}", TextForLog("fw.bin\r\n[ota] fake")), r"fw.bin\r\n[ota] fake");
/// ```
#[derive(Debug, Clone, Copy)]
pub struct TextForLog<'a>(pub &'a str);

impl fmt::Display for TextForLog<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut plain = 0;
        for (i, c) in self.0.char_indices() {
            if c.is_control() {
                f.write_str(&self.0[plain..i])?;
                for e in c.escape_debug() {
                    f.write_char(e)?;
                }
                plain = i + c.len_utf8();
            }
        }
        f.write_str(&self.0[plain..])
    }
}

#[cfg(test)]
mod tests {
    use super::{TextForLog, UrlForLog};
    use alloc::format;
    use alloc::string::String;

    fn url(u: &str) -> String {
        format!("{}", UrlForLog(u))
    }

    #[test]
    fn plain_urls_unchanged() {
        assert_eq!(
            url("http://192.168.1.1/fw.bin"),
            "http://192.168.1.1/fw.bin"
        );
        assert_eq!(
            url("http://example.com:8080/x"),
            "http://example.com:8080/x"
        );
        assert_eq!(url("http://host"), "http://host");
        assert_eq!(url("http://[::1]:80/a/b"), "http://[::1]:80/a/b");
    }

    #[test]
    fn userinfo_stripped() {
        assert_eq!(
            url("http://user:pass@192.168.1.1/fw.bin"),
            "http://192.168.1.1/fw.bin"
        );
        assert_eq!(url("http://user@host/path"), "http://host/path");
        assert_eq!(url("http://user:pass@host"), "http://host");
        assert_eq!(url("http://a@b@host/x"), "http://host/x");
    }

    #[test]
    fn query_and_fragment_stripped() {
        assert_eq!(url("http://h/fw.bin?c=0123abcd"), "http://h/fw.bin");
        assert_eq!(url("http://h/fw.bin#frag"), "http://h/fw.bin");
        assert_eq!(url("http://h/a?x=1#frag"), "http://h/a");
        assert_eq!(url("http://h?x=1"), "http://h");
        assert_eq!(url("http://h#frag"), "http://h");
    }

    #[test]
    fn authority_ends_at_query_or_fragment() {
        assert_eq!(url("http://user@host?x"), "http://host");
        assert_eq!(url("http://host?x=a@b/c"), "http://host");
        assert_eq!(url("http://host#a@b"), "http://host");
    }

    #[test]
    fn control_characters_escaped() {
        assert_eq!(url("http://h/a\r\nb"), r"http://h/a\r\nb");
        assert_eq!(url("http://h\u{1b}[31m/x"), r"http://h\u{1b}[31m/x");
        assert_eq!(url("not\na url"), r"not\na url");
    }

    #[test]
    fn non_url_shown_whole() {
        assert_eq!(url("not a url"), "not a url");
        assert_eq!(url(""), "");
    }

    #[test]
    fn text_escapes_only_control_characters() {
        let t = |s: &str| format!("{}", TextForLog(s));
        assert_eq!(t("fw-1.2.3.bin"), "fw-1.2.3.bin");
        assert_eq!(t("a\tb\0c\u{7f}d\u{85}e"), r"a\tb\0c\u{7f}d\u{85}e");
        assert_eq!(t("ümlaut \"q\" \\"), "ümlaut \"q\" \\");
        assert_eq!(t("\n"), r"\n");
        assert_eq!(t(""), "");
    }
}
