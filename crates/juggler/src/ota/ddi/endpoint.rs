//! The DDI server endpoint: the configured base URL and controller id, every request URL built from them, and the origin check for server links.
//!
//! A transport builds the URLs of its own requests here and checks every server-supplied [`Href`] against the configured origin before it sends the token anywhere (ADR 027).
//! The check is the origin binding of the fleet DDI profile (§2): a URL is accepted only in the shape `scheme://host[:port]` followed by `/`, `?` or its end, and what the transport hands its HTTP client is a [`CheckedUrl`] rebuilt from the checked parts, never the link text.

use alloc::string::String;
use core::fmt::{self, Write};

use super::wire::{ActionId, Href};

/// Experimental: API may change before 1.0.
///
/// Longest accepted base URL in bytes.
pub const MAX_BASE_URL_BYTES: usize = 256;
/// Experimental: API may change before 1.0.
///
/// Shortest accepted controller id in bytes ([`crate::identity::DEVICE_ID_MIN_LEN`]).
pub const CONTROLLER_ID_MIN_LEN: usize = crate::identity::DEVICE_ID_MIN_LEN;
/// Experimental: API may change before 1.0.
///
/// Longest accepted controller id in bytes ([`crate::identity::DEVICE_ID_MAX_LEN`]).
pub const CONTROLLER_ID_MAX_LEN: usize = crate::identity::DEVICE_ID_MAX_LEN;

/// Experimental: API may change before 1.0.
///
/// The URL scheme of a DDI endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scheme {
    /// `http`, plain text; a transport accepts it only in a lab build.
    Http,
    /// `https`.
    Https,
}

impl Scheme {
    /// The port a URL without an explicit port uses: 80 for `http`, 443 for `https`.
    pub const fn default_port(self) -> u16 {
        match self {
            Scheme::Http => 80,
            Scheme::Https => 443,
        }
    }

    /// `http` or `https` in any ASCII case; anything else is `None`.
    fn parse(text: &str) -> Option<Self> {
        if text.eq_ignore_ascii_case("http") {
            Some(Scheme::Http)
        } else if text.eq_ignore_ascii_case("https") {
            Some(Scheme::Https)
        } else {
            None
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Scheme::Http => "http",
            Scheme::Https => "https",
        }
    }
}

/// Experimental: API may change before 1.0.
///
/// A DDI controller id, the device id rule `^[a-z0-9][a-z0-9-]{2,19}$`.
/// It is a URL path segment as-is, with nothing to escape.
/// Not a secret: `Display` writes it.
///
/// # Examples
///
/// ```
/// use juggler::ota::ddi::ControllerId;
///
/// assert_eq!(ControllerId::parse("dev-1").unwrap().as_str(), "dev-1");
/// assert!(ControllerId::parse("-dev").is_err());
/// assert!(ControllerId::parse("Dev1").is_err());
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControllerId(String);

impl ControllerId {
    /// Accepts `text` only when it matches `^[a-z0-9][a-z0-9-]{2,19}$` ([`crate::identity::is_device_id`], the rule the provisioning portal shares); anything else is refused, never trimmed.
    pub fn parse(text: &str) -> Result<Self, EndpointError> {
        if !crate::identity::is_device_id(text) {
            return Err(EndpointError::InvalidControllerId);
        }
        Ok(ControllerId(String::from(text)))
    }

    /// The id text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ControllerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Experimental: API may change before 1.0.
///
/// Why an endpoint was refused.
/// Carries no input text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointError {
    /// The base URL failed the check of [`DdiEndpoint::new`]; the case names what failed, never the URL.
    InvalidBaseUrl(UrlRefusal),
    /// The controller id does not match `^[a-z0-9][a-z0-9-]{2,19}$`.
    InvalidControllerId,
}

impl fmt::Display for EndpointError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EndpointError::InvalidBaseUrl(case) => write!(f, "DDI base URL invalid: {case}"),
            EndpointError::InvalidControllerId => f.write_str("DDI controller id invalid"),
        }
    }
}

/// Experimental: API may change before 1.0.
///
/// Why a URL failed the origin check (fleet DDI profile §2).
/// Carries no URL text; `Display` and [`name`](Self::name) write the case's name, the fleet's vocabulary (`forbidden_char`, `syntax`, `scheme`, `host`, `port`, `origin`, `length`, `path`), so a refusal is logged without the URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UrlRefusal {
    /// An `@`, `\`, `%`, whitespace, control or non-ASCII character anywhere in the URL, userinfo included.
    ForbiddenChar,
    /// Not `scheme://…` with a scheme of ASCII letters.
    Syntax,
    /// A scheme other than `http` or `https` (compared ASCII case-insensitively).
    Scheme,
    /// The host is not a DNS name (labels of `[A-Za-z0-9-]` joined by single dots, no trailing dot, a letter in the last label), a canonical dotted-decimal IPv4 address or a bracketed IPv6 literal; what follows the host must be `:`, `/`, `?` or the end of the URL, so a `#` right after it lands here or in [`Port`](Self::Port).
    Host,
    /// The port is not 1 to 5 ASCII digits naming 1 to 65535.
    Port,
    /// Scheme, host or port differ from the configured endpoint; an IP literal never equals a DNS name.
    Origin,
    /// A link whose `href` was empty or longer than [`MAX_HREF_BYTES`](super::wire::MAX_HREF_BYTES), refused on the wire before this check, or a base URL that, as given or rebuilt, is longer than [`MAX_BASE_URL_BYTES`].
    Length,
    /// A path the device does not accept: a `.` or `..` segment in any URL (a server that normalises the path would resolve another resource than the one the device reads), a base URL path with a query, a fragment or a character outside `[A-Za-z0-9-._~/]`, or a `deploymentBase` link whose path does not end in `/controller/v1/<controller id>/deploymentBase/<action id>`.
    Path,
}

