//! Device-conformance scenario matrix: the fleet DDI profile 0.4.0 §7 device rows, mirrored against the
//! sans-IO client (`docs/features/ddi-client-v1.md`, the conformance decision; critical-path T12).
//!
//! Every `#[test]` is one row of the profile's device list, named after its id. A row drives the real
//! machines ([`PollMachine`], [`admit_retrieved`], [`VerdictSender`], [`CancelMachine`],
//! [`DownloadMachine`], the pure health policy of `ota::health` and [`OtaStore`] over the fault-injecting
//! in-memory KV) through a scripted fake server, in the order the ESP-IDF worker uses them, with time as an
//! input. A simulated reboot reopens the store over the same KV, as a power cycle would.
//! Run by `just test-ota` (`--features ota-ddi`, host target).
//!
//! # Rows not run here, and where they are covered
//!
//! | Row | Why not on the host | Covered by |
//! |:--|:--|:--|
//! | S01, S02, S09, S10, S11, S13, S17b (server side) | the action state, `installedDS` and `lastStatusCode` live on a real server | bench `c3/ddi` and `c6/ddi` (B2); the device side of each row is a test below |
//! | S15 | a broker restart and the re-subscription | the IDF `MqttBuilder::subscribe` re-sends on every connect; the nudge itself is `s15_and_sec_24` below |
//! | SEC-25 part b | server-side nudge coalescing | the fleet server |
//! | S-DEV-04 `read_bounded` | the server must see the connection closed early: a real socket | `ResponseBody` tests in `response.rs`; the cap itself is `s_dev_04` below |
//! | S-ORIG-01 exit code 2 and the single log line | a process start | `DdiRuntime::new` refuses before any I/O (IDF); `[app] refusing to start` in `crates/rustyfarian-esp-idf-network/examples/common/ddi_app.rs`; the refusal cases are `s_orig_01` below and `base_url_refusals_name_the_case` in `endpoint.rs` |
//! | S-ORIG-02, S-ORIG-04 WARN lines | log output | IDF worker `[ota] DDI poll link refused`, transport `DDI link refused`; the behaviour is `s_orig_02` and `s_orig_04` below |
//! | S-HYG-01 `http://` part | a build-time opt-in | the `ota-ddi-insecure-http` feature (`just check-ota-ddi-idf`), `https://` refused with `TlsUnavailable` until S1; the plain-MQTT part is F7 below |
//! | S-TLS-02 | a TLS stack: an untrusted CA, a wrong name or an expired certificate | the TLS round S1 to S4 (the HTTPS acceptance row of the feature doc), on the image a device runs |
//! | S-HYG-02 untrusted certificate, plaintext on a TLS port | a TLS stack | the TLS round S1 to S4; the http-link and redirect-to-http parts are `s_hyg_02` below |
//! | S-HYG-03 | environment variables, trust anchors, proxies | no provisioning key for an anchor or an opt-in exists (T10 identity rule); the TLS round |
//! | S-HYG-04 "never logged" | log output | `Secret` redaction tests in `token.rs`, `scripts/check-no-credential-logging.sh`; the request side is `s_hyg_04` below |
//! | S-HYG-05 malformed status line, "never logged" | the HTTP parser and the logs | the transport (`response.rs`); the no-echo part is `s_hyg_05` below |
//!
//! # Harness expectations this client meets differently
//!
//! The fleet harness asserts a few things the feature doc does not require; the tests below assert the
//! documented behaviour and name the gap, so the profile owner can decide (critical-path T21):
//!
//! | Finding | Harness | This client |
//! |:--|:--|:--|
//! | F1 | `proceeding`, cancel and plain-success bodies carry 1 to 10 `details`; `boot_ok` on a plain success, `cancel_acknowledged` / `cancel_rejected` on E5 | `details: []` on `proceeding`, on both cancel answers and on a success without a marker detail |
//! | F2 | a resent terminal report is byte-identical | the status object is identical, the `timestamp` is the send time |
//! | F3 | a closed action id is not retrieved again (S-DEV-12); a resumed device does not retrieve the document again (S13) | the loop guard refuses at application (`begin_ddi_attempt`, `persist_verdict`), and an interrupted attempt is offered and retrieved again |
//! | F4 | after a restart the stored report goes out before the first poll (S-DEV-14) | the verdict is delivered after the first accepted poll answer |
//! | F5 | unreadable records: every offered action is reported 1005 and the records stay untouched (S-DEV-10) | the records are repaired at boot, one 1005 is reported, the next offer is handled normally |
//! | F6 | the short-body fixture of S-HYG-05 (a 100-byte body under a `Content-Length` 64 bytes larger, with a document size of 4096) is `1001 download_failed`; a `stopId` that is not an action id is still acknowledged | a `Content-Length` other than the declared size is `1001 size_mismatch` before any byte is read, retried like every transient error until the download deadline passes (a body that does end early is `download_failed`, as the harness wants); a non-numeric `stopId` is an invalid document, answered with nothing |
//! | F7 | a nudge host without the build-time opt-in is refused at start (`mqtt_without_opt_in`) with nothing sent (S-HYG-01) | the nudge rides on the app's MQTT session (`DdiNudge::on_message`), which the IDF tier always opens as `mqtt://` with no opt-in; `mqtts://` for the nudge is the TLS round |

use alloc::collections::{BTreeMap, VecDeque};
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use core::time::Duration;

extern crate std;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::ota::ddi::wire::{ActionId, Board, Chip, ConfigData, Href};
use crate::ota::ddi::{
    admit_retrieved, decide_download_retry, AdmissionFacts, AdmittedOffer, BodyError, BodyFailure,
    CancelInput, CancelMachine, CancelResult, CancelStep, ControllerId, DdiEndpoint, DdiResponse,
    DeploymentVerdict, DownloadRetry, EndpointError, FeedbackPath, FeedbackPost, Nudge, OfferGate,
    PollAction, PollAnswer, PollInput, PollMachine, PollVerdict, RequestOutcome, ResponseBody,
    RetryAfter, SendOutcome, UrlRefusal, VerdictSender, VerdictStep, WorkOutcome,
    WALL_CLOCK_FLOOR_SECS,
};
use crate::ota::ddi::{
    after_repair, plan_boot, repair_at_boot, DdiRequest, DdiSettings, DdiWorker, DownloadFlow,
    DownloadFlowAction, DownloadFlowInput, SettleNote, Stamp, Wake, WorkerAction, WorkerConfig,
    WorkerEvent, WorkerInput,
};
use crate::ota::download::{CancelGate, DownloadAction, DownloadInput, DownloadMachine, ImageChip};
use crate::ota::health::{
    deadline_plan, poll_decision, refresh_after_repair, repair_all, repair_report_owed,
    HealthAction, HealthConfig, HealthFlow, HealthInput, HealthNote, HealthVerdict, PollStep,
    RuntimeFlags, SlotReading,
};
use crate::ota::persist::fault_kv::FaultKv;
use crate::ota::persist::keys::STAGE_CLOSED;
use crate::ota::persist::{
    note_boot_slot, reconcile_boot, DdiVerdict, HardwareFacts, OtaStore, StoreError, UpdateSlot,
    VerdictNote,
};
use crate::ota::{CancelRequest, Deadline, FailReason, OtaError, SlotId, SlotState, Version};

// ── fixture constants (placeholders, loopback only) ─────────────────────────────

const BASE: &str = "http://127.0.0.1:38080/DEFAULT";
const FOREIGN: &str = "http://127.0.0.1:38081/DEFAULT";
const DEVICE_ID: &str = "dev-01";
const CHIP: &str = "esp32c3";
const OTHER_CHIP: &str = "esp32c6";
const BOARD: &str = "sim-board";
const SLOT_CAPACITY: u64 = 2_097_152;
const ARTIFACT_LEN: usize = 4096;
const POLL_SLEEP: &str = "00:00:30";
const SLOT_A: SlotId = SlotId(0);
const SLOT_B: SlotId = SlotId(1);
const BOOT_REPAIR_PASSES: u32 = 32;
const RESTART_GRACE: Duration = Duration::from_secs(1);
const FIRMWARE_DEADLINE: Duration = Duration::from_secs(300);
const PER_OP: Duration = Duration::from_secs(60);
/// A server value that must never come back in a request the device makes (S-HYG-05).
const MARKER: &str = "xecho0badc0de";

const fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

fn v(text: &str) -> Version {
    Version::parse(text).unwrap_or_else(|e| panic!("fixture version {text}: {e:?}"))
}

