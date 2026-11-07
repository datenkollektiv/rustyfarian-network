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
use juggler::provisioning::templates::{
    template_for, COMMITTED_HTML, FACTORY_RESET_HTML, PROBE_PATHS, RESUBMIT_HINT,
    SESSION_FINISHED_BODY,
};
use juggler::provisioning::{
    body_nonce_matches, parse_form, Field, FieldErrors, PortalAction, PortalDefaults, PortalInput,
    Prefill, ProvisioningConfig, SchemaProfile,
};

use crate::provisioning::store::ProvisioningStore;
use crate::provisioning::{ProvisioningEvent, SharedState};

/// HTTP port the portal listens on.
const HTTP_PORT: u16 = 80;

/// httpd worker stack size (bytes); the stack exists only while the portal runs.
///
/// `POST /save` needs about 12 KB: the handler frame (1.7 KB) plus the shared
/// `juggler` `parse_form` (4.5 KB of fixed-size field buffers) on top of httpd's
/// own frames. The earlier 10 KB overflowed on the first submit on a C3
/// (stack protection fault in `parse_form`, bench 2026-11-08), so the device
/// rebooted before committing and reopened the portal every boot.
const HTTPD_STACK_SIZE: usize = 16384;

/// Explicit URI-handler budget. Counted against the registrations below
/// (4 functional + 6 probe routes = 10); 12 leaves headroom for one or two
/// future probe endpoints without another audit.
const MAX_URI_HANDLERS: usize = 12;

