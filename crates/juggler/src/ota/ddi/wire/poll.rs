//! The DDI base poll resource: the polling interval and the links to the pending resources.

use core::time::Duration;

use serde::Deserialize;

use super::bounded::{Href, Obj};
use super::error::DdiError;
use super::parse_object;

/// Experimental: API may change before 1.0.
///
/// The hawkBit DDI v1 base poll resource, reduced to what the device acts on.
/// Unknown fields and links are ignored, and a `config`, `_links` or link that is absent or `null` is `None` (serde reads a `null` optional field as absent).
/// A link whose `href` is empty or longer than [`MAX_HREF_BYTES`](super::MAX_HREF_BYTES) is refused, not malformed: it is `None` and [`links_refused`](Self::links_refused) is set, so the rest of the answer, the polling interval included, still counts.
///
/// `Debug` redacts every link because it may carry credentials or a token.
///
/// # Examples
///
/// ```
/// use core::time::Duration;
/// use juggler::ota::ddi::wire::PollResponse;
///
/// let body = br#"{"config":{"polling":{"sleep":"00:05:00"}},"_links":{"deploymentBase":{"href":"http://h/deploymentBase/12"}}}"#;
/// let poll = PollResponse::parse(body).unwrap();
/// assert_eq!(poll.sleep, Some(Duration::from_secs(300)));
/// assert_eq!(poll.deployment_base.unwrap().as_str(), "http://h/deploymentBase/12");
/// assert!(poll.cancel_action.is_none());
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PollResponse {
    /// `config.polling.sleep` parsed by [`parse_sleep`]; `None` when `config`, `polling` or `sleep` is absent or `null`.
    /// Not clamped here: the machine clamps it to 30 seconds .. 24 hours.
    pub sleep: Option<Duration>,
    /// `_links.deploymentBase.href`; `None` when the link is absent or `null`.
    pub deployment_base: Option<Href>,
    /// `_links.cancelAction.href`; `None` when the link is absent or `null`.
    pub cancel_action: Option<Href>,
    /// `_links.configData.href`; `None` when the link is absent or `null`.
    pub config_data: Option<Href>,
    /// A `deploymentBase`, `cancelAction` or `configData` link was present with an empty or over-long `href`; that link is `None`.
    pub links_refused: bool,
}

#[derive(Deserialize)]
struct RawPoll {
    config: Option<Obj<RawConfig>>,
    #[serde(rename = "_links")]
    links: Option<Obj<RawLinks>>,
}

#[derive(Deserialize)]
struct RawConfig {
    polling: Option<Obj<RawPolling>>,
}

#[derive(Deserialize)]
struct RawPolling {
    sleep: Option<alloc::string::String>,
}

#[derive(Deserialize)]
struct RawLinks {
    #[serde(rename = "deploymentBase")]
    deployment_base: Option<Obj<RawLink>>,
    #[serde(rename = "cancelAction")]
    cancel_action: Option<Obj<RawLink>>,
    #[serde(rename = "configData")]
    config_data: Option<Obj<RawLink>>,
}

#[derive(Deserialize)]
struct RawLink {
    href: LinkHref,
}

/// A poll link's `href` string: a usable [`Href`], or `None` when it is empty or over-long (a refused link).
///
/// The string is not capped while it is read; the body limit bounds it.
struct LinkHref(Option<Href>);

impl<'de> Deserialize<'de> for LinkHref {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = alloc::string::String::deserialize(deserializer)?;
        Ok(LinkHref(Href::from_text(text)))
    }
}

impl PollResponse {
    /// Parses a poll body.
    ///
    /// # Errors
    ///
    /// [`DdiError::TooLarge`] for more than [`MAX_RESPONSE_BYTES`](super::MAX_RESPONSE_BYTES) bytes (checked before parsing),
    /// [`DdiError::Malformed`] for anything that is not a JSON object of the expected shape (a link without an `href`, a non-string `href` or `sleep`),
    /// [`DdiError::InvalidSleep`] when `sleep` is not `HH:MM:SS`.
    pub fn parse(body: &[u8]) -> Result<Self, DdiError> {
        let raw: RawPoll = parse_object(body)?;
        let sleep = raw
            .config
            .and_then(|c| c.0.polling)
            .and_then(|p| p.0.sleep)
            .map(|s| parse_sleep(&s))
            .transpose()?;
        let (deployment_base, cancel_action, config_data) = match raw.links {
            Some(Obj(l)) => (l.deployment_base, l.cancel_action, l.config_data),
            None => (None, None, None),
        };
        let mut links_refused = false;
        let mut take = |link: Option<Obj<RawLink>>| {
            let href = link?.0.href.0;
            links_refused |= href.is_none();
            href
        };
        Ok(PollResponse {
            sleep,
            deployment_base: take(deployment_base),
            cancel_action: take(cancel_action),
            config_data: take(config_data),
            links_refused,
        })
    }
}

