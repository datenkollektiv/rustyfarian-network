//! The `pub(crate)` captive-portal HTTP server.
//!
//! Wires an [`EspHttpServer`] with the four functional routes (`GET /`,
//! `POST /save`, `GET /status`, `POST /factory-reset`) plus a set of OS
//! captive-portal probe routes that redirect to the portal root. The HTML is
//! embedded via [`include_str!`].
//!
//! # Handlers run on the httpd task
//!
//! Every handler closure — and the user's event callback invoked from inside
//! them — runs on the ESP-IDF `httpd` task. Following the same rule as
//! `MqttBuilder::on_connect`, handlers must return quickly and never block:
//! blocking the httpd task stalls every other request and can wedge the
//! captive-portal flow. The handlers here only touch an in-memory `Mutex`, the
//! NVS store, and the event callback, all of which return promptly.
//!
//! # Secrets
//!
//! Pre-fill substitutes only non-secret fields into the HTML. The Wi-Fi
//! password and AppKey inputs are always rendered empty and must be re-entered
//! on every submission (single-shot commit). No credential value is ever logged
//! or echoed.

use std::sync::Arc;

use anyhow::Context as _;

use embedded_svc::io::{Read, Write};
use esp_idf_svc::http::server::{Configuration, EspHttpServer};
use esp_idf_svc::http::Method;

use provisioning_pure::{parse_form, Field, FieldErrors, ProvisioningInput};

use crate::{ProvisioningEvent, SharedState};
use crate::store::ProvisioningStore;

/// The embedded portal HTML template.
const PORTAL_HTML: &str = include_str!("portal.html");

/// HTTP port the portal listens on.
const HTTP_PORT: u16 = 80;

/// httpd worker stack size (bytes). The form handler parses a small body and
/// touches NVS; 10 KB is comfortable headroom over the ESP-IDF default.
const HTTPD_STACK_SIZE: usize = 10240;

/// Explicit URI-handler budget. Counted against the registrations below
/// (4 functional + 6 probe routes = 10); 12 leaves headroom for one or two
/// future probe endpoints without another audit.
const MAX_URI_HANDLERS: usize = 12;

/// Largest POST body the portal will buffer (bytes). A well-formed submission
/// is well under 1 KB; 2 KB is a generous cap. Larger bodies get `413`.
const MAX_BODY_LEN: usize = 2048;

/// OS captive-portal probe paths answered with a `302` to the portal root.
///
/// This list is illustrative and expected to evolve as OS behaviour changes; it
/// is not a frozen contract.
const PROBE_PATHS: [&str; 6] = [
    "/generate_204",
    "/gen_204",
    "/hotspot-detect.html",
    "/ncsi.txt",
    "/connecttest.txt",
    "/canonical.html",
];