fn root() -> String {
    format!("{BASE}/controller/v1/{DEVICE_ID}")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn sha256_of(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

/// An ESP app image of `len` bytes for `chip`: the magic byte, the little-endian chip id at offset 12 and filler.
fn artifact(len: usize, chip: ImageChip) -> Vec<u8> {
    let mut bytes = vec![0x5A_u8; len];
    bytes[0] = 0xE9;
    let [lo, hi] = chip.id().to_le_bytes();
    bytes[12] = lo;
    bytes[13] = hi;
    for (i, b) in bytes.iter_mut().enumerate().skip(16) {
        *b = (i % 251) as u8;
    }
    bytes
}

// ── the scripted server ──────────────────────────────────────────────────────────

/// The DDI endpoints of the profile, numbered as the fleet harness numbers them (E7 does not exist).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    /// GET the controller base: the poll.
    E1,
    /// GET `deploymentBase/{id}`.
    E2,
    /// POST `deploymentBase/{id}/feedback`.
    E3,
    /// GET `cancelAction/{id}`.
    E4,
    /// POST `cancelAction/{id}/feedback`.
    E5,
    /// PUT `configData`.
    E6,
    /// GET the artifact.
    E8,
}

/// What the server does with one request; the device's transport turns it into a [`RequestOutcome`].
#[derive(Debug, Clone)]
enum Reply {
    Status {
        status: u16,
        body: Vec<u8>,
        retry_after: Option<&'static str>,
        /// The `Content-Length` of an artifact answer when it differs from the body (a short body).
        declared_len: Option<u64>,
    },
    /// A 3xx; the transport never follows it.
    Redirect(u16),
    /// The connection fails or is dropped.
    Unreachable,
}

fn ok(body: &str) -> Reply {
    status(200, body)
}

fn status(code: u16, body: &str) -> Reply {
    Reply::Status {
        status: code,
        body: body.as_bytes().to_vec(),
        retry_after: None,
        declared_len: None,
    }
}

fn status_retry_after(code: u16, retry_after: &'static str) -> Reply {
    Reply::Status {
        status: code,
        body: Vec::new(),
        retry_after: Some(retry_after),
        declared_len: None,
    }
}

/// One request the device made.
#[derive(Debug, Clone)]
struct Req {
    kind: Kind,
    /// The action id in the path, when the path names one.
    action: Option<String>,
    url: String,
    body: String,
    t: Duration,
}

impl Req {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }

    fn execution(&self) -> String {
        self.json()["status"]["execution"]
            .as_str()
            .unwrap_or("")
            .to_string()
    }

    fn finished(&self) -> String {
        self.json()["status"]["result"]["finished"]
            .as_str()
            .unwrap_or("")
            .to_string()
    }

    fn code(&self) -> Option<u64> {
        self.json()["status"]["code"].as_u64()
    }

    fn details(&self) -> Vec<String> {
        self.json()["status"]["details"]
            .as_array()
            .map(|d| {
                d.iter()
                    .filter_map(|x| x.as_str().map(ToString::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn detail0(&self) -> String {
        self.details().first().cloned().unwrap_or_default()
    }

    fn is_terminal(&self) -> bool {
        self.kind == Kind::E3 && self.execution() == "closed"
    }

    /// The status object without the timestamp: what a resend must repeat (F2).
    fn status_object(&self) -> Value {
        self.json()["status"].clone()
    }
}

/// A deployment the server offers: one action, one document, one artifact.
#[derive(Debug, Clone)]
struct Offer {
    action: String,
    doc: Vec<u8>,
    artifact: Vec<u8>,
    /// Stays offered after its closing feedback (S-DEV-12).
    sticky: bool,
    closed: bool,
}

#[derive(Debug, Clone)]
struct Cancel {
    action: String,
    body: Vec<u8>,
    closed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfigLink {
    UntilPut,
    Never,
}

struct Server {
    sleep: &'static str,
    offers: Vec<Offer>,
    cancels: Vec<Cancel>,
    config_link: ConfigLink,
    config_put_seen: bool,
    /// Replaces the `_links` object of every poll answer when set (the harness's `e1_extra`).
    links_override: Option<Value>,
    /// Extra links next to the default ones.
    links_extra: Value,
    /// Extra top-level members of every poll answer.
    poll_extra: Value,
    queues: BTreeMap<Kind, VecDeque<Reply>>,
    standing: BTreeMap<Kind, Reply>,
    requests: Vec<Req>,
}

/// The differential trace keeps every request the device made, in order.
impl Drop for Server {
    fn drop(&mut self) {
        for r in &self.requests {
            record(format!(
                "request {:?} {} t={:?} {}",
                r.kind, r.url, r.t, r.body
            ));
        }
    }
}

impl Server {
    fn new() -> Self {
        Self {
            sleep: POLL_SLEEP,
            offers: Vec::new(),
            cancels: Vec::new(),
            config_link: ConfigLink::UntilPut,
            config_put_seen: false,
            links_override: None,
            links_extra: json!({}),
            poll_extra: json!({}),
            queues: BTreeMap::new(),
            standing: BTreeMap::new(),
            requests: Vec::new(),
        }
    }

    fn offer(&mut self, doc: &Doc) -> &mut Self {
        self.offers.push(Offer {
            action: doc.action.clone(),
            doc: doc.body(),
            artifact: doc.artifact.clone(),
            sticky: false,
            closed: false,
        });
        self
    }

    fn offer_sticky(&mut self, doc: &Doc) -> &mut Self {
        self.offer(doc);
        self.offers.last_mut().expect("offer").sticky = true;
        self
    }

    /// Replaces the document of an open offer in place (S-DEV-05 flips `skip` to `forced`).
    fn set_doc(&mut self, doc: &Doc) {
        let offer = self
            .offers
            .iter_mut()
            .find(|o| o.action == doc.action)
            .expect("offer to replace");
        offer.doc = doc.body();
        offer.artifact = doc.artifact.clone();
    }

    fn close_offer(&mut self, action: &str) {
        if let Some(o) = self.offers.iter_mut().find(|o| o.action == action) {
            o.closed = true;
        }
    }

    fn cancel(&mut self, action: &str) -> &mut Self {
        let body = format!(r#"{{"id":"{action}","cancelAction":{{"stopId":"{action}"}}}}"#);
        self.cancel_with_body(action, body.as_bytes())
    }

    fn cancel_with_body(&mut self, action: &str, body: &[u8]) -> &mut Self {
        self.cancels.push(Cancel {
            action: action.to_string(),
            body: body.to_vec(),
            closed: false,
        });
        self
    }

    fn queue(&mut self, kind: Kind, replies: &[Reply]) -> &mut Self {
        self.queues
            .entry(kind)
            .or_default()
            .extend(replies.iter().cloned());
        self
    }

    fn on(&mut self, kind: Kind, reply: Reply) -> &mut Self {
        self.standing.insert(kind, reply);
        self
    }

    fn clear_standing(&mut self, kind: Kind) {
        self.standing.remove(&kind);
    }

    fn of(&self, kind: Kind) -> Vec<&Req> {
        self.requests.iter().filter(|r| r.kind == kind).collect()
    }

    fn count(&self, kind: Kind) -> usize {
        self.of(kind).len()
    }

    fn terminals(&self) -> Vec<&Req> {
        self.requests.iter().filter(|r| r.is_terminal()).collect()
    }

    fn mark(&self) -> usize {
        self.requests.len()
    }

    fn since(&self, mark: usize) -> Vec<&Req> {
        self.requests.iter().skip(mark).collect()
    }

    fn poll_body(&self) -> Vec<u8> {
        let links = match &self.links_override {
            Some(links) => links.clone(),
            None => self.default_links(),
        };
        let mut body = json!({"config": {"polling": {"sleep": self.sleep}}, "_links": links});
        if let (Some(extra), Some(obj)) = (self.poll_extra.as_object(), body.as_object_mut()) {
            for (k, val) in extra {
                obj.insert(k.clone(), val.clone());
            }
        }
        serde_json::to_vec(&body).expect("poll json")
    }

    fn default_links(&self) -> Value {
        let mut links = serde_json::Map::new();
        let r = root();
        if let Some(c) = self.cancels.iter().find(|c| !c.closed) {
            links.insert(
                "cancelAction".into(),
                json!({"href": format!("{r}/cancelAction/{}", c.action)}),
            );
        } else if let Some(o) = self.offers.iter().find(|o| !o.closed) {
            links.insert(
                "deploymentBase".into(),
                json!({"href": format!("{r}/deploymentBase/{}?c=1", o.action)}),
            );
        }
        let config = match self.config_link {
            ConfigLink::UntilPut => !self.config_put_seen,
            ConfigLink::Never => false,
        };
        if config {
            links.insert(
                "configData".into(),
                json!({"href": format!("{r}/configData")}),
            );
        }
        if let Some(extra) = self.links_extra.as_object() {
            for (k, val) in extra {
                links.insert(k.clone(), val.clone());
            }
        }
        Value::Object(links)
    }

    fn default_reply(&self, kind: Kind, action: Option<&str>, url: &str) -> Reply {
        match kind {
            Kind::E1 => Reply::Status {
                status: 200,
                body: self.poll_body(),
                retry_after: None,
                declared_len: None,
            },
            Kind::E2 => match self
                .offers
                .iter()
                .find(|o| Some(o.action.as_str()) == action)
            {
                Some(o) => Reply::Status {
                    status: 200,
                    body: o.doc.clone(),
                    retry_after: None,
                    declared_len: None,
                },
                None => status(404, ""),
            },
            Kind::E4 => match self
                .cancels
                .iter()
                .find(|c| Some(c.action.as_str()) == action)
            {
                Some(c) => Reply::Status {
                    status: 200,
                    body: c.body.clone(),
                    retry_after: None,
                    declared_len: None,
                },
                None => status(404, ""),
            },
            Kind::E8 => {
                let name = url.rsplit('/').next().unwrap_or("");
                let id = name
                    .strip_prefix("image-")
                    .and_then(|s| s.strip_suffix(".bin"));
                match self.offers.iter().find(|o| Some(o.action.as_str()) == id) {
                    Some(o) => Reply::Status {
                        status: 200,
                        body: o.artifact.clone(),
                        retry_after: None,
                        declared_len: None,
                    },
                    None => status(404, ""),
                }
            }
            Kind::E3 | Kind::E5 | Kind::E6 => status(200, ""),
        }
    }

    /// Records the request, picks the reply and applies the state changes a 200 answer causes.
    fn request(&mut self, kind: Kind, url: &str, body: &str, t: Duration) -> Reply {
        let action = action_in(url);
        self.requests.push(Req {
            kind,
            action: action.clone(),
            url: url.to_string(),
            body: body.to_string(),
            t,
        });
        let reply = self
            .queues
            .get_mut(&kind)
            .and_then(VecDeque::pop_front)
            .or_else(|| self.standing.get(&kind).cloned())
            .unwrap_or_else(|| self.default_reply(kind, action.as_deref(), url));
        if let Reply::Status { status, .. } = &reply {
            let status = *status;
            let execution = serde_json::from_str::<Value>(body)
                .ok()
                .and_then(|j| j["status"]["execution"].as_str().map(ToString::to_string))
                .unwrap_or_default();
            match kind {
                // A closing report the device counts as delivered (200, 404, 410) ends the action on the server too.
                Kind::E3 if execution == "closed" && matches!(status, 200 | 404 | 410) => {
                    if let Some(o) = self
                        .offers
                        .iter_mut()
                        .find(|o| Some(&o.action) == action.as_ref())
                    {
                        if !o.sticky {
                            o.closed = true;
                        }
                    }
                }
                Kind::E5 if status == 200 && (execution == "closed" || execution == "rejected") => {
                    if let Some(c) = self
                        .cancels
                        .iter_mut()
                        .find(|c| Some(&c.action) == action.as_ref())
                    {
                        c.closed = true;
                    }
                    if execution == "closed" {
                        if let Some(o) = self
                            .offers
                            .iter_mut()
                            .find(|o| Some(&o.action) == action.as_ref())
                        {
                            o.closed = true;
                        }
                    }
                }
                Kind::E6 if status == 200 => self.config_put_seen = true,
                _ => {}
            }
        }
        reply
    }
}

/// The action id named by a DDI path: `.../deploymentBase/7`, `.../cancelAction/7`, `.../deploymentBase/7/feedback`.
fn action_in(url: &str) -> Option<String> {
    let path = url.split('?').next().unwrap_or(url);
    let parts: Vec<&str> = path.split('/').collect();
    for (i, p) in parts.iter().enumerate() {
        if *p == "deploymentBase" || *p == "cancelAction" {
            return parts.get(i + 1).map(|s| (*s).to_string());
        }
    }
    None
}

// ── deployment documents ─────────────────────────────────────────────────────────

/// A deployment document under construction; `body()` renders it, `raw` replaces it wholesale.
#[derive(Debug, Clone)]
struct Doc {
    action: String,
    value: Value,
    artifact: Vec<u8>,
    raw: Option<Vec<u8>>,
    pad_to: Option<usize>,
}

impl Doc {
    /// The default document of the harness: one chunk, one artifact of [`ARTIFACT_LEN`] bytes named
    /// `image-<action>.bin`, `chip` metadata and forced handling.
    fn new(action: &str, version: &str) -> Self {
        Self::for_chip(action, version, ImageChip::Esp32C3, CHIP)
    }

    fn for_chip(action: &str, version: &str, image_chip: ImageChip, chip: &str) -> Self {
        let artifact = artifact(ARTIFACT_LEN, image_chip);
        let value = json!({
            "id": action,
            "deployment": {
                "download": "forced",
                "update": "forced",
                "chunks": [{
                    "part": "os",
                    "version": version,
                    "name": "image",
                    "artifacts": [{
                        "filename": format!("image-{action}.bin"),
                        "size": artifact.len(),
                        "hashes": {"sha256": hex(&sha256_of(&artifact))},
                        "_links": {"download-http": {"href": format!("{}/softwaremodules/1/artifacts/image-{action}.bin", root())}}
                    }],
                    "metadata": [{"key": "chip", "value": chip}]
                }]
            }
        });
        Self {
            action: action.to_string(),
            value,
            artifact,
            raw: None,
            pad_to: None,
        }
    }

    fn chunk(&mut self) -> &mut Value {
        &mut self.value["deployment"]["chunks"][0]
    }

    fn artifact_value(&mut self) -> &mut Value {
        &mut self.chunk()["artifacts"][0]
    }

    fn handling(mut self, download: &str, update: &str) -> Self {
        self.value["deployment"]["download"] = json!(download);
        self.value["deployment"]["update"] = json!(update);
        self
    }

    fn metadata(mut self, entries: &[(&str, &str)]) -> Self {
        self.chunk()["metadata"] = json!(entries
            .iter()
            .map(|(k, val)| json!({"key": k, "value": val}))
            .collect::<Vec<_>>());
        self
    }

    fn add_metadata(mut self, key: &str, value: &str) -> Self {
        if let Some(list) = self.chunk()["metadata"].as_array_mut() {
            list.push(json!({"key": key, "value": value}));
        }
        self
    }

    fn marker(self) -> Self {
        self.add_metadata("downgrade", "allowed")
    }

    fn href(mut self, href: &str) -> Self {
        self.artifact_value()["_links"]["download-http"]["href"] = json!(href);
        self
    }

    fn sha256_text(mut self, text: &str) -> Self {
        self.artifact_value()["hashes"]["sha256"] = json!(text);
        self
    }

    /// A declared digest that does not match the bytes (S17b).
    fn wrong_sha256(self) -> Self {
        let mut wrong = sha256_of(&self.artifact);
        wrong[0] ^= 0xFF;
        let text = hex(&wrong);
        self.sha256_text(&text)
    }

    fn size(mut self, size: Value) -> Self {
        self.artifact_value()["size"] = size;
        self
    }

    fn version(mut self, text: &str) -> Self {
        self.chunk()["version"] = json!(text);
        self
    }

    fn encrypted(mut self) -> Self {
        self.chunk()["encrypted"] = json!(true);
        self
    }

    /// Edits any part of the document.
    fn edit(mut self, f: impl FnOnce(&mut Value)) -> Self {
        f(&mut self.value);
        self
    }

    fn raw(mut self, bytes: &[u8]) -> Self {
        self.raw = Some(bytes.to_vec());
        self
    }

    /// Pads the rendered JSON with trailing spaces to exactly `len` bytes.
    fn padded_to(mut self, len: usize) -> Self {
        self.pad_to = Some(len);
        self
    }

    fn body(&self) -> Vec<u8> {
        if let Some(raw) = &self.raw {
            return raw.clone();
        }
        let mut body = serde_json::to_vec(&self.value).expect("document json");
        if let Some(len) = self.pad_to {
            assert!(body.len() <= len, "document longer than the padding target");
            body.resize(len, b' ');
        }
        body
    }
}

// ── SCENARIO-HARNESS-DEVICE ──────────────────────────────────────────────────────

/// Where a power cut is injected (S13, S-DEV-14).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CrashPoint {
    None,
    /// Right after the first `proceeding` of an admitted offer, before any download byte.
    AfterFirstProceeding,
    /// After the terminal verdict is persisted, before its first send.
    BeforeVerdictSend,
    /// After the first send of the terminal verdict, before its answer is handled.
    AfterVerdictSend,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Event {
    Boot,
    Crash,
    HealthApplied,
    HealthNothingToVerify,
    HealthDeadline,
}

#[derive(Debug, Clone, Copy)]
struct Image {
    version: Version,
    healthy: bool,
}

#[derive(Debug, Clone, Copy)]
struct Slot {
    state: SlotState,
    image: Option<Image>,
}

/// A download in progress: the artifact bytes the transport fetched and the machine's next input.
struct InFlight {
    offer: AdmittedOffer,
    target: SlotId,
    machine: DownloadMachine,
    gate: CancelGate,
    bytes: Vec<u8>,
    pos: usize,
    started: Duration,
    pending: Option<DownloadInput>,
    due: Duration,
    /// When the first attempt of this action began: the one download deadline counts from here.
    first_started: Duration,
    /// The attempts begun so far, the running one included.
    attempts: u32,
    /// Between two attempts: `due` is when the next one begins.
    waiting: bool,
    /// The error of the attempt that was just retried: the reason if the deadline has passed when the wait ends.
    last_error: Option<OtaError>,
    /// The wait the failed attempt's `Retry-After` asked for.
    retry_after: Option<Duration>,
    /// The failed attempt's `Retry-After` as the transport captured it; the machine driver reads it against the wall clock itself.
    retry_after_raw: Option<RetryAfter>,
}

const CHUNK: usize = 1024;

fn idx(slot: SlotId) -> usize {
    slot.0 as usize
}

fn other(slot: SlotId) -> SlotId {
    if slot == SLOT_A {
        SLOT_B
    } else {
        SLOT_A
    }
}

fn min_opt(a: Option<Duration>, b: Option<Duration>) -> Option<Duration> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, None) => a,
        (None, b) => b,
    }
}

/// The transport's view of a reply: a 3xx is never followed, a document body above the cap fails.
fn outcome_of(reply: Reply, capped: bool) -> RequestOutcome {
    match reply {
        Reply::Status {
            status,
            body,
            retry_after,
            ..
        } => {
            let retry_after = retry_after.and_then(RetryAfter::capture);
            if !capped {
                return RequestOutcome::Response(DdiResponse {
                    status,
                    retry_after,
                    body,
                });
            }
            // A document body goes through the transport's own accumulator, which owns the cap.
            let collected = ResponseBody::new(Some(body.len() as u64))
                .and_then(|mut collected| collected.push(&body).map(|()| collected))
                .and_then(ResponseBody::finish);
            match collected {
                Ok(body) => RequestOutcome::Response(DdiResponse {
                    status,
                    retry_after,
                    body,
                }),
                Err(e) => RequestOutcome::BodyFailed {
                    status,
                    retry_after,
                    failure: match e {
                        BodyError::TooLarge => BodyFailure::TooLarge,
                        BodyError::LengthMismatch => BodyFailure::LengthMismatch,
                    },
                },
            }
        }
        Reply::Redirect(status) => RequestOutcome::Redirect { status },
        Reply::Unreachable => RequestOutcome::Unreachable,
    }
}

fn is_2xx(outcome: &RequestOutcome) -> bool {
    matches!(outcome, RequestOutcome::Response(r) if (200..300).contains(&r.status))
}

/// The device: the real machines over a shared KV, two slots and a bootloader, driven with time as an input.
struct Device {
    kv: FaultKv,
    store: OtaStore<FaultKv>,
    endpoint: DdiEndpoint,
    poller: PollMachine,
    sender: VerdictSender,
    cancel: CancelMachine,
    chip: Chip,
    image_chip: ImageChip,
    board: Board,
    floor: Option<Version>,
    /// Versions whose image never turns healthy (the `DDI_DEMO_UNHEALTHY` analogue).
    unhealthy: Vec<Version>,
    running_slot: SlotId,
    boot_slot: SlotId,
    slots: [Slot; 2],
    flags: RuntimeFlags,
    unreported_corruption: bool,
    health_cfg: HealthConfig,
    download_rate: u64,
    crash: CrashPoint,
    booted: bool,
    epoch: u32,
    now: Duration,
    boot_at: Duration,
    next_poll_at: Option<Duration>,
    health_at: Option<Duration>,
    restart_at: Option<Duration>,
    nudge_pending: bool,
    download: Option<InFlight>,
    events: Vec<(Duration, Event)>,
    boots: Vec<String>,
    poll_verdicts: Vec<PollVerdict>,
    admissions: Vec<String>,
    cancel_results: Vec<Option<CancelResult>>,
    download_errors: Vec<OtaError>,
    /// Test hook: a cancel lands right after a failed attempt, before the retry decision.
    cancel_after_failure: bool,
    /// Test hook: a cancel lands right before the give-up at the deadline.
    cancel_at_giveup: bool,
    /// Every failed attempt the retry policy sent back to a back-off: when it failed and why.
    download_retries: Vec<(Duration, OtaError)>,
    /// Which composition drives the machines (see [`differential`]).
    driver: Driver,
    /// The machine driver's worker, rebuilt at each boot; started at the first poll cycle.
    worker: Option<DdiWorker>,
    worker_started: bool,
    /// The machine driver's health policy, started when it is first due.
    health: Option<HealthFlow>,
    /// The machine driver's download flow of the live download.
    flow: Option<DownloadFlow>,
    /// Wake-ups the machine driver keeps for the worker until its next sleep.
    wake: Wake,
}

impl Device {
    fn new(running: &str) -> Self {
        Self::with_base(BASE, running, CHIP, ImageChip::Esp32C3)
    }

    fn for_chip(running: &str, chip: &str, image_chip: ImageChip) -> Self {
        Self::with_base(BASE, running, chip, image_chip)
    }

    fn with_base(base: &str, running: &str, chip: &str, image_chip: ImageChip) -> Self {
        let kv = FaultKv::new();
        Self::over(kv, base, running, chip, image_chip)
    }

    /// A device over an existing KV: the flash content of an earlier life.
    fn over(kv: FaultKv, base: &str, running: &str, chip: &str, image_chip: ImageChip) -> Self {
        let id = ControllerId::parse(DEVICE_ID).expect("controller id");
        let endpoint = DdiEndpoint::new(base, &id).expect("base url");
        let store = OtaStore::open(kv.clone(), || 1);
        let running_image = Image {
            version: v(running),
            healthy: true,
        };
        Self {
            kv,
            store,
            poller: PollMachine::new(endpoint.clone()),
            sender: VerdictSender::new(),
            cancel: CancelMachine::new(),
            endpoint,
            chip: Chip::parse(chip).expect("chip"),
            image_chip,
            board: Board::parse(BOARD).expect("board"),
            floor: None,
            unhealthy: Vec::new(),
            running_slot: SLOT_A,
            boot_slot: SLOT_A,
            slots: [
                Slot {
                    state: SlotState::Valid,
                    image: Some(running_image),
                },
                Slot {
                    state: SlotState::Unknown,
                    image: None,
                },
            ],
            flags: RuntimeFlags {
                admission_open: false,
                health_settled: false,
                refuse_mark_valid: false,
                roll_back_now_at_boot: false,
                records_suspect: false,
            },
            unreported_corruption: false,
            health_cfg: HealthConfig::default(),
            download_rate: 0,
            crash: CrashPoint::None,
            booted: false,
            epoch: 0,
            now: Duration::ZERO,
            boot_at: Duration::ZERO,
            next_poll_at: None,
            health_at: None,
            restart_at: None,
            nudge_pending: false,
            download: None,
            events: Vec::new(),
            boots: Vec::new(),
            poll_verdicts: Vec::new(),
            admissions: Vec::new(),
            cancel_results: Vec::new(),
            download_errors: Vec::new(),
            cancel_after_failure: false,
            cancel_at_giveup: false,
            download_retries: Vec::new(),
            driver: DRIVER.get(),
            worker: None,
            worker_started: false,
            health: None,
            flow: None,
            wake: Wake::default(),
        }
    }

    fn floor(mut self, floor: &str) -> Self {
        self.floor = Some(v(floor));
        self
    }

    fn unhealthy(mut self, version: &str) -> Self {
        self.unhealthy.push(v(version));
        self
    }

    /// Artifact bytes per simulated second; 0 downloads at once.
    fn download_rate(mut self, bytes_per_second: u64) -> Self {
        self.download_rate = bytes_per_second;
        self
    }

    fn crash_at(mut self, point: CrashPoint) -> Self {
        self.crash = point;
        self
    }

    fn running(&self) -> Version {
        self.slots[idx(self.running_slot)]
            .image
            .expect("a running image")
            .version
    }

    fn hardware(&self) -> HardwareFacts {
        let update = other(self.running_slot);
        HardwareFacts {
            running_slot: self.running_slot,
            running_state: self.slots[idx(self.running_slot)].state,
            update_slot: UpdateSlot::Present(update, self.slots[idx(update)].state),
        }
    }

    fn wall(&self) -> Option<u64> {
        Some(WALL_CLOCK_FLOOR_SECS + self.now.as_secs())
    }

    fn ts(&self) -> Option<u64> {
        Some(
            (WALL_CLOCK_FLOOR_SECS + self.now.as_secs()) * 1000
                + u64::from(self.now.subsec_millis()),
        )
    }

    fn request(&mut self, srv: &mut Server, kind: Kind, url: &str, body: &str) -> RequestOutcome {
        let reply = srv.request(kind, url, body, self.now);
        outcome_of(reply, kind != Kind::E8)
    }

    fn work(&self, outcome: RequestOutcome) -> WorkOutcome {
        WorkOutcome {
            outcome,
            wall_clock: self.wall(),
            random: 0,
        }
    }

    // ── boot and reboot ──────────────────────────────────────────────────────────

    /// A boot of the running slot: the early boot note, the boot reconciliation, the repair of corrupt records,
    /// fresh machines, the post-boot `proceeding` and the health policy, as the IDF runtime does them.
    fn boot(&mut self, srv: &mut Server) {
        self.epoch += 1;
        self.booted = true;
        self.store = OtaStore::open(self.kv.clone(), || 1);
        let running_slot = self.running_slot;
        let _ = note_boot_slot(&mut self.store, || Ok(running_slot));
        if self.driver == Driver::Machines {
            self.boot_machines();
            return;
        }
        let hw = self.hardware();
        let running = self.running();
        let outcome = reconcile_boot(&mut self.store, Ok(hw), Some(running));
        self.boots.push(format!("{:?}", outcome.disposition));
        let mut flags = RuntimeFlags::from_boot(&outcome);
        self.unreported_corruption = false;
        if outcome.records_suspect() {
            let repaired = repair_all(&mut self.store, BOOT_REPAIR_PASSES);
            if let Some(r) = refresh_after_repair(
                &mut self.store,
                Ok(hw),
                Some(running),
                flags.roll_back_now_at_boot,
            ) {
                flags.records_suspect = r.records_suspect;
                flags.refuse_mark_valid = r.refuse_mark_valid;
            }
            self.unreported_corruption = repair_report_owed(repaired, flags.records_suspect);
        }
        assert!(
            !flags.roll_back_now_at_boot,
            "the fake device does not model a rollback demanded at boot"
        );
        self.flags = flags;
        self.poller = PollMachine::new(self.endpoint.clone());
        self.sender = VerdictSender::new();
        self.cancel = CancelMachine::new();
        self.download = None;
        self.restart_at = None;
        self.nudge_pending = false;
        self.boot_at = self.now;
        self.next_poll_at = Some(self.now);
        self.health_at = if self.flags.health_settled {
            None
        } else {
            Some(self.now + self.health_cfg.healthy_dwell)
        };
        self.events.push((self.now, Event::Boot));
        if !self.flags.health_settled {
            if let Ok(Some(action)) = self.store.running_ddi_action() {
                self.send_proceeding(srv, &action);
            }
        }
    }

    /// The restart the download thread asks for, or the bootloader's fall-back after a rollback.
    fn reboot(&mut self, srv: &mut Server) {
        self.running_slot = self.boot_slot;
        self.boot(srv);
    }

    /// A power cut right now: the next boot runs the slot the bootloader would pick.
    fn crash(&mut self, srv: &mut Server) {
        self.events.push((self.now, Event::Crash));
        self.reboot(srv);
    }

    /// A serial re-flash of the running slot with another image (S-DEV-09 case c).
    fn reflash(&mut self, srv: &mut Server, version: &str) {
        let slot = &mut self.slots[idx(self.running_slot)];
        slot.image = Some(Image {
            version: v(version),
            healthy: true,
        });
        slot.state = SlotState::Valid;
        self.boot_slot = self.running_slot;
        self.reboot(srv);
    }

    /// The MQTT nudge arrives; its payload never reaches the machine, because `PollMachine::nudge` takes none.
    fn nudge(&mut self) {
        self.nudge_pending = true;
        self.wake.nudge = true;
        if let Some(at) = self.next_poll_at {
            if at > self.now {
                self.next_poll_at = Some(self.now);
            }
        }
    }

    // ── the clock ────────────────────────────────────────────────────────────────

    fn run_for(&mut self, srv: &mut Server, span: Duration) {
        let end = self.now + span;
        self.run_until(srv, end);
    }

    /// Runs every due event until `end`, jumping the clock from event to event.
    fn run_until(&mut self, srv: &mut Server, end: Duration) {
        if !self.booted {
            self.boot(srv);
        }
        let mut steps = 0u32;
        loop {
            steps += 1;
            assert!(steps < 100_000, "the device loop does not settle");
            let download_due = self.download.as_ref().map(|d| d.due);
            let next = [
                self.next_poll_at,
                self.health_at,
                self.restart_at,
                download_due,
            ]
            .into_iter()
            .fold(None, min_opt);
            let Some(t) = next else {
                self.now = end;
                return;
            };
            if t > end {
                self.now = end;
                return;
            }
            self.now = self.now.max(t);
            if self.restart_at.is_some_and(|r| r <= self.now) {
                self.restart_at = None;
                self.reboot(srv);
                continue;
            }
            if download_due.is_some_and(|d| d <= self.now) {
                self.download_step(srv);
                continue;
            }
            if self.health_at.is_some_and(|h| h <= self.now) {
                self.health_step(srv);
                continue;
            }
            if self.next_poll_at.is_some_and(|p| p <= self.now) {
                self.poll_cycle(srv);
            }
        }
    }

    // ── the health policy ────────────────────────────────────────────────────────

    fn health_step(&mut self, srv: &mut Server) {
        if self.driver == Driver::Machines {
            self.health_step_machines(srv);
            return;
        }
        if self.slots[idx(self.running_slot)].state != SlotState::PendingVerify {
            // The IDF driver answers `NothingToVerify` at once for a slot that is not pending: nothing is marked.
            self.settle_health(Event::HealthNothingToVerify);
            return;
        }
        let elapsed = self.now.saturating_sub(self.boot_at);
        let step = poll_decision(&self.health_cfg, elapsed, self.flags.refuse_mark_valid);
        match step {
            PollStep::Evaluate => {
                let healthy = self.slots[idx(self.running_slot)]
                    .image
                    .is_some_and(|i| i.healthy);
                if healthy {
                    self.slots[idx(self.running_slot)].state = SlotState::Valid;
                    let running = self.running();
                    assert_eq!(self.store.complete_attempt(running), Ok(()));
                    self.settle_health(Event::HealthApplied);
                } else if elapsed < self.health_cfg.failure_deadline {
                    self.health_at = Some(self.boot_at + self.health_cfg.failure_deadline);
                } else {
                    self.at_deadline(srv);
                }
            }
            PollStep::Wait(d) => self.health_at = Some(self.now + d),
            PollStep::Deadline => self.at_deadline(srv),
        }
    }

    /// A terminal health verdict: admission opens, the policy is settled and the worker is woken, as the IDF
    /// handle's settle step does.
    fn settle_health(&mut self, event: Event) {
        self.flags.admission_open = true;
        self.flags.health_settled = true;
        self.health_at = None;
        self.nudge_pending = true;
        self.events.push((self.now, event));
        if let Some(at) = self.next_poll_at {
            self.next_poll_at = Some(at.min(self.now));
        }
    }

    /// The failure deadline: the reason is noted, the running slot is invalidated and the bootloader falls back.
    fn at_deadline(&mut self, srv: &mut Server) {
        let plan = deadline_plan(&self.health_cfg);
        let noted = self.store.note_rollback(plan.reason, self.running_slot);
        assert!(noted.is_ok(), "{noted:?}");
        self.slots[idx(self.running_slot)].state = SlotState::Invalid;
        self.boot_slot = other(self.running_slot);
        self.health_at = None;
        self.events.push((self.now, Event::HealthDeadline));
        self.reboot(srv);
    }

    // ── the poll loop ────────────────────────────────────────────────────────────

    fn poll_cycle(&mut self, srv: &mut Server) {
        if self.driver == Driver::Machines {
            self.poll_cycle_machines(srv);
            return;
        }
        let epoch = self.epoch;
        // As the IDF worker's `sleep_until`: a poll that is due anyway serves a kept nudge, which is cleared.
        if self.poller.next_poll_at().is_some_and(|at| at <= self.now) {
            self.nudge_pending = false;
        }
        let mut input = PollInput::Tick;
        loop {
            let answered = matches!(input, PollInput::Answered(_));
            let action = self.poller.poll(input, self.now);
            if answered {
                if let Some(verdict) = self.poller.verdict() {
                    self.sender.poll_answered(verdict);
                    self.poll_verdicts.push(verdict);
                }
            }
            input = match action {
                PollAction::Sleep { until } => {
                    if !self.nudge_pending {
                        self.next_poll_at = Some(until);
                        return;
                    }
                    match self.poller.nudge(self.now) {
                        Nudge::Moved | Nudge::AlreadyDue => {
                            self.nudge_pending = false;
                            PollInput::Tick
                        }
                        Nudge::TooSoon => {
                            let allowed = self.poller.nudge_allowed_at().unwrap_or(until);
                            self.next_poll_at = Some(allowed.min(until));
                            return;
                        }
                        Nudge::InFlight => {
                            // Kept, as the IDF worker keeps it; it cannot happen while the machine sleeps.
                            self.next_poll_at = Some(until);
                            return;
                        }
                        Nudge::Throttled | Nudge::Stopped => {
                            self.nudge_pending = false;
                            self.next_poll_at = Some(until);
                            return;
                        }
                    }
                }
                PollAction::Poll => PollInput::Answered(self.do_poll(srv)),
                PollAction::PutConfigData => PollInput::Performed(self.put_config_data(srv)),
                PollAction::RetrieveCancel(href) => {
                    PollInput::Performed(self.retrieve_cancel(srv, &href))
                }
                PollAction::RetrieveDeployment(href) => {
                    PollInput::Performed(self.retrieve_deployment(srv, &href))
                }
                PollAction::ReportUnreadable(action) => {
                    PollInput::Performed(self.report_unreadable(srv, &action))
                }
                PollAction::DeliverVerdict => PollInput::Performed(self.deliver_verdict(srv)),
                PollAction::Stopped => {
                    self.next_poll_at = None;
                    return;
                }
                PollAction::Ignored => panic!("the poll machine ignored its input"),
            };
            if self.epoch != epoch {
                return;
            }
        }
    }

    fn do_poll(&mut self, srv: &mut Server) -> PollAnswer {
        let url = self.endpoint.poll_url().to_string();
        let outcome = self.request(srv, Kind::E1, &url, "");
        let gate = if self.unreported_corruption {
            OfferGate::RecordsUnreadable
        } else if !self.flags.admission_open {
            OfferGate::Blocked
        } else {
            self.store.offer_gate()
        };
        let verdict_pending = matches!(self.store.pending_verdict(), Ok(Some(_)));
        PollAnswer {
            outcome,
            gate,
            wall_clock: self.wall(),
            random: 0,
            verdict_pending,
        }
    }

    fn put_config_data(&mut self, srv: &mut Server) -> Option<WorkOutcome> {
        let body = ConfigData {
            chip: &self.chip,
            board: &self.board,
            version: self.running(),
        }
        .encode();
        let url = self.endpoint.config_data_url();
        let outcome = self.request(srv, Kind::E6, &url, &body);
        if is_2xx(&outcome) {
            None
        } else {
            Some(self.work(outcome))
        }
    }

    fn send_proceeding(&mut self, srv: &mut Server, action: &ActionId) {
        let post = FeedbackPost::proceeding(action.clone(), self.ts());
        let url = post.url(&self.endpoint);
        let body = post.feedback().encode().expect("proceeding encodes");
        let _ = self.request(srv, Kind::E3, &url, &body);
    }

    fn retrieve_deployment(&mut self, srv: &mut Server, href: &Href) -> Option<WorkOutcome> {
        let outcome = match self.endpoint.check_href(href) {
            Ok(url) => self.request(srv, Kind::E2, url.as_str(), ""),
            Err(_) => RequestOutcome::ForeignOrigin,
        };
        let facts = AdmissionFacts {
            running: self.running(),
            rolled_back: self.store.refused().ok().flatten(),
            floor: self.floor,
            partition_capacity: SLOT_CAPACITY,
            chip: self.chip.clone(),
        };
        match admit_retrieved(href, &outcome, &facts, &self.endpoint) {
            DeploymentVerdict::NotRetrieved(t) => {
                self.admissions.push(format!("not_retrieved {t:?}"));
                Some(self.work(outcome))
            }
            DeploymentVerdict::Unreportable(reason) => {
                self.admissions.push(format!("unreportable {reason}"));
                None
            }
            DeploymentVerdict::Deferred(action) => {
                self.admissions.push(format!("deferred {action}"));
                None
            }
            DeploymentVerdict::Refused(refusal) => {
                let case = refusal
                    .url_case
                    .map(|c| format!(" {}", c.name()))
                    .unwrap_or_default();
                self.admissions.push(format!(
                    "refused {} {} {}{case}",
                    refusal.action,
                    refusal.code.as_u16(),
                    refusal.reason
                ));
                self.persist_and_deliver(srv, &DdiVerdict::refused(&refusal))
            }
            DeploymentVerdict::Admitted(offer) => {
                self.admissions.push(format!("admitted {}", offer.action));
                self.admit(srv, offer)
            }
        }
    }

    fn persist_and_deliver(
        &mut self,
        srv: &mut Server,
        verdict: &DdiVerdict,
    ) -> Option<WorkOutcome> {
        match self.store.persist_verdict(verdict) {
            Ok(VerdictNote::Written | VerdictNote::KeptPending) => {}
            Ok(VerdictNote::AlreadyClosed) | Err(StoreError::VerdictPending) => return None,
            Err(e) => panic!("verdict not persisted: {e:?}"),
        }
        if self.crash == CrashPoint::BeforeVerdictSend {
            self.crash = CrashPoint::None;
            self.crash(srv);
            return None;
        }
        self.deliver_verdict(srv)
    }

    fn admit(&mut self, srv: &mut Server, offer: AdmittedOffer) -> Option<WorkOutcome> {
        if self.slots[idx(self.running_slot)].state == SlotState::PendingVerify {
            self.admissions
                .push(format!("pending_verify {}", offer.action));
            return None;
        }
        let target = other(self.running_slot);
        let slot_was_invalid = self.slots[idx(target)].state == SlotState::Invalid;
        match self
            .store
            .begin_ddi_attempt(&offer, target, slot_was_invalid)
        {
            Ok(_) => {}
            Err(StoreError::ActionClosed) => {
                self.admissions.push(format!("closed {}", offer.action));
                return None;
            }
            Err(StoreError::NotAdmitted(_) | StoreError::VerdictPending) => {
                self.admissions.push(format!("blocked {}", offer.action));
                return None;
            }
            Err(e) => panic!("attempt not begun: {e:?}"),
        }
        self.send_proceeding(srv, &offer.action);
        if self.crash == CrashPoint::AfterFirstProceeding {
            self.crash = CrashPoint::None;
            self.crash(srv);
            return None;
        }
        self.start_download(srv, offer, target);
        None
    }

    // ── the download thread ──────────────────────────────────────────────────────

    fn start_download(&mut self, srv: &mut Server, offer: AdmittedOffer, target: SlotId) {
        if self.driver == Driver::Machines {
            let mut flow = DownloadFlow::new(offer.action.clone(), &self.settings());
            let at = self.stamp();
            let first = flow.poll(&mut self.store, DownloadFlowInput::Start, at);
            assert_eq!(
                first,
                DownloadFlowAction::Attempt {
                    remaining: FIRMWARE_DEADLINE
                }
            );
            self.flow = Some(flow);
        }
        self.begin_attempt(srv, offer, target, self.now, 1, CancelGate::for_retries());
    }

    /// One download attempt: its machine gets what is left of the one deadline that counts from `first_started`.
    fn begin_attempt(
        &mut self,
        srv: &mut Server,
        offer: AdmittedOffer,
        target: SlotId,
        first_started: Duration,
        attempts: u32,
        mut gate: CancelGate,
    ) {
        let remaining = FIRMWARE_DEADLINE.saturating_sub(self.now.saturating_sub(first_started));
        let mut machine = DownloadMachine::new(Deadline::after(remaining), PER_OP, offer.sha256)
            .with_expected_size(offer.size)
            .with_expected_chip(self.image_chip);
        let first = gate
            .poll(&mut machine, DownloadInput::Begin, Duration::ZERO)
            .expect("begin");
        assert!(matches!(first, DownloadAction::Connect { .. }), "{first:?}");
        let url = self
            .endpoint
            .check_href(&offer.artifact)
            .expect("an admitted artifact link is on the origin")
            .into_string();
        let reply = srv.request(Kind::E8, &url, "", self.now);
        let mut retry_after = None;
        let mut retry_after_raw = None;
        let (input, bytes) = match reply {
            Reply::Status {
                status: 200,
                body,
                declared_len,
                ..
            } => {
                retry_after = None;
                let len = declared_len.unwrap_or(body.len() as u64);
                (
                    DownloadInput::Headers {
                        content_length: Some(len),
                        chunked: false,
                        capacity: SLOT_CAPACITY,
                    },
                    body,
                )
            }
            Reply::Status {
                status,
                retry_after: text,
                ..
            } => {
                retry_after_raw = text.and_then(RetryAfter::capture);
                retry_after = retry_after_raw.clone().and_then(|r| r.delay(self.wall()));
                (
                    DownloadInput::ConnectFailed(OtaError::DownloadFailed { status }),
                    Vec::new(),
                )
            }
            Reply::Redirect(status) => (
                DownloadInput::ConnectFailed(OtaError::Redirect { status }),
                Vec::new(),
            ),
            Reply::Unreachable => (
                DownloadInput::ConnectFailed(OtaError::ServerUnreachable),
                Vec::new(),
            ),
        };
        self.download = Some(InFlight {
            offer,
            target,
            machine,
            gate,
            bytes,
            pos: 0,
            started: self.now,
            pending: Some(input),
            due: self.now,
            first_started,
            attempts,
            waiting: false,
            last_error: None,
            retry_after,
            retry_after_raw,
        });
        self.download_step(srv);
    }

    /// Drives the download until it ends or until the next chunk is due (at `download_rate`).
    fn download_step(&mut self, srv: &mut Server) {
        let Some(mut d) = self.download.take() else {
            return;
        };
        if d.waiting && self.driver == Driver::Machines {
            self.wait_ended_machines(srv, d);
            return;
        }
        if d.waiting {
            let over = self.now.saturating_sub(d.first_started) >= FIRMWARE_DEADLINE;
            if d.gate.is_requested() {
                self.give_up(&mut d.gate, OtaError::Cancelled);
            } else if let (true, Some(e)) = (over, d.last_error) {
                if self.cancel_at_giveup {
                    assert_eq!(d.gate.request(), CancelRequest::Accepted);
                }
                self.give_up(&mut d.gate, e);
            } else {
                self.begin_attempt(
                    srv,
                    d.offer,
                    d.target,
                    d.first_started,
                    d.attempts + 1,
                    d.gate,
                );
            }
            return;
        }
        let mut input = d.pending.take().expect("a pending download input");
        loop {
            let elapsed = self.now.saturating_sub(d.started);
            let action = d
                .gate
                .poll(&mut d.machine, input, elapsed)
                .expect("a download step");
            input = match action {
                DownloadAction::Connect { .. } => panic!("a second connect"),
                DownloadAction::ReadHeader { max, .. } => {
                    let n = (max as usize).min(d.bytes.len() - d.pos);
                    let chunk = &d.bytes[d.pos..d.pos + n];
                    d.pos += n;
                    DownloadInput::header_read(chunk)
                }
                DownloadAction::Prepare { .. } => {
                    self.slots[idx(d.target)].state = SlotState::Unknown;
                    self.slots[idx(d.target)].image = None;
                    DownloadInput::Prepared
                }
                DownloadAction::WriteHeader(_) => DownloadInput::Written,
                DownloadAction::Read { max, .. } => {
                    let n = (max as usize).min(d.bytes.len() - d.pos).min(CHUNK);
                    d.pos += n;
                    if self.download_rate > 0 && n > 0 {
                        let millis = (n as u64) * 1000 / self.download_rate;
                        d.due = self.now + Duration::from_millis(millis.max(1));
                        d.pending = Some(DownloadInput::Read(n));
                        self.download = Some(d);
                        return;
                    }
                    DownloadInput::Read(n)
                }
                DownloadAction::Write { .. } => DownloadInput::Written,
                DownloadAction::Finalize => DownloadInput::Digest(sha256_of(&d.bytes[..d.pos])),
                DownloadAction::Abort(_) => DownloadInput::Aborted,
                DownloadAction::Activate(_) => {
                    let healthy = !self.unhealthy.contains(&d.offer.version);
                    let slot = &mut self.slots[idx(d.target)];
                    slot.image = Some(Image {
                        version: d.offer.version,
                        healthy,
                    });
                    slot.state = SlotState::PendingVerify;
                    self.boot_slot = d.target;
                    DownloadInput::Activated(Ok(()))
                }
                DownloadAction::Done(result) if self.driver == Driver::Machines => {
                    self.attempt_ended_machines(srv, d, result);
                    return;
                }
                DownloadAction::Done(Ok(())) => {
                    assert_eq!(self.store.mark_boot_selected(), Ok(()));
                    let action = d.offer.action.clone();
                    self.send_proceeding(srv, &action);
                    self.restart_at = Some(self.now + RESTART_GRACE);
                    return;
                }
                DownloadAction::Done(Err(e)) => {
                    let elapsed = self.now.saturating_sub(d.first_started);
                    if self.cancel_after_failure {
                        assert_eq!(d.gate.request(), CancelRequest::Accepted);
                    }
                    let decision = if e == OtaError::Cancelled {
                        DownloadRetry::GiveUp
                    } else {
                        decide_download_retry(
                            &e,
                            d.attempts,
                            elapsed,
                            FIRMWARE_DEADLINE,
                            d.retry_after,
                        )
                    };
                    match decision {
                        DownloadRetry::Retry { wait } => {
                            self.download_retries.push((self.now, e));
                            d.waiting = true;
                            d.last_error = Some(e);
                            d.due = self.now + wait;
                            self.download = Some(d);
                        }
                        _ => self.give_up(&mut d.gate, e),
                    }
                    return;
                }
                DownloadAction::Ignored => panic!("the download machine ignored its input"),
            };
        }
    }

    /// The download gives up on `e`: the gate closes for good, and a cancel accepted before that wins over the error.
    fn give_up(&mut self, gate: &mut CancelGate, e: OtaError) {
        let e = if gate.close() { OtaError::Cancelled } else { e };
        self.settle_download_error(e);
    }

    /// The download is over for good: the attempt is settled with `e` as its reason and the poll loop is nudged.
    fn settle_download_error(&mut self, e: OtaError) {
        self.download_errors.push(e);
        let hw = self.hardware();
        let running = self.running();
        let settled = self
            .store
            .settle_failed_ddi_download(hw, Some(running), FailReason::Ota(e));
        assert!(settled.is_ok(), "{settled:?}");
        self.nudge_pending = true;
        if let Some(at) = self.next_poll_at {
            self.next_poll_at = Some(at.min(self.now));
        }
    }

    /// Lets a suspended download run to its end now (the cancel path joins the thread).
    fn finish_download(&mut self, srv: &mut Server) {
        let rate = self.download_rate;
        self.download_rate = 0;
        if let Some(d) = self.download.as_mut() {
            d.due = self.now;
        }
        self.download_step(srv);
        self.download_rate = rate;
    }

    // ── verdicts, unreadable records and cancels ─────────────────────────────────

    fn deliver_verdict(&mut self, srv: &mut Server) -> Option<WorkOutcome> {
        let verdict = match self.store.pending_verdict() {
            Ok(Some(verdict)) => verdict,
            _ => return None,
        };
        let step = self.sender.next(&verdict, self.now, self.ts());
        let VerdictStep::Send(post) = step else {
            return None;
        };
        let kind = match post.path {
            FeedbackPath::Deployment => Kind::E3,
            FeedbackPath::Cancel => Kind::E5,
        };
        let url = post.url(&self.endpoint);
        let body = post.feedback().encode().expect("a verdict encodes");
        let outcome = self.request(srv, kind, &url, &body);
        if self.crash == CrashPoint::AfterVerdictSend {
            self.crash = CrashPoint::None;
            self.crash(srv);
            return None;
        }
        let work = self.work(outcome);
        match self.sender.answered(work) {
            SendOutcome::Delivered(action) => {
                let _ = self.store.mark_verdict_delivered(&action);
                None
            }
            SendOutcome::Retry(hand_back) => hand_back,
            SendOutcome::Suspended(work) => Some(work),
            SendOutcome::Ignored => None,
        }
    }

    fn report_unreadable(&mut self, srv: &mut Server, action: &ActionId) -> Option<WorkOutcome> {
        if self
            .store
            .persist_verdict(&DdiVerdict::records_unreadable(action.clone()))
            .is_err()
        {
            return None;
        }
        self.unreported_corruption = false;
        let hw = self.hardware();
        let running = self.running();
        if let Some(r) = refresh_after_repair(
            &mut self.store,
            Ok(hw),
            Some(running),
            self.flags.roll_back_now_at_boot,
        ) {
            self.flags.records_suspect = r.records_suspect;
            self.flags.refuse_mark_valid = r.refuse_mark_valid;
        }
        self.deliver_verdict(srv)
    }

    fn retrieve_cancel(&mut self, srv: &mut Server, href: &Href) -> Option<WorkOutcome> {
        let outcome = match self.endpoint.check_href(href) {
            Ok(url) => self.request(srv, Kind::E4, url.as_str(), ""),
            Err(_) => RequestOutcome::ForeignOrigin,
        };
        let work = self.work(outcome);
        let mut step = self
            .cancel
            .poll(CancelInput::Retrieved(work), self.now, self.ts());
        loop {
            step = match step {
                CancelStep::LookUp(stop_id) => {
                    let downloading = self.download.as_ref().map(|d| d.offer.action.clone());
                    let target = self.store.cancel_target(&stop_id, downloading.as_ref());
                    self.cancel
                        .poll(CancelInput::Target(target), self.now, self.ts())
                }
                CancelStep::RequestStop => {
                    let request = match self.download.as_mut() {
                        Some(d) => d.gate.request(),
                        None => CancelRequest::TooLate,
                    };
                    self.cancel
                        .poll(CancelInput::StopRequested(request), self.now, self.ts())
                }
                CancelStep::AwaitDownload => {
                    self.finish_download(srv);
                    self.cancel
                        .poll(CancelInput::DownloadReturned, self.now, self.ts())
                }
                CancelStep::Send(post) => {
                    let kind = match post.path {
                        FeedbackPath::Deployment => Kind::E3,
                        FeedbackPath::Cancel => Kind::E5,
                    };
                    let url = post.url(&self.endpoint);
                    let body = post.feedback().encode().expect("a cancel answer encodes");
                    let outcome = self.request(srv, kind, &url, &body);
                    let work = self.work(outcome);
                    self.cancel
                        .poll(CancelInput::Sent(work), self.now, self.ts())
                }
                CancelStep::Done(hand_back) => {
                    self.cancel_results.push(self.cancel.result());
                    return hand_back;
                }
                CancelStep::Ignored => panic!("the cancel machine ignored its input"),
            };
        }
    }
}

// ── SCENARIO-HARNESS-MACHINES ────────────────────────────────────────────────────

/// Which composition a [`Device`] runs; [`differential`] runs every row under both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Driver {
    /// The IDF-order reference above: the device composes the sans-IO machines itself, in the order of the ESP-IDF worker, download thread, health driver and boot.
    Reference,
    /// The juggler compositions a tier drives: [`DdiWorker`], [`DownloadFlow`], [`HealthFlow`] and the boot plan functions ([`plan_boot`], [`repair_at_boot`], [`after_repair`]).
    Machines,
}

std::thread_local! {
    static DRIVER: core::cell::Cell<Driver> = const { core::cell::Cell::new(Driver::Reference) };
    static TRACE: core::cell::RefCell<Vec<String>> = const { core::cell::RefCell::new(Vec::new()) };
}

/// Appends one line to the differential trace of the running row.
fn record(line: String) {
    TRACE.with(|t| t.borrow_mut().push(line));
}

/// Runs one row under both drivers and asserts the same trace: every request in order with its time, URL and body (feedback codes and details included), the persisted records at the end, and the device's observations.
fn differential(row: impl Fn()) {
    let reference = run_as(Driver::Reference, &row);
    let machines = run_as(Driver::Machines, &row);
    if reference != machines {
        std::eprintln!("reference trace:\n{}", reference.join("\n"));
        std::eprintln!("machines trace:\n{}", machines.join("\n"));
    }
    for (i, (r, m)) in reference.iter().zip(&machines).enumerate() {
        assert_eq!(
            r, m,
            "trace line {i}: the machines differ from the IDF-order reference"
        );
    }
    assert_eq!(
        reference.len(),
        machines.len(),
        "the machines' trace is {} lines, the reference's {}",
        machines.len(),
        reference.len()
    );
}

fn run_as(driver: Driver, row: &impl Fn()) -> Vec<String> {
    DRIVER.set(driver);
    TRACE.take();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(row));
    DRIVER.set(Driver::Reference);
    if let Err(e) = result {
        let msg = e
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| e.downcast_ref::<&str>().map(|s| (*s).to_string()))
            .unwrap_or_default();
        panic!("{driver:?} driver: {msg}");
    }
    TRACE.take()
}

/// The differential trace keeps the persisted records and what the device observed.
impl Drop for Device {
    fn drop(&mut self) {
        record(format!("store {:?}", self.kv.snapshot()));
        record(format!("events {:?}", self.events));
        record(format!("boots {:?}", self.boots));
        record(format!("admissions {:?}", self.admissions));
        record(format!("poll verdicts {:?}", self.poll_verdicts));
        record(format!("cancels {:?}", self.cancel_results));
        record(format!("download errors {:?}", self.download_errors));
        record(format!("download retries {:?}", self.download_retries));
        record(format!(
            "slots {:?} running {:?} boot {:?}",
            self.slots, self.running_slot, self.boot_slot
        ));
    }
}

impl Device {
    fn stamp(&self) -> Stamp {
        Stamp {
            now: self.now,
            wall_millis: self.ts(),
            random: 0,
        }
    }

    /// The runtime settings the machines are built from: the device's version, floor and board, and the default timings the reference spells out as constants.
    fn settings(&self) -> DdiSettings {
        let mut settings = DdiSettings::new(self.running(), BOARD).expect("settings");
        settings.floor = self.floor;
        assert_eq!(settings.timings.firmware_deadline, FIRMWARE_DEADLINE);
        assert_eq!(settings.timings.restart_grace, RESTART_GRACE);
        assert_eq!(
            Duration::from_secs(settings.timings.firmware_timeout_secs),
            PER_OP
        );
        assert_eq!(settings.timings.health, self.health_cfg);
        settings
    }

    /// The boot of the machine driver: the flat boot plan, then a fresh worker that starts at the first poll cycle (where it sends the post-boot `proceeding`, at the same time as the reference does it here).
    fn boot_machines(&mut self) {
        let hw = self.hardware();
        let running = self.running();
        let plan = plan_boot(&mut self.store, Ok(hw), running);
        self.boots.push(format!("{:?}", plan.outcome.disposition));
        let mut flags = plan.flags;
        let mut report_owed = false;
        if plan.needs_repair() {
            let repaired = repair_at_boot(&mut self.store);
            let hw = self.hardware();
            report_owed =
                after_repair(&mut self.store, repaired, Ok(hw), running, &mut flags).report_owed;
        }
        assert!(
            !flags.roll_back_now_at_boot,
            "the fake device does not model a rollback demanded at boot"
        );
        self.flags = flags;
        let config = WorkerConfig::new(
            &self.settings(),
            self.chip.clone(),
            SLOT_CAPACITY,
            &flags,
            report_owed,
        );
        self.worker = Some(DdiWorker::new(self.endpoint.clone(), config));
        self.worker_started = false;
        self.health = None;
        self.flow = None;
        self.wake = Wake::default();
        self.download = None;
        self.restart_at = None;
        self.nudge_pending = false;
        self.boot_at = self.now;
        self.next_poll_at = Some(self.now);
        self.health_at = if flags.health_settled {
            None
        } else {
            Some(self.now + self.health_cfg.healthy_dwell)
        };
        self.events.push((self.now, Event::Boot));
    }

    /// The worker until it sleeps: each action is performed against the fake server and the fake hardware.
    fn poll_cycle_machines(&mut self, srv: &mut Server) {
        let epoch = self.epoch;
        let mut input = if self.worker_started {
            WorkerInput::Woken(core::mem::take(&mut self.wake))
        } else {
            self.worker_started = true;
            WorkerInput::Start {
                health_settled: self.flags.health_settled,
            }
        };
        loop {
            let at = self.stamp();
            let worker = self.worker.as_mut().expect("a worker after boot");
            // The reference reads the floor live, and S-DEV-09 case f raises it mid-life.
            worker.config_mut().floor = self.floor;
            let action = worker.poll(&mut self.store, input, at);
            let events = worker.drain_events();
            self.observe(events);
            input = match action {
                WorkerAction::Send(request) => match self.perform(srv, request) {
                    Some(input) => input,
                    None => return,
                },
                WorkerAction::ReadHardware(_) => WorkerInput::Hardware(Ok(self.hardware())),
                WorkerAction::SetFlags {
                    records_suspect,
                    refuse_mark_valid,
                } => {
                    self.flags.records_suspect = records_suspect;
                    self.flags.refuse_mark_valid = refuse_mark_valid;
                    WorkerInput::FlagsSet
                }
                WorkerAction::StartDownload(offer) => {
                    if self.crash == CrashPoint::AfterFirstProceeding {
                        self.crash = CrashPoint::None;
                        self.crash(srv);
                        return;
                    }
                    let target = other(self.running_slot);
                    self.start_download(srv, offer, target);
                    WorkerInput::DownloadStarted(true)
                }
                WorkerAction::CheckDownload => {
                    WorkerInput::DownloadRunning(self.download.is_some())
                }
                WorkerAction::RequestStop => {
                    WorkerInput::StopRequested(match self.download.as_mut() {
                        Some(d) => d.gate.request(),
                        None => CancelRequest::TooLate,
                    })
                }
                WorkerAction::AwaitDownload => {
                    self.finish_download(srv);
                    WorkerInput::DownloadEnded
                }
                WorkerAction::Sleep { until } => {
                    // A kept wake-up is handed over at once, as the tier's wake signal would end the sleep.
                    self.next_poll_at = match until {
                        Some(_) if self.wake.any() => Some(self.now),
                        until => until,
                    };
                    return;
                }
                WorkerAction::Ignored => panic!("the worker ignored its input"),
            };
            if self.epoch != epoch {
                return;
            }
        }
    }

    /// One request of the worker, through the same fake transport as the reference; `None` when a crash point cut the device.
    fn perform(&mut self, srv: &mut Server, request: DdiRequest) -> Option<WorkerInput> {
        let outcome = match &request {
            DdiRequest::Poll => {
                let url = self.endpoint.poll_url().to_string();
                let outcome = self.request(srv, Kind::E1, &url, "");
                return Some(WorkerInput::PollAnswered {
                    outcome,
                    admission_open: self.flags.admission_open,
                });
            }
            DdiRequest::ConfigData => {
                let body = self
                    .worker
                    .as_ref()
                    .expect("a worker")
                    .config_data()
                    .encode();
                let url = self.endpoint.config_data_url();
                self.request(srv, Kind::E6, &url, &body)
            }
            DdiRequest::Deployment(href) => match self.endpoint.check_href(href) {
                Ok(url) => self.request(srv, Kind::E2, url.as_str(), ""),
                Err(_) => RequestOutcome::ForeignOrigin,
            },
            DdiRequest::CancelAction(href) => match self.endpoint.check_href(href) {
                Ok(url) => self.request(srv, Kind::E4, url.as_str(), ""),
                Err(_) => RequestOutcome::ForeignOrigin,
            },
            DdiRequest::Feedback(_) | DdiRequest::Verdict { .. } => {
                let verdict = matches!(request, DdiRequest::Verdict { .. });
                if verdict && self.crash == CrashPoint::BeforeVerdictSend {
                    self.crash = CrashPoint::None;
                    self.crash(srv);
                    return None;
                }
                let post = request.feedback().expect("a feedback request");
                let kind = match post.path {
                    FeedbackPath::Deployment => Kind::E3,
                    FeedbackPath::Cancel => Kind::E5,
                };
                let url = post.url(&self.endpoint);
                let body = post.feedback().encode().expect("a feedback encodes");
                let outcome = self.request(srv, kind, &url, &body);
                if verdict && self.crash == CrashPoint::AfterVerdictSend {
                    self.crash = CrashPoint::None;
                    self.crash(srv);
                    return None;
                }
                outcome
            }
        };
        Some(WorkerInput::Answered(outcome))
    }

    /// The worker's events, mapped to what the reference records; an internal error fails the row.
    fn observe(&mut self, events: Vec<WorkerEvent>) {
        for event in events {
            match event {
                WorkerEvent::PollAnswered(verdict) => self.poll_verdicts.push(verdict),
                WorkerEvent::NotRetrieved(t) => {
                    self.admissions.push(format!("not_retrieved {t:?}"))
                }
                WorkerEvent::Unreportable(reason) => {
                    self.admissions.push(format!("unreportable {reason}"));
                }
                WorkerEvent::Deferred(action) => self.admissions.push(format!("deferred {action}")),
                WorkerEvent::Refused(refusal) => {
                    let case = refusal
                        .url_case
                        .map(|c| format!(" {}", c.name()))
                        .unwrap_or_default();
                    self.admissions.push(format!(
                        "refused {} {} {}{case}",
                        refusal.action,
                        refusal.code.as_u16(),
                        refusal.reason
                    ));
                }
                WorkerEvent::Admitted { action, .. } => {
                    self.admissions.push(format!("admitted {action}"));
                }
                WorkerEvent::PendingVerify(action) => {
                    self.admissions.push(format!("pending_verify {action}"));
                }
                WorkerEvent::ActionClosed(action) => {
                    self.admissions.push(format!("closed {action}"))
                }
                WorkerEvent::OfferBlocked(action) => {
                    self.admissions.push(format!("blocked {action}"));
                }
                WorkerEvent::CancelEnded(result) => self.cancel_results.push(result),
                WorkerEvent::AttemptNotBegun { error, .. } => {
                    panic!("attempt not begun: {error:?}")
                }
                WorkerEvent::VerdictPersisted { note: Err(e), .. }
                    if e != StoreError::VerdictPending =>
                {
                    panic!("verdict not persisted: {e:?}")
                }
                WorkerEvent::PollIgnored
                | WorkerEvent::CancelIgnored
                | WorkerEvent::VerdictAnswerIgnored
                | WorkerEvent::VerdictInFlight => panic!("internal error: {event:?}"),
                _ => {}
            }
        }
    }

    /// The health policy of the machine driver: the flow is started when the reference would first look at the slot (after the dwell) and stepped at each of its waits.
    fn health_step_machines(&mut self, srv: &mut Server) {
        let elapsed = self.now.saturating_sub(self.boot_at);
        let mut input = if self.health.is_some() {
            HealthInput::Waited
        } else {
            self.health = Some(HealthFlow::new(self.health_cfg, self.running()));
            HealthInput::Start
        };
        loop {
            let refuse = self.flags.refuse_mark_valid;
            let health = self.health.as_mut().expect("a health flow");
            let action = health.poll(&mut self.store, input, elapsed, refuse);
            let note = health.take_note();
            assert!(
                !matches!(
                    note,
                    Some(HealthNote::CompleteFailed(_) | HealthNote::RollbackNotNoted { .. })
                ),
                "{note:?}"
            );
            let running = self.running_slot;
            input = match action {
                HealthAction::ReadSlot => HealthInput::Slot(
                    if self.slots[idx(running)].state == SlotState::PendingVerify {
                        SlotReading::PendingVerify(running)
                    } else {
                        SlotReading::NotPending
                    },
                ),
                HealthAction::Wait(pause) => {
                    self.health_at = Some(self.now + pause);
                    return;
                }
                HealthAction::AskHealthy => {
                    HealthInput::Healthy(self.slots[idx(running)].image.is_some_and(|i| i.healthy))
                }
                HealthAction::MarkValid => {
                    self.slots[idx(running)].state = SlotState::Valid;
                    HealthInput::MarkedValid(true)
                }
                HealthAction::Done(verdict) => {
                    self.settle_health_machines(verdict);
                    return;
                }
                HealthAction::Rollback => {
                    self.slots[idx(running)].state = SlotState::Invalid;
                    self.boot_slot = other(running);
                    self.health_at = None;
                    self.events.push((self.now, Event::HealthDeadline));
                    self.reboot(srv);
                    return;
                }
                other => panic!("the fake device does not model {other:?}"),
            };
        }
    }

    /// The tier's settle of a health verdict: the flags, then the worker's wake-up.
    fn settle_health_machines(&mut self, verdict: HealthVerdict) {
        let event = match verdict {
            HealthVerdict::Applied => Event::HealthApplied,
            HealthVerdict::NothingToVerify => Event::HealthNothingToVerify,
            other => panic!("the fake device does not model {other:?}"),
        };
        if verdict.opens_admission() {
            self.flags.admission_open = true;
        }
        self.flags.health_settled = true;
        self.health_at = None;
        self.wake.health = Some(verdict);
        self.events.push((self.now, event));
        if let Some(at) = self.next_poll_at {
            self.next_poll_at = Some(at.min(self.now));
        }
    }

    fn flow_poll(&mut self, input: DownloadFlowInput) -> DownloadFlowAction {
        let at = self.stamp();
        self.flow
            .as_mut()
            .expect("a download flow")
            .poll(&mut self.store, input, at)
    }

    /// An attempt ended: the flow decides between the restart, a wait and the give-up.
    fn attempt_ended_machines(
        &mut self,
        srv: &mut Server,
        mut d: InFlight,
        result: Result<(), OtaError>,
    ) {
        if result.is_err() && self.cancel_after_failure {
            assert_eq!(d.gate.request(), CancelRequest::Accepted);
        }
        let input = DownloadFlowInput::AttemptEnded {
            result,
            retry_after: d.retry_after_raw.clone(),
        };
        match self.flow_poll(input) {
            DownloadFlowAction::Selected {
                recorded,
                proceeding,
            } => {
                assert_eq!(recorded, Ok(()));
                let url = proceeding.url(&self.endpoint);
                let body = proceeding.feedback().encode().expect("proceeding encodes");
                let outcome = self.request(srv, Kind::E3, &url, &body);
                let next = self.flow_poll(DownloadFlowInput::Answered(outcome));
                let DownloadFlowAction::RestartAt(at) = next else {
                    panic!("{next:?}");
                };
                self.restart_at = Some(at);
            }
            DownloadFlowAction::Wait { until, error, .. } => {
                self.download_retries.push((self.now, error));
                d.waiting = true;
                d.last_error = Some(error);
                d.due = until;
                self.download = Some(d);
            }
            DownloadFlowAction::Close => self.close_machines(&mut d.gate),
            other => panic!("{other:?}"),
        }
    }

    /// The wait between two attempts ended: the next attempt, or the give-up.
    fn wait_ended_machines(&mut self, srv: &mut Server, mut d: InFlight) {
        let cancel_requested = d.gate.is_requested();
        match self.flow_poll(DownloadFlowInput::WaitEnded { cancel_requested }) {
            DownloadFlowAction::Attempt { remaining } => {
                assert_eq!(
                    remaining,
                    FIRMWARE_DEADLINE.saturating_sub(self.now.saturating_sub(d.first_started))
                );
                self.begin_attempt(
                    srv,
                    d.offer,
                    d.target,
                    d.first_started,
                    d.attempts + 1,
                    d.gate,
                );
            }
            DownloadFlowAction::Close => {
                if !cancel_requested && self.cancel_at_giveup {
                    assert_eq!(d.gate.request(), CancelRequest::Accepted);
                }
                self.close_machines(&mut d.gate);
            }
            other => panic!("{other:?}"),
        }
    }

    /// The give-up: the switch closes, the flow settles the attempt, and the worker gets the ended-download wake-up.
    fn close_machines(&mut self, gate: &mut CancelGate) {
        let cancel_first = gate.close();
        let read = self.flow_poll(DownloadFlowInput::Closed { cancel_first });
        assert!(
            matches!(read, DownloadFlowAction::ReadHardware(_)),
            "{read:?}"
        );
        let hw = self.hardware();
        let ended = self.flow_poll(DownloadFlowInput::Hardware(Ok(hw)));
        let DownloadFlowAction::Ended(end) = ended else {
            panic!("{ended:?}");
        };
        assert!(matches!(end.settled, SettleNote::Settled(_)), "{end:?}");
        self.flow = None;
        self.download_errors.push(end.reason);
        self.wake.download_ended = true;
        if let Some(at) = self.next_poll_at {
            self.next_poll_at = Some(at.min(self.now));
        }
    }
}

// ── SCENARIO-HARNESS-ASSERTIONS ──────────────────────────────────────────────────

/// A fixed detail token, as the harness's `details_ok` reads one: `[a-z][a-z0-9_]{0,63}`.
fn is_token(d: &str) -> bool {
    let b = d.as_bytes();
    !b.is_empty()
        && b.len() <= 64
        && b[0].is_ascii_lowercase()
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_')
}

/// The harness's `_feedback_problems` over every E3 and E5: a closed body carries a result, no deployment
/// feedback is `rejected` or `canceled`, and every detail is a token. An empty `details` list is finding F1
/// and is not counted here.
fn feedback_problems(srv: &Server) -> Vec<String> {
    let mut problems = Vec::new();
    for r in srv
        .requests
        .iter()
        .filter(|r| matches!(r.kind, Kind::E3 | Kind::E5))
    {
        let execution = r.execution();
        let finished = r.finished();
        if execution == "closed" && finished != "success" && finished != "failure" {
            problems.push(format!("closed_without_result {}", r.body));
        }
        if r.kind == Kind::E3 && (execution == "rejected" || execution == "canceled") {
            problems.push(format!("unexpected_feedback {}", r.body));
        }
        let details = r.details();
        if details.len() > 10 || details.iter().any(|d| !is_token(d)) {
            problems.push(format!("details_shape {}", r.body));
        }
    }
    problems
}

fn bodies(reqs: &[&Req]) -> Vec<String> {
    reqs.iter().map(|r| r.body.clone()).collect()
}

/// `expect_refusal`: exactly `count` closed failures with `code` and `reason` as `details[0]`, no artifact
/// request, no download progress, no cancel feedback.
/// The action a terminal must name when the server offered exactly one.
fn check_action(srv: &Server, t: &Req) {
    if let [offer] = srv.offers.as_slice() {
        assert_eq!(
            t.action.as_deref(),
            Some(offer.action.as_str()),
            "{}",
            t.body
        );
    }
}

fn expect_refusal(srv: &Server, code: u64, reason: &str, count: usize) {
    let terminals = srv.terminals();
    assert_eq!(terminals.len(), count, "terminals {:?}", bodies(&terminals));
    for t in &terminals {
        check_action(srv, t);
        assert_eq!(t.finished(), "failure", "{}", t.body);
        assert_eq!(t.code(), Some(code), "{}", t.body);
        assert_eq!(t.detail0(), reason, "{}", t.body);
    }
    assert_eq!(srv.count(Kind::E8), 0, "an artifact was requested");
    assert!(
        srv.of(Kind::E3).iter().all(|r| r.is_terminal()),
        "a refusal sends no progress before its terminal"
    );
    assert_eq!(srv.count(Kind::E5), 0, "cancel feedback sent");
    assert_eq!(feedback_problems(srv), Vec::<String>::new());
}

/// `expect_success`: one closed success without a code, `detail` as its only detail (`None` for a plain
/// upgrade, see F1), exactly one artifact download, no cancel feedback.
fn expect_success(srv: &Server, detail: Option<&str>) {
    let terminals = srv.terminals();
    assert_eq!(terminals.len(), 1, "terminals {:?}", bodies(&terminals));
    let t = terminals[0];
    check_action(srv, t);
    assert_eq!(t.finished(), "success", "{}", t.body);
    assert_eq!(t.code(), None, "code on success: {}", t.body);
    let want: Vec<String> = detail.into_iter().map(String::from).collect();
    assert_eq!(t.details(), want, "{}", t.body);
    assert_eq!(srv.count(Kind::E8), 1, "artifact downloads");
    assert_eq!(srv.count(Kind::E5), 0, "cancel feedback sent");
    assert_eq!(feedback_problems(srv), Vec::<String>::new());
}

/// `expect_rolled_back`: one closed failure 1004 with `rolled_back` first (the second detail is the reason).
fn expect_rolled_back(srv: &Server) {
    let terminals = srv.terminals();
    assert_eq!(terminals.len(), 1, "terminals {:?}", bodies(&terminals));
    let t = terminals[0];
    check_action(srv, t);
    assert_eq!(t.finished(), "failure", "{}", t.body);
    assert_eq!(t.code(), Some(1004), "{}", t.body);
    assert_eq!(t.detail0(), "rolled_back", "{}", t.body);
    assert_eq!(srv.count(Kind::E8), 1, "artifact downloads");
    assert_eq!(feedback_problems(srv), Vec::<String>::new());
}

/// `expect_deferred`: no feedback of any kind, no download, and the document retrieved again on a later poll.
fn expect_deferred(srv: &Server) {
    assert_eq!(srv.count(Kind::E3), 0, "deployment feedback sent");
    assert_eq!(srv.count(Kind::E5), 0, "cancel feedback sent");
    assert_eq!(srv.count(Kind::E8), 0, "artifact requested");
    assert!(srv.count(Kind::E2) >= 2, "E2 count {}", srv.count(Kind::E2));
}

/// `expect_silent`: no document, feedback or artifact request at all (polls and configData are not judged).
fn expect_silent(srv: &Server) {
    for kind in [Kind::E2, Kind::E3, Kind::E5, Kind::E8] {
        assert_eq!(srv.count(kind), 0, "{kind:?} requested");
    }
}

/// `expect_cancel`: the cancel document retrieved, never the deployment, no deployment feedback, exactly
/// `attempts` cancel answers of the one shape, none with a code. The `cancel_acknowledged` and
/// `cancel_rejected` details of the harness are finding F1.
fn expect_cancel(srv: &Server, acknowledged: bool, gets: usize, attempts: usize) {
    assert_eq!(srv.count(Kind::E4), gets, "cancel document retrievals");
    assert_eq!(srv.count(Kind::E2), 0, "deployment retrieved");
    assert_eq!(srv.count(Kind::E3), 0, "deployment feedback sent");
    let answers = srv.of(Kind::E5);
    assert_eq!(
        answers.len(),
        attempts,
        "cancel answers {:?}",
        bodies(&answers)
    );
    let want = if acknowledged {
        ("closed", "success")
    } else {
        ("rejected", "none")
    };
    for a in &answers {
        assert_eq!(
            (a.execution().as_str(), a.finished().as_str()),
            want,
            "{}",
            a.body
        );
        assert_eq!(a.code(), None, "{}", a.body);
    }
    assert_eq!(feedback_problems(srv), Vec::<String>::new());
}

/// The harness's `outcome`: every terminal as `success <detail>` or `<code> <detail>`, joined with `; `.
fn outcome(srv: &Server) -> String {
    let parts: Vec<String> = srv
        .terminals()
        .iter()
        .map(|t| {
            let d0 = t.detail0();
            if t.finished() == "success" {
                if d0.is_empty() {
                    "success".to_string()
                } else {
                    format!("success {d0}")
                }
            } else {
                format!("{} {d0}", t.code().unwrap_or(0))
            }
        })
        .collect();
    if parts.is_empty() {
        "none".to_string()
    } else {
        parts.join("; ")
    }
}

/// The harness's `config_problems` for one configData PUT.
fn config_check(r: &Req, chip: &str, board: &str, version: Option<&str>) {
    let body = r.json();
    let obj = body.as_object().expect("configData is an object");
    let mut keys: Vec<&String> = obj.keys().collect();
    keys.sort();
    assert_eq!(keys, ["data", "mode"], "{}", r.body);
    assert_eq!(body["mode"], "merge", "{}", r.body);
    let data = body["data"].as_object().expect("data is an object");
    let mut data_keys: Vec<&String> = data.keys().collect();
    data_keys.sort();
    assert_eq!(data_keys, ["board", "chip", "version"], "{}", r.body);
    assert_eq!(body["data"]["chip"], chip, "{}", r.body);
    assert_eq!(body["data"]["board"], board, "{}", r.body);
    let text = body["data"]["version"].as_str().expect("version text");
    assert!(Version::parse(text).is_ok(), "{}", r.body);
    if let Some(want) = version {
        assert_eq!(text, want, "{}", r.body);
    }
}

fn config_versions(srv: &Server) -> Vec<String> {
    srv.of(Kind::E6)
        .iter()
        .map(|r| {
            r.json()["data"]["version"]
                .as_str()
                .unwrap_or("?")
                .to_string()
        })
        .collect()
}

/// The status objects of the requests are identical (a resend repeats the report; the timestamp differs, F2).
fn same_status(reqs: &[&Req]) -> bool {
    match reqs.split_first() {
        Some((first, rest)) => rest
            .iter()
            .all(|r| r.status_object() == first.status_object()),
        None => false,
    }
}

fn gaps_at_least(reqs: &[&Req], floor: Duration) -> bool {
    reqs.windows(2)
        .all(|w| w[1].t.saturating_sub(w[0].t) >= floor)
}

fn e2_actions(srv: &Server) -> Vec<String> {
    srv.of(Kind::E2)
        .iter()
        .map(|r| r.action.clone().unwrap_or_default())
        .collect()
}

fn boots(d: &Device) -> usize {
    d.events.iter().filter(|(_, e)| *e == Event::Boot).count()
}

// ── shared scenarios: the device side ────────────────────────────────────────────

/// S01: a forced assignment is downloaded, verified, booted and reported `closed` + `success` after the reboot.
#[test]
fn s01_forced_assignment_installs_and_reports_success() {
    differential(|| {
        let mut srv = Server::new();
        srv.offer(&Doc::new("1", "1.1.0"));
        let mut d = Device::new("1.0.0");
        d.run_for(&mut srv, secs(120));

        expect_success(&srv, None);
        assert_eq!(d.running(), v("1.1.0"));
        assert_eq!(d.admissions, ["admitted 1"]);
        assert_eq!(boots(&d), 2, "the install reboots once");
        let terminal_t = srv.terminals()[0].t;
        assert_eq!(srv.of(Kind::E3)[0].execution(), "proceeding");
        assert!(
            srv.of(Kind::E8)[0].t <= terminal_t,
            "the download precedes the terminal"
        );
        assert!(
            srv.of(Kind::E3)
                .iter()
                .all(|r| r.is_terminal() || r.t <= terminal_t),
            "every progress report precedes the terminal"
        );
        let reboot_t = d
            .events
            .iter()
            .filter(|(_, e)| *e == Event::Boot)
            .nth(1)
            .map(|(t, _)| *t);
        assert!(
            reboot_t.is_some_and(|t| t <= terminal_t),
            "the reboot precedes the terminal"
        );
        assert_eq!(
            config_versions(&srv),
            ["1.0.0", "1.1.0"],
            "one configData per boot"
        );
    });
}

/// S02: `attempt` handling is applied like `forced`.
#[test]
fn s02_soft_assignment_is_applied_like_forced() {
    differential(|| {
        let mut srv = Server::new();
        srv.offer(&Doc::new("1", "1.1.0").handling("attempt", "attempt"));
        let mut d = Device::new("1.0.0");
        d.run_for(&mut srv, secs(120));

        expect_success(&srv, None);
        assert_eq!(d.running(), v("1.1.0"));
    });
}

/// S09: an image that never turns healthy is rolled back at the failure deadline, and the old image reports
/// `closed` + `failure` + 1004 with `rolled_back` first; the version stays remembered.
#[test]
fn s09_self_rollback_reports_1004_rolled_back() {
    differential(|| {
        let mut srv = Server::new();
        srv.offer(&Doc::new("1", "1.1.0"));
        let mut d = Device::new("1.0.0").unhealthy("1.1.0");
        d.run_for(&mut srv, secs(400));

        expect_rolled_back(&srv);
        assert_eq!(d.running(), v("1.0.0"));
        assert!(d.events.iter().any(|(_, e)| *e == Event::HealthDeadline));
        assert!(
            d.boots
                .last()
                .is_some_and(|b| b.contains("ReportedRollback")),
            "{:?}",
            d.boots
        );
        assert_eq!(
            srv.terminals()[0].details(),
            ["rolled_back", "health_deadline"]
        );
        assert_eq!(d.store.refused(), Ok(Some(v("1.1.0"))));
    });
}

/// S17b: a digest that does not match the bytes ends the download after the transfer with 1002
/// `checksum_mismatch`, before any activation; the device keeps running the old image.
#[test]
fn s17b_digest_mismatch_reports_1002_checksum_mismatch() {
    differential(|| {
        let mut srv = Server::new();
        srv.offer(&Doc::new("1", "1.1.0").wrong_sha256());
        let mut d = Device::new("1.0.0");
        d.run_for(&mut srv, secs(120));

        let terminals = srv.terminals();
        assert_eq!(terminals.len(), 1, "{:?}", bodies(&terminals));
        assert_eq!(terminals[0].code(), Some(1002));
        assert_eq!(terminals[0].details(), ["checksum_mismatch"]);
        assert_eq!(srv.count(Kind::E8), 1);
        assert_eq!(d.running(), v("1.0.0"));
        assert_eq!(boots(&d), 1, "no reboot");
        assert_eq!(d.download_errors, [OtaError::ChecksumMismatch]);
        assert!(
            d.download_retries.is_empty(),
            "a final error is not retried"
        );
        assert_eq!(feedback_problems(&srv), Vec::<String>::new());
    });
}

/// S-DL-01: a transient download error (a 503, then a connection that cannot be made) is retried after the back-off inside the one
/// deadline; the install then completes with `closed` + `success` and no 1001.
#[test]
fn s_dl_01_transient_download_errors_are_retried_then_the_install_succeeds() {
    differential(|| {
        let mut srv = Server::new();
        srv.offer(&Doc::new("1", "1.1.0"));
        srv.queue(Kind::E8, &[status(503, ""), Reply::Unreachable]);
        let mut d = Device::new("1.0.0");
        d.run_for(&mut srv, secs(120));

        let terminals = srv.terminals();
        assert_eq!(terminals.len(), 1, "{:?}", bodies(&terminals));
        assert_eq!(terminals[0].finished(), "success", "{}", terminals[0].body);
        assert_eq!(terminals[0].code(), None);
        assert_eq!(srv.count(Kind::E8), 3, "two failed attempts and the third");
        assert_eq!(
            d.download_retries,
            [
                (secs(0), OtaError::DownloadFailed { status: 503 }),
                (secs(5), OtaError::ServerUnreachable)
            ],
            "back-off 5 s, then 10 s"
        );
        let attempts: Vec<Duration> = srv.of(Kind::E8).iter().map(|r| r.t).collect();
        assert_eq!(attempts, [secs(0), secs(5), secs(15)]);
        assert!(
            d.download_errors.is_empty(),
            "no attempt was settled failed"
        );
        assert_eq!(d.running(), v("1.1.0"));
        assert_eq!(feedback_problems(&srv), Vec::<String>::new());
    });
}

/// S-DL-02: a transient error that lasts is retried until the 300 s deadline passes (the last wait is clamped to the time left), then
/// it is settled once as 1001 with the last error as its reason; the verdict is not sent before that.
#[test]
fn s_dl_02_a_transient_error_until_the_deadline_is_one_terminal_1001() {
    differential(|| {
        let mut srv = Server::new();
        srv.offer(&Doc::new("1", "1.1.0"));
        srv.on(Kind::E8, status(503, ""));
        let mut d = Device::new("1.0.0");
        d.run_for(&mut srv, secs(400));

        let terminals = srv.terminals();
        assert_eq!(terminals.len(), 1, "{:?}", bodies(&terminals));
        assert_eq!(terminals[0].code(), Some(1001));
        assert_eq!(terminals[0].details(), ["download_failed"]);
        let attempts: Vec<Duration> = srv.of(Kind::E8).iter().map(|r| r.t).collect();
        assert_eq!(
        attempts,
        [0, 5, 15, 35, 75, 135, 195, 255].map(secs),
        "5 s doubling to 60 s; the last wait is clamped to 45 s and ends at the deadline, with no attempt after it"
    );
        assert!(
            terminals[0].t >= secs(300),
            "no verdict before the deadline passes"
        );
        assert_eq!(d.download_retries.len(), 8);
        assert_eq!(
            d.download_errors,
            [OtaError::DownloadFailed { status: 503 }],
            "settled once"
        );
        assert_eq!(d.running(), v("1.0.0"));
        assert_eq!(feedback_problems(&srv), Vec::<String>::new());
    });
}

/// S-DL-03: a final error (an artifact 404, a digest mismatch) is settled at once: one request, no retry, 1001 or 1002.
#[test]
fn s_dl_03_a_final_download_error_is_not_retried() {
    differential(|| {
        let mut srv = Server::new();
        srv.offer(&Doc::new("1", "1.1.0"));
        srv.on(Kind::E8, status(404, ""));
        let mut d = Device::new("1.0.0");
        d.run_for(&mut srv, secs(120));

        let terminals = srv.terminals();
        assert_eq!(terminals.len(), 1, "{:?}", bodies(&terminals));
        assert_eq!(terminals[0].code(), Some(1001));
        assert_eq!(srv.count(Kind::E8), 1);
        assert!(d.download_retries.is_empty());
        assert_eq!(
            d.download_errors,
            [OtaError::DownloadFailed { status: 404 }]
        );
        assert!(
            terminals[0].t < secs(60),
            "settled at once, not after a back-off"
        );
        assert_eq!(feedback_problems(&srv), Vec::<String>::new());
    });
}

/// S-DL-04: a cancel that arrives during the back-off between two attempts ends the download at once, acknowledged, with no verdict.
#[test]
fn s_dl_04_cancel_during_the_retry_back_off_is_acknowledged() {
    differential(|| {
        let mut srv = Server::new();
        srv.offer(&Doc::new("1", "1.1.0"));
        srv.on(Kind::E8, status(503, ""));
        let mut d = Device::new("1.0.0");
        d.run_for(&mut srv, secs(8));
        assert!(
            d.download.as_ref().is_some_and(|x| x.waiting),
            "in the back-off"
        );

        srv.cancel("1");
        d.nudge();
        d.run_for(&mut srv, secs(60));

        assert_eq!(d.cancel_results, [Some(CancelResult::Acknowledged)]);
        assert_eq!(d.download_errors, [OtaError::Cancelled]);
        assert_eq!(srv.count(Kind::E8), 2, "no attempt after the cancel");
        assert!(
            srv.terminals().is_empty(),
            "a cancelled attempt reports no verdict"
        );
        assert_eq!(d.running(), v("1.0.0"));
        assert_eq!(feedback_problems(&srv), Vec::<String>::new());
    });
}

/// S-DL-05: a cancel that lands right after a failed attempt, before the retry decision, stays accepted: the download ends cancelled, makes no later attempt and reports no verdict.
#[test]
fn s_dl_05_cancel_between_a_failed_attempt_and_the_retry_is_acknowledged() {
    differential(|| {
        let mut srv = Server::new();
        srv.offer(&Doc::new("1", "1.1.0"));
        srv.on(Kind::E8, status(503, ""));
        let mut d = Device::new("1.0.0");
        d.cancel_after_failure = true;
        d.run_for(&mut srv, secs(6));

        let attempts: Vec<Duration> = srv.of(Kind::E8).iter().map(|r| r.t).collect();
        assert_eq!(attempts[0], secs(0));
        assert_eq!(
            d.download_retries.len(),
            1,
            "the retry was decided, then the cancel ended the wait"
        );
        assert_eq!(d.download_errors, [OtaError::Cancelled]);
        assert!(
            srv.terminals().is_empty(),
            "a cancelled attempt reports no verdict"
        );
        assert_eq!(d.running(), v("1.0.0"));
        assert_eq!(feedback_problems(&srv), Vec::<String>::new());
    });
}

/// S-DL-06: a cancel that lands at the final give-up (the deadline has passed after the last wait) wins over the 1001: nothing is reported.
#[test]
fn s_dl_06_cancel_at_the_deadline_give_up_is_acknowledged() {
    differential(|| {
        let mut srv = Server::new();
        srv.offer(&Doc::new("1", "1.1.0"));
        srv.on(Kind::E8, status(503, ""));
        let mut d = Device::new("1.0.0");
        d.cancel_at_giveup = true;
        d.run_for(&mut srv, secs(300));

        let attempts: Vec<Duration> = srv.of(Kind::E8).iter().map(|r| r.t).collect();
        assert_eq!(
            attempts[..8],
            [0, 5, 15, 35, 75, 135, 195, 255].map(secs),
            "no attempt at the deadline"
        );
        assert_eq!(d.download_errors, [OtaError::Cancelled]);
        assert!(
            srv.terminals().is_empty(),
            "no 1001 after an accepted cancel"
        );
        assert_eq!(d.running(), v("1.0.0"));
        assert_eq!(feedback_problems(&srv), Vec::<String>::new());
    });
}

/// S-DL-07: a 429 with `Retry-After: 120` delays the next attempt to 120 s or later, and a 413 is retried after the plain back-off.
#[test]
fn s_dl_07_retry_after_sets_the_wait_and_a_413_is_retried() {
    differential(|| {
        let mut srv = Server::new();
        srv.offer(&Doc::new("1", "1.1.0"));
        srv.queue(Kind::E8, &[status_retry_after(429, "120")]);
        let mut d = Device::new("1.0.0");
        d.run_for(&mut srv, secs(300));
        let attempts: Vec<Duration> = srv.of(Kind::E8).iter().map(|r| r.t).collect();
        assert_eq!(attempts, [secs(0), secs(120)]);
        let terminals = srv.terminals();
        assert_eq!(terminals.len(), 1, "{:?}", bodies(&terminals));
        assert_eq!(terminals[0].finished(), "success", "{}", terminals[0].body);

        let mut srv = Server::new();
        srv.offer(&Doc::new("1", "1.1.0"));
        srv.queue(Kind::E8, &[status(413, "")]);
        let mut d = Device::new("1.0.0");
        d.run_for(&mut srv, secs(120));
        let attempts: Vec<Duration> = srv.of(Kind::E8).iter().map(|r| r.t).collect();
        assert_eq!(attempts, [secs(0), secs(5)]);
        assert!(d.download_errors.is_empty());
        assert_eq!(d.running(), v("1.1.0"));
    });
}

// ── case tables ──────────────────────────────────────────────────────────────────

/// Runs one table case and names it in the failure.
fn check(name: &str, f: impl FnOnce()) {
    if let Err(e) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        let msg = e
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| e.downcast_ref::<&str>().map(|s| (*s).to_string()))
            .unwrap_or_default();
        panic!("case {name}: {msg}");
    }
}