impl UrlRefusal {
    /// The case's name.
    pub const fn name(self) -> &'static str {
        match self {
            UrlRefusal::ForbiddenChar => "forbidden_char",
            UrlRefusal::Syntax => "syntax",
            UrlRefusal::Scheme => "scheme",
            UrlRefusal::Host => "host",
            UrlRefusal::Port => "port",
            UrlRefusal::Origin => "origin",
            UrlRefusal::Length => "length",
            UrlRefusal::Path => "path",
        }
    }
}

impl fmt::Display for UrlRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Experimental: API may change before 1.0.
///
/// A URL that passed the origin check, rebuilt from the checked parts: the scheme and host lower-cased, the port explicit (`http://h` becomes `http://h:80`), and everything after the authority as received (the path and query; a fragment, which an HTTP client does not send, stays as well).
/// It is the only form a transport hands its HTTP client, never the link text, because the client parses the URL again (fleet DDI profile §2).
/// Like [`Href`], it may carry a token in its query, so `Debug` prints `CheckedUrl(<redacted>)` and there is no `Display`.
#[derive(Clone, PartialEq, Eq)]
pub struct CheckedUrl(String);

impl CheckedUrl {
    /// The URL text.
    /// Do not log it.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Takes the URL text.
    /// Do not log it.
    pub fn into_string(self) -> String {
        self.0
    }
}

impl fmt::Debug for CheckedUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CheckedUrl(<redacted>)")
    }
}

/// Experimental: API may change before 1.0.
///
/// The configured DDI server and controller: the origin every request must stay on, and the URLs of the requests a device builds itself.
///
/// The controller resource is `<base>/controller/v1/<controller id>`, so the base carries the tenant path (`https://ddi.example/DEFAULT`).
/// The feedback and configData URLs are built from it, never taken from a link.
/// Every URL built here starts with the origin rebuilt from the checked base (lower-cased scheme and host, explicit port), the same form [`check_href`](Self::check_href) returns.
///
/// # Examples
///
/// ```
/// use juggler::ota::ddi::{ControllerId, DdiEndpoint, Scheme, UrlRefusal};
/// use juggler::ota::ddi::wire::ActionId;
///
/// let id = ControllerId::parse("dev-1").unwrap();
/// let endpoint = DdiEndpoint::new("https://DDI.example/DEFAULT/", &id).unwrap();
/// assert_eq!(endpoint.scheme(), Scheme::Https);
/// assert_eq!(endpoint.poll_url(), "https://ddi.example:443/DEFAULT/controller/v1/dev-1");
/// assert_eq!(
///     endpoint.deployment_feedback_url(&ActionId::parse("7").unwrap()),
///     "https://ddi.example:443/DEFAULT/controller/v1/dev-1/deploymentBase/7/feedback"
/// );
/// assert_eq!(
///     endpoint.check_url("HTTPS://DDI.example/x?c=1").unwrap().as_str(),
///     "https://ddi.example:443/x?c=1"
/// );
/// assert_eq!(endpoint.check_url("http://ddi.example/x"), Err(UrlRefusal::Origin));
/// assert_eq!(endpoint.check_url("https://u@ddi.example/x"), Err(UrlRefusal::ForbiddenChar));
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DdiEndpoint {
    controller_url: String,
    /// `/controller/v1/<controller id>`, the path a `deploymentBase` link must end in before `/deploymentBase/<id>`.
    controller_path: String,
    scheme: Scheme,
    /// Lower-cased.
    host: String,
    port: u16,
}

impl DdiEndpoint {
    /// Validates `base_url` with [`check_base_url`](Self::check_base_url) and builds the controller resource for `controller_id` on the rebuilt base.
    ///
    /// A transport calls this at every boot, so a base that fails the check never gets a request.
    /// Whether a scheme is allowed (plain HTTP only in a lab build) is the transport's decision; read it with [`scheme`](Self::scheme).
    pub fn new(base_url: &str, controller_id: &ControllerId) -> Result<Self, EndpointError> {
        let (origin, base) = checked_base(base_url, 16 + CONTROLLER_ID_MAX_LEN)
            .map_err(EndpointError::InvalidBaseUrl)?;
        let mut controller_url = base.into_string();
        let base_len = controller_url.len();
        controller_url.push_str("/controller/v1/");
        controller_url.push_str(controller_id.as_str());
        let controller_path = String::from(&controller_url[base_len..]);
        let host_start = origin.scheme.as_str().len() + "://".len();
        let host = String::from(&controller_url[host_start..host_start + origin.host.len()]);
        Ok(DdiEndpoint {
            controller_url,
            controller_path,
            scheme: origin.scheme,
            host,
            port: origin.port,
        })
    }

    /// The origin check of a base URL, the one a provisioning step runs before it stores a base and [`new`](Self::new) runs at every boot.
    ///
    /// The base URL is accepted only when all of these hold:
    ///
    /// - at most [`MAX_BASE_URL_BYTES`] bytes, as given and as rebuilt ([`UrlRefusal::Length`]), so the result passes this check again (a provisioning step may store it);
    /// - the shape of [`check_url`](Self::check_url): `http` or `https`, a DNS name, canonical IPv4 address or bracketed IPv6 literal, an optional port of 1 to 65535, no `@`, `\`, `%`, whitespace, control or non-ASCII character anywhere;
    /// - a path of ASCII letters, digits and `-._~/`, so no query and no fragment ([`UrlRefusal::Path`]).
    ///
    /// The result is the base rebuilt from the checked parts, with trailing slashes dropped.
    pub fn check_base_url(base_url: &str) -> Result<CheckedUrl, UrlRefusal> {
        checked_base(base_url, 0).map(|(_, base)| base)
    }

    /// The configured scheme.
    pub fn scheme(&self) -> Scheme {
        self.scheme
    }

    /// The controller base resource, the target of the poll GET.
    pub fn poll_url(&self) -> &str {
        &self.controller_url
    }

    /// `<controller>/deploymentBase/<action>/feedback`, the target of the deployment feedback POST.
    pub fn deployment_feedback_url(&self, action: &ActionId) -> String {
        self.action_url("/deploymentBase/", action)
    }