/// Builds and starts the portal HTTP server.
///
/// `ap_ip` backs the probe-route redirects; `nonce` is the per-session token
/// embedded in the form and required on every mutating POST; `state` and
/// `store` are shared with the session; `on_event` and `status_entries` come
/// from the builder.
#[allow(clippy::too_many_arguments)]
pub(crate) fn start(
    ap_ip: std::net::Ipv4Addr,
    nonce: Arc<String>,
    state: SharedState,
    store: Arc<std::sync::Mutex<ProvisioningStore>>,
    on_event: Arc<dyn Fn(ProvisioningEvent) + Send + Sync + 'static>,
    device_name: Arc<String>,
    firmware_version: Arc<String>,
    status_entries: Arc<Vec<(String, String)>>,
) -> anyhow::Result<EspHttpServer<'static>> {
    let config = Configuration {
        http_port: HTTP_PORT,
        max_uri_handlers: MAX_URI_HANDLERS,
        stack_size: HTTPD_STACK_SIZE,
        ..Default::default()
    };

    let mut server = EspHttpServer::new(&config).context("failed to start captive-portal httpd")?;

    {
        let state = state.clone();
        let store_for_load = store.clone();
        let nonce = nonce.clone();
        server.fn_handler("/", Method::Get, move |request| {
            let prefill = load_prefill(&store_for_load);
            let html = render_form(&nonce, &prefill, &FieldErrors::new());
            let cur = state.current();
            log::debug!("GET / (state={})", cur.as_str());
            let mut response = request.into_ok_response()?;
            response.write_all(html.as_bytes())?;
            Ok::<(), anyhow::Error>(())
        })?;
    }

    {
        let state = state.clone();
        let store = store.clone();
        let nonce = nonce.clone();
        let on_event = on_event.clone();
        server.fn_handler("/save", Method::Post, move |mut request| {
            let body = match read_body(&mut request) {
                BodyRead::Ok(b) => b,
                BodyRead::TooLarge => {
                    let errors = form_level_errors();
                    let html = render_form(&nonce, &Prefill::empty(), &errors);
                    let mut response = request.into_status_response(413)?;
                    response.write_all(html.as_bytes())?;
                    return Ok(());
                }
            };

            if !nonce_matches(&body, &nonce) {
                log::warn!("POST /save rejected: nonce mismatch");
                let mut response = request.into_status_response(403)?;
                response.write_all(b"Forbidden: session token mismatch.")?;
                return Ok(());
            }

            match parse_form(&body) {
                Ok(config) => {
                    state.apply(ProvisioningInput::ValidSubmission);
                    let persist = store
                        .lock()
                        .map_err(|_| anyhow::anyhow!("store mutex poisoned"))
                        .and_then(|mut s| s.save(&config));
                    match persist {
                        Ok(()) => {
                            state.apply(ProvisioningInput::PersistOk);
                            state.set_committed(config);
                            (on_event)(ProvisioningEvent::Committed);
                            let mut response = request.into_ok_response()?;
                            response.write_all(COMMITTED_HTML.as_bytes())?;
                        }
                        Err(e) => {
                            log::error!("POST /save persist failed: {e:#}");
                            state.apply(ProvisioningInput::PersistFailed);
                            let errors = form_level_errors();
                            let html = render_form(&nonce, &Prefill::empty(), &errors);
                            let mut response = request.into_status_response(500)?;
                            response.write_all(html.as_bytes())?;
                        }
                    }
                }
                Err(errors) => {
                    state.apply(ProvisioningInput::InvalidSubmission);
                    (on_event)(ProvisioningEvent::SubmissionRejected);
                    log::info!("POST /save rejected: {} field error(s)", errors.len());
                    let html = render_form(&nonce, &Prefill::empty(), &errors);
                    let mut response = request.into_status_response(400)?;
                    response.write_all(html.as_bytes())?;
                }
            }
            Ok::<(), anyhow::Error>(())
        })?;
    }

    {
        let state = state.clone();
        let store_for_status = store.clone();
        let device_name = device_name.clone();
        let firmware_version = firmware_version.clone();
        let status_entries = status_entries.clone();
        server.fn_handler("/status", Method::Get, move |request| {
            let provisioned = store_for_status
                .lock()
                .ok()
                .and_then(|s| s.is_provisioned().ok())
                .unwrap_or(false);
            let json = render_status(
                state.current().as_str(),
                provisioned,
                &device_name,
                &firmware_version,
                state.uptime_secs(),
                &status_entries,
            );
            let headers = [("Content-Type", "application/json")];
            let mut response = request.into_response(200, Some("OK"), &headers)?;
            response.write_all(json.as_bytes())?;
            Ok::<(), anyhow::Error>(())
        })?;
    }

    {
        let state = state.clone();
        let nonce = nonce.clone();
        let on_event = on_event.clone();
        server.fn_handler("/factory-reset", Method::Post, move |mut request| {
            let body = match read_body(&mut request) {
                BodyRead::Ok(b) => b,
                BodyRead::TooLarge => {
                    let mut response = request.into_status_response(413)?;
                    response.write_all(b"Request body too large.")?;
                    return Ok(());
                }
            };
            if !nonce_matches(&body, &nonce) {
                log::warn!("POST /factory-reset rejected: nonce mismatch");
                let mut response = request.into_status_response(403)?;
                response.write_all(b"Forbidden: session token mismatch.")?;
                return Ok(());
            }
            state.apply(ProvisioningInput::FactoryReset);
            (on_event)(ProvisioningEvent::FactoryResetRequested);
            log::info!("Factory reset requested via portal");
            let mut response = request.into_ok_response()?;
            response.write_all(FACTORY_RESET_HTML.as_bytes())?;
            Ok::<(), anyhow::Error>(())
        })?;
    }

    let redirect = format!("http://{ap_ip}/");
    for path in PROBE_PATHS {
        let location = redirect.clone();
        server.fn_handler(path, Method::Get, move |request| {
            let headers = [("Location", location.as_str())];
            let _ = request.into_response(302, Some("Found"), &headers)?;
            Ok::<(), anyhow::Error>(())
        })?;
    }

    Ok(server)
}

