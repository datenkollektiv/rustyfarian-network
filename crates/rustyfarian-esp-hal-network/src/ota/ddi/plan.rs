//! The decisions of the esp-hal DDI transport, without I/O: what a configuration is accepted as, which URL and body each endpoint method requests, and the gates of an artifact download.
//!
//! The async transport (`transport.rs`) only performs what these functions planned, so the origin binding and the URL rules are host-tested here (ADR 027).

use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};
use core::time::Duration;

use juggler::ota::ddi::wire::{ActionId, ConfigData, Feedback, Href};
use juggler::ota::ddi::{
    check_scheme, CheckedUrl, ControllerId, DdiEndpoint, DdiToken, DdiTransportConfig,
    DdiTransportError, EndpointError, Scheme, MAX_REQUEST_BYTES,
};
use juggler::ota::download::{DownloadMachine, ImageChip};
use juggler::ota::{Deadline, UrlForLog};

use crate::ota::http::ddi::Method;

/// Set once the plain-HTTP WARN has been logged; statics reset on every boot.
///
/// A load and a store, not a swap: the ESP32-C3 has no atomic read-modify-write; two transports built at once may both warn, which is harmless.
static INSECURE_HTTP_WARNED: AtomicBool = AtomicBool::new(false);

/// An accepted [`DdiTransportConfig`].
pub(crate) struct Accepted {
    pub(crate) endpoint: DdiEndpoint,
    pub(crate) token: DdiToken,
    pub(crate) timeout: Duration,
    pub(crate) deadline: Duration,
}

/// Validates the controller id and the base URL, then applies the scheme rule ([`check_scheme`]).
///
/// A refused base URL is logged as a WARN by its case, never by its text; an accepted plain `http://` base logs a WARN on the first transport of the boot.
pub(crate) fn accept(
    config: DdiTransportConfig,
    plain_http_allowed: bool,
) -> Result<Accepted, DdiTransportError> {
    let controller_id = ControllerId::parse(&config.controller_id)?;
    let endpoint = DdiEndpoint::new(&config.base_url, &controller_id).inspect_err(|e| {
        if let EndpointError::InvalidBaseUrl(case) = e {
            log::warn!("DDI base URL refused: {case}");
        }
    })?;
    check_scheme(endpoint.scheme(), plain_http_allowed)?;
    if endpoint.scheme() == Scheme::Http && !INSECURE_HTTP_WARNED.load(Ordering::Relaxed) {
        INSECURE_HTTP_WARNED.store(true, Ordering::Relaxed);
        log::warn!(
            "DDI transport uses insecure HTTP: {} (ota-ddi-insecure-http lab build; logged once per boot)",
            UrlForLog(&config.base_url)
        );
    }
    Ok(Accepted {
        endpoint,
        token: config.token,
        timeout: config.timeout,
        deadline: config.deadline,
    })
}

/// One planned request: the method, the URL to send it to and the JSON body of a `POST` or `PUT`.
///
/// The URL is built from the configured base or rebuilt from a link that passed the origin check; it may carry a token in its query, so it is logged only through `UrlForLog`.
pub(crate) struct Planned {
    pub(crate) method: Method,
    pub(crate) url: String,
    pub(crate) body: Option<Vec<u8>>,
}

/// GET of the controller base resource.
pub(crate) fn poll(endpoint: &DdiEndpoint) -> Planned {
    Planned {
        method: Method::Get,
        url: String::from(endpoint.poll_url()),
        body: None,
    }
}

/// The URL to request for `href` when it is on the configured origin, rebuilt from the checked parts; a refusal is logged by its case, never by the link, and no request is made.
pub(crate) fn checked_link(
    endpoint: &DdiEndpoint,
    href: &Href,
) -> Result<CheckedUrl, DdiTransportError> {
    endpoint.check_href(href).map_err(|case| {
        log::warn!("DDI link refused: {case}");
        DdiTransportError::ForeignOrigin
    })
}

/// GET of a `deploymentBase` or `cancelAction` link, after the origin check.
pub(crate) fn link(endpoint: &DdiEndpoint, href: &Href) -> Result<Planned, DdiTransportError> {
    Ok(Planned {
        method: Method::Get,
        url: checked_link(endpoint, href)?.into_string(),
        body: None,
    })
}

/// POST of a deployment feedback to `<controller>/deploymentBase/<action>/feedback`, never to a link.
pub(crate) fn deployment_feedback(
    endpoint: &DdiEndpoint,
    action: &ActionId,
    feedback: &Feedback,
) -> Result<Planned, DdiTransportError> {
    let body = feedback.encode().map_err(DdiTransportError::Feedback)?;
    with_body(Method::Post, endpoint.deployment_feedback_url(action), body)
}

/// POST of a cancel feedback to `<controller>/cancelAction/<action>/feedback`, never to a link.
pub(crate) fn cancel_feedback(
    endpoint: &DdiEndpoint,
    action: &ActionId,
    feedback: &Feedback,
) -> Result<Planned, DdiTransportError> {
    let body = feedback.encode().map_err(DdiTransportError::Feedback)?;
    with_body(Method::Post, endpoint.cancel_feedback_url(action), body)
}