    /// `<controller>/cancelAction/<action>/feedback`, the target of the cancel feedback POST.
    pub fn cancel_feedback_url(&self, action: &ActionId) -> String {
        self.action_url("/cancelAction/", action)
    }

    /// `<controller>/configData`, the target of the configData PUT.
    pub fn config_data_url(&self) -> String {
        let mut url = self.controller_url.clone();
        url.push_str("/configData");
        url
    }

    /// The origin check of the fleet DDI profile (§2) against this endpoint, with the URL rebuilt from the checked parts.
    ///
    /// `url` is accepted only as `scheme://host[:port]` followed by `/`, `?` or its end, where
    ///
    /// - no `@`, `\`, `%`, whitespace, control or non-ASCII character appears anywhere (userinfo included);
    /// - the scheme is `http` or `https` in any case;
    /// - the host is a DNS name of `[A-Za-z0-9-]` labels joined by single dots with no trailing dot and a letter in the last label, a canonical dotted-decimal IPv4 address, or a bracketed IPv6 literal;
    /// - the port, if any, is 1 to 5 ASCII digits naming 1 to 65535;
    /// - scheme, host and port equal this endpoint's: the scheme and host ASCII case-insensitively, the port after the scheme's default is filled in (`https://h` and `https://h:443` are one origin), and an IP literal never equals a DNS name.
    ///
    /// The [`CheckedUrl`] carries the scheme and host lower-cased, the port explicit, and everything after the authority verbatim; a transport sends it, never `url`.
    /// Anything a URL parser could read differently is refused, with the [`UrlRefusal`] naming the case.
    pub fn check_url(&self, url: &str) -> Result<CheckedUrl, UrlRefusal> {
        let origin = Origin::parse(url)?;
        if !self.is_origin(&origin) {
            return Err(UrlRefusal::Origin);
        }
        Ok(origin.rebuild(origin.tail, 0))
    }

    /// The case that makes [`check_url`](Self::check_url) refuse `url`, if any, without building the rebuilt URL; for a caller that only decides whether a link is acceptable.
    pub fn refusal(&self, url: &str) -> Option<UrlRefusal> {
        match Origin::parse(url) {
            Err(case) => Some(case),
            Ok(origin) if !self.is_origin(&origin) => Some(UrlRefusal::Origin),
            Ok(_) => None,
        }
    }

    /// `true` when `url` passes [`check_url`](Self::check_url).
    pub fn same_origin(&self, url: &str) -> bool {
        self.refusal(url).is_none()
    }

    fn is_origin(&self, o: &Origin<'_>) -> bool {
        o.scheme == self.scheme && o.port == self.port && o.host.eq_ignore_ascii_case(&self.host)
    }

    /// [`check_url`](Self::check_url) of a server link: the URL to request when the link is on this endpoint's origin, or the case that refused it.
    pub fn check_href(&self, href: &Href) -> Result<CheckedUrl, UrlRefusal> {
        self.check_url(href.as_str())
    }

    /// The action id a `deploymentBase` link names, read from the link without requesting it.
    ///
    /// The link must pass [`check_url`](Self::check_url) and its path must end in `/controller/v1/<controller id>/deploymentBase/<action id>`; a query (hawkBit appends `?c=<hash>`) or fragment is ignored.
    /// The path before `/controller/v1` (the tenant, or a prefix a reverse proxy adds) need not match the configured base, as for a link the device retrieves; a link naming another controller is refused.
    /// Anything else is `None`.
    /// The device reports a deployment it cannot take, such as one offered while its records are unreadable, to this id without retrieving it.
    ///
    /// ```
    /// use juggler::ota::ddi::wire::PollResponse;
    /// use juggler::ota::ddi::{ControllerId, DdiEndpoint};
    ///
    /// let endpoint = DdiEndpoint::new("https://ddi.example/DEFAULT", &ControllerId::parse("dev-1").unwrap()).unwrap();
    /// let body = br#"{"_links":{"deploymentBase":{"href":"https://ddi.example/DEFAULT/controller/v1/dev-1/deploymentBase/42?c=-9"}}}"#;
    /// let link = PollResponse::parse(body).unwrap().deployment_base.unwrap();
    /// assert_eq!(endpoint.deployment_action(&link).unwrap().as_str(), "42");
    /// ```
    pub fn deployment_action(&self, href: &Href) -> Option<ActionId> {
        let link = Origin::parse(href.as_str())
            .ok()
            .filter(|o| self.is_origin(o))?;
        let path = link.tail.split(['?', '#']).next()?;
        let (controller, id) = path.rsplit_once("/deploymentBase/")?;
        if !controller.ends_with(self.controller_path.as_str()) {
            return None;
        }
        ActionId::parse(id).ok()
    }

    fn action_url(&self, resource: &str, action: &ActionId) -> String {
        let mut url = self.controller_url.clone();
        url.push_str(resource);
        url.push_str(action.as_str());
        url.push_str("/feedback");
        url
    }
}

/// The origin of a valid base URL and the base rebuilt with trailing slashes dropped, with room for `extra` more bytes.
///
/// Both the base as given and the rebuilt base (which gains an explicit port) must fit [`MAX_BASE_URL_BYTES`], so a stored rebuilt base passes this check again at the next boot.
fn checked_base(base_url: &str, extra: usize) -> Result<(Origin<'_>, CheckedUrl), UrlRefusal> {
    if base_url.len() > MAX_BASE_URL_BYTES {
        return Err(UrlRefusal::Length);
    }
    let origin = Origin::parse(base_url)?;
    let path_ok = origin
        .tail
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'/'));
    if !path_ok {
        return Err(UrlRefusal::Path);
    }
    let base = origin.rebuild(origin.tail.trim_end_matches('/'), extra);
    if base.as_str().len() > MAX_BASE_URL_BYTES {
        return Err(UrlRefusal::Length);
    }
    Ok((origin, base))
}