/// What a table case expects of the device, as the harness's `verdict` tuples.
#[derive(Debug, Clone, Copy)]
enum Expect {
    Refuse(u64, &'static str),
    Success(Option<&'static str>),
    Silent,
}

const INV: Expect = Expect::Refuse(1006, "manifest_invalid");
const UTD: Expect = Expect::Refuse(1007, "up_to_date");
const TMM: Expect = Expect::Refuse(1007, "target_mismatch");

fn verdict(srv: &Server, expect: Expect) {
    match expect {
        Expect::Refuse(code, reason) => expect_refusal(srv, code, reason, 1),
        Expect::Success(detail) => expect_success(srv, detail),
        Expect::Silent => expect_silent(srv),
    }
}

/// One offered document against a fresh device running `running`, for long enough to install.
fn run_case(running: &str, doc: &Doc, setup: impl FnOnce(Device) -> Device) -> (Server, Device) {
    let mut srv = Server::new();
    srv.offer(doc);
    let mut d = setup(Device::new(running));
    d.run_for(&mut srv, secs(200));
    (srv, d)
}

fn same(d: Device) -> Device {
    d
}

fn hex_of(doc: &Doc) -> String {
    hex(&sha256_of(&doc.artifact))
}

// ── shared scenarios: the device side, continued ─────────────────────────────────

/// S10: a cancel that arrives while the download runs stops it and is acknowledged on the cancel path; the
/// attempt is cleared without a verdict and the old image keeps running.
#[test]
fn s10_cancel_during_the_download_is_acknowledged() {
    differential(|| {
        let mut srv = Server::new();
        srv.offer(&Doc::new("1", "1.1.0"));
        let mut d = Device::new("1.0.0").download_rate(100);
        d.run_for(&mut srv, secs(2));
        assert!(d.download.is_some(), "the download is in flight");

        srv.cancel("1");
        d.nudge();
        d.run_for(&mut srv, secs(60));

        assert_eq!(srv.count(Kind::E4), 1);
        let answers = srv.of(Kind::E5);
        assert_eq!(answers.len(), 1, "{:?}", bodies(&answers));
        assert_eq!(
            (
                answers[0].execution().as_str(),
                answers[0].finished().as_str()
            ),
            ("closed", "success")
        );
        assert_eq!(
            srv.of(Kind::E4)[0].t,
            secs(10),
            "the nudge waits exactly 10 s after the last poll"
        );
        assert_eq!(d.cancel_results, [Some(CancelResult::Acknowledged)]);
        assert_eq!(d.download_errors, [OtaError::Cancelled]);
        assert!(
            srv.terminals().is_empty(),
            "a cancelled attempt reports no verdict"
        );
        assert!(!d.kv.has("att_ver"), "the attempt is cleared");
        assert_eq!(d.running(), v("1.0.0"));
        assert_eq!(boots(&d), 1);
        assert_eq!(feedback_problems(&srv), Vec::<String>::new());
    });
}

/// S11: a cancel that arrives after the activation (here at the first poll of the new image, after the restart)
/// is rejected on the cancel path, and the install completes.
#[test]
fn s11_cancel_after_activation_is_rejected_and_the_install_completes() {
    differential(|| {
        let mut srv = Server::new();
        srv.offer(&Doc::new("1", "1.1.0"));
        let mut d = Device::new("1.0.0");
        d.run_for(&mut srv, Duration::from_millis(500));
        assert_eq!(
            d.boot_slot, SLOT_B,
            "the new slot is selected, the restart is pending"
        );

        srv.cancel("1");
        d.run_for(&mut srv, secs(120));

        assert_eq!(srv.count(Kind::E4), 1);
        let answers = srv.of(Kind::E5);
        assert_eq!(answers.len(), 1, "{:?}", bodies(&answers));
        assert_eq!(
            (
                answers[0].execution().as_str(),
                answers[0].finished().as_str()
            ),
            ("rejected", "none")
        );
        assert_eq!(d.cancel_results, [Some(CancelResult::Rejected)]);
        let terminals = srv.terminals();
        assert_eq!(terminals.len(), 1, "{:?}", bodies(&terminals));
        assert_eq!(terminals[0].finished(), "success");
        assert_eq!(d.running(), v("1.1.0"));
        assert_eq!(feedback_problems(&srv), Vec::<String>::new());
    });
}

/// S13: a power cut right after the first `proceeding` clears the interrupted attempt at the next boot, and the
/// action is completed in the next life with one artifact download and one terminal. The document is
/// retrieved again (F3): this client resumes nothing from a copy of the document.
#[test]
fn s13_restart_mid_action_completes_the_action_once() {
    differential(|| {
        let mut srv = Server::new();
        srv.offer(&Doc::new("1", "1.1.0"));
        let mut d = Device::new("1.0.0").crash_at(CrashPoint::AfterFirstProceeding);
        d.run_for(&mut srv, secs(200));

        expect_success(&srv, None);
        assert_eq!(boots(&d), 3, "the crash and the install reboot");
        assert!(d.events.iter().any(|(_, e)| *e == Event::Crash));
        assert!(
            d.boots[1].starts_with("Settled("),
            "the interrupted attempt is cleared, not awaited: {:?}",
            d.boots
        );
        assert_eq!(
            e2_actions(&srv),
            ["1", "1"],
            "F3: the interrupted attempt is offered and retrieved again"
        );
        assert_eq!(d.running(), v("1.1.0"));
    });
}

/// S15 and SEC-24: a nudge makes one ordinary authenticated poll at once; the nudge carries no payload into
/// the machine (`PollMachine::nudge` takes only the time), so a hostile payload cannot name a URL.
#[test]
fn s15_and_sec_24_a_nudge_polls_at_once_and_carries_no_payload() {
    differential(|| {
        let mut srv = Server::new();
        let mut d = Device::new("1.0.0");
        d.run_for(&mut srv, secs(15));
        assert_eq!(srv.count(Kind::E1), 1);

        d.nudge();
        d.run_until(&mut srv, secs(16));

        let polls = srv.of(Kind::E1);
        assert_eq!(polls.len(), 2, "the nudge polls at once");
        assert_eq!(polls[1].t, secs(15));
        assert_eq!(
            polls[1].url,
            d.endpoint.poll_url(),
            "an ordinary poll of the configured base"
        );
        expect_silent(&srv);
    });
}

/// SEC-25 (device part): a burst of nudges is debounced to one poll per 10 s.
#[test]
fn sec_25_a_burst_of_nudges_is_debounced_to_one_poll_per_10_s() {
    differential(|| {
        let mut srv = Server::new();
        let mut d = Device::new("1.0.0");
        d.run_for(&mut srv, secs(20));
        for i in 0..100u64 {
            d.run_until(&mut srv, secs(20) + Duration::from_millis(i * 10));
            d.nudge();
        }
        d.run_until(&mut srv, secs(60));

        let polls = srv.of(Kind::E1);
        let times: Vec<Duration> = polls.iter().map(|p| p.t).collect();
        assert_eq!(times, [secs(0), secs(20), secs(30), secs(60)]);
        let in_window = times
            .iter()
            .filter(|t| **t >= secs(20) && **t < secs(30))
            .count();
        assert_eq!(in_window, 1);
        assert!(
            polls.iter().any(|p| p.t >= secs(30)),
            "the kept nudge polls once the 10 s have passed"
        );
    });
}

// ── device reference rows ────────────────────────────────────────────────────────

/// S-DEV-01: a verified download, `closed` + `success` only after the reboot, `configData` once per boot; the
/// digest compares case-insensitively.
#[test]
fn s_dev_01_install_order_and_config_data_per_boot() {
    differential(|| {
        for (name, upper) in [("lower_case_sha256", false), ("upper_case_sha256", true)] {
            let mut doc = Doc::new("1", "1.0.0");
            if upper {
                let text = hex_of(&doc).to_uppercase();
                doc = doc.sha256_text(&text);
            }
            let (srv, d) = run_case("0.0.0", &doc, same);
            check(name, || {
                expect_success(&srv, None);
                let terminal_t = srv.terminals()[0].t;
                assert_eq!(srv.of(Kind::E3)[0].execution(), "proceeding");
                assert!(srv.of(Kind::E8)[0].t <= terminal_t);
                assert!(srv
                    .of(Kind::E3)
                    .iter()
                    .all(|r| r.is_terminal() || r.t <= terminal_t));
                assert_eq!(boots(&d), 2);
                let puts = srv.of(Kind::E6);
                assert_eq!(puts.len(), 2, "one configData per boot");
                for p in &puts {
                    config_check(p, CHIP, BOARD, None);
                }
                assert_eq!(config_versions(&srv), ["0.0.0", "1.0.0"]);
            });
        }
    });
}

/// S-DEV-02: the document rules of §3a refuse a defective document with 1006 before any download, and accept
/// the edge cases; an accepted document of the running version ends 1007 `up_to_date`.
#[test]
fn s_dev_02_document_rules_refuse_1006_before_any_download() {
    differential(|| {
        let base = Doc::new("1", "1.0.0");
        let sha = hex_of(&base);
        let dup = |list: &mut Value| {
            let list = list.as_array_mut().expect("an array");
            let first = list[0].clone();
            list.push(first);
        };
        let refused: Vec<(&str, Doc)> = vec![
            (
                "chunks_0",
                base.clone().edit(|v| v["deployment"]["chunks"] = json!([])),
            ),
            (
                "chunks_2",
                base.clone().edit(|v| dup(&mut v["deployment"]["chunks"])),
            ),
            (
                "artifacts_0",
                base.clone()
                    .edit(|v| v["deployment"]["chunks"][0]["artifacts"] = json!([])),
            ),
            (
                "artifacts_2",
                base.clone()
                    .edit(|v| dup(&mut v["deployment"]["chunks"][0]["artifacts"])),
            ),
            (
                "size_missing",
                base.clone().edit(|v| {
                    v["deployment"]["chunks"][0]["artifacts"][0]
                        .as_object_mut()
                        .unwrap()
                        .remove("size");
                }),
            ),
            ("size_string", base.clone().size(json!("4096"))),
            ("size_negative", base.clone().size(json!(-1))),
            (
                "sha256_missing",
                base.clone().edit(|v| {
                    v["deployment"]["chunks"][0]["artifacts"][0]["hashes"]
                        .as_object_mut()
                        .unwrap()
                        .remove("sha256");
                }),
            ),
            ("sha256_63_chars", base.clone().sha256_text(&sha[..63])),
            ("sha256_non_hex", base.clone().sha256_text(&"g".repeat(64))),
            (
                "sha256_number",
                base.clone().edit(|v| {
                    v["deployment"]["chunks"][0]["artifacts"][0]["hashes"]["sha256"] = json!(12345)
                }),
            ),
            (
                "only_download_link",
                base.clone().edit(|v| {
                    let href = v["deployment"]["chunks"][0]["artifacts"][0]["_links"]
                        ["download-http"]["href"]
                        .clone();
                    v["deployment"]["chunks"][0]["artifacts"][0]["_links"] =
                        json!({"download": {"href": href}});
                }),
            ),
            (
                "size_above_slot",
                base.clone().size(json!(SLOT_CAPACITY + 1)),
            ),
            ("encrypted", base.clone().encrypted()),
            ("download_maybe", base.clone().handling("maybe", "forced")),
            (
                "update_number",
                base.clone().edit(|v| v["deployment"]["update"] = json!(7)),
            ),
            ("version_two_parts", base.clone().version("1.0")),
            ("version_leading_zero", base.clone().version("01.0.0")),
            ("version_pre_release", base.clone().version("1.0.0-rc1")),
            ("version_above_65535", base.clone().version("1.0.65536")),
            ("version_plus_sign", base.clone().version("+1.0.0")),
            ("version_trailing_newline", base.clone().version("1.0.0\n")),
            ("not_json", base.clone().raw(b"this is not json")),
            ("json_array", base.clone().raw(b"[1, 2]")),
            ("no_deployment", base.clone().raw(br#"{"id":"1"}"#)),
        ];
        for (name, doc) in &refused {
            let (srv, _) = run_case("1.0.0", doc, same);
            check(name, || verdict(&srv, INV));
        }

        let accepted: Vec<(&str, &str, Doc)> = vec![
            ("baseline_valid", "1.0.0", base.clone()),
            ("version_0_0_0", "0.0.0", base.clone().version("0.0.0")),
            (
                "version_65535_each",
                "65535.65535.65535",
                base.clone().version("65535.65535.65535"),
            ),
            (
                "size_equals_slot",
                "1.0.0",
                base.clone().size(json!(SLOT_CAPACITY)),
            ),
            (
                "sha256_upper_case",
                "1.0.0",
                base.clone().sha256_text(&sha.to_uppercase()),
            ),
            ("document_4_kib", "1.0.0", base.clone().padded_to(4096)),
        ];
        for (name, running, doc) in &accepted {
            let (srv, _) = run_case(running, doc, same);
            check(name, || verdict(&srv, UTD));
        }
    });
}

/// S-DEV-03: `chip` exactly once with the right grammar, `downgrade` at most once with exactly `allowed`,
/// keys case-sensitive, other keys ignored.
#[test]
fn s_dev_03_metadata_rules() {
    differential(|| {
        let base = Doc::new("1", "1.0.0");
        let long32 = "a".repeat(32);
        let long33 = "a".repeat(33);
        let invalid: Vec<(&str, Doc)> = vec![
            ("chip_missing", base.clone().metadata(&[])),
            ("chip_twice_same", base.clone().add_metadata("chip", CHIP)),
            (
                "chip_twice_different",
                base.clone().add_metadata("chip", OTHER_CHIP),
            ),
            ("chip_empty", base.clone().metadata(&[("chip", "")])),
            (
                "chip_upper_case",
                base.clone().metadata(&[("chip", "ESP32C3")]),
            ),
            (
                "chip_with_dash",
                base.clone().metadata(&[("chip", "esp32-c3")]),
            ),
            ("chip_33_chars", base.clone().metadata(&[("chip", &long33)])),
            ("only_Chip_key", base.clone().metadata(&[("Chip", CHIP)])),
            ("downgrade_twice", base.clone().marker().marker()),
            (
                "downgrade_Allowed",
                base.clone().add_metadata("downgrade", "Allowed"),
            ),
            (
                "downgrade_ALLOWED",
                base.clone().add_metadata("downgrade", "ALLOWED"),
            ),
            (
                "downgrade_true",
                base.clone().add_metadata("downgrade", "true"),
            ),
            (
                "downgrade_empty",
                base.clone().add_metadata("downgrade", ""),
            ),
            (
                "metadata_key_absent",
                base.clone().edit(|v| {
                    v["deployment"]["chunks"][0]
                        .as_object_mut()
                        .unwrap()
                        .remove("metadata");
                }),
            ),
        ];
        for (name, doc) in &invalid {
            let (srv, _) = run_case("1.0.0", doc, same);
            check(name, || verdict(&srv, INV));
        }

        let valid: Vec<(&str, &str, Doc, Expect)> = vec![
            (
                "ignored_Chip_key",
                "1.0.0",
                base.clone().add_metadata("Chip", OTHER_CHIP),
                UTD,
            ),
            (
                "ignored_unknown_key",
                "1.0.0",
                base.clone().add_metadata("foo", "x"),
                UTD,
            ),
            (
                "chip_32_chars",
                "1.0.0",
                base.clone().metadata(&[("chip", &long32)]),
                TMM,
            ),
            (
                "Downgrade_key_is_not_the_marker",
                "2.0.0",
                base.clone().add_metadata("Downgrade", "allowed"),
                Expect::Refuse(1007, "downgrade"),
            ),
            (
                "downgrade_marker_control",
                "2.0.0",
                base.clone().marker(),
                Expect::Success(Some("downgrade")),
            ),
        ];
        for (name, running, doc, expect) in &valid {
            let (srv, _) = run_case(running, doc, same);
            check(name, || verdict(&srv, *expect));
        }
    });
}

/// S-DEV-04: a document of 8192 bytes is read, one of 8193 is refused 1006; an over-cap poll answer is never
/// acted on and polling continues. Whether the connection is closed early is a transport property
/// (`ResponseBody`), not judged here.
#[test]
fn s_dev_04_document_cap_8192_accepted_8193_refused() {
    differential(|| {
        let base = Doc::new("1", "1.0.0");
        let (srv, _) = run_case("1.0.0", &base.clone().padded_to(8192), same);
        check("document_8192_bytes", || verdict(&srv, UTD));
        let (srv, _) = run_case("1.0.0", &base.clone().padded_to(8193), same);
        check("document_8193_bytes", || verdict(&srv, INV));

        let big = vec![b' '; 65536];
        let mut srv = Server::new();
        srv.offer(&base);
        srv.on(
            Kind::E2,
            Reply::Status {
                status: 200,
                body: big.clone(),
                retry_after: None,
                declared_len: None,
            },
        );
        let mut d = Device::new("1.0.0");
        d.run_for(&mut srv, secs(60));
        check("document_stream", || verdict(&srv, INV));

        let mut srv = Server::new();
        srv.offer(&base);
        srv.on(
            Kind::E1,
            Reply::Status {
                status: 200,
                body: big,
                retry_after: None,
                declared_len: None,
            },
        );
        let mut d = Device::new("1.0.0");
        d.run_for(&mut srv, secs(40 * 60));
        check("poll_answer_stream", || {
            verdict(&srv, Expect::Silent);
            let polls: Vec<Duration> = srv.of(Kind::E1).iter().map(|p| p.t).take(4).collect();
            assert_eq!(
                polls,
                [secs(0), secs(300), secs(900), secs(2100)],
                "polling continues with the back-off from the default interval"
            );
        });
    });
}

/// S-DEV-05: `skip` defers a valid document without any feedback and the document is retrieved again; the
/// chip and version gates come after the deferral, the document rules before it.
#[test]
fn s_dev_05_skip_defers_after_validity_and_before_the_gates() {
    differential(|| {
        let deferred: Vec<(&str, &str, Doc, Expect)> = vec![
            (
                "update_skip",
                "0.0.0",
                Doc::new("1", "1.0.0").handling("forced", "skip"),
                Expect::Success(None),
            ),
            (
                "download_skip",
                "0.0.0",
                Doc::new("1", "1.0.0").handling("skip", "forced"),
                Expect::Success(None),
            ),
            (
                "both_skip",
                "0.0.0",
                Doc::new("1", "1.0.0").handling("skip", "skip"),
                Expect::Success(None),
            ),
            (
                "skip_and_chip_mismatch",
                "0.0.0",
                Doc::new("1", "1.0.0")
                    .handling("forced", "skip")
                    .metadata(&[("chip", OTHER_CHIP)]),
                TMM,
            ),
            (
                "skip_and_older_version",
                "2.0.0",
                Doc::new("1", "1.0.0").handling("forced", "skip"),
                Expect::Refuse(1007, "downgrade"),
            ),
        ];
        for (name, running, doc, expect) in &deferred {
            let mut srv = Server::new();
            srv.offer(doc);
            let mut d = Device::new(running);
            d.run_for(&mut srv, secs(100));
            check(name, || expect_deferred(&srv));
            let flipped = doc.clone().handling("forced", "forced");
            srv.set_doc(&flipped);
            d.run_for(&mut srv, secs(200));
            check(name, || verdict(&srv, *expect));
        }

        let invalid: Vec<(&str, Doc)> = vec![
            (
                "invalid_no_chip_and_skip",
                Doc::new("1", "1.0.0")
                    .handling("forced", "skip")
                    .metadata(&[]),
            ),
            (
                "invalid_encrypted_and_skip",
                Doc::new("1", "1.0.0")
                    .handling("forced", "skip")
                    .encrypted(),
            ),
            (
                "invalid_8193_bytes_and_skip",
                Doc::new("1", "1.0.0")
                    .handling("forced", "skip")
                    .padded_to(8193),
            ),
        ];
        for (name, doc) in &invalid {
            let (srv, _) = run_case("0.0.0", doc, same);
            check(name, || verdict(&srv, INV));
        }
    });
}

fn position(srv: &Server, kind: Kind, nth: usize) -> Option<usize> {
    srv.requests
        .iter()
        .enumerate()
        .filter(|(_, r)| r.kind == kind)
        .nth(nth)
        .map(|(i, _)| i)
}

/// S-DEV-06: `configData` goes once per boot after the first 200 poll, to the URL built from the base and
/// whatever the link says, with the fixed keys; a refused PUT is not retried in the same boot.
#[test]
fn s_dev_06_config_data_once_per_boot_from_the_base() {
    differential(|| {
        let mut srv = Server::new();
        srv.config_link = ConfigLink::Never;
        srv.queue(Kind::E1, &[status(503, "")]);
        let mut d = Device::new("1.2.3");
        d.run_for(&mut srv, secs(20 * 60));
        check("first_poll_503_no_link", || {
            assert!(srv.count(Kind::E1) >= 2, "polls {}", srv.count(Kind::E1));
            assert_eq!(srv.count(Kind::E6), 1);
            assert!(
                position(&srv, Kind::E6, 0) > position(&srv, Kind::E1, 1),
                "the PUT follows the first 200 poll"
            );
            config_check(srv.of(Kind::E6)[0], CHIP, BOARD, Some("1.2.3"));
        });

        for (name, href) in [
            (
                "foreign_origin_link",
                format!("{FOREIGN}/controller/v1/{DEVICE_ID}/configData"),
            ),
            (
                "same_origin_other_path_link",
                format!("{}/trap/configData", root()),
            ),
        ] {
            let mut srv = Server::new();
            srv.links_override = Some(json!({"configData": {"href": href}}));
            let mut d = Device::new("1.2.3");
            d.run_for(&mut srv, secs(90));
            check(name, || {
                assert_eq!(srv.count(Kind::E6), 1);
                config_check(srv.of(Kind::E6)[0], CHIP, BOARD, Some("1.2.3"));
                assert!(
                    srv.requests
                        .iter()
                        .all(|r| !r.url.starts_with(FOREIGN) && !r.url.contains("/trap/")),
                    "the link was followed"
                );
                assert_eq!(srv.of(Kind::E6)[0].url, d.endpoint.config_data_url());
            });
        }

        let mut srv = Server::new();
        srv.on(Kind::E6, status(400, ""));
        let mut d = Device::new("1.2.3");
        d.run_for(&mut srv, secs(90));
        check("answered_400", || {
            assert_eq!(srv.count(Kind::E6), 1, "no retry in the same boot");
            assert!(
                srv.count(Kind::E1) >= 3,
                "polling continues: {}",
                srv.count(Kind::E1)
            );
        });

        let (mut srv, mut d) = run_case("0.0.0", &Doc::new("1", "1.0.0"), same);
        d.reboot(&mut srv);
        d.run_for(&mut srv, secs(60));
        check("install_then_restart", || {
            expect_success(&srv, None);
            for p in srv.of(Kind::E6) {
                config_check(p, CHIP, BOARD, None);
            }
            assert_eq!(config_versions(&srv), ["0.0.0", "1.0.0", "1.0.0"]);
        });
    });
}

/// S-DEV-07: the build chip decides; a different `chip` is 1007 `target_mismatch` before the version gate,
/// and the marker does not lift it.
#[test]
fn s_dev_07_chip_gate_refuses_before_the_version_gate() {
    differential(|| {
        let c3 = |running: &str| Device::new(running);
        let c6 = |running: &str| Device::for_chip(running, OTHER_CHIP, ImageChip::Esp32C6);

        let (srv, _) = run_case(
            "0.0.0",
            &Doc::new("1", "1.0.0").metadata(&[("chip", OTHER_CHIP)]),
            same,
        );
        check("build_c3_document_c6", || verdict(&srv, TMM));

        let mut srv = Server::new();
        srv.offer(&Doc::new("1", "1.0.0"));
        let mut d = c6("0.0.0");
        d.run_for(&mut srv, secs(200));
        check("build_c6_document_c3", || verdict(&srv, TMM));

        let (srv, _) = run_case(
            "2.0.0",
            &Doc::new("1", "1.0.0")
                .metadata(&[("chip", OTHER_CHIP)])
                .marker(),
            same,
        );
        check("marker_does_not_override", || verdict(&srv, TMM));

        let (srv, _) = run_case(
            "1.0.0",
            &Doc::new("1", "1.0.0").metadata(&[("chip", OTHER_CHIP)]),
            same,
        );
        check("chip_gate_before_version_gate", || verdict(&srv, TMM));

        let mut srv = Server::new();
        srv.offer(&Doc::new("1", "1.0.0"));
        let mut d = c3("0.0.0");
        d.run_for(&mut srv, secs(200));
        check("build_c3_document_c3", || {
            verdict(&srv, Expect::Success(None))
        });

        let mut srv = Server::new();
        srv.offer(&Doc::for_chip("1", "1.0.0", ImageChip::Esp32C6, OTHER_CHIP));
        let mut d = c6("0.0.0");
        d.run_for(&mut srv, secs(200));
        check("build_c6_document_c6", || {
            verdict(&srv, Expect::Success(None))
        });
    });
}

/// One S-DEV-08 case: name, running version, offered version, marker present, floor, expectation.
type VersionCase = (
    &'static str,
    &'static str,
    &'static str,
    bool,
    Option<&'static str>,
    Expect,
);

/// S-DEV-08: numeric version order, the `downgrade=allowed` marker and the device floor.
#[test]
fn s_dev_08_version_gate_order_marker_and_floor() {
    differential(|| {
        let cases: Vec<VersionCase> = vec![
            (
                "older_offer",
                "2.0.0",
                "1.0.0",
                false,
                None,
                Expect::Refuse(1007, "downgrade"),
            ),
            ("same_version", "1.0.0", "1.0.0", false, None, UTD),
            (
                "minor_10_is_newer_than_9",
                "1.9.0",
                "1.10.0",
                false,
                None,
                Expect::Success(None),
            ),
            (
                "minor_9_is_older_than_10",
                "1.10.0",
                "1.9.0",
                false,
                None,
                Expect::Refuse(1007, "downgrade"),
            ),
            (
                "marker_older_offer",
                "2.0.0",
                "1.0.0",
                true,
                None,
                Expect::Success(Some("downgrade")),
            ),
            (
                "marker_same_version",
                "1.0.0",
                "1.0.0",
                true,
                None,
                Expect::Success(Some("downgrade")),
            ),
            (
                "marker_newer_offer_does_not_decide",
                "1.0.0",
                "2.0.0",
                true,
                None,
                Expect::Success(None),
            ),
            (
                "below_floor",
                "1.0.0",
                "1.4.0",
                false,
                Some("1.5.0"),
                Expect::Refuse(1007, "below_floor"),
            ),
            (
                "below_floor_with_marker",
                "1.0.0",
                "1.4.0",
                true,
                Some("1.5.0"),
                Expect::Refuse(1007, "below_floor"),
            ),
            (
                "below_floor_over_downgrade",
                "2.0.0",
                "1.4.0",
                false,
                Some("1.5.0"),
                Expect::Refuse(1007, "below_floor"),
            ),
            (
                "marker_above_floor_installs",
                "2.0.0",
                "1.6.0",
                true,
                Some("1.5.0"),
                Expect::Success(Some("downgrade")),
            ),
            (
                "equal_to_floor_installs",
                "1.0.0",
                "1.5.0",
                false,
                Some("1.5.0"),
                Expect::Success(None),
            ),
        ];
        for (name, running, offered, marker, floor, expect) in cases {
            let mut doc = Doc::new("1", offered);
            if marker {
                doc = doc.marker();
            }
            let (srv, _) = run_case(running, &doc, |d| match floor {
                Some(f) => d.floor(f),
                None => d,
            });
            check(name, || verdict(&srv, expect));
        }
    });
}

/// S-DEV-09: a rollback is reported 1004 and remembered; the remembered version is refused
/// `previously_rolled_back` over `downgrade` and across restarts and a re-flash, installed with the marker
/// (and still remembered), cleared by a healthy install of another version, and `below_floor` wins.
#[test]
fn s_dev_09_rollback_record_across_restarts() {
    differential(|| {
        fn rolled_back_life() -> (Server, Device) {
            let mut srv = Server::new();
            srv.offer(&Doc::new("1", "1.1.0"));
            let mut d = Device::new("1.0.0").unhealthy("1.1.0");
            d.run_for(&mut srv, secs(400));
            expect_rolled_back(&srv);
            (srv, d)
        }

        let (mut srv, mut d) = rolled_back_life();
        d.unhealthy.clear();
        srv.offer(&Doc::new("2", "1.1.0"));
        srv.offer(&Doc::new("3", "1.1.0").marker());
        srv.offer(&Doc::new("4", "1.1.0"));
        d.run_for(&mut srv, secs(600));
        check("a", || {
            assert_eq!(
            outcome(&srv),
            "1004 rolled_back; 1007 previously_rolled_back; success previously_rolled_back; 1007 previously_rolled_back"
        );
            assert_eq!(srv.count(Kind::E8), 2);
            assert_eq!(feedback_problems(&srv), Vec::<String>::new());
        });

        let (mut srv, mut d) = rolled_back_life();
        d.unhealthy.clear();
        srv.offer(&Doc::new("2", "1.2.0"));
        srv.offer(&Doc::new("3", "1.1.0"));
        d.run_for(&mut srv, secs(600));
        check("b", || {
            assert_eq!(outcome(&srv), "1004 rolled_back; success; 1007 downgrade");
            assert_eq!(srv.count(Kind::E8), 2);
            assert_eq!(feedback_problems(&srv), Vec::<String>::new());
        });

        let (mut srv, mut d) = rolled_back_life();
        d.unhealthy.clear();
        d.reflash(&mut srv, "1.2.0");
        srv.offer(&Doc::new("2", "1.1.0"));
        srv.offer(&Doc::new("3", "1.1.0").marker());
        d.run_for(&mut srv, secs(600));
        check("c", || {
            assert_eq!(
                outcome(&srv),
                "1004 rolled_back; 1007 previously_rolled_back; success previously_rolled_back"
            );
            assert_eq!(srv.count(Kind::E8), 2);
            assert_eq!(feedback_problems(&srv), Vec::<String>::new());
        });

        let (mut srv, mut d) = rolled_back_life();
        d.floor = Some(v("1.2.0"));
        srv.offer(&Doc::new("2", "1.1.0"));
        d.run_for(&mut srv, secs(300));
        check("f", || {
            assert_eq!(outcome(&srv), "1004 rolled_back; 1007 below_floor");
            assert_eq!(srv.count(Kind::E8), 1);
            assert_eq!(feedback_problems(&srv), Vec::<String>::new());
        });
    });
}

/// S-DEV-10: unreadable records are reported 1005 `records_unreadable` without retrieving the deployment;
/// the report is persisted before it is sent. This client repairs the records at boot and reports once,
/// then handles the next offer normally (F5).
#[test]
fn s_dev_10_unreadable_records_report_1005_without_retrieval() {
    differential(|| {
        let mut srv = Server::new();
        srv.offer(&Doc::new("1", "1.0.0"));
        srv.offer(&Doc::new("2", "1.0.0"));
        let mut d = Device::new("1.0.0");
        d.kv.put_str("att_ver", "9.9.9")
            .put_str("att_slot", "bogus");
        d.run_for(&mut srv, secs(120));
        check("unreadable_records", || {
            let terminals = srv.terminals();
            assert_eq!(terminals.len(), 2, "{:?}", bodies(&terminals));
            assert_eq!(terminals[0].action.as_deref(), Some("1"));
            assert_eq!(terminals[0].code(), Some(1005));
            assert_eq!(terminals[0].details(), ["records_unreadable"]);
            assert!(
                position(&srv, Kind::E2, 0)
                    .is_none_or(|e2| e2 > position(&srv, Kind::E3, 0).unwrap()),
                "nothing retrieved before the 1005"
            );
            assert_eq!(srv.count(Kind::E8), 0);
            assert_eq!(terminals[1].action.as_deref(), Some("2"));
            assert_eq!(
                terminals[1].code(),
                Some(1007),
                "F5: the next offer is handled normally"
            );
            assert_eq!(e2_actions(&srv), ["2"]);
            assert!(!d.kv.has("att_slot"), "the corrupt attempt was repaired");
            assert_eq!(feedback_problems(&srv), Vec::<String>::new());
        });

        let (srv, _) = run_case("1.0.0", &Doc::new("1", "1.0.0"), same);
        check("control", || {
            verdict(&srv, UTD);
            assert_eq!(srv.count(Kind::E2), 1);
        });
    });
}

/// S-DEV-11: while a terminal report awaits delivery, no other deployment is retrieved; the report is
/// retried with the same status, and the next action follows once the report is answered.
#[test]
fn s_dev_11_not_yet_while_a_terminal_report_is_pending() {
    differential(|| {
        let mut srv = Server::new();
        srv.offer(&Doc::new("1", "1.0.0"));
        srv.offer(&Doc::new("2", "1.0.0"));
        srv.on(Kind::E3, status(503, ""));
        let mut d = Device::new("1.0.0");
        d.run_for(&mut srv, secs(100));
        assert!(
            srv.of(Kind::E3)
                .iter()
                .filter(|r| r.action.as_deref() == Some("1"))
                .count()
                >= 2
        );

        srv.close_offer("1");
        let deferred_before = d
            .poll_verdicts
            .iter()
            .filter(|p| matches!(p, PollVerdict::OfferDeferred))
            .count();
        d.run_for(&mut srv, secs(70));
        let mark = srv.mark();
        let not_yet = d
            .poll_verdicts
            .iter()
            .filter(|p| matches!(p, PollVerdict::OfferDeferred))
            .count()
            - deferred_before;
        assert!(not_yet >= 2, "not yet: {not_yet}");

        srv.clear_standing(Kind::E3);
        d.run_for(&mut srv, secs(100));

        let before: Vec<&Req> = srv.requests.iter().take(mark).collect();
        let e2_before: Vec<&str> = before
            .iter()
            .filter(|r| r.kind == Kind::E2)
            .filter_map(|r| r.action.as_deref())
            .collect();
        assert_eq!(e2_before, ["1"]);
        let e2_after: Vec<&str> = srv
            .since(mark)
            .iter()
            .filter(|r| r.kind == Kind::E2)
            .filter_map(|r| r.action.as_deref())
            .collect();
        assert_eq!(e2_after, ["2"]);
        let action1: Vec<&Req> = srv
            .terminals()
            .into_iter()
            .filter(|r| r.action.as_deref() == Some("1"))
            .collect();
        assert!(action1.len() >= 3, "{:?}", bodies(&action1));
        assert!(same_status(&action1), "{:?}", bodies(&action1));
        assert_eq!(
            (action1[0].code(), action1[0].detail0().as_str()),
            (Some(1007), "up_to_date")
        );
        let action2: Vec<&Req> = srv
            .terminals()
            .into_iter()
            .filter(|r| r.action.as_deref() == Some("2"))
            .collect();
        assert_eq!(action2.len(), 1, "{:?}", bodies(&action2));
        assert_eq!(
            (action2[0].code(), action2[0].detail0().as_str()),
            (Some(1007), "up_to_date")
        );
        assert_eq!(feedback_problems(&srv), Vec::<String>::new());
    });
}

/// S-DEV-12: a closed action id is never applied or reported again, in the same life and after a restart; a
/// sticky offer of it is retrieved again by this client and refused at application (F3).
#[test]
fn s_dev_12_loop_guard_never_reapplies_a_closed_action() {
    differential(|| {
        let mut srv = Server::new();
        // The marker admits the running version, so the sticky re-offer reaches `begin_ddi_attempt`, where the guard holds.
        srv.offer_sticky(&Doc::new("1", "1.0.0").marker());
        srv.offer(&Doc::new("2", "1.0.0"));
        let mut d = Device::new("0.0.0");
        d.run_for(&mut srv, secs(300));
        check("same_process_after_install", || {
            expect_success(&srv, None);
            assert!(
                srv.count(Kind::E2) >= 2,
                "F3: the sticky offer is retrieved again"
            );
            assert!(
                d.admissions.iter().any(|a| a == "closed 1"),
                "the closed action is refused at application: {:?}",
                d.admissions
            );
            assert_eq!(
                d.kv.u8_of("ddi_stage"),
                Some(3),
                "the action is closed in the records"
            );
        });
        srv.close_offer("1");
        d.run_for(&mut srv, secs(60));
        check("same_process_after_install_withdrawn", || {
            assert_eq!(outcome(&srv), "success; 1007 up_to_date");
            assert_eq!(e2_actions(&srv).iter().filter(|a| *a == "2").count(), 1);
        });

        let mut srv = Server::new();
        srv.offer_sticky(&Doc::new("1", "1.0.0"));
        let mut d = Device::new("1.0.0");
        d.run_for(&mut srv, secs(300));
        check("same_process_after_refusal", || {
            expect_refusal(&srv, 1007, "up_to_date", 1);
            assert!(srv.count(Kind::E2) >= 2, "F3");
        });

        let mut srv = Server::new();
        srv.offer_sticky(&Doc::new("1", "1.0.0"));
        let mut d = Device::new("0.0.0");
        d.run_for(&mut srv, secs(120));
        expect_success(&srv, None);
        d.reboot(&mut srv);
        d.run_for(&mut srv, secs(120));
        check("after_restart_after_install", || {
            expect_success(&srv, None);
            assert_eq!(srv.count(Kind::E8), 1);
        });

        let mut srv = Server::new();
        srv.offer_sticky(&Doc::new("1", "1.0.0"));
        let mut d = Device::new("1.0.0");
        d.run_for(&mut srv, secs(60));
        expect_refusal(&srv, 1007, "up_to_date", 1);
        d.reboot(&mut srv);
        d.run_for(&mut srv, secs(120));
        check("after_restart_after_refusal", || {
            expect_refusal(&srv, 1007, "up_to_date", 1)
        });
    });
}

/// S-DEV-13: a terminal report is retried, paced, with the same status, until the server answers 200, 404 or
/// 410; every other answer, a throttle with `Retry-After` and a lost connection keep it pending.
#[test]
fn s_dev_13_terminal_feedback_retried_until_200_404_410() {
    differential(|| {
        for (name, reply) in [
            ("200", status(200, "")),
            ("404", status(404, "")),
            ("410", status(410, "")),
        ] {
            let mut srv = Server::new();
            srv.offer(&Doc::new("1", "1.0.0"));
            srv.on(Kind::E3, reply);
            let mut d = Device::new("1.0.0");
            d.run_for(&mut srv, secs(600));
            check(name, || {
                expect_refusal(&srv, 1007, "up_to_date", 1);
                assert_eq!(srv.count(Kind::E2), 1);
            });
        }

        let keep: Vec<(&str, Reply)> = vec![
            ("400", status(400, "")),
            ("401", status(401, "")),
            ("413", status(413, "")),
            ("429_retry_after_2", status_retry_after(429, "2")),
            ("429_retry_after_120", status_retry_after(429, "120")),
            ("500", status(500, "")),
            ("503", status(503, "")),
            ("dropped", Reply::Unreachable),
        ];
        for (name, reply) in keep {
            let mut srv = Server::new();
            srv.offer(&Doc::new("1", "1.0.0"));
            srv.on(Kind::E3, reply);
            let mut d = Device::new("1.0.0");
            // Nudges every 10 s keep the polls closer than the sender's 30 s pacing, so only that pacing spaces the sends.
            for t in 1..30u64 {
                d.run_until(&mut srv, secs(t * 10));
                d.nudge();
            }
            d.run_until(&mut srv, secs(300));
            let attempts = srv.count(Kind::E3);
            srv.clear_standing(Kind::E3);
            d.run_for(&mut srv, secs(120));
            let answered_at = srv.count(Kind::E3);
            d.run_for(&mut srv, secs(600));
            check(name, || {
                assert!(attempts >= 3, "attempts before the 200: {attempts}");
                assert_eq!(answered_at, attempts + 1, "one send after the switch");
                assert_eq!(srv.count(Kind::E3), answered_at, "no send after the 200");
                expect_refusal(&srv, 1007, "up_to_date", answered_at);
                let sends = srv.of(Kind::E3);
                assert!(
                    gaps_at_least(&sends, secs(30)),
                    "pacing {:?}",
                    sends.iter().map(|r| r.t).collect::<Vec<_>>()
                );
                assert!(same_status(&sends));
                if name == "429_retry_after_120" {
                    assert!(
                        gaps_at_least(&sends, secs(120)),
                        "Retry-After honoured: {:?}",
                        sends.iter().map(|r| r.t).collect::<Vec<_>>()
                    );
                }
                assert_eq!(srv.count(Kind::E2), 1);
            });
        }
    });
}

/// S-DEV-14: the terminal report is persisted before its first send and survives a power cut before and after
/// that send; the next life sends it (after its first accepted poll, F4) and never re-evaluates the offer.
#[test]
fn s_dev_14_terminal_feedback_persisted_before_the_first_send() {
    differential(|| {
        let mut srv = Server::new();
        srv.offer(&Doc::new("1", "1.0.0"));
        let mut d = Device::new("1.0.0").crash_at(CrashPoint::BeforeVerdictSend);
        d.run_for(&mut srv, secs(200));
        check("cut_before_first_send", || {
            assert!(d.events.iter().any(|(_, e)| *e == Event::Crash));
            let first_send = position(&srv, Kind::E3, 0).expect("a send");
            let next_life_poll = position(&srv, Kind::E1, 1).expect("the next life's poll");
            assert!(
            first_send > next_life_poll,
            "no send before the cut; the stored report goes out after the next life's first accepted poll (F4)"
        );
            expect_refusal(&srv, 1007, "up_to_date", 1);
            assert_eq!(srv.count(Kind::E2), 1, "no re-evaluation after the restart");
            assert_eq!(d.kv.u8_of("ddi_stage"), Some(STAGE_CLOSED));
        });

        let mut srv = Server::new();
        srv.offer(&Doc::new("1", "1.0.0"));
        let mut d = Device::new("1.0.0").crash_at(CrashPoint::AfterVerdictSend);
        d.run_for(&mut srv, secs(200));
        check("cut_after_first_send", || {
            let sends = srv.of(Kind::E3);
            assert_eq!(sends.len(), 2, "{:?}", bodies(&sends));
            assert!(
                position(&srv, Kind::E3, 1) > position(&srv, Kind::E1, 1),
                "the resend follows the next life's first poll (F4)"
            );
            assert!(same_status(&sends));
            expect_refusal(&srv, 1007, "up_to_date", 2);
            assert_eq!(srv.count(Kind::E2), 1);
            assert_eq!(d.kv.u8_of("ddi_stage"), Some(STAGE_CLOSED));
        });
    });
}

/// S-DEV-15: a `cancelAction` next to a `deploymentBase` is answered on the cancel path only: the document is
/// never retrieved, the cancel is acknowledged when nothing runs and rejected after the activation, and a
/// refused answer is sent again.
#[test]
fn s_dev_15_cancel_path_only() {
    differential(|| {
        let both = json!({
            "cancelAction": {"href": format!("{}/cancelAction/1", root())},
            "deploymentBase": {"href": format!("{}/deploymentBase/1?c=1", root())}
        });

        let mut srv = Server::new();
        srv.links_override = Some(both.clone());
        srv.cancel("1").offer(&Doc::new("1", "1.0.0"));
        let mut d = Device::new("1.0.0");
        d.run_for(&mut srv, secs(29));
        check("acknowledged", || {
            expect_cancel(&srv, true, 1, 1);
        });

        let mut srv = Server::new();
        srv.links_override = Some(both.clone());
        srv.cancel("1").offer(&Doc::new("1", "1.0.0"));
        srv.queue(Kind::E5, &[status(503, "")]);
        let mut d = Device::new("1.0.0");
        d.run_for(&mut srv, secs(45));
        check("acknowledged_answer_503_once", || {
            expect_cancel(&srv, true, 2, 2);
            assert!(same_status(&srv.of(Kind::E5)));
        });

        let mut srv = Server::new();
        srv.offer(&Doc::new("1", "1.1.0"));
        let mut d = Device::new("1.0.0");
        d.run_for(&mut srv, Duration::from_millis(500));
        srv.links_override = Some(both);
        srv.cancel("1");
        d.run_for(&mut srv, secs(20));
        check("rejected", || {
            assert_eq!(srv.count(Kind::E4), 1);
            assert_eq!(
                e2_actions(&srv),
                ["1"],
                "the document was retrieved once, before the cancel"
            );
            let answers = srv.of(Kind::E5);
            assert_eq!(answers.len(), 1, "{:?}", bodies(&answers));
            assert_eq!(
                (
                    answers[0].execution().as_str(),
                    answers[0].finished().as_str()
                ),
                ("rejected", "none")
            );
            assert_eq!(d.cancel_results, [Some(CancelResult::Rejected)]);
        });
    });
}

// ── origin rows ──────────────────────────────────────────────────────────────────

fn https_root() -> String {
    format!("https://127.0.0.1:38443/DEFAULT/controller/v1/{DEVICE_ID}")
}

/// S-ORIG-01: a base URL the §2 grammar refuses is refused when the endpoint is built, before any request;
/// the controller-id grammar and the per-case names are the unit tests of `endpoint.rs`.
#[test]
fn s_orig_01_a_refused_base_url_never_connects() {
    differential(|| {
        let id = ControllerId::parse(DEVICE_ID).expect("controller id");
        let cases = [
            (
                "userinfo",
                "http://u@127.0.0.1:38080/DEFAULT",
                UrlRefusal::ForbiddenChar,
            ),
            (
                "percent_path",
                "http://127.0.0.1:38080/DEFAULT/%2e",
                UrlRefusal::ForbiddenChar,
            ),
            (
                "trailing_dot_host",
                "http://127.0.0.1.:38080/DEFAULT",
                UrlRefusal::Host,
            ),
            ("port_zero", "http://127.0.0.1:0/DEFAULT", UrlRefusal::Port),
            (
                "port_plus",
                "http://127.0.0.1:+38080/DEFAULT",
                UrlRefusal::Port,
            ),
            (
                "ftp_scheme",
                "ftp://127.0.0.1:38080/DEFAULT",
                UrlRefusal::Scheme,
            ),
        ];
        for (name, base, want) in cases {
            check(name, || match DdiEndpoint::new(base, &id) {
                Err(EndpointError::InvalidBaseUrl(case)) => assert_eq!(case, want),
                other => panic!("accepted or another error: {other:?}"),
            });
        }
        let control = DdiEndpoint::new(BASE, &id).expect("the plain base");
        assert_eq!(
            control.poll_url(),
            format!("http://127.0.0.1:38080/DEFAULT/controller/v1/{DEVICE_ID}")
        );
    });
}

/// S-ORIG-02: one foreign or refused link among `deploymentBase`, `cancelAction` and `configData` makes the
/// device act on none of that poll's links; `configData` still goes to the base; good links recover.
#[test]
fn s_orig_02_a_refused_poll_link_gates_the_whole_poll() {
    differential(|| {
        let good_deployment = format!("{}/deploymentBase/1?c=1", root());
        let cases: Vec<(&str, Value, UrlRefusal)> = vec![
            (
                "foreign_deployment",
                json!({"deploymentBase": {"href": format!("{FOREIGN}/controller/v1/{DEVICE_ID}/deploymentBase/1?c=1")}}),
                UrlRefusal::Origin,
            ),
            (
                "foreign_config_next_to_good_deployment",
                json!({"deploymentBase": {"href": good_deployment}, "configData": {"href": format!("{FOREIGN}/controller/v1/{DEVICE_ID}/configData")}}),
                UrlRefusal::Origin,
            ),
            (
                "userinfo_cancel_next_to_good_deployment",
                json!({"cancelAction": {"href": format!("http://u@127.0.0.1:38080/DEFAULT/controller/v1/{DEVICE_ID}/cancelAction/1")}, "deploymentBase": {"href": good_deployment}}),
                UrlRefusal::ForbiddenChar,
            ),
            (
                "percent_trap_deployment",
                json!({"deploymentBase": {"href": format!("{}/trap/%2e%2e/deploymentBase/1", root())}}),
                UrlRefusal::ForbiddenChar,
            ),
            (
                "localhost_trap_deployment",
                json!({"deploymentBase": {"href": format!("http://localhost:38080/DEFAULT/controller/v1/{DEVICE_ID}/deploymentBase/1?c=1")}}),
                UrlRefusal::Origin,
            ),
            (
                "https_trap_deployment_on_http_base",
                json!({"deploymentBase": {"href": format!("{}/deploymentBase/1?c=1", https_root().replace(":38443", ":38080"))}}),
                UrlRefusal::Origin,
            ),
        ];
        for (name, links, want) in cases {
            let mut srv = Server::new();
            srv.offer(&Doc::new("1", "1.1.0"));
            srv.links_override = Some(links);
            let mut d = Device::new("1.0.0");
            d.run_for(&mut srv, secs(100));
            check(name, || {
                assert!(
                    srv.requests
                        .iter()
                        .all(|r| matches!(r.kind, Kind::E1 | Kind::E6)),
                    "{:?}",
                    srv.requests.iter().map(|r| r.kind).collect::<Vec<_>>()
                );
                assert!(srv.requests.iter().all(|r| !r.url.starts_with(FOREIGN)
                    && !r.url.contains("/trap/")
                    && !r.url.contains("localhost")));
                let refusals: Vec<&PollVerdict> = d
                    .poll_verdicts
                    .iter()
                    .filter(|p| matches!(p, PollVerdict::LinksRefused(_)))
                    .collect();
                assert!(refusals.len() >= 2, "{:?}", d.poll_verdicts);
                assert_eq!(srv.count(Kind::E6), 1, "configData still goes to the base");
                assert_eq!(srv.of(Kind::E6)[0].url, d.endpoint.config_data_url());
                assert!(
                    refusals
                        .iter()
                        .all(|p| **p == PollVerdict::LinksRefused(want)),
                    "{refusals:?}"
                );
                assert!(srv.count(Kind::E1) >= 2);
            });
            srv.links_override = None;
            d.run_for(&mut srv, secs(200));
            check(name, || expect_success(&srv, None));
        }
    });
}

/// S-ORIG-03: a foreign or refused artifact link is refused 1006 `foreign_origin` before any download and
/// before the `skip` deferral, with one terminal and no `proceeding`; other links of the artifact are ignored.
#[test]
fn s_orig_03_artifact_link_off_origin_is_1006_foreign_origin() {
    differential(|| {
        let cases: Vec<(&str, Doc, &str)> = vec![
        ("foreign_listener", Doc::new("1", "1.1.0").href(&format!("{FOREIGN}/controller/v1/{DEVICE_ID}/softwaremodules/1/artifacts/image-1.bin")), "origin"),
        ("userinfo", Doc::new("1", "1.1.0").href(&format!("http://u@127.0.0.1:38080/DEFAULT/controller/v1/{DEVICE_ID}/softwaremodules/1/artifacts/image-1.bin")), "forbidden_char"),
        ("percent_trap", Doc::new("1", "1.1.0").href(&format!("{}/trap/%2e%2e/image.bin", root())), "forbidden_char"),
        ("https_same_host_port", Doc::new("1", "1.1.0").href(&format!("{}/softwaremodules/1/artifacts/image-1.bin", https_root().replace(":38443", ":38080"))), "origin"),
        ("localhost", Doc::new("1", "1.1.0").href(&format!("http://localhost:38080/DEFAULT/controller/v1/{DEVICE_ID}/softwaremodules/1/artifacts/image-1.bin")), "origin"),
        ("foreign_with_update_skip", Doc::new("1", "1.1.0").handling("forced", "skip").href(&format!("{FOREIGN}/controller/v1/{DEVICE_ID}/softwaremodules/1/artifacts/image-1.bin")), "origin"),
    ];
        for (name, doc, case) in cases {
            let (srv, d) = run_case("1.0.0", &doc, same);
            check(name, || {
                expect_refusal(&srv, 1006, "foreign_origin", 1);
                assert_eq!(srv.count(Kind::E3), 1, "no progress before the refusal");
                assert_eq!(
                    d.admissions,
                    [format!("refused 1 1006 foreign_origin {case}")]
                );
            });
        }

        let control = Doc::new("1", "1.1.0").edit(|v| {
        let links = &mut v["deployment"]["chunks"][0]["artifacts"][0]["_links"];
        links["md5sum-http"] = json!({"href": format!("{FOREIGN}/controller/v1/{DEVICE_ID}/softwaremodules/1/artifacts/image-1.bin.MD5SUM")});
        links["download"] = json!({"href": format!("{FOREIGN}/controller/v1/{DEVICE_ID}/softwaremodules/1/artifacts/image-1.bin")});
    });
        let (srv, _) = run_case("1.0.0", &control, same);
        check("good_link_foreign_others", || {
            expect_success(&srv, None);
            assert!(srv.requests.iter().all(|r| !r.url.starts_with(FOREIGN)));
        });
    });
}

/// S-ORIG-04: a 3xx on the document or the artifact is never followed and refused 1006 `redirect`.
#[test]
fn s_orig_04_redirects_on_document_and_artifact_are_1006_redirect() {
    differential(|| {
        let cases = [
            ("E2_302", Kind::E2, 302, 0),
            ("E2_308", Kind::E2, 308, 0),
            ("E8_307", Kind::E8, 307, 1),
            ("E8_301", Kind::E8, 301, 1),
        ];
        for (name, kind, status, artifact_requests) in cases {
            let mut srv = Server::new();
            srv.offer(&Doc::new("1", "1.1.0"));
            srv.queue(kind, &[Reply::Redirect(status)]);
            let mut d = Device::new("1.0.0");
            d.run_for(&mut srv, secs(120));
            check(name, || {
                let terminals = srv.terminals();
                assert_eq!(terminals.len(), 1, "{:?}", bodies(&terminals));
                assert_eq!(
                    (terminals[0].finished().as_str(), terminals[0].code()),
                    ("failure", Some(1006))
                );
                assert_eq!(terminals[0].detail0(), "redirect");
                assert_eq!(srv.count(Kind::E8), artifact_requests);
                assert!(srv
                    .requests
                    .iter()
                    .all(|r| !r.url.starts_with(FOREIGN) && !r.url.contains("/trap/")));
                assert_eq!(feedback_problems(&srv), Vec::<String>::new());
                assert_eq!(d.running(), v("1.0.0"));
            });
        }
    });
}

/// S-ORIG-05: a 3xx anywhere else is never followed and treated as a transient failure: the poll backs off,
/// progress is best effort, `configData` is not retried in the boot, the terminal and the cancel answer are
/// retried, and the cancel document is retrieved again at the next poll.
#[test]
fn s_orig_05_redirects_elsewhere_are_transient() {
    differential(|| {
        let mut srv = Server::new();
        srv.offer(&Doc::new("1", "1.1.0"));
        srv.queue(Kind::E1, &[Reply::Redirect(302), Reply::Redirect(302)]);
        let mut d = Device::new("1.0.0");
        d.run_for(&mut srv, secs(40 * 60));
        check("poll_302_twice", || {
            expect_success(&srv, None);
            let polls: Vec<Duration> = srv.of(Kind::E1).iter().map(|p| p.t).take(3).collect();
            assert_eq!(
                polls,
                [secs(0), secs(300), secs(900)],
                "two back-offs from the default interval"
            );
        });

        let mut srv = Server::new();
        srv.offer(&Doc::new("1", "1.1.0"));
        srv.queue(Kind::E3, &[Reply::Redirect(302)]);
        let mut d = Device::new("1.0.0");
        d.run_for(&mut srv, secs(200));
        check("progress_302_once", || expect_success(&srv, None));

        let mut srv = Server::new();
        srv.offer(&Doc::new("1", "1.1.0"));
        srv.queue(Kind::E6, &[Reply::Redirect(302)]);
        let mut d = Device::new("1.0.0");
        d.run_for(&mut srv, secs(200));
        check("config_302_once", || {
            expect_success(&srv, None);
            let first_put = position(&srv, Kind::E6, 0).unwrap();
            let puts_before = srv
                .requests
                .iter()
                .take(position(&srv, Kind::E8, 0).unwrap())
                .filter(|r| r.kind == Kind::E6)
                .count();
            assert_eq!(
                puts_before, 1,
                "no retry in the same boot (first PUT at {first_put})"
            );
        });

        let mut srv = Server::new();
        srv.offer(&Doc::new("1", "1.0.0"));
        srv.queue(Kind::E3, &[Reply::Redirect(302), Reply::Redirect(302)]);
        let mut d = Device::new("1.0.0");
        d.run_for(&mut srv, secs(200));
        check("terminal_302_twice", || {
            expect_refusal(&srv, 1007, "up_to_date", 3);
            assert!(same_status(&srv.of(Kind::E3)));
        });

        let mut srv = Server::new();
        srv.cancel("1");
        srv.queue(Kind::E4, &[Reply::Redirect(303)]);
        let mut d = Device::new("1.0.0");
        d.run_for(&mut srv, secs(200));
        check("cancel_303_once", || {
            expect_cancel(&srv, true, 2, 1);
        });

        let mut srv = Server::new();
        srv.cancel("1");
        srv.queue(Kind::E5, &[Reply::Redirect(307)]);
        let mut d = Device::new("1.0.0");
        d.run_for(&mut srv, secs(200));
        check("ack_307_once", || {
            expect_cancel(&srv, true, 2, 2);
            assert!(same_status(&srv.of(Kind::E5)));
        });
    });
}

// ── hygiene rows: the parts a sans-IO client can show ────────────────────────────

/// S-HYG-02 (the link part): on an `https://` base an `http://` poll link, an `http://` download link and a
/// redirect to `http://` are refused; nothing is requested over http. The TLS failures are the TLS round.
#[test]
fn s_hyg_02_http_links_on_an_https_base_are_refused() {
    differential(|| {
        let https_base = "https://127.0.0.1:38443/DEFAULT";
        let http_trap = format!("http://127.0.0.1:38443/DEFAULT/controller/v1/{DEVICE_ID}");

        let mut srv = Server::new();
        srv.offer(&Doc::new("1", "1.1.0"));
        srv.links_override =
            Some(json!({"deploymentBase": {"href": format!("{http_trap}/trap/deploymentBase/1")}}));
        let mut d = Device::with_base(https_base, "1.0.0", CHIP, ImageChip::Esp32C3);
        d.run_for(&mut srv, secs(100));
        check("http_poll_link", || {
            expect_silent(&srv);
            assert!(
                d.poll_verdicts
                    .iter()
                    .all(|p| *p == PollVerdict::LinksRefused(UrlRefusal::Origin)),
                "{:?}",
                d.poll_verdicts
            );
        });

        let mut srv = Server::new();
        srv.offer(&Doc::new("1", "1.1.0").href(&format!("{http_trap}/trap/image.bin")));
        srv.links_override = Some(
            json!({"deploymentBase": {"href": format!("{}/deploymentBase/1?c=1", https_root())}}),
        );
        let mut d = Device::with_base(https_base, "1.0.0", CHIP, ImageChip::Esp32C3);
        d.run_for(&mut srv, secs(100));
        check("http_download_link", || {
            expect_refusal(&srv, 1006, "foreign_origin", 1)
        });

        let mut srv = Server::new();
        srv.offer(&Doc::new("1", "1.1.0"));
        srv.links_override = Some(
            json!({"deploymentBase": {"href": format!("{}/deploymentBase/1?c=1", https_root())}}),
        );
        srv.queue(Kind::E2, &[Reply::Redirect(302)]);
        let mut d = Device::with_base(https_base, "1.0.0", CHIP, ImageChip::Esp32C3);
        d.run_for(&mut srv, secs(100));
        check("redirect_on_the_document", || {
            expect_refusal(&srv, 1006, "redirect", 1);
            assert_eq!(srv.count(Kind::E8), 0);
        });
        assert!(
            srv.requests.iter().all(|r| r.url.starts_with("https://")),
            "a request over http"
        );
    });
}

/// S-HYG-04 (the request part): answers that echo the credential, a 401, a 500, a redirect (never followed) and a
/// dropped connection neither stop the device nor change what it sends; the redirect is never followed.
#[test]
fn s_hyg_04_answers_that_echo_the_credential_do_not_stop_the_device() {
    differential(|| {
        let token = "0123456789abcdef0123456789abcdef";
        let echo =
            |status_code: u16| status(status_code, &format!("Authorization: TargetToken {token}"));
        let bad_polls = [
            echo(401),
            status(500, token),
            Reply::Redirect(302),
            Reply::Unreachable,
        ];

        let mut srv = Server::new();
        srv.offer(&Doc::new("1", "1.1.0"));
        srv.queue(Kind::E1, &bad_polls);
        srv.queue(Kind::E3, &[echo(400)]);
        let mut d = Device::new("1.0.0");
        d.run_for(&mut srv, secs(90 * 60));
        check("deployment_paths", || {
            expect_success(&srv, None);
            assert!(srv.count(Kind::E1) >= 5);
        });

        let mut srv = Server::new();
        srv.cancel("1");
        srv.queue(Kind::E1, &bad_polls);
        srv.queue(Kind::E5, &[status(503, token)]);
        let mut d = Device::new("1.0.0");
        d.run_for(&mut srv, secs(90 * 60));
        check("cancel_paths", || expect_cancel(&srv, true, 2, 2));
    });
}

/// Every body the device sent is free of the server's marker, in either case.
fn no_echo(srv: &Server) {
    let upper = MARKER.to_uppercase();
    let sent: Vec<&Req> = srv
        .requests
        .iter()
        .filter(|r| matches!(r.kind, Kind::E3 | Kind::E5 | Kind::E6))
        .collect();
    assert!(!sent.is_empty(), "nothing was sent");
    for r in sent {
        assert!(
            !r.body.contains(MARKER) && !r.body.contains(&upper),
            "echoed: {}",
            r.body
        );
    }
}

/// S-HYG-05: server values never come back in feedback: unknown fields, links, names and hashes are ignored
/// and the install succeeds; a marker in a checked field is 1006; a short artifact body ends 1001 (as
/// `size_mismatch`, F6); a `stopId` that is no action id is an invalid cancel document (F6).
#[test]
fn s_hyg_05_no_echo_of_server_values() {
    differential(|| {
        let mut srv = Server::new();
        let doc = Doc::new("1", "1.1.0").edit(|v| {
            v[MARKER] = json!(MARKER);
            let chunk = &mut v["deployment"]["chunks"][0];
            chunk["name"] = json!(MARKER);
            chunk["part"] = json!(MARKER);
            chunk["metadata"]
                .as_array_mut()
                .unwrap()
                .push(json!({"key": MARKER, "value": MARKER}));
            let artifact = &mut chunk["artifacts"][0];
            artifact["filename"] = json!(MARKER);
            artifact["hashes"]["md5"] = json!(MARKER);
            artifact["_links"]["md5sum-http"] =
                json!({"href": format!("{}/trap/{MARKER}", root())});
        });
        srv.offer(&doc);
        srv.queue(
            Kind::E1,
            &[
                Reply::Unreachable,
                ok(r#"{"config":{"polling":{"sleep":"00:00:30"}},"_links":{}}"#),
            ],
        );
        srv.poll_extra = json!({MARKER: MARKER});
        srv.links_extra = json!({
            MARKER: {"href": format!("{BASE}/{MARKER}")},
            "installedBase": {"href": format!("{}/installedBase/{MARKER}", root())}
        });
        srv.queue(Kind::E3, &[status(400, MARKER)]);
        let mut d = Device::new("1.0.0");
        d.run_for(&mut srv, secs(40 * 60));
        check("marker_everywhere", || {
            expect_success(&srv, None);
            no_echo(&srv);
            assert!(srv
                .requests
                .iter()
                .all(|r| !r.url.contains("/trap/") && !r.url.contains(MARKER)));
        });

        let (srv, _) = run_case("1.0.0", &Doc::new("1", "1.1.0").version(MARKER), same);
        check("marker_version", || {
            verdict(&srv, INV);
            no_echo(&srv);
        });
        let (srv, _) = run_case(
            "1.0.0",
            &Doc::new("1", "1.1.0").handling(MARKER, "forced"),
            same,
        );
        check("marker_download_value", || {
            verdict(&srv, INV);
            no_echo(&srv);
        });

        let mut srv = Server::new();
        srv.offer(&Doc::new("1", "1.0.0"));
        srv.queue(Kind::E3, &[status(400, MARKER)]);
        let mut d = Device::new("1.0.0");
        d.run_for(&mut srv, secs(100));
        check("marker_in_terminal_answer", || {
            expect_refusal(&srv, 1007, "up_to_date", 2);
            assert!(
                same_status(&srv.of(Kind::E3)),
                "the resend repeats the report"
            );
            no_echo(&srv);
        });

        let mut srv = Server::new();
        let doc = Doc::new("1", "1.1.0");
        let short: Vec<u8> = doc.artifact[..100].to_vec();
        srv.offer(&doc);
        srv.on(
            Kind::E8,
            Reply::Status {
                status: 200,
                body: short,
                retry_after: None,
                declared_len: Some(164),
            },
        );
        let mut d = Device::new("1.0.0");
        d.run_for(&mut srv, secs(400));
        check("short_artifact_body_fixture", || {
            let terminals = srv.terminals();
            assert_eq!(terminals.len(), 1, "{:?}", bodies(&terminals));
            assert_eq!(terminals[0].code(), Some(1001));
            assert_eq!(
            terminals[0].details(),
            ["size_mismatch"],
            "F6: the Content-Length differs from the declared size, refused before any byte; the harness expects download_failed"
        );
            assert_eq!(
                srv.count(Kind::E8),
                8,
                "one attempt at 0, 5, 15, 35, 75, 135, 195 and 255 s, none at the 300 s deadline"
            );
            assert_eq!(d.running(), v("1.0.0"));
            no_echo(&srv);
        });

        // The same body under the declared size: the stream ends early, which is `download_failed`.
        let mut srv = Server::new();
        let doc = Doc::new("1", "1.1.0");
        let short: Vec<u8> = doc.artifact[..100].to_vec();
        srv.offer(&doc);
        srv.on(
            Kind::E8,
            Reply::Status {
                status: 200,
                body: short,
                retry_after: None,
                declared_len: Some(ARTIFACT_LEN as u64),
            },
        );
        let mut d = Device::new("1.0.0");
        d.run_for(&mut srv, secs(400));
        check("short_artifact_body", || {
            let terminals = srv.terminals();
            assert_eq!(terminals.len(), 1, "{:?}", bodies(&terminals));
            assert_eq!(terminals[0].code(), Some(1001));
            assert_eq!(terminals[0].details(), ["download_failed"]);
            assert_eq!(d.download_errors, [OtaError::DownloadFailed { status: 0 }]);
            assert_eq!(d.running(), v("1.0.0"));
            no_echo(&srv);
        });

        let mut srv = Server::new();
        srv.cancel_with_body(
            "1",
            format!(r#"{{"id":"1","cancelAction":{{"stopId":"{MARKER}"}}}}"#).as_bytes(),
        );
        let mut d = Device::new("1.0.0");
        d.run_for(&mut srv, secs(100));
        check("marker_stop_id", || {
            assert!(srv.count(Kind::E4) >= 1);
            assert_eq!(
                srv.count(Kind::E5),
                0,
                "F6: the harness expects an acknowledgement"
            );
            assert!(
                matches!(
                    d.cancel_results.first(),
                    Some(Some(CancelResult::Invalid(_)))
                ),
                "{:?}",
                d.cancel_results
            );
            assert!(srv.requests.iter().all(|r| !r.body.contains(MARKER)));
        });
    });
}