/// PUT of the configData body to `<controller>/configData`.
pub(crate) fn config_data(
    endpoint: &DdiEndpoint,
    data: &ConfigData,
) -> Result<Planned, DdiTransportError> {
    with_body(Method::Put, endpoint.config_data_url(), data.encode())
}

/// A request with a body; a body over [`MAX_REQUEST_BYTES`] is [`DdiTransportError::RequestTooLarge`] before any I/O.
fn with_body(method: Method, url: String, body: String) -> Result<Planned, DdiTransportError> {
    if body.len() > MAX_REQUEST_BYTES {
        return Err(DdiTransportError::RequestTooLarge);
    }
    Ok(Planned {
        method,
        url,
        body: Some(body.into_bytes()),
    })
}

/// The gates of a DDI artifact download: the artifact's declared `size` and the chip the image must be built for.
///
/// Both are required: this tier's activation never reads the image header, so the download always checks the chip (fleet DDI profile, `TARGET_CHIP`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DdiGate {
    pub(crate) size: u64,
    pub(crate) chip: ImageChip,
}

impl DdiGate {
    /// The download machine of one artifact fetch: the declared `Content-Length` must equal [`size`](Self::size) and the image header must name [`chip`](Self::chip), both before any flash is touched.
    pub(crate) fn machine(
        self,
        deadline: Deadline,
        per_op: Duration,
        sha256: [u8; 32],
    ) -> DownloadMachine {
        DownloadMachine::new(deadline, per_op, sha256)
            .with_expected_size(self.size)
            .with_expected_chip(self.chip)
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use juggler::ota::ddi::wire::{Board, Chip, Execution, Finished, PollResponse};
    use juggler::ota::ddi::UrlRefusal;
    use juggler::ota::download::{DownloadAction, DownloadInput, IMAGE_HEADER_LEN};
    use juggler::ota::{OtaError, Version};

    /// A test token generated at run time (no credential literal, see CLAUDE.md "CodeQL").
    fn token() -> DdiToken {
        use core::hash::{BuildHasher, Hasher};
        let mut text = String::new();
        let state = std::collections::hash_map::RandomState::new();
        while text.len() < 32 {
            let mut h = state.build_hasher();
            h.write_usize(text.len());
            text.push_str(&alloc::format!("{:016x}", h.finish()));
        }
        DdiToken::parse(&text[..32]).unwrap()
    }

    fn config(base: &str) -> DdiTransportConfig {
        DdiTransportConfig::new(base, "dev-1", token())
    }

    fn endpoint() -> DdiEndpoint {
        accept(config("http://10.0.0.2:8080/DEFAULT"), true)
            .ok()
            .unwrap()
            .endpoint
    }

    fn href(url: &str) -> Href {
        let body = alloc::format!(r#"{{"_links":{{"deploymentBase":{{"href":"{url}"}}}}}}"#);
        PollResponse::parse(body.as_bytes())
            .unwrap()
            .deployment_base
            .unwrap()
    }

    fn feedback() -> Feedback<'static> {
        Feedback {
            execution: Execution::Proceeding,
            finished: Finished::None,
            code: None,
            details: &[],
            timestamp: None,
        }
    }

    #[test]
    fn scheme_rule() {
        assert!(matches!(
            accept(config("https://ddi.example/DEFAULT"), true),
            Err(DdiTransportError::TlsUnavailable)
        ));
        assert!(matches!(
            accept(config("http://ddi.example/DEFAULT"), false),
            Err(DdiTransportError::InsecureHttp)
        ));
        let accepted = accept(
            config("http://ddi.example/DEFAULT").with_timeout(Duration::from_secs(3)),
            true,
        )
        .ok()
        .unwrap();
        assert_eq!(accepted.timeout, Duration::from_secs(3));
        assert_eq!(accepted.deadline, juggler::ota::ddi::DEFAULT_DEADLINE);
        assert!(!alloc::format!("{:?}", accepted.token).contains("0"));
    }

    #[test]
    fn invalid_configurations_are_refused() {
        assert!(matches!(
            accept(config("http://u@ddi.example/DEFAULT"), true),
            Err(DdiTransportError::InvalidBaseUrl(UrlRefusal::ForbiddenChar))
        ));
        assert!(matches!(
            accept(
                DdiTransportConfig::new("http://h/DEFAULT", "Dev", token()),
                true
            ),
            Err(DdiTransportError::InvalidControllerId)
        ));
    }

    #[test]
    fn poll_goes_to_the_controller_resource() {
        let planned = poll(&endpoint());
        assert_eq!(planned.method, Method::Get);
        assert_eq!(
            planned.url,
            "http://10.0.0.2:8080/DEFAULT/controller/v1/dev-1"
        );
        assert!(planned.body.is_none());
    }

    #[test]
    fn a_foreign_link_plans_no_request() {
        let e = endpoint();
        for url in [
            "http://10.0.0.3:8080/DEFAULT/controller/v1/dev-1/deploymentBase/1",
            "http://10.0.0.2:8081/DEFAULT/controller/v1/dev-1/deploymentBase/1",
            "http://10.0.0.2/DEFAULT/controller/v1/dev-1/deploymentBase/1",
            "https://10.0.0.2:8080/DEFAULT/controller/v1/dev-1/deploymentBase/1",
            "http://evil.example:8080/DEFAULT/controller/v1/dev-1/deploymentBase/1",
        ] {
            assert!(
                matches!(link(&e, &href(url)), Err(DdiTransportError::ForeignOrigin)),
                "{url}"
            );
        }
    }

    #[test]
    fn an_accepted_link_is_requested_at_the_rebuilt_url() {
        let planned = link(
            &endpoint(),
            &href("HTTP://10.0.0.2:8080/DEFAULT/controller/v1/dev-1/deploymentBase/7?c=-1"),
        )
        .ok()
        .unwrap();
        assert_eq!(planned.method, Method::Get);
        assert_eq!(
            planned.url,
            "http://10.0.0.2:8080/DEFAULT/controller/v1/dev-1/deploymentBase/7?c=-1"
        );
    }

    #[test]
    fn feedback_and_config_data_urls_come_from_the_base_and_the_action() {
        let e = endpoint();
        let action = ActionId::parse("42").unwrap();
        let planned = deployment_feedback(&e, &action, &feedback()).ok().unwrap();
        assert_eq!(planned.method, Method::Post);
        assert_eq!(
            planned.url,
            "http://10.0.0.2:8080/DEFAULT/controller/v1/dev-1/deploymentBase/42/feedback"
        );
        assert!(!planned.body.unwrap().is_empty());

        let planned = cancel_feedback(&e, &action, &feedback()).ok().unwrap();
        assert_eq!(planned.method, Method::Post);
        assert_eq!(
            planned.url,
            "http://10.0.0.2:8080/DEFAULT/controller/v1/dev-1/cancelAction/42/feedback"
        );

        let chip = Chip::parse("esp32c3").unwrap();
        let board = Board::parse("c3-supermini").unwrap();
        let data = ConfigData {
            chip: &chip,
            board: &board,
            version: Version::new(1, 0, 0),
        };
        let planned = config_data(&e, &data).ok().unwrap();
        assert_eq!(planned.method, Method::Put);
        assert_eq!(
            planned.url,
            "http://10.0.0.2:8080/DEFAULT/controller/v1/dev-1/configData"
        );
        assert_eq!(planned.body.unwrap(), data.encode().into_bytes());
    }

    #[test]
    fn an_oversize_body_is_refused_before_any_io() {
        let body = "x".repeat(MAX_REQUEST_BYTES + 1);
        assert!(matches!(
            with_body(Method::Post, String::from("http://h:80/x"), body),
            Err(DdiTransportError::RequestTooLarge)
        ));
        let body = "x".repeat(MAX_REQUEST_BYTES);
        assert!(with_body(Method::Post, String::from("http://h:80/x"), body).is_ok());
    }

    fn drive_to_headers(gate: DdiGate, content_length: u64) -> DownloadAction {
        let mut machine = gate.machine(Deadline::none(), Duration::from_secs(5), [0; 32]);
        let connect = machine.poll(DownloadInput::Begin, Duration::ZERO);
        assert!(matches!(connect, DownloadAction::Connect { .. }));
        machine.poll(
            DownloadInput::Headers {
                content_length: Some(content_length),
                chunked: false,
                capacity: 1 << 20,
            },
            Duration::ZERO,
        )
    }

    #[test]
    fn a_size_mismatch_fails_before_any_flash_write() {
        let gate = DdiGate {
            size: 4096,
            chip: ImageChip::Esp32C3,
        };
        assert!(matches!(
            drive_to_headers(gate, 4097),
            DownloadAction::Done(Err(OtaError::SizeMismatch))
        ));
    }

    #[test]
    fn the_gate_always_checks_the_chip() {
        let gate = DdiGate {
            size: 4096,
            chip: ImageChip::Esp32C6,
        };
        assert!(matches!(
            drive_to_headers(gate, 4096),
            DownloadAction::ReadHeader { max, .. } if max as usize == IMAGE_HEADER_LEN
        ));
        let mut machine = gate.machine(Deadline::none(), Duration::from_secs(5), [0; 32]);
        machine.poll(DownloadInput::Begin, Duration::ZERO);
        machine.poll(
            DownloadInput::Headers {
                content_length: Some(4096),
                chunked: false,
                capacity: 1 << 20,
            },
            Duration::ZERO,
        );
        let mut header = [0u8; IMAGE_HEADER_LEN];
        header[0] = 0xE9;
        header[12] = ImageChip::Esp32C3.id().to_le_bytes()[0];
        assert!(matches!(
            machine.poll(DownloadInput::header_read(&header), Duration::ZERO),
            DownloadAction::Done(Err(OtaError::TargetMismatch))
        ));
    }
}