/// The parts of an absolute `http` or `https` URL that decide its origin.
#[derive(Clone, Copy)]
struct Origin<'a> {
    scheme: Scheme,
    /// As written; a DNS name or IPv6 literal may carry upper-case letters.
    host: &'a str,
    port: u16,
    /// Everything after the authority: path, query and fragment, starting with `/` or `?` unless empty.
    tail: &'a str,
}

impl<'a> Origin<'a> {
    /// The grammar of [`DdiEndpoint::check_url`] without the comparison with an endpoint, in the order of [`UrlRefusal`].
    fn parse(url: &'a str) -> Result<Self, UrlRefusal> {
        if url
            .bytes()
            .any(|b| !b.is_ascii_graphic() || matches!(b, b'@' | b'\\' | b'%'))
        {
            return Err(UrlRefusal::ForbiddenChar);
        }
        let (scheme, rest) = url.split_once("://").ok_or(UrlRefusal::Syntax)?;
        if scheme.is_empty() || !scheme.bytes().all(|b| b.is_ascii_alphabetic()) {
            return Err(UrlRefusal::Syntax);
        }
        let scheme = Scheme::parse(scheme).ok_or(UrlRefusal::Scheme)?;
        let (authority, tail) = rest.split_at(rest.find(['/', '?']).unwrap_or(rest.len()));
        let (host, port) = split_host_port(authority)?;
        let port = match port {
            None => scheme.default_port(),
            Some(text) => parse_port(text).ok_or(UrlRefusal::Port)?,
        };
        let path = tail.split(['?', '#']).next().unwrap_or_default();
        if path.split('/').any(|segment| matches!(segment, "." | "..")) {
            return Err(UrlRefusal::Path);
        }
        Ok(Origin {
            scheme,
            host,
            port,
            tail,
        })
    }

    /// `scheme://host:port` from the checked parts (scheme and host lower-cased, port explicit) with `tail` verbatim, with room for `extra` more bytes.
    fn rebuild(&self, tail: &str, extra: usize) -> CheckedUrl {
        let mut url = String::with_capacity(
            "https://".len() + self.host.len() + ":65535".len() + tail.len() + extra,
        );
        url.push_str(self.scheme.as_str());
        url.push_str("://");
        url.extend(self.host.chars().map(|c| c.to_ascii_lowercase()));
        // Writing to a `String` cannot fail.
        let _ = write!(url, ":{}", self.port);
        url.push_str(tail);
        CheckedUrl(url)
    }
}

/// Splits an authority into a validated host and the port text after `:`, if any.
fn split_host_port(authority: &str) -> Result<(&str, Option<&str>), UrlRefusal> {
    if let Some(inner) = authority.strip_prefix('[') {
        let (literal, after) = inner.split_once(']').ok_or(UrlRefusal::Host)?;
        if literal.parse::<core::net::Ipv6Addr>().is_err() {
            return Err(UrlRefusal::Host);
        }
        let port = match after {
            "" => None,
            _ => Some(after.strip_prefix(':').ok_or(UrlRefusal::Host)?),
        };
        Ok((&authority[..literal.len() + 2], port))
    } else {
        let (host, port) = match authority.split_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        };
        if !is_canonical_ipv4(host) && !is_dns_name(host) {
            return Err(UrlRefusal::Host);
        }
        Ok((host, port))
    }
}

/// Four decimal octets joined by dots, each 0 to 255 with no leading zero.
fn is_canonical_ipv4(host: &str) -> bool {
    host.split('.').count() == 4 && host.split('.').all(is_canonical_octet)
}

fn is_canonical_octet(part: &str) -> bool {
    let bytes = part.as_bytes();
    (1..=3).contains(&bytes.len())
        && bytes.iter().all(u8::is_ascii_digit)
        && (bytes.len() == 1 || bytes[0] != b'0')
        && part.parse::<u8>().is_ok()
}

/// Labels of `[A-Za-z0-9-]` joined by single dots, no empty label (so no leading or trailing dot), a letter in the last label, and a last label that is not a number in any base.
///
/// The last rule is the URL standard's "ends in a number": `0x7f000001` or `127.0.0.0x1` has a letter, yet URL parsers and lwIP's `ipaddr_aton` read it as an IPv4 address, so it is neither a DNS name nor a canonical IPv4 address here.
fn is_dns_name(host: &str) -> bool {
    let mut last = "";
    for label in host.split('.') {
        if label.is_empty()
            || !label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return false;
        }
        last = label;
    }
    let hex_number = last.len() >= 2
        && last[..2].eq_ignore_ascii_case("0x")
        && last[2..].bytes().all(|b| b.is_ascii_hexdigit());
    last.bytes().any(|b| b.is_ascii_alphabetic()) && !hex_number
}