/// Largest POST body the portal will buffer (bytes). A well-formed submission
/// is well under 1 KB; 2 KB is a generous cap. Larger bodies get `413`.
const MAX_BODY_LEN: usize = 2048;

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
    defaults: Arc<PortalDefaultsOwned>,
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
        let defaults_for_load = defaults.clone();
        let device_name = device_name.clone();
        server.fn_handler("/", Method::Get, move |request| {
            let stored = load_stored(&store_for_load);
            let defaults = defaults_for_load.as_borrowed();
            let prefill = Prefill::select(stored.as_ref(), profile, &defaults, &device_name);
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
        let defaults = defaults.clone();
        let device_name = device_name.clone();
        server.fn_handler("/save", Method::Post, move |mut request| {
            let body = match read_body(&mut request) {
                BodyRead::Ok(b) => b,
                BodyRead::TooLarge => {
                    let stored = load_stored(&store);
                    let defaults = defaults.as_borrowed();
                    let prefill =
                        Prefill::select(stored.as_ref(), profile, &defaults, &device_name);
                    let html = render_form_with_banner(
                        profile,
                        &nonce,
                        &prefill,
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

            // The form is parsed only behind a valid nonce; `PortalMachine`
            // sequences the rest (nonce → gate → persist → commit).
            let nonce_ok = body_nonce_matches(body.as_bytes(), &nonce);
            let parsed = nonce_ok.then(|| parse_form(&body, profile));
            let form_valid = matches!(parsed, Some(Ok(_)));
            match (
                state.poll(PortalInput::Save {
                    nonce_ok,
                    form_valid,
                }),
                parsed,
            ) {
                (PortalAction::Forbidden, _) => {
                    log::warn!("POST /save rejected: nonce mismatch");
                    let mut response = request.into_status_response(403)?;
                    response.write_all(b"Forbidden: session token mismatch.")?;
                }
                (PortalAction::RejectForm, Some(Err(errors))) => {
                    (on_event)(ProvisioningEvent::SubmissionRejected);
                    log::info!("POST /save rejected: {} field error(s)", errors.len());
                    let stored = load_stored(&store);
                    let defaults = defaults.as_borrowed();
                    let prefill =
                        Prefill::select(stored.as_ref(), profile, &defaults, &device_name);
                    let html = render_form(profile, &nonce, &prefill, &errors);
                    let mut response = request.into_status_response(400)?;
                    response.write_all(html.as_bytes())?;
                }
                (PortalAction::Persist, Some(Ok(config))) => {
                    let persist = store
                        .lock()
                        .map_err(|_| anyhow::anyhow!("store mutex poisoned"))
                        .and_then(|mut s| s.save(&config));
                    let saved = match persist {
                        Ok(()) => Some(config),
                        Err(e) => {
                            log::error!("POST /save persist failed: {e:#}");
                            None
                        }
                    };
                    match state.persist_done(saved) {
                        PortalAction::Committed => {
                            (on_event)(ProvisioningEvent::Committed);
                            let mut response = request.into_ok_response()?;
                            response.write_all(COMMITTED_HTML.as_bytes())?;
                        }
                        PortalAction::PersistFailed => {
                            let stored = load_stored(&store);
                            let defaults = defaults.as_borrowed();
                            let prefill =
                                Prefill::select(stored.as_ref(), profile, &defaults, &device_name);
                            let html = render_form_with_banner(
                                profile,
                                &nonce,
                                &prefill,
                                "Could not save credentials to flash. Please try again.",
                            );
                            let mut response = request.into_status_response(500)?;
                            response.write_all(html.as_bytes())?;
                        }
                        other => {
                            log::error!("POST /save: persisted but the session answered {other:?}");
                            let mut response = request.into_status_response(409)?;
                            response.write_all(SESSION_FINISHED_BODY.as_bytes())?;
                        }
                    }
                }
                (PortalAction::Refused(t), _) => {
                    log::warn!("POST /save refused: session already {:?}", t.state);
                    let mut response = request.into_status_response(409)?;
                    response.write_all(SESSION_FINISHED_BODY.as_bytes())?;
                }
                (other, _) => {
                    log::error!("POST /save: unexpected portal action {other:?}");
                    let mut response = request.into_status_response(500)?;
                    response.write_all(b"Internal error.")?;
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
            match state.poll(PortalInput::FactoryReset {
                nonce_ok: body_nonce_matches(body.as_bytes(), &nonce),
            }) {
                PortalAction::Forbidden => {
                    log::warn!("POST /factory-reset rejected: nonce mismatch");
                    let mut response = request.into_status_response(403)?;
                    response.write_all(b"Forbidden: session token mismatch.")?;
                }
                PortalAction::ResetAccepted => {
                    (on_event)(ProvisioningEvent::FactoryResetRequested);
                    log::info!("Factory reset requested via portal");
                    let mut response = request.into_ok_response()?;
                    response.write_all(FACTORY_RESET_HTML.as_bytes())?;
                }
                // A reset after a commit, or while a submission is being
                // persisted, is refused: the saved credentials stand and the
                // host is never told a reset was requested.
                PortalAction::Refused(t) => {
                    log::warn!("POST /factory-reset refused: session already {:?}", t.state);
                    let mut response = request.into_status_response(409)?;
                    response.write_all(SESSION_FINISHED_BODY.as_bytes())?;
                }
                other => {
                    log::error!("POST /factory-reset: unexpected portal action {other:?}");
                    let mut response = request.into_status_response(500)?;
                    response.write_all(b"Internal error.")?;
                }
            }
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

/// Owned, non-secret pre-fill defaults threaded from [`PortalConfig`].
///
/// Seeds the form on a fresh / factory-reset device (empty store). Non-secret
/// by construction (mirrors [`PortalDefaults`]); carries no password or AppKey.
#[derive(Debug, Clone, Default)]
pub(crate) struct PortalDefaultsOwned {
    pub wifi_ssid: String,
    pub dev_eui: String,
    pub join_eui: String,
    pub mqtt_host: String,
    pub mqtt_port: String,
    pub mqtt_user: String,
    pub mqtt_client: String,
    pub ota_url: String,
}

impl PortalDefaultsOwned {
    /// Copies the borrowed defaults into owned `String`s.
    ///
    /// Values are passed through verbatim — **not validated or truncated here**.
    /// Defaults are a best-effort convenience for the initial form render; an
    /// over-long or malformed default surfaces as an ordinary per-field
    /// validation error on `POST /save` (via `parse_form`), exactly as if it had
    /// been typed. (The bare-metal tier caps its owned copies to fixed render
    /// buffers because it has no heap; this `std` tier has no such constraint,
    /// so it stores the full string.)
    pub(crate) fn from_borrowed(d: &PortalDefaults<'_>) -> Self {
        Self {
            wifi_ssid: d.wifi_ssid.to_string(),
            dev_eui: d.dev_eui.to_string(),
            join_eui: d.join_eui.to_string(),
            mqtt_host: d.mqtt_host.to_string(),
            mqtt_port: d.mqtt_port.to_string(),
            mqtt_user: d.mqtt_user.to_string(),
            mqtt_client: d.mqtt_client.to_string(),
            ota_url: d.ota_url.to_string(),
        }
    }

    /// Borrows the owned defaults as the [`PortalDefaults`] view
    /// [`Prefill::select`] reads.
    pub(crate) fn as_borrowed(&self) -> PortalDefaults<'_> {
        PortalDefaults {
            wifi_ssid: &self.wifi_ssid,
            dev_eui: &self.dev_eui,
            join_eui: &self.join_eui,
            mqtt_host: &self.mqtt_host,
            mqtt_port: &self.mqtt_port,
            mqtt_user: &self.mqtt_user,
            mqtt_client: &self.mqtt_client,
            ota_url: &self.ota_url,
        }
    }
}

/// Reads the stored record for the form pre-fill.
///
/// A store or mutex error reads as "no record", so the form still comes up
/// pre-filled from the defaults; [`Prefill::select`] then picks the source.
/// Also used to re-render the form after a rejected `POST /save`: the rejected
/// submission itself is deliberately not round-tripped (no secret is ever
/// echoed), but the form shows the stored / default values instead of blanks.
fn load_stored(store: &Arc<std::sync::Mutex<ProvisioningStore>>) -> Option<ProvisioningConfig> {
    match store.lock() {
        Ok(guard) => match guard.load() {
            Ok(cfg) => cfg,
            Err(e) => {
                log::debug!("load_stored: store.load() failed, rendering defaults: {e:#}");
                None
            }
        },
        Err(_) => {
            log::warn!("load_stored: store mutex poisoned, rendering defaults");
            None
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
    prefill: &Prefill<'_>,
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
    prefill: &Prefill<'_>,
    message: &str,
) -> String {
    render_template(profile, nonce, prefill, &render_banner(message))
}

fn render_template(
    profile: SchemaProfile,
    nonce: &str,
    prefill: &Prefill<'_>,
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
        .replace("{{WIFI_SSID}}", &html_escape(prefill.wifi_ssid))
        .replace("{{DEV_EUI}}", &html_escape(prefill.dev_eui))
        .replace("{{JOIN_EUI}}", &html_escape(prefill.join_eui))
        .replace("{{MQTT_URI}}", &html_escape(&prefill.mqtt_uri))
        .replace("{{MQTT_USER}}", &html_escape(prefill.mqtt_user))
        .replace("{{MQTT_CLIENT}}", &html_escape(prefill.mqtt_client))
        .replace("{{OTA_URL}}", &html_escape(prefill.ota_url))
        .replace("{{DEV_NAME}}", &html_escape(prefill.dev_name))
}

/// Renders a freeform banner (`<div class="errors">…</div>`) used when the
/// failure is not attributable to a specific form field.
fn render_banner(message: &str) -> String {
    let mut out = String::from("<div class=\"errors\">");
    out.push_str(&html_escape(message));
    push_resubmit_hint(&mut out);
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
    out.push_str("</ul>");
    push_resubmit_hint(&mut out);
    out.push_str("</div>");
    out
}

/// Appends the shared [`RESUBMIT_HINT`] paragraph to an error block.
fn push_resubmit_hint(out: &mut String) {
    out.push_str("<p>");
    out.push_str(RESUBMIT_HINT);
    out.push_str("</p>");
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

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    #[test]
    fn error_blocks_carry_resubmit_hint() {
        let mut errors = FieldErrors::new();
        let _ = errors.push(juggler::provisioning::FieldError {
            field: Field::OtaUrl,
            error: juggler::provisioning::ValidationError::InvalidUrl,
        });
        let block = render_errors(&errors);
        assert!(block.contains(RESUBMIT_HINT));
        assert!(block.ends_with("</p></div>"));

        let banner = render_banner("Could not save credentials to flash. Please try again.");
        assert!(banner.contains(RESUBMIT_HINT));
        assert!(banner.ends_with("</p></div>"));

        assert!(render_errors(&FieldErrors::new()).is_empty());
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

    /// Runtime-generated secret fixture (16 hex digits from a fresh
    /// `RandomState`), so no string literal flows into a password field — see
    /// `test_psk()` in `juggler::wifi` for why.
    pub(crate) fn test_secret() -> String {
        use std::collections::hash_map::RandomState;
        use std::hash::{BuildHasher, Hasher};

        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u8(0);
        format!("{:016x}", hasher.finish())
    }

    /// Runtime-generated nonce fixture (8 hex digits, the same shape
    /// `generate_nonce` produces), so no string literal flows into a `nonce`
    /// parameter — see `test_nonce()` in the bare-metal portal for why.
    ///
    /// Callers only need a well-formed token; these tests assert on rendered
    /// form fields, not on the nonce itself.
    fn test_nonce() -> String {
        use std::collections::hash_map::RandomState;
        use std::hash::{BuildHasher, Hasher};

        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u8(0);
        format!("{:08x}", hasher.finish() as u32)
    }

    /// A `WifiMqttDevice` record with runtime-generated secrets, built the
    /// way [`ProvisioningStore::load`] builds one; shared with the `store` and
    /// `boot` redaction tests.
    pub(crate) fn stored_wifi_mqtt(device_name: &str) -> ProvisioningConfig {
        let mqtt = juggler::provisioning::MqttFields::from_storage_parts(
            "stored.local".try_into().unwrap(),
            8883,
            Some("stored-user".try_into().unwrap()),
            Some(test_secret().as_str().try_into().unwrap()),
            None,
        );
        ProvisioningConfig::from_storage_parts(
            "StoredNet".try_into().unwrap(),
            test_secret().as_str().try_into().unwrap(),
            "".try_into().unwrap(),
            device_name.try_into().unwrap(),
            None,
            Some(mqtt),
            Default::default(),
        )
    }

    /// Pre-fill selection is tested in `juggler::provisioning::portal`; this
    /// checks the render keeps the configured device name and never a secret.
    #[test]
    fn empty_store_renders_configured_device_name() {
        let owned = PortalDefaultsOwned::from_borrowed(&PortalDefaults::default());
        let defaults = owned.as_borrowed();
        for profile in [
            SchemaProfile::WifiMqttDevice,
            SchemaProfile::LorawanFieldDevice,
        ] {
            let p = Prefill::select(None, profile, &defaults, "rgb-clock");
            let html = render_form(profile, &test_nonce(), &p, &FieldErrors::new());
            assert!(html.contains("value=\"rgb-clock\""));
        }
    }

    #[test]
    fn stored_prefill_never_renders_secrets() {
        let owned = PortalDefaultsOwned::from_borrowed(&PortalDefaults::default());
        let defaults = owned.as_borrowed();
        let stored = stored_wifi_mqtt("dev");
        let wifi_password = stored.wifi_password().to_owned();
        let mqtt_pass = stored
            .mqtt()
            .and_then(|m| m.password())
            .unwrap_or_default()
            .to_owned();
        let p = Prefill::select(
            Some(&stored),
            SchemaProfile::WifiMqttDevice,
            &defaults,
            "rgb-clock",
        );
        let html = render_form(
            SchemaProfile::WifiMqttDevice,
            &test_nonce(),
            &p,
            &FieldErrors::new(),
        );
        assert!(!html.contains(&wifi_password));
        assert!(!html.contains(&mqtt_pass));
        assert!(html.contains("mqtt://stored.local:8883"));
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