/// Experimental: API may change before 1.0.
///
/// Parses the `HH:MM:SS` polling interval of a poll response.
///
/// `HH` is one or more ASCII digits of any count (checked arithmetic), `MM` and `SS` are exactly two digits in `00..=59`.
/// Nothing else is accepted: no whitespace, sign, fraction or extra field.
/// The result is not clamped.
///
/// # Examples
///
/// ```
/// use core::time::Duration;
/// use juggler::ota::ddi::wire::{parse_sleep, DdiError};
///
/// assert_eq!(parse_sleep("00:05:00"), Ok(Duration::from_secs(300)));
/// assert_eq!(parse_sleep("5:00"), Err(DdiError::InvalidSleep));
/// ```
///
/// # Errors
///
/// [`DdiError::InvalidSleep`] for any other text, including an `HH` that overflows `u64` seconds.
pub fn parse_sleep(text: &str) -> Result<Duration, DdiError> {
    let mut parts = text.split(':');
    let (Some(hours), Some(minutes), Some(seconds), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(DdiError::InvalidSleep);
    };
    let hours = digits(hours.as_bytes()).ok_or(DdiError::InvalidSleep)?;
    let minutes = two_digits(minutes).ok_or(DdiError::InvalidSleep)?;
    let seconds = two_digits(seconds).ok_or(DdiError::InvalidSleep)?;
    hours
        .checked_mul(3600)
        .and_then(|h| h.checked_add(minutes * 60 + seconds))
        .map(Duration::from_secs)
        .ok_or(DdiError::InvalidSleep)
}

/// One or more ASCII digits as a `u64`, `None` on any other byte, an empty text or an overflow.
///
/// The one digit parser of `ota::ddi`: the poll interval here, the `Retry-After` value and HTTP date in `schedule`.
pub(in crate::ota::ddi) fn digits(text: &[u8]) -> Option<u64> {
    if text.is_empty() {
        return None;
    }
    text.iter().try_fold(0u64, |acc, &b| {
        if b.is_ascii_digit() {
            acc.checked_mul(10)?.checked_add(u64::from(b - b'0'))
        } else {
            None
        }
    })
}