/// Body-read outcome: the decoded body, or a too-large signal.
enum BodyRead {
    Ok(String),
    TooLarge,
}

/// Reads the full request body in chunks into a bounded buffer.
///
/// Returns [`BodyRead::TooLarge`] once the accumulated length would exceed
/// [`MAX_BODY_LEN`]. Bytes that are not valid UTF-8 are lossily replaced; the
/// pure parser then reports any genuinely malformed pairs.
fn read_body<R: Read>(request: &mut R) -> BodyRead {
    let mut body: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 512];
    loop {
        match request.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                if body.len() + n > MAX_BODY_LEN {
                    return BodyRead::TooLarge;
                }
                body.extend_from_slice(&chunk[..n]);
            }
            Err(_) => break,
        }
    }
    BodyRead::Ok(String::from_utf8_lossy(&body).into_owned())
}

/// Extracts the raw `_nonce` value from a form body and compares it to the
/// session nonce.
///
/// The pure parser ignores underscore-prefixed keys, so the nonce is read here
/// from the raw body before parsing. This defends against blind cross-client
/// request forgery on an open AP: a third party on the same network cannot
/// forge a mutating POST without first reading the per-session token from the
/// form.
fn nonce_matches(body: &str, expected: &str) -> bool {
    for pair in body.split('&') {
        if let Some(value) = pair.strip_prefix("_nonce=") {
            return value == expected;
        }
    }
    false
}

/// Non-secret pre-fill values for the form.
struct Prefill {
    wifi_ssid: String,
    dev_eui: String,
    join_eui: String,
    ota_url: String,
    dev_name: String,
}

impl Prefill {
    /// An all-empty pre-fill (used when re-rendering after a POST: secrets are
    /// never echoed and non-secret values came from the rejected submission,
    /// which we deliberately do not round-trip in v1).
    fn empty() -> Self {
        Self {
            wifi_ssid: String::new(),
            dev_eui: String::new(),
            join_eui: String::new(),
            ota_url: String::new(),
            dev_name: String::new(),
        }
    }
}

/// Loads non-secret pre-fill values from NVS, falling back to empty on any
/// error or when unprovisioned.
fn load_prefill(store: &Arc<std::sync::Mutex<ProvisioningStore>>) -> Prefill {
    let loaded = store
        .lock()
        .ok()
        .and_then(|s| s.load().ok().flatten());
    match loaded {
        Some(cfg) => Prefill {
            wifi_ssid: cfg.wifi_ssid,
            dev_eui: cfg.dev_eui_hex,
            join_eui: cfg.join_eui_hex,
            ota_url: cfg.ota_url,
            dev_name: cfg.device_name,
        },
        None => Prefill::empty(),
    }
}

/// Substitutes pre-fill values and rendered errors into the portal template.
///
/// Secret inputs (`wifi_pass`, `app_key`) carry no placeholder substitution and
/// are always emitted empty.
fn render_form(nonce: &str, prefill: &Prefill, errors: &FieldErrors) -> String {
    PORTAL_HTML
        .replace("{{NONCE}}", &html_escape(nonce))
        .replace("{{ERRORS}}", &render_errors(errors))
        .replace("{{WIFI_SSID}}", &html_escape(&prefill.wifi_ssid))
        .replace("{{DEV_EUI}}", &html_escape(&prefill.dev_eui))
        .replace("{{JOIN_EUI}}", &html_escape(&prefill.join_eui))
        .replace("{{OTA_URL}}", &html_escape(&prefill.ota_url))
        .replace("{{DEV_NAME}}", &html_escape(&prefill.dev_name))
}

/// Renders the accumulated field errors as an HTML error block, or the empty
/// string when there are none.
fn render_errors(errors: &FieldErrors) -> String {
    if errors.is_empty() {
        return String::new();
    }
    let mut out = String::from("<div class=\"errors\"><strong>Please fix:</strong><ul>");
    for error in errors {
        let label = field_label(error.field);
        out.push_str("<li>");
        out.push_str(&html_escape(label));
        out.push_str(": ");
        out.push_str(&html_escape(&error.error.to_string()));
        out.push_str("</li>");
    }
    out.push_str("</ul></div>");
    out
}