/// 1 to 5 ASCII digits naming a port from 1 to 65535.
fn parse_port(text: &str) -> Option<u16> {
    if text.is_empty() || text.len() > 5 || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse::<u16>().ok().filter(|&p| p != 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::format;

    fn id() -> ControllerId {
        ControllerId::parse("dev-1").unwrap()
    }

    fn endpoint(base: &str) -> DdiEndpoint {
        DdiEndpoint::new(base, &id()).unwrap()
    }

    fn href(url: &str) -> Href {
        let json = format!(r#"{{"h":"{url}"}}"#);
        #[derive(serde::Deserialize)]
        struct H {
            h: Href,
        }
        serde_json::from_str::<H>(&json).unwrap().h
    }

    #[test]
    fn deployment_action_reads_the_id_from_the_link() {
        let e = endpoint("https://ddi.example/DEFAULT/");
        let base = "https://ddi.example/DEFAULT/controller/v1/dev-1/deploymentBase";
        for (url, id) in [
            (format!("{base}/42"), "42"),
            (format!("{base}/42?c=-2129030598"), "42"),
            (format!("{base}/7#x"), "7"),
            (
                String::from("https://ddi.example/OTHER/controller/v1/dev-1/deploymentBase/5"),
                "5",
            ),
            (
                String::from(
                    "https://ddi.example/proxy/DEFAULT/controller/v1/dev-1/deploymentBase/6",
                ),
                "6",
            ),
            (
                String::from(
                    "HTTPS://DDI.example:443/DEFAULT/controller/v1/dev-1/deploymentBase/9",
                ),
                "9",
            ),
        ] {
            assert_eq!(
                e.deployment_action(&href(&url)).unwrap().as_str(),
                id,
                "{url}"
            );
        }
        let root = endpoint("http://h:8080");
        assert_eq!(
            root.deployment_action(&href("http://h:8080/controller/v1/dev-1/deploymentBase/1"))
                .unwrap()
                .as_str(),
            "1"
        );
    }

    #[test]
    fn deployment_action_refuses_any_other_link() {
        let e = endpoint("https://ddi.example/DEFAULT");
        for url in [
            "http://ddi.example/DEFAULT/controller/v1/dev-1/deploymentBase/42",
            "https://evil.example/DEFAULT/controller/v1/dev-1/deploymentBase/42",
            "https://ddi.example:8443/DEFAULT/controller/v1/dev-1/deploymentBase/42",
            "https://u@ddi.example/DEFAULT/controller/v1/dev-1/deploymentBase/42",
            "https://ddi.example/DEFAULT/controller/v1/dev-1/deploymentBase/4%32",
            "https://ddi.example/DEFAULT/controller/v1/my-dev-1/deploymentBase/42",
            "https://ddi.example/DEFAULT/controller/v2/dev-1/deploymentBase/42",
            "https://ddi.example/DEFAULT/dev-1/deploymentBase/42",
            "https://ddi.example/DEFAULT/controller/v1/dev-2/deploymentBase/42",
            "https://ddi.example/DEFAULT/controller/v1/dev-1/cancelAction/42",
            "https://ddi.example/DEFAULT/controller/v1/dev-1/deploymentBase/",
            "https://ddi.example/DEFAULT/controller/v1/dev-1/deploymentBase/42/",
            "https://ddi.example/DEFAULT/controller/v1/dev-1/deploymentBase/42/feedback",
            "https://ddi.example/DEFAULT/controller/v1/dev-1/deploymentBase/4a",
            "https://ddi.example/DEFAULT/controller/v1/dev-1/deploymentBase/-1",
            "https://ddi.example/DEFAULT/controller/v1/dev-1/deploymentBase/12345678901234567890",
            "https://ddi.example/DEFAULT/controller/v1/dev-1/deploymentBase/042",
            "https://ddi.example/DEFAULT/controller/v1/dev-1/deploymentBase/9223372036854775808",
            "https://ddi.example/DEFAULT/controller/v1/dev-1deploymentBase/42",
            "/DEFAULT/controller/v1/dev-1/deploymentBase/42",
            "deploymentBase/42",
        ] {
            assert!(e.deployment_action(&href(url)).is_none(), "{url}");
        }
    }

    #[test]
    fn controller_id_grammar() {
        for ok in [
            "abc",
            "dev-1",
            "0ab",
            "a-------------------",
            "abcdefghij0123456789",
        ] {
            assert!(ControllerId::parse(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "ab",
            "abcdefghij01234567890",
            "-ab",
            "Abc",
            "a_b",
            "a b",
            "ab/c",
            "ab\r\n",
            "äbc",
        ] {
            assert_eq!(
                ControllerId::parse(bad),
                Err(EndpointError::InvalidControllerId),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn urls_are_built_from_base_and_controller() {
        let e = endpoint("http://192.168.1.10:8080/DEFAULT");
        let action = ActionId::parse("42").unwrap();
        assert_eq!(e.scheme(), Scheme::Http);
        assert_eq!(
            e.poll_url(),
            "http://192.168.1.10:8080/DEFAULT/controller/v1/dev-1"
        );
        assert_eq!(
            e.deployment_feedback_url(&action),
            "http://192.168.1.10:8080/DEFAULT/controller/v1/dev-1/deploymentBase/42/feedback"
        );
        assert_eq!(
            e.cancel_feedback_url(&action),
            "http://192.168.1.10:8080/DEFAULT/controller/v1/dev-1/cancelAction/42/feedback"
        );
        assert_eq!(
            e.config_data_url(),
            "http://192.168.1.10:8080/DEFAULT/controller/v1/dev-1/configData"
        );
    }

    #[test]
    fn base_url_is_rebuilt_from_the_checked_parts() {
        assert_eq!(
            endpoint("HTTPS://DDI.Example/DEFAULT/").poll_url(),
            "https://ddi.example:443/DEFAULT/controller/v1/dev-1"
        );
        assert_eq!(
            endpoint("http://H:08080").poll_url(),
            "http://h:8080/controller/v1/dev-1"
        );
        assert_eq!(
            endpoint("http://[FD00::1]/t").config_data_url(),
            "http://[fd00::1]:80/t/controller/v1/dev-1/configData"
        );
        for (base, rebuilt) in [
            ("HTTP://H.Example:80/t/", "http://h.example:80/t"),
            ("https://ddi.example", "https://ddi.example:443"),
            ("https://ddi.example/", "https://ddi.example:443"),
            (
                "http://192.168.1.10:8080/DEFAULT",
                "http://192.168.1.10:8080/DEFAULT",
            ),
        ] {
            assert_eq!(
                DdiEndpoint::check_base_url(base).unwrap().as_str(),
                rebuilt,
                "{base}"
            );
            assert_eq!(
                DdiEndpoint::check_base_url(rebuilt).unwrap().as_str(),
                rebuilt,
                "{base}"
            );
        }
        let e = endpoint("HTTPS://DDI.Example/DEFAULT/");
        assert_eq!(e.check_url(e.poll_url()).unwrap().as_str(), e.poll_url());
    }

    #[test]
    fn trailing_slashes_and_empty_path() {
        assert_eq!(
            endpoint("https://ddi.example/").poll_url(),
            "https://ddi.example:443/controller/v1/dev-1"
        );
        assert_eq!(
            endpoint("https://ddi.example").poll_url(),
            "https://ddi.example:443/controller/v1/dev-1"
        );
        assert_eq!(
            endpoint("https://ddi.example/t//").poll_url(),
            "https://ddi.example:443/t/controller/v1/dev-1"
        );
    }

    #[test]
    fn base_url_refusals_name_the_case() {
        use UrlRefusal::*;
        for (bad, case) in [
            ("", Syntax),
            ("ddi.example", Syntax),
            ("ftp://ddi.example", Scheme),
            ("https://", Host),
            ("https:///path", Host),
            ("https://user@ddi.example", ForbiddenChar),
            ("https://user:pass@ddi.example/t", ForbiddenChar),
            ("https://ddi.example/t?tenant=x", Path),
            ("https://ddi.example/t#frag", Path),
            ("https://ddi.example#frag", Host),
            ("https://ddi.example:0/t", Port),
            ("https://ddi.example:/t", Port),
            ("https://ddi.example:65536/t", Port),
            ("https://ddi.example:+80/t", Port),
            ("https://ddi.example:80:80/t", Port),
            ("https://ddi_example/t", Host),
            ("https://ddi.example./t", Host),
            ("https://1.2.3/t", Host),
            ("https://010.0.0.1/t", Host),
            ("https://256.1.1.1/t", Host),
            ("https://ddi.example/t%2F", ForbiddenChar),
            ("https://ddi.example/a b", ForbiddenChar),
            ("https://ddi.example/t\r\nX: y", ForbiddenChar),
            ("https://ddi.example\\@evil.example/", ForbiddenChar),
            ("https://[]/t", Host),
            ("https://[zz::1]/t", Host),
            ("https://[::1]x/t", Host),
            ("https://dd\u{e4}.example/t", ForbiddenChar),
        ] {
            assert_eq!(
                DdiEndpoint::new(bad, &id()),
                Err(EndpointError::InvalidBaseUrl(case)),
                "{bad:?}"
            );
            assert_eq!(DdiEndpoint::check_base_url(bad), Err(case), "{bad:?}");
        }
        let long = format!("https://ddi.example/{}", "a".repeat(MAX_BASE_URL_BYTES));
        assert_eq!(
            DdiEndpoint::new(&long, &id()),
            Err(EndpointError::InvalidBaseUrl(Length))
        );
        // The rebuilt base gains `:443`, so a base at the cap without a port is too long once rebuilt.
        let at_cap_no_port = format!(
            "https://ddi.example/{}",
            "a".repeat(MAX_BASE_URL_BYTES - "https://ddi.example/".len())
        );
        assert_eq!(DdiEndpoint::check_base_url(&at_cap_no_port), Err(Length));
        let at_cap = format!(
            "https://ddi.example:443/{}",
            "a".repeat(MAX_BASE_URL_BYTES - "https://ddi.example:443/".len())
        );
        assert!(DdiEndpoint::new(&at_cap, &id()).is_ok());
        let checked = DdiEndpoint::check_base_url(&at_cap).unwrap();
        assert_eq!(checked.as_str(), at_cap);
        assert_eq!(
            DdiEndpoint::check_base_url(checked.as_str()),
            Ok(checked.clone()),
            "a stored checked base passes again"
        );
    }

    #[test]
    fn a_last_label_that_is_a_number_in_any_base_is_no_dns_name() {
        let e = endpoint("http://ddi.example:8080");
        for host in [
            "0x7f000001",
            "127.0.0.0x1",
            "a.0X1F",
            "0x",
            "1.2.3",
            "a.b.123",
        ] {
            let url = format!("http://{host}:8080/x");
            assert_eq!(e.check_url(&url), Err(UrlRefusal::Host), "{url}");
            assert_eq!(
                DdiEndpoint::check_base_url(&url),
                Err(UrlRefusal::Host),
                "{url}"
            );
        }
        for host in ["0xg.example", "x0.example", "a.0xz", "ddi-0x1.example"] {
            let url = format!("http://{host}/x");
            assert!(DdiEndpoint::check_base_url(&url).is_ok(), "{url}");
        }
    }

    #[test]
    fn dot_segments_are_refused_anywhere_in_the_path() {
        let e = endpoint("http://ddi.example");
        for url in [
            "http://ddi.example/DEFAULT/controller/v1/dev-1/deploymentBase/5/../../../../controller/v1/dev-1/deploymentBase/6",
            "http://ddi.example/./x",
            "http://ddi.example/a/..",
            "http://ddi.example/..?c=1",
        ] {
            assert_eq!(e.check_url(url), Err(UrlRefusal::Path), "{url}");
            assert_eq!(e.refusal(url), Some(UrlRefusal::Path), "{url}");
        }
        assert_eq!(
            DdiEndpoint::check_base_url("http://ddi.example/a/../DEFAULT"),
            Err(UrlRefusal::Path)
        );
        let dotted = href(
            "http://ddi.example/DEFAULT/controller/v1/dev-1/deploymentBase/5/../../../../controller/v1/dev-1/deploymentBase/6",
        );
        assert_eq!(e.deployment_action(&dotted), None);
        for url in [
            "http://ddi.example/a.b/c..d/.e/f./x?q=../y",
            "http://ddi.example/x#../y",
        ] {
            assert!(e.check_url(url).is_ok(), "{url}");
        }
    }

    #[test]
    fn refusal_agrees_with_check_url() {
        let e = endpoint("https://ddi.example/DEFAULT");
        for url in [
            "https://ddi.example/DEFAULT/x",
            "https://DDI.example:443/x",
            "https://evil.example/x",
            "https://u@ddi.example/x",
            "ftp://ddi.example/x",
            "https://ddi.example:0/x",
        ] {
            assert_eq!(e.refusal(url), e.check_url(url).err(), "{url}");
        }
    }

    #[test]
    fn url_check_names_the_refusal_case() {
        use UrlRefusal::*;
        let e = endpoint("http://127.0.0.1:38080");
        for (url, case) in [
            ("http://u@127.0.0.1:38080/", ForbiddenChar),
            ("http://u:p@127.0.0.1:38080/x", ForbiddenChar),
            ("http://127.0.0.1:38080@evil.example/", ForbiddenChar),
            ("http://127.0.0.1:38080/a@b", ForbiddenChar),
            ("http://127.0.0.1:38080/?x=a@b", ForbiddenChar),
            ("http://127.0.0.1:38080/a%2e%2e", ForbiddenChar),
            (
                "http://127.0.0.1:38080/trap/%2e%2e/deploymentBase/1",
                ForbiddenChar,
            ),
            ("http://127.0.0.1:38080/x?c=%41", ForbiddenChar),
            ("http://127.0.0.1:38080\\a", ForbiddenChar),
            ("http://127.0.0.1:38080\\@evil.example/", ForbiddenChar),
            ("http://127.0.0.1:38080/a b", ForbiddenChar),
            ("http://127.0.0.1:38080/a\tb", ForbiddenChar),
            ("http://127.0.0.1:38080/\x01", ForbiddenChar),
            ("http://127.0.0.1:38080/x\r\nX: y", ForbiddenChar),
            ("http://127.0.0.1:38080/\u{7f}", ForbiddenChar),
            ("http://exämple.com/", ForbiddenChar),
            ("http://127.0.0.1:38080/ä", ForbiddenChar),
            (" http://127.0.0.1:38080/", ForbiddenChar),
            ("http://127.0.0.1:38080/x ", ForbiddenChar),
            ("", Syntax),
            ("127.0.0.1:38080/x", Syntax),
            ("//127.0.0.1:38080/x", Syntax),
            ("/DEFAULT/controller/v1/dev-1", Syntax),
            ("http:/127.0.0.1:38080/", Syntax),
            ("://127.0.0.1:38080/", Syntax),
            ("h1tp://127.0.0.1:38080/", Syntax),
            ("ftp://127.0.0.1:38080/", Scheme),
            ("mqtt://127.0.0.1:38080/", Scheme),
            ("httpx://127.0.0.1:38080/", Scheme),
            ("http://01.0.0.1:38080/", Host),
            ("http://010.0.0.1:38080/", Host),
            ("http://256.0.0.1:38080/", Host),
            ("http://1.2.3.4.5/", Host),
            ("http://1.2.3/", Host),
            ("http://2130706433/", Host),
            ("http://fleet.example./", Host),
            ("http://a..b/", Host),
            ("http://.a/", Host),
            ("http://-/", Host),
            ("http://ddi_example/", Host),
            ("http://[zz]/", Host),
            ("http://[]/", Host),
            ("http://[zz::1]/", Host),
            ("http://[::1]x/", Host),
            ("http://[::1]#f", Host),
            ("http://[::1/", Host),
            ("http://[1:2:3:4:5:6:7:8:9]/", Host),
            ("http:///x", Host),
            ("http://:38080/", Host),
            ("http://127.0.0.1#f", Host),
            ("http://127.0.0.1:0/", Port),
            ("http://127.0.0.1:65536/", Port),
            ("http://127.0.0.1:/", Port),
            ("http://127.0.0.1:", Port),
            ("http://127.0.0.1:+38080/", Port),
            ("http://127.0.0.1:38080:38080/", Port),
            ("http://127.0.0.1:038080/", Port),
            ("http://127.0.0.1:38o80/", Port),
            ("http://[::1]:/", Port),
            ("http://127.0.0.1:38080#f", Port),
            ("http://localhost:38080/", Origin),
            ("https://127.0.0.1:38080/", Origin),
            ("http://127.0.0.1:38081/", Origin),
            ("http://127.0.0.1/", Origin),
            ("http://127.0.0.1:80/", Origin),
            ("http://[::ffff:127.0.0.1]:38080/", Origin),
            ("http://127.0.0.1.example:38080/", Origin),
            ("http://127.0.0.2:38080/", Origin),
        ] {
            assert_eq!(e.check_url(url), Err(case), "{url:?}");
            assert!(!e.same_origin(url), "{url:?}");
        }
    }

    #[test]
    fn url_check_rebuilds_scheme_host_and_port_and_keeps_path_and_query() {
        for (base, url, rebuilt) in [
            (
                "http://127.0.0.1:38080",
                "http://127.0.0.1:38080/DEFAULT/controller/v1/d?c=-1",
                "http://127.0.0.1:38080/DEFAULT/controller/v1/d?c=-1",
            ),
            (
                "http://127.0.0.1:38080",
                "HTTP://127.0.0.1:38080",
                "http://127.0.0.1:38080",
            ),
            (
                "http://127.0.0.1:38080",
                "http://127.0.0.1:38080?x=1",
                "http://127.0.0.1:38080?x=1",
            ),
            (
                "http://127.0.0.1:38080",
                "Http://127.0.0.1:38080/A/b/.x..//c?Q=../V&r=;x#F",
                "http://127.0.0.1:38080/A/b/.x..//c?Q=../V&r=;x#F",
            ),
            (
                "http://fleet.example:80",
                "http://Fleet.Example/x",
                "http://fleet.example:80/x",
            ),
            (
                "http://fleet.example",
                "HTTP://FLEET.EXAMPLE:80/x",
                "http://fleet.example:80/x",
            ),
            (
                "https://ddi.example/DEFAULT",
                "https://DDI.Example/DEFAULT/controller/v1/dev-1/deploymentBase/7?c=1",
                "https://ddi.example:443/DEFAULT/controller/v1/dev-1/deploymentBase/7?c=1",
            ),
            (
                "https://ddi.example/DEFAULT",
                "https://ddi.example:443/other",
                "https://ddi.example:443/other",
            ),
            (
                "http://ddi.example:8080/t",
                "http://ddi.example:08080/x",
                "http://ddi.example:8080/x",
            ),
            (
                "https://[FD00::1]/t",
                "HTTPS://[fd00::1]/x",
                "https://[fd00::1]:443/x",
            ),
            (
                "https://[fd00::1]:8443/t",
                "https://[FD00::1]:8443/x",
                "https://[fd00::1]:8443/x",
            ),
            (
                "http://a.b-c.d1",
                "http://A.B-C.D1/x",
                "http://a.b-c.d1:80/x",
            ),
        ] {
            let checked = endpoint(base).check_url(url).unwrap();
            assert_eq!(checked.as_str(), rebuilt, "{url}");
            assert_eq!(checked, endpoint(base).check_url(rebuilt).unwrap(), "{url}");
        }
    }

    #[test]
    fn ipv6_literals_compare_as_lower_cased_text() {
        let e = endpoint("http://[fd00::1]:8080/DEFAULT");
        assert_eq!(
            e.check_url("http://[FD00::1]:8080/x").unwrap().as_str(),
            "http://[fd00::1]:8080/x"
        );
        for (url, case) in [
            ("http://[fd00::2]:8080/x", UrlRefusal::Origin),
            ("http://[fd00::1]/x", UrlRefusal::Origin),
            ("http://[fd00:0::1]:8080/x", UrlRefusal::Origin),
            ("http://[fd00::1%25eth0]:8080/x", UrlRefusal::ForbiddenChar),
            ("http://fd00::1:8080/x", UrlRefusal::Port),
        ] {
            assert_eq!(e.check_url(url), Err(case), "{url}");
        }
        let mapped = endpoint("http://[::ffff:10.0.0.1]/t");
        assert_eq!(
            mapped
                .check_url("http://[::FFFF:10.0.0.1]/x")
                .unwrap()
                .as_str(),
            "http://[::ffff:10.0.0.1]:80/x"
        );
        assert_eq!(
            mapped.check_url("http://10.0.0.1/x"),
            Err(UrlRefusal::Origin)
        );
    }

    #[test]
    fn an_ip_literal_never_equals_a_name() {
        let e = endpoint("http://localhost:8080/t");
        assert!(e.same_origin("http://LOCALHOST:8080/x"));
        for url in ["http://127.0.0.1:8080/x", "http://[::1]:8080/x"] {
            assert_eq!(e.check_url(url), Err(UrlRefusal::Origin), "{url}");
        }
    }

    #[test]
    fn same_origin_matches_scheme_host_and_effective_port() {
        let e = endpoint("https://ddi.example/DEFAULT");
        for same in [
            "https://ddi.example/DEFAULT/controller/v1/dev-1/deploymentBase/7?c=1",
            "https://ddi.example:443/other/path",
            "https://DDI.Example",
            "HTTPS://ddi.example/x",
            "https://ddi.example?x=1",
            "https://ddi.example/#f",
        ] {
            assert!(e.same_origin(same), "{same}");
        }
        for foreign in [
            "http://ddi.example/x",
            "https://ddi.example:8443/x",
            "https://ddi.example.evil/x",
            "https://evil.example/ddi.example",
            "https://ddi.example@evil.example/x",
            "https://evil.example@ddi.example/x",
            "https://ddi.example\\@evil.example/x",
            "https://ddi.example/a b",
            "https://ddi.example/\u{e4}",
            "https://ddi.example#f",
            "//ddi.example/x",
            "/DEFAULT/controller/v1/dev-1",
            "",
        ] {
            assert!(!e.same_origin(foreign), "{foreign:?}");
        }
    }

    #[test]
    fn default_ports_fill_in_per_scheme() {
        let e = endpoint("http://ddi.example:80/t");
        assert!(e.same_origin("http://ddi.example/x"));
        assert!(!e.same_origin("https://ddi.example:80/x"));
        let e = endpoint("http://ddi.example:8080/t");
        assert!(!e.same_origin("http://ddi.example/x"));
        assert!(e.same_origin("http://ddi.example:08080/x"));
    }

    #[test]
    fn check_href_returns_the_rebuilt_link_or_the_case() {
        let e = endpoint("https://ddi.example/DEFAULT");
        let on = href("https://DDI.example/DEFAULT/controller/v1/dev-1/deploymentBase/7?c=1");
        assert_eq!(
            e.check_href(&on).unwrap().as_str(),
            "https://ddi.example:443/DEFAULT/controller/v1/dev-1/deploymentBase/7?c=1"
        );
        let off = href("https://evil.example/DEFAULT/controller/v1/dev-1/deploymentBase/7");
        assert_eq!(e.check_href(&off), Err(UrlRefusal::Origin));
        let user = href("https://u@ddi.example/DEFAULT/controller/v1/dev-1/deploymentBase/7");
        assert_eq!(e.check_href(&user), Err(UrlRefusal::ForbiddenChar));
    }

    #[test]
    fn errors_and_checked_urls_carry_no_text() {
        use UrlRefusal::*;
        assert_eq!(
            format!("{}", EndpointError::InvalidBaseUrl(Host)),
            "DDI base URL invalid: host"
        );
        assert_eq!(
            format!("{}", EndpointError::InvalidControllerId),
            "DDI controller id invalid"
        );
        for (case, name) in [
            (ForbiddenChar, "forbidden_char"),
            (Syntax, "syntax"),
            (Scheme, "scheme"),
            (Host, "host"),
            (Port, "port"),
            (Origin, "origin"),
            (Length, "length"),
            (Path, "path"),
        ] {
            assert_eq!(format!("{case}"), name);
            assert_eq!(case.name(), name);
        }
        let checked = endpoint("https://ddi.example")
            .check_url("https://ddi.example/?token=x")
            .unwrap();
        assert_eq!(format!("{checked:?}"), "CheckedUrl(<redacted>)");
        assert_eq!(checked.into_string(), "https://ddi.example:443/?token=x");
    }
}