/// Exactly two ASCII digits with a value of at most 59.
fn two_digits(text: &str) -> Option<u64> {
    if text.len() != 2 {
        return None;
    }
    digits(text.as_bytes()).filter(|&n| n <= 59)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ota::ddi::wire::MAX_HREF_BYTES;
    use alloc::format;
    use alloc::string::String;

    const FULL: &str = r#"{"config":{"polling":{"sleep":"00:05:00"}},"_links":{"deploymentBase":{"href":"http://h/db/12"},"cancelAction":{"href":"http://h/ca/11"},"configData":{"href":"http://h/cd"}}}"#;

    #[test]
    fn full_document_parses_every_field() {
        let p = PollResponse::parse(FULL.as_bytes()).unwrap();
        assert_eq!(p.sleep, Some(Duration::from_secs(300)));
        assert_eq!(p.deployment_base.unwrap().as_str(), "http://h/db/12");
        assert_eq!(p.cancel_action.unwrap().as_str(), "http://h/ca/11");
        assert_eq!(p.config_data.unwrap().as_str(), "http://h/cd");
    }

    #[test]
    fn missing_config_and_links_are_none() {
        let p = PollResponse::parse(b"{}").unwrap();
        assert_eq!(
            p,
            PollResponse {
                sleep: None,
                deployment_base: None,
                cancel_action: None,
                config_data: None,
                links_refused: false,
            }
        );
    }

    #[test]
    fn missing_polling_and_sleep_are_none() {
        assert_eq!(
            PollResponse::parse(br#"{"config":{}}"#).unwrap().sleep,
            None
        );
        assert_eq!(
            PollResponse::parse(br#"{"config":{"polling":{}}}"#)
                .unwrap()
                .sleep,
            None
        );
    }

    #[test]
    fn each_link_is_independent() {
        let p = PollResponse::parse(br#"{"_links":{"cancelAction":{"href":"c"}}}"#).unwrap();
        assert!(p.deployment_base.is_none());
        assert_eq!(p.cancel_action.unwrap().as_str(), "c");
        assert!(p.config_data.is_none());
        let p = PollResponse::parse(br#"{"_links":{"configData":{"href":"d"}}}"#).unwrap();
        assert_eq!(p.config_data.unwrap().as_str(), "d");
        assert!(p.cancel_action.is_none());
        let p = PollResponse::parse(br#"{"_links":{}}"#).unwrap();
        assert!(p.deployment_base.is_none());
    }

    #[test]
    fn unknown_fields_and_links_are_ignored() {
        let body = br#"{"x":[1,2],"config":{"registration":{"a":1},"polling":{"sleep":"00:00:30","y":null}},"_links":{"self":{"href":"s"},"deploymentBase":{"href":"d","title":"t"},"other":7}}"#;
        let p = PollResponse::parse(body).unwrap();
        assert_eq!(p.sleep, Some(Duration::from_secs(30)));
        assert_eq!(p.deployment_base.unwrap().as_str(), "d");
    }

    #[test]
    fn a_link_without_href_is_malformed() {
        let r = PollResponse::parse(br#"{"_links":{"deploymentBase":{}}}"#);
        assert!(matches!(r, Err(DdiError::Malformed { .. })));
    }

    #[test]
    fn parse_sleep_accepts_valid_forms() {
        assert_eq!(parse_sleep("00:00:30"), Ok(Duration::from_secs(30)));
        assert_eq!(parse_sleep("01:00:00"), Ok(Duration::from_secs(3600)));
        assert_eq!(parse_sleep("24:00:00"), Ok(Duration::from_secs(86_400)));
        assert_eq!(parse_sleep("100:00:00"), Ok(Duration::from_secs(360_000)));
        assert_eq!(parse_sleep("1:00:00"), Ok(Duration::from_secs(3600)));
        assert_eq!(parse_sleep("00:59:59"), Ok(Duration::from_secs(3599)));
        assert_eq!(parse_sleep("00:00:00"), Ok(Duration::ZERO));
    }

    #[test]
    fn parse_sleep_rejects_everything_else() {
        let long = "9".repeat(25);
        let overflow = format!("{long}:00:00");
        for text in [
            "",
            "00:60:00",
            "00:00:60",
            "00:0:00",
            "00:00:0",
            "0:00",
            "00:00:00:00",
            " 00:00:30",
            "00:00:30 ",
            "+1:00:00",
            "-1:00:00",
            "aa:bb:cc",
            "00:00:30.5",
            ":00:00",
            "::",
            "00:000:00",
            "\u{661}:00:00",
            overflow.as_str(),
        ] {
            assert_eq!(parse_sleep(text), Err(DdiError::InvalidSleep), "{text:?}");
        }
    }

    #[test]
    fn parse_sleep_overflow_boundary() {
        let max_hours = u64::MAX / 3600;
        assert!(parse_sleep(&format!("{max_hours}:00:00")).is_ok());
        assert_eq!(
            parse_sleep(&format!("{}:00:00", max_hours + 1)),
            Err(DdiError::InvalidSleep)
        );
    }

    #[test]
    fn invalid_sleep_surfaces_as_invalid_sleep() {
        let r = PollResponse::parse(br#"{"config":{"polling":{"sleep":"soon"}}}"#);
        assert_eq!(r, Err(DdiError::InvalidSleep));
    }

    #[test]
    fn non_string_sleep_is_malformed() {
        for body in [
            r#"{"config":{"polling":{"sleep":300}}}"#,
            r#"{"config":{"polling":{"sleep":true}}}"#,
            r#"{"config":{"polling":{"sleep":["00:05:00"]}}}"#,
        ] {
            assert!(
                matches!(
                    PollResponse::parse(body.as_bytes()),
                    Err(DdiError::Malformed { .. })
                ),
                "{body}"
            );
        }
    }

    fn with_href(href: &str) -> String {
        format!(r#"{{"_links":{{"deploymentBase":{{"href":"{href}"}}}}}}"#)
    }

    #[test]
    fn href_cap_is_exact() {
        let at = "h".repeat(MAX_HREF_BYTES);
        let p = PollResponse::parse(with_href(&at).as_bytes()).unwrap();
        assert!(p.deployment_base.is_some() && !p.links_refused);
        let over = "h".repeat(MAX_HREF_BYTES + 1);
        let p = PollResponse::parse(with_href(&over).as_bytes()).unwrap();
        assert!(p.deployment_base.is_none() && p.links_refused);
    }

    #[test]
    fn empty_href_is_a_refused_link() {
        let p = PollResponse::parse(with_href("").as_bytes()).unwrap();
        assert!(p.deployment_base.is_none() && p.links_refused);
    }

    #[test]
    fn a_refused_link_keeps_the_sleep_and_the_other_links() {
        let body = r#"{"config":{"polling":{"sleep":"00:01:00"}},"_links":{"deploymentBase":{"href":""},"configData":{"href":"http://h/cd"}}}"#;
        let p = PollResponse::parse(body.as_bytes()).unwrap();
        assert_eq!(p.sleep, Some(Duration::from_secs(60)));
        assert!(p.deployment_base.is_none());
        assert_eq!(p.config_data.unwrap().as_str(), "http://h/cd");
        assert!(p.links_refused);
    }

    #[test]
    fn a_non_string_href_is_still_malformed() {
        for body in [
            r#"{"_links":{"deploymentBase":{"href":7}}}"#,
            r#"{"_links":{"deploymentBase":{"href":null}}}"#,
            r#"{"_links":{"deploymentBase":{"href":["x"]}}}"#,
        ] {
            assert!(
                matches!(
                    PollResponse::parse(body.as_bytes()),
                    Err(DdiError::Malformed { .. })
                ),
                "{body}"
            );
        }
    }

    #[test]
    fn nested_objects_in_array_form_are_malformed() {
        for body in [
            r#"{"_links":{"deploymentBase":["http://h/x"]}}"#,
            r#"{"_links":{"cancelAction":["x"]}}"#,
            r#"{"_links":{"configData":["x"]}}"#,
            r#"{"_links":["x"]}"#,
            r#"{"config":["x"]}"#,
            r#"{"config":{"polling":["00:05:00"]}}"#,
        ] {
            assert!(
                matches!(
                    PollResponse::parse(body.as_bytes()),
                    Err(DdiError::Malformed { .. })
                ),
                "{body}"
            );
        }
    }

    #[test]
    fn duplicate_fields_are_malformed() {
        for body in [
            r#"{"_links":{"deploymentBase":{"href":"a"},"deploymentBase":{"href":"b"}}}"#,
            r#"{"_links":{"deploymentBase":{"href":"a","href":"b"}}}"#,
            r#"{"config":{"polling":{"sleep":"00:05:00","sleep":"00:06:00"}}}"#,
        ] {
            assert!(
                matches!(
                    PollResponse::parse(body.as_bytes()),
                    Err(DdiError::Malformed { .. })
                ),
                "{body}"
            );
        }
    }

    #[test]
    fn escaped_href_cap_applies_to_the_decoded_text() {
        let at = r"\u0041".repeat(MAX_HREF_BYTES);
        let p = PollResponse::parse(with_href(&at).as_bytes()).unwrap();
        assert_eq!(p.deployment_base.unwrap().as_str().len(), MAX_HREF_BYTES);
        let over = r"\u0041".repeat(MAX_HREF_BYTES + 1);
        let p = PollResponse::parse(with_href(&over).as_bytes()).unwrap();
        assert!(p.deployment_base.is_none() && p.links_refused);
    }

    #[test]
    fn oversized_body_is_too_large_before_parsing() {
        let body = alloc::vec![b'x'; crate::ota::ddi::wire::MAX_RESPONSE_BYTES + 1];
        assert_eq!(PollResponse::parse(&body), Err(DdiError::TooLarge));
    }

    #[test]
    fn array_and_non_object_bodies_are_malformed_at_the_first_token() {
        assert_eq!(
            PollResponse::parse(b"\n  [1]"),
            Err(DdiError::Malformed { line: 2, column: 3 })
        );
        assert_eq!(
            PollResponse::parse(b""),
            Err(DdiError::Malformed { line: 1, column: 1 })
        );
        assert!(matches!(
            PollResponse::parse(b"null"),
            Err(DdiError::Malformed { .. })
        ));
    }

    #[test]
    fn leading_whitespace_is_accepted() {
        assert!(PollResponse::parse(b" \n\t {}").is_ok());
    }

    #[test]
    fn garbage_never_panics() {
        let corpus: [&[u8]; 8] = [
            b"",
            b"{",
            b"{\"config\":",
            b"\xff\xfe{}",
            b"{\"config\":{\"polling\":{\"sleep\":\"\xff\"}}}",
            b"{}{}",
            b"{} x",
            b"\x00\x00",
        ];
        for body in corpus {
            let _ = PollResponse::parse(body);
        }
    }

    #[test]
    fn debug_and_errors_never_contain_payload_text() {
        let body = r#"{"config":{"polling":{"sleep":"00:05:00"}},"_links":{"deploymentBase":{"href":"http://u:secretpass@h/x"}}}"#;
        let p = PollResponse::parse(body.as_bytes()).unwrap();
        assert!(!format!("{p:?}").contains("secretpass"));
        let refused = format!(
            r#"{{"_links":{{"deploymentBase":{{"href":"secretpass{}"}}}}}}"#,
            "x".repeat(600)
        );
        let p = PollResponse::parse(refused.as_bytes()).unwrap();
        assert!(!format!("{p:?}").contains("secretpass"));
        let bad = [
            r#"{"config":{"polling":{"sleep":"secretpass"}}}"#.into(),
            r#"{"config":{"polling":{"sleep":5,"secretpass":1}}}"#.into(),
            String::from(r#"{"_links":{"deploymentBase":{"href":5,"secretpass":1}}}"#),
            String::from(r#"secretpass"#),
        ];
        for body in bad {
            let e = PollResponse::parse(body.as_bytes()).unwrap_err();
            assert!(!format!("{e}").contains("secretpass"));
            assert!(!format!("{e:?}").contains("secretpass"));
        }
    }
}
