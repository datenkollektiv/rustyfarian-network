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
//! password, the LoRaWAN AppKey, and the MQTT password inputs are always
//! rendered empty and must be re-entered on every submission (single-shot
//! commit) — no template carries an `{{MQTT_PASS}}` placeholder. No credential
//! value is ever logged or echoed.

use std::sync::Arc;

use anyhow::Context as _;

use embedded_svc::io::{Read, Write};
use esp_idf_svc::http::server::{Configuration, EspHttpServer};
use esp_idf_svc::http::Method;

use juggler::provisioning::html_json_escape::{html_escape_to, json_escape_to};
use juggler::provisioning::templates::{LORAWAN_PORTAL_HTML, WIFI_MQTT_PORTAL_HTML};
use juggler::provisioning::{parse_form, Field, FieldErrors, ProvisioningInput, SchemaProfile};

use crate::provisioning::store::ProvisioningStore;
use crate::provisioning::{ProvisioningEvent, SharedState};

/// Selects the shared template for `profile`.
fn template_for(profile: SchemaProfile) -> &'static str {
    match profile {
        SchemaProfile::LorawanFieldDevice => LORAWAN_PORTAL_HTML,
        SchemaProfile::WifiMqttDevice => WIFI_MQTT_PORTAL_HTML,
    }
}

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
    profile: SchemaProfile,
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
            let prefill = load_prefill(&store_for_load, profile);
            let html = render_form(profile, &nonce, &prefill, &FieldErrors::new());
            let cur = state.current();
            log::debug!("GET / (state={})", cur.as_str());
            let headers = [
                ("Content-Type", "text/html; charset=utf-8"),
                ("Cache-Control", "no-store"),
            ];
            let mut response = request.into_response(200, Some("OK"), &headers)?;
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
                    let html = render_form_with_banner(
                        profile,
                        &nonce,
                        &Prefill::empty(),
                        "Request body too large — please try again.",
                    );
                    let mut response = request.into_status_response(413)?;
                    response.write_all(html.as_bytes())?;
                    return Ok(());
                }
                BodyRead::ReadError => {
                    log::warn!("POST /save aborted: transport read failure");
                    let mut response = request.into_status_response(400)?;
                    response.write_all(b"Bad request: could not read body.")?;
                    return Ok(());
                }
            };

            if !nonce_matches(&body, &nonce) {
                log::warn!("POST /save rejected: nonce mismatch");
                let mut response = request.into_status_response(403)?;
                response.write_all(b"Forbidden: session token mismatch.")?;
                return Ok(());
            }

            match parse_form(&body, profile) {
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
                            let html = render_form_with_banner(
                                profile,
                                &nonce,
                                &Prefill::empty(),
                                "Could not save credentials to flash. Please try again.",
                            );
                            let mut response = request.into_status_response(500)?;
                            response.write_all(html.as_bytes())?;
                        }
                    }
                }
                Err(errors) => {
                    state.apply(ProvisioningInput::InvalidSubmission);
                    (on_event)(ProvisioningEvent::SubmissionRejected);
                    log::info!("POST /save rejected: {} field error(s)", errors.len());
                    let html = render_form(profile, &nonce, &Prefill::empty(), &errors);
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
                profile,
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
                BodyRead::ReadError => {
                    log::warn!("POST /factory-reset aborted: transport read failure");
                    let mut response = request.into_status_response(400)?;
                    response.write_all(b"Bad request: could not read body.")?;
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

/// Body-read outcome: the decoded body, an oversized signal, or a transport
/// read error.
enum BodyRead {
    Ok(String),
    TooLarge,
    ReadError,
}

/// Reads the full request body in chunks into a bounded buffer.
///
/// Returns [`BodyRead::TooLarge`] once the accumulated length would exceed
/// the internal buffer limit, or [`BodyRead::ReadError`] on a transport read failure
/// (previously these surfaced as silent truncation that then failed validation
/// for the wrong reason). Bytes that are not valid UTF-8 are lossily replaced;
/// the pure parser then reports any genuinely malformed pairs.
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
            Err(_) => return BodyRead::ReadError,
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
        if let Some(raw) = pair.strip_prefix("_nonce=") {
            // Today's nonces are 8 lowercase hex characters that need no
            // decoding, but the rest of the form is percent-decoded — keeping
            // this path consistent removes a foot-gun if the nonce alphabet
            // ever changes (e.g. base64 with `+` or `/`). Bounded length: a
            // legitimate _nonce never exceeds a few bytes; anything longer is
            // a malformed body and won't match anyway.
            let Some(decoded) = percent_decode_simple(raw) else {
                return false;
            };
            return decoded == expected;
        }
    }
    false
}

/// Percent-decodes an `application/x-www-form-urlencoded` value (`+` → space,
/// `%XX` → byte) into a `String`. Returns `None` if any `%` is not followed by
/// two valid hex digits. Used only for the per-request nonce; the main form
/// parser has its own buffered, bounded decoder.
fn percent_decode_simple(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16)?;
                let lo = (bytes[i + 2] as char).to_digit(16)?;
                out.push(((hi << 4) | lo) as u8);
                i += 3;
            }
            b'%' => return None,
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