/// A single body-level error block (used for `413`/`500` re-renders).
fn form_level_errors() -> FieldErrors {
    let mut errors = FieldErrors::new();
    let _ = errors.push(provisioning_pure::FieldError {
        field: Field::Form,
        error: provisioning_pure::ValidationError::MalformedBody,
    });
    errors
}

/// Human-readable label for a form field.
fn field_label(field: Field) -> &'static str {
    match field {
        Field::WifiSsid => "Wi-Fi network name",
        Field::WifiPassword => "Wi-Fi password",
        Field::DevEui => "DevEUI",
        Field::JoinEui => "JoinEUI",
        Field::AppKey => "AppKey",
        Field::OtaUrl => "OTA URL",
        Field::DeviceName => "Device name",
        Field::Form => "Submission",
    }
}

/// Builds the `/status` JSON document with `core` formatting.
///
/// Schema: `{"schema":1,"state":"…","provisioned":bool,"device_name":"…",
/// "firmware_version":"…","uptime_s":N,"extra":{…}}`. The `extra` entries come
/// from `with_status_entry` and are served unauthenticated to anyone on the AP;
/// they must never contain secrets.
fn render_status(
    state: &str,
    provisioned: bool,
    device_name: &str,
    firmware_version: &str,
    uptime_s: u64,
    extras: &[(String, String)],
) -> String {
    let mut json = String::with_capacity(160);
    json.push_str("{\"schema\":1,\"state\":\"");
    json.push_str(&json_escape(state));
    json.push_str("\",\"provisioned\":");
    json.push_str(if provisioned { "true" } else { "false" });
    json.push_str(",\"device_name\":\"");
    json.push_str(&json_escape(device_name));
    json.push_str("\",\"firmware_version\":\"");
    json.push_str(&json_escape(firmware_version));
    json.push_str("\",\"uptime_s\":");
    json.push_str(&uptime_s.to_string());
    json.push_str(",\"extra\":{");
    for (i, (key, value)) in extras.iter().enumerate() {
        if i > 0 {
            json.push(',');
        }
        json.push('"');
        json.push_str(&json_escape(key));
        json.push_str("\":\"");
        json.push_str(&json_escape(value));
        json.push('"');
    }
    json.push_str("}}");
    json
}

/// HTML-escapes the five significant characters.
fn html_escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for c in input.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

/// Escapes a string for inclusion in a JSON string literal.
fn json_escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for c in input.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            other => out.push(other),
        }
    }
    out
}

/// Page shown after a successful commit.
const COMMITTED_HTML: &str = "<!DOCTYPE html><html><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
<title>Provisioned</title></head><body style=\"font-family:system-ui,sans-serif;padding:1rem\">\
<h1>Provisioned</h1><p>Credentials saved. The device will restart and join your network.</p>\
</body></html>";

/// Page shown after a factory-reset request.
const FACTORY_RESET_HTML: &str = "<!DOCTYPE html><html><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
<title>Factory reset</title></head><body style=\"font-family:system-ui,sans-serif;padding:1rem\">\
<h1>Factory reset requested</h1><p>The host application will complete the reset.</p>\
</body></html>";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nonce_matches_extracts_raw_value() {
        assert!(nonce_matches("_nonce=abc123&wifi_ssid=x", "abc123"));
        assert!(!nonce_matches("_nonce=wrong&wifi_ssid=x", "abc123"));
        assert!(!nonce_matches("wifi_ssid=x", "abc123"));
    }

    #[test]
    fn html_escape_covers_all_five() {
        assert_eq!(html_escape("<a href=\"x\">&'"), "&lt;a href=&quot;x&quot;&gt;&amp;&#39;");
    }

    #[test]
    fn status_json_has_required_fields() {
        let json = render_status("awaiting_submission", false, "dev", "1.0", 42, &[]);
        assert!(json.contains("\"schema\":1"));
        assert!(json.contains("\"state\":\"awaiting_submission\""));
        assert!(json.contains("\"provisioned\":false"));
        assert!(json.contains("\"uptime_s\":42"));
        assert!(json.ends_with("\"extra\":{}}"));
    }

    #[test]
    fn status_json_renders_extras() {
        let extras = vec![("battery".to_string(), "88".to_string())];
        let json = render_status("committed", true, "d", "v", 1, &extras);
        assert!(json.contains("\"extra\":{\"battery\":\"88\"}"));
    }
}