/// Non-secret pre-fill values for the form.
///
/// Carries the union of both profiles' non-secret inputs; only the active
/// profile's placeholders are substituted, so the unused fields stay empty.
/// Secret inputs — `wifi_pass`, `app_key`, and `mqtt_pass` — are deliberately
/// absent: they are never pre-filled and must be re-entered on every
/// submission.
struct Prefill {
    wifi_ssid: String,
    dev_eui: String,
    join_eui: String,
    mqtt_uri: String,
    mqtt_user: String,
    mqtt_client: String,
    ota_url: String,
    dev_name: String,
}

impl Prefill {
    /// An all-empty pre-fill (used when re-rendering after a POST: secrets are
    /// never echoed and non-secret values came from the rejected submission,
    /// which we deliberately do not round-trip).
    fn empty() -> Self {
        Self {
            wifi_ssid: String::new(),
            dev_eui: String::new(),
            join_eui: String::new(),
            mqtt_uri: String::new(),
            mqtt_user: String::new(),
            mqtt_client: String::new(),
            ota_url: String::new(),
            dev_name: String::new(),
        }
    }
}

/// Loads non-secret pre-fill values from NVS for `profile`, falling back to
/// empty on any error or when unprovisioned.
///
/// For the `WifiMqttDevice` profile the `mqtt_uri` field is recomposed from the
/// stored `mqtt_host` + `mqtt_port` (the inverse of the parse-time split). The
/// `mqtt_pass` secret is never read into the prefill — it joins `wifi_pass` and
/// `app_key` in the no-prefill set.
fn load_prefill(
    store: &Arc<std::sync::Mutex<ProvisioningStore>>,
    profile: SchemaProfile,
) -> Prefill {
    let guard = match store.lock() {
        Ok(g) => g,
        Err(_) => {
            log::warn!("load_prefill: store mutex poisoned, rendering empty form");
            return Prefill::empty();
        }
    };
    match guard.load() {
        Ok(Some(cfg)) if cfg.profile == profile => {
            let mut prefill = Prefill {
                wifi_ssid: cfg.wifi_ssid,
                dev_eui: cfg.dev_eui_hex,
                join_eui: cfg.join_eui_hex,
                mqtt_uri: String::new(),
                mqtt_user: cfg.mqtt_user.unwrap_or_default(),
                mqtt_client: cfg.mqtt_client.unwrap_or_default(),
                ota_url: cfg.ota_url,
                dev_name: cfg.device_name,
            };
            if profile == SchemaProfile::WifiMqttDevice && !cfg.mqtt_host.is_empty() {
                prefill.mqtt_uri = format!("mqtt://{}:{}", cfg.mqtt_host, cfg.mqtt_port);
            }
            prefill
        }
        // A stored record under the *other* profile must not pre-fill this
        // form (its fields do not map); render empty.
        Ok(Some(_)) => Prefill::empty(),
        Ok(None) => Prefill::empty(),
        Err(e) => {
            log::debug!("load_prefill: store.load() failed, rendering empty form: {e:#}");
            Prefill::empty()
        }
    }
}

/// Substitutes pre-fill values and rendered errors into the active profile's
/// template.
///
/// Secret inputs (`wifi_pass`, `app_key`, `mqtt_pass`) carry no placeholder
/// substitution and are always emitted empty.
fn render_form(
    profile: SchemaProfile,
    nonce: &str,
    prefill: &Prefill,
    errors: &FieldErrors,
) -> String {
    render_template(profile, nonce, prefill, &render_errors(errors))
}

/// Renders the form with a non-field-specific banner instead of per-field
/// errors. Used for transport/persistence failures (413, 500) where the form
/// data itself was never validated and the per-field error model would
/// misrepresent the cause.
fn render_form_with_banner(
    profile: SchemaProfile,
    nonce: &str,
    prefill: &Prefill,
    message: &str,
) -> String {
    render_template(profile, nonce, prefill, &render_banner(message))
}

fn render_template(
    profile: SchemaProfile,
    nonce: &str,
    prefill: &Prefill,
    errors_html: &str,
) -> String {
    // Every profile shares the Core/OTA placeholders; the profile-specific
    // ones (`{{DEV_EUI}}` / `{{JOIN_EUI}}` for LoRaWAN, `{{MQTT_*}}` for
    // Wi-Fi+MQTT) simply do not appear in the other profile's template, so the
    // corresponding `replace` calls are no-ops there. No `{{MQTT_PASS}}`
    // placeholder exists in any template — the secret is never pre-filled.
    template_for(profile)
        .replace("{{NONCE}}", &html_escape(nonce))
        .replace("{{ERRORS}}", errors_html)
        .replace("{{WIFI_SSID}}", &html_escape(&prefill.wifi_ssid))
        .replace("{{DEV_EUI}}", &html_escape(&prefill.dev_eui))
        .replace("{{JOIN_EUI}}", &html_escape(&prefill.join_eui))
        .replace("{{MQTT_URI}}", &html_escape(&prefill.mqtt_uri))
        .replace("{{MQTT_USER}}", &html_escape(&prefill.mqtt_user))
        .replace("{{MQTT_CLIENT}}", &html_escape(&prefill.mqtt_client))
        .replace("{{OTA_URL}}", &html_escape(&prefill.ota_url))
        .replace("{{DEV_NAME}}", &html_escape(&prefill.dev_name))
}

/// Renders a freeform banner (`<div class="errors">…</div>`) used when the
/// failure is not attributable to a specific form field.
fn render_banner(message: &str) -> String {
    let mut out = String::from("<div class=\"errors\">");
    out.push_str(&html_escape(message));
    out.push_str("</div>");
    out
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

/// Human-readable label for a form field.
///
/// [`Field::Form`] covers every body-level problem, including a
/// [`ValidationError::UnexpectedForProfile`](juggler::provisioning::ValidationError::UnexpectedForProfile)
/// — the message text comes from the error's `Display`, this only labels the
/// owning input as the whole submission.
fn field_label(field: Field) -> &'static str {
    match field {
        Field::WifiSsid => "Wi-Fi network name",
        Field::WifiPassword => "Wi-Fi password",
        Field::DevEui => "DevEUI",
        Field::JoinEui => "JoinEUI",
        Field::AppKey => "AppKey",
        Field::MqttUri => "MQTT broker URI",
        Field::MqttUser => "MQTT username",
        Field::MqttPass => "MQTT password",
        Field::MqttClient => "MQTT client ID",
        Field::OtaUrl => "OTA URL",
        Field::DeviceName => "Device name",
        Field::Form => "Submission",
    }
}

/// Builds the `/status` JSON document with `core` formatting.
///
/// Schema: `{"schema":2,"state":"…","provisioned":bool,"profile":"…",
/// "device_name":"…","firmware_version":"…","uptime_s":N,"extra":{…}}`. The
/// `profile` field is additive (`"lorawan"` / `"wifi_mqtt"`), consistent with
/// the `/status` contract where everything beyond `schema` / `state` /
/// `provisioned` is optional. The `extra` entries come from `with_status_entry`
/// and are served unauthenticated to anyone on the AP; they must never contain
/// secrets.
fn render_status(
    state: &str,
    provisioned: bool,
    profile: SchemaProfile,
    device_name: &str,
    firmware_version: &str,
    uptime_s: u64,
    extras: &[(String, String)],
) -> String {
    let mut json = String::with_capacity(176);
    json.push_str("{\"schema\":2,\"state\":\"");
    json.push_str(&json_escape(state));
    json.push_str("\",\"provisioned\":");
    json.push_str(if provisioned { "true" } else { "false" });
    json.push_str(",\"profile\":\"");
    json.push_str(profile.as_str());
    json.push_str("\",\"device_name\":\"");
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
///
/// Delegates to [`juggler::provisioning::html_json_escape::html_escape_to`],
/// accumulating the result into an owned `String`.
fn html_escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    html_escape_to(input, |s| out.push_str(s));
    out
}

/// Escapes a string for inclusion in a JSON string literal.
///
/// Delegates to [`juggler::provisioning::html_json_escape::json_escape_to`],
/// accumulating the result into an owned `String`.
fn json_escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    json_escape_to(input, |s| out.push_str(s));
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
        assert_eq!(
            html_escape("<a href=\"x\">&'"),
            "&lt;a href=&quot;x&quot;&gt;&amp;&#39;"
        );
    }

    #[test]
    fn status_json_has_required_fields() {
        let json = render_status(
            "awaiting_submission",
            false,
            SchemaProfile::LorawanFieldDevice,
            "dev",
            "1.0",
            42,
            &[],
        );
        assert!(json.contains("\"schema\":2"));
        assert!(json.contains("\"state\":\"awaiting_submission\""));
        assert!(json.contains("\"provisioned\":false"));
        assert!(json.contains("\"profile\":\"lorawan\""));
        assert!(json.contains("\"uptime_s\":42"));
        assert!(json.ends_with("\"extra\":{}}"));
    }

    #[test]
    fn status_json_renders_wifi_mqtt_profile() {
        let json = render_status(
            "committed",
            true,
            SchemaProfile::WifiMqttDevice,
            "d",
            "v",
            1,
            &[],
        );
        assert!(json.contains("\"profile\":\"wifi_mqtt\""));
    }

    #[test]
    fn status_json_renders_extras() {
        let extras = vec![("battery".to_string(), "88".to_string())];
        let json = render_status(
            "committed",
            true,
            SchemaProfile::LorawanFieldDevice,
            "d",
            "v",
            1,
            &extras,
        );
        assert!(json.contains("\"extra\":{\"battery\":\"88\"}"));
    }
}
