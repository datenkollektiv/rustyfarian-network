//! Deployment admission (ADR 023 rung 3): what the device does with the `deploymentBase` it retrieved for a [`PollAction::RetrieveDeployment`](super::PollAction::RetrieveDeployment).
//!
//! The tier performs the GET, hands its [`RequestOutcome`] to [`admit_retrieved`], acts on the [`DeploymentVerdict`], and then answers the poll machine with [`PollInput::Performed`](super::PollInput::Performed): with the GET's outcome when the verdict is [`NotRetrieved`](DeploymentVerdict::NotRetrieved), so a throttling, failing, 401 or 410 answer times the next poll as a poll answer would, and with `None` otherwise.
//! Admission is pure: the running version, the remembered rolled-back version, the minimum-version floor, the update-partition capacity and the compiled-in chip arrive as [`AdmissionFacts`].
//! [`admit_retrieved`] documents the order of the checks.
//!
//! # Seams
//!
//! - Sending the refusal as `closed` + `failure` with `details[0]` the reason, persisting it, and the cancel belong to the feedback work.
//! - [`AdmittedOffer`] carries what the download and the loop guard need: the action id, the version, the checked artifact link, its digest and size, and the `details[0]` of the later `closed` + `success`.
//! - The image-header chip check runs inside the download, after admission.

use alloc::vec::Vec;

use super::endpoint::{DdiEndpoint, UrlRefusal};
use super::poller::Transient;
use super::response::{BodyFailure, RequestOutcome};
use super::wire::{ActionId, Chip, Chunk, DeploymentBase, Href, Metadata, ProfileCode, UpdateMode};
use crate::ota::wire::FailReason;
use crate::ota::{decide_version_gate, OtaError, Version, VersionFacts, VersionGate};

/// Experimental: API may change before 1.0.
///
/// What the device knows about itself when it judges an offer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmissionFacts {
    /// The running firmware version.
    pub running: Version,
    /// The version the device last rolled back from, kept until a different version passes its health check.
    pub rolled_back: Option<Version>,
    /// The device's minimum version; an offer below it is refused `below_floor`, marker or not.
    pub floor: Option<Version>,
    /// The size of the update partition in bytes; a larger artifact is refused before any download.
    pub partition_capacity: u64,
    /// The chip the image was built for, compared with the deployment's `chip` metadata.
    pub chip: Chip,
}

/// Experimental: API may change before 1.0.
///
/// A deployment the device refuses: it is reported `closed` + `failure` with `code` and `details[0]` the [`FailReason`] string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    /// The action the feedback goes to.
    pub action: ActionId,
    /// 1006 ([`ProfileCode::Deployment`]) or 1007 ([`ProfileCode::Policy`]).
    pub code: ProfileCode,
    /// The reason, `details[0]` of the feedback.
    pub reason: FailReason,
    /// For `Ota(ForeignOrigin)`, the case the artifact link's origin check named, so the tier logs it (never the link); `None` for every other reason.
    pub url_case: Option<UrlRefusal>,
}

impl Refusal {
    fn new(action: ActionId, reason: FailReason) -> Self {
        let code = ProfileCode::for_reason(reason);
        debug_assert!(
            matches!(code, Some(ProfileCode::Deployment | ProfileCode::Policy)),
            "admission refuses only with 1006 or 1007 reasons, not {reason:?}"
        );
        let code = code.unwrap_or(ProfileCode::Deployment);
        Refusal {
            action,
            code,
            reason,
            url_case: None,
        }
    }
}

/// Experimental: API may change before 1.0.
///
/// A deployment the device takes: download `artifact`, check `size` and `sha256`, and install it.
///
/// `Debug` redacts the link, as [`Href`] does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmittedOffer {
    /// The action id, for the loop guard and every feedback of the action.
    pub action: ActionId,
    /// The offered version.
    pub version: Version,
    /// The `download-http` link, already checked against the configured origin.
    pub artifact: Href,
    /// The expected SHA-256 digest of the image.
    pub sha256: [u8; 32],
    /// The expected image size in bytes, at most the update partition.
    pub size: u64,
    /// `details[0]` of the `closed` + `success` feedback: [`FailReason::PreviouslyRolledBack`] or [`FailReason::Downgrade`] when the offer passed only because of the downgrade marker, `None` for a plain upgrade.
    pub success_detail: Option<FailReason>,
}

/// Experimental: API may change before 1.0.
///
/// What admission decided about a retrieved deployment (see [`admit_retrieved`] for the order).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeploymentVerdict {
    /// The document did not arrive; nothing is reported, and a later poll offers the deployment again.
    ///
    /// The tier answers [`PollInput::Performed`](super::PollInput::Performed) with the GET's outcome, so the next poll honours it.
    NotRetrieved(Transient),
    /// The deployment is refused, but neither the body nor the link names an action id to report it to.
    Unreportable(FailReason),
    /// `skip`: not now; no download and no feedback, the action stays open.
    Deferred(ActionId),
    /// Report `closed` + `failure` and download nothing.
    Refused(Refusal),
    /// Download and install the artifact.
    Admitted(AdmittedOffer),
}

/// Experimental: API may change before 1.0.
///
/// Judges the outcome of the deployment GET for `link`, the href of [`PollAction::RetrieveDeployment`](super::PollAction::RetrieveDeployment); a 200 body goes through [`admit_deployment`].
///
/// # Order (fleet DDI profile §3a, §3b, §4)
///
/// The first step that applies decides; only an offer that passes every step is downloaded.
///
/// | Step | Rule | Verdict |
/// |:--|:--|:--|
/// | 1. The GET | a redirect (a 3xx other than 304, see [`is_redirect`](super::is_redirect)) | [`Refused`](DeploymentVerdict::Refused) 1006 `redirect` |
/// | | a 200 body over 8 KiB or not a deployment document (an unknown update mode included), or a document whose `id` differs from the action id of the link | [`Refused`](DeploymentVerdict::Refused) 1006 `manifest_invalid`, to the link's action |
/// | | a timeout, no connection, any other status (304 included), a body that was not read whole, a link the tier refused as foreign (no feedback, as for a refused poll link) | [`NotRetrieved`](DeploymentVerdict::NotRetrieved): no feedback, a later poll offers it again |
/// | 2. Document rules | not exactly one chunk with exactly one artifact; `encrypted: true`; no `sha256`; no `download-http` link; a chunk version that is not `MAJOR.MINOR.PATCH`; not exactly one `chip` entry of `[a-z0-9]{1,32}`; a second `downgrade` entry or a value other than `allowed`; a `size` of 0 or above the update partition | [`Refused`](DeploymentVerdict::Refused) 1006 `manifest_invalid` |
/// | 3. Artifact origin | the `download-http` link is off the configured origin | [`Refused`](DeploymentVerdict::Refused) 1006 `foreign_origin` |
/// | 4. Skip | `download` or `update` is `skip` | [`Deferred`](DeploymentVerdict::Deferred): no download, no feedback, the action stays open |
/// | 5. Chip gate | the `chip` entry differs from the compiled-in chip; the downgrade marker does not lift this | [`Refused`](DeploymentVerdict::Refused) 1007 `target_mismatch` |
/// | 6. Version gate | [`decide_version_gate`]: `below_floor`, then `previously_rolled_back`, then `downgrade` or `up_to_date` | [`Refused`](DeploymentVerdict::Refused) 1007, or [`Admitted`](DeploymentVerdict::Admitted) |
///
/// A refusal goes to the action id of the document, which must equal the id read from the link ([`DdiEndpoint::deployment_action`]) when the link names one; when the body never parsed, to the link's id, and when the link names none it is [`Unreportable`](DeploymentVerdict::Unreportable).
///
/// # Examples
///
/// ```
/// use juggler::ota::ddi::wire::{Chip, PollResponse};
/// use juggler::ota::ddi::{
///     admit_retrieved, AdmissionFacts, ControllerId, DdiEndpoint, DdiResponse, DeploymentVerdict,
///     RequestOutcome,
/// };
/// use juggler::ota::Version;
///
/// let endpoint = DdiEndpoint::new("http://ddi.lab/DEFAULT", &ControllerId::parse("dev-1").unwrap()).unwrap();
/// let poll = br#"{"_links":{"deploymentBase":{"href":"http://ddi.lab/DEFAULT/controller/v1/dev-1/deploymentBase/7"}}}"#;
/// let link = PollResponse::parse(poll).unwrap().deployment_base.unwrap();
/// let facts = AdmissionFacts {
///     running: Version::new(1, 0, 0),
///     rolled_back: None,
///     floor: None,
///     partition_capacity: 0x1E_0000,
///     chip: Chip::parse("esp32c3").unwrap(),
/// };
/// let body = br#"{"id":"7","deployment":{"download":"forced","update":"forced","chunks":[
///     {"version":"1.1.0","metadata":[{"key":"chip","value":"esp32c3"}],"artifacts":[
///         {"filename":"fw.bin","size":1234,
///          "hashes":{"sha256":"2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"},
///          "_links":{"download-http":{"href":"http://ddi.lab/DEFAULT/fw.bin"}}}]}]}}"#;
/// let outcome = RequestOutcome::Response(DdiResponse { status: 200, retry_after: None, body: body.to_vec() });
///
/// let DeploymentVerdict::Admitted(offer) = admit_retrieved(&link, &outcome, &facts, &endpoint) else {
///     panic!("not admitted");
/// };
/// assert_eq!((offer.action.as_str(), offer.size), ("7", 1234));
/// assert_eq!(offer.success_detail, None);
/// ```
pub fn admit_retrieved(
    link: &Href,
    outcome: &RequestOutcome,
    facts: &AdmissionFacts,
    endpoint: &DdiEndpoint,
) -> DeploymentVerdict {
    let link_action = endpoint.deployment_action(link);
    let refuse = |reason| match &link_action {
        Some(action) => refused(action.clone(), reason),
        None => DeploymentVerdict::Unreportable(reason),
    };
    match outcome {
        RequestOutcome::Response(response) if response.status == 200 => {
            match DeploymentBase::parse(&response.body) {
                Ok(deployment) if link_action.as_ref().is_some_and(|a| *a != deployment.id) => {
                    refuse(FailReason::ManifestInvalid)
                }
                Ok(deployment) => admit_deployment(deployment, facts, endpoint),
                Err(_) => refuse(FailReason::ManifestInvalid),
            }
        }
        RequestOutcome::Response(response) => {
            DeploymentVerdict::NotRetrieved(Transient::Status(response.status))
        }
        RequestOutcome::BodyFailed {
            status: 200,
            failure: BodyFailure::TooLarge,
            ..
        } => refuse(FailReason::ManifestInvalid),
        RequestOutcome::BodyFailed { status, .. } => {
            DeploymentVerdict::NotRetrieved(Transient::Body(*status))
        }
        RequestOutcome::Timeout => DeploymentVerdict::NotRetrieved(Transient::Timeout),
        RequestOutcome::Unreachable => DeploymentVerdict::NotRetrieved(Transient::Unreachable),
        RequestOutcome::Redirect { status } => {
            refuse(FailReason::Ota(OtaError::Redirect { status: *status }))
        }
        RequestOutcome::ForeignOrigin => DeploymentVerdict::NotRetrieved(Transient::ForeignOrigin),
    }
}

/// Experimental: API may change before 1.0.
///
/// Judges a parsed deployment, steps 2 to 6 of the order in [`admit_retrieved`].
///
/// Never [`NotRetrieved`](DeploymentVerdict::NotRetrieved) or [`Unreportable`](DeploymentVerdict::Unreportable): every refusal goes to the document's action id.
/// It does not compare that id with the link; [`admit_retrieved`] does.
pub fn admit_deployment(
    deployment: DeploymentBase,
    facts: &AdmissionFacts,
    endpoint: &DdiEndpoint,
) -> DeploymentVerdict {
    let DeploymentBase {
        id,
        download,
        update,
        chunks,
    } = deployment;
    let offer = match valid_offer(chunks, facts) {
        Ok(offer) => offer,
        Err(reason) => return refused(id, reason),
    };
    if let Some(case) = endpoint.refusal(offer.artifact.as_str()) {
        let mut refusal = Refusal::new(id, FailReason::Ota(OtaError::ForeignOrigin));
        refusal.url_case = Some(case);
        return DeploymentVerdict::Refused(refusal);
    }
    if download == UpdateMode::Skip || update == UpdateMode::Skip {
        return DeploymentVerdict::Deferred(id);
    }
    if !offer.chip_matches {
        return refused(id, FailReason::TargetMismatch);
    }
    let versions = VersionFacts {
        running: facts.running,
        rolled_back: facts.rolled_back,
        floor: facts.floor,
    };
    let success_detail =
        match decide_version_gate(offer.version, &versions, offer.downgrade_allowed) {
            VersionGate::Apply { marked } => marked.map(FailReason::from),
            VersionGate::Refuse(reason) => return refused(id, reason.into()),
        };
    DeploymentVerdict::Admitted(AdmittedOffer {
        action: id,
        version: offer.version,
        artifact: offer.artifact,
        sha256: offer.sha256,
        size: offer.size,
        success_detail,
    })
}

/// The one way admission builds a refusal.
fn refused(action: ActionId, reason: FailReason) -> DeploymentVerdict {
    DeploymentVerdict::Refused(Refusal::new(action, reason))
}

/// The one artifact of a document that passed the document rules.
struct ValidOffer {
    version: Version,
    chip_matches: bool,
    downgrade_allowed: bool,
    artifact: Href,
    sha256: [u8; 32],
    size: u64,
}

/// Step 2 of the module docs; every failure is `manifest_invalid`.
fn valid_offer(chunks: Vec<Chunk>, facts: &AdmissionFacts) -> Result<ValidOffer, FailReason> {
    let invalid = FailReason::ManifestInvalid;
    let [chunk] = <[Chunk; 1]>::try_from(chunks).map_err(|_| invalid)?;
    let Chunk {
        version,
        encrypted,
        metadata,
        artifacts,
    } = chunk;
    if encrypted {
        return Err(invalid);
    }
    let [artifact] = <[_; 1]>::try_from(artifacts).map_err(|_| invalid)?;
    let (Some(sha256), Some(href)) = (artifact.sha256, artifact.download_http) else {
        return Err(invalid);
    };
    let version = Version::parse(&version).map_err(|_| invalid)?;
    let chip = single_value(&metadata, "chip")?
        .filter(|chip| Chip::is_valid(chip))
        .ok_or(invalid)?;
    let downgrade_allowed = match single_value(&metadata, "downgrade")? {
        None => false,
        Some("allowed") => true,
        Some(_) => return Err(invalid),
    };
    if artifact.size == 0 || artifact.size > facts.partition_capacity {
        return Err(invalid);
    }
    Ok(ValidOffer {
        version,
        chip_matches: chip == facts.chip.as_str(),
        downgrade_allowed,
        artifact: href,
        sha256,
        size: artifact.size,
    })
}

/// The value of the only `key` entry, `None` when there is none, `manifest_invalid` when there are several.
fn single_value<'a>(metadata: &'a [Metadata], key: &str) -> Result<Option<&'a str>, FailReason> {
    let mut values = metadata
        .iter()
        .filter(|m| m.key == key)
        .map(|m| m.value.as_str());
    match (values.next(), values.next()) {
        (value, None) => Ok(value),
        _ => Err(FailReason::ManifestInvalid),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ota::ddi::wire::{PollResponse, MAX_RESPONSE_BYTES};
    use crate::ota::ddi::{ControllerId, DdiResponse};
    use crate::ota::hex_to_bytes;
    use alloc::format;
    use alloc::string::String;
    use alloc::vec;

    const BASE: &str = "http://ddi.lab:8080/DEFAULT";
    const LINK: &str = "http://ddi.lab:8080/DEFAULT/controller/v1/dev-1/deploymentBase/12";
    const ARTIFACT: &str =
        "http://ddi.lab:8080/DEFAULT/controller/v1/dev-1/softwaremodules/3/artifacts/fw.bin";
    const HASH: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
    const CAPACITY: u64 = 0x1E_0000;

    fn v(major: u16, minor: u16, patch: u16) -> Version {
        Version::new(major, minor, patch)
    }

    fn endpoint() -> DdiEndpoint {
        DdiEndpoint::new(BASE, &ControllerId::parse("dev-1").unwrap()).unwrap()
    }

    fn href(url: &str) -> Href {
        let body = format!(r#"{{"_links":{{"deploymentBase":{{"href":"{url}"}}}}}}"#);
        PollResponse::parse(body.as_bytes())
            .unwrap()
            .deployment_base
            .unwrap()
    }

    /// Running 2.0.0 on an esp32c3, nothing remembered, no floor.
    fn facts() -> AdmissionFacts {
        AdmissionFacts {
            running: v(2, 0, 0),
            rolled_back: None,
            floor: None,
            partition_capacity: CAPACITY,
            chip: Chip::parse("esp32c3").unwrap(),
        }
    }

    /// One deployment document; [`spec`] is a valid upgrade to 2.1.0 for an esp32c3.
    #[derive(Clone)]
    struct Spec {
        id: &'static str,
        download: &'static str,
        update: &'static str,
        chunks: usize,
        artifacts: usize,
        encrypted: bool,
        version: &'static str,
        metadata: Vec<(&'static str, &'static str)>,
        size: u64,
        sha256: bool,
        href: Option<&'static str>,
    }

    fn spec() -> Spec {
        Spec {
            id: "12",
            download: "forced",
            update: "forced",
            chunks: 1,
            artifacts: 1,
            encrypted: false,
            version: "2.1.0",
            metadata: vec![("chip", "esp32c3")],
            size: 1234,
            sha256: true,
            href: Some(ARTIFACT),
        }
    }

    impl Spec {
        fn version(self, version: &'static str) -> Self {
            Spec { version, ..self }
        }

        fn metadata(self, metadata: &[(&'static str, &'static str)]) -> Self {
            Spec {
                metadata: metadata.to_vec(),
                ..self
            }
        }

        /// The default chip entry plus the downgrade marker.
        fn marked(self) -> Self {
            self.metadata(&[("chip", "esp32c3"), ("downgrade", "allowed")])
        }

        fn skip(self) -> Self {
            Spec {
                update: "skip",
                ..self
            }
        }

        fn body(&self) -> String {
            let hashes = if self.sha256 {
                format!(r#","hashes":{{"sha256":"{HASH}"}}"#)
            } else {
                String::new()
            };
            let links = match self.href {
                Some(h) => format!(r#","_links":{{"download-http":{{"href":"{h}"}}}}"#),
                None => String::new(),
            };
            let artifact = format!(
                r#"{{"filename":"fw.bin","size":{}{hashes}{links}}}"#,
                self.size
            );
            let metadata: Vec<String> = self
                .metadata
                .iter()
                .map(|(k, val)| format!(r#"{{"key":"{k}","value":"{val}"}}"#))
                .collect();
            let chunk = format!(
                r#"{{"part":"os","version":"{}","encrypted":{},"metadata":[{}],"artifacts":[{}]}}"#,
                self.version,
                self.encrypted,
                metadata.join(","),
                vec![artifact; self.artifacts].join(",")
            );
            format!(
                r#"{{"id":"{}","deployment":{{"download":"{}","update":"{}","chunks":[{}]}}}}"#,
                self.id,
                self.download,
                self.update,
                vec![chunk; self.chunks].join(",")
            )
        }
    }

    fn ok(body: &str) -> RequestOutcome {
        RequestOutcome::Response(DdiResponse {
            status: 200,
            retry_after: None,
            body: Vec::from(body.as_bytes()),
        })
    }

    fn judge(spec: &Spec, facts: &AdmissionFacts) -> DeploymentVerdict {
        admit_retrieved(&href(LINK), &ok(&spec.body()), facts, &endpoint())
    }

    /// The verdict without the payload the table does not compare.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Expect {
        NotRetrieved(Transient),
        Unreportable(FailReason),
        Deferred,
        Refused(u16, FailReason),
        Admitted(Option<FailReason>),
    }

    fn summary(verdict: &DeploymentVerdict) -> Expect {
        match verdict {
            DeploymentVerdict::NotRetrieved(cause) => Expect::NotRetrieved(*cause),
            DeploymentVerdict::Unreportable(reason) => Expect::Unreportable(*reason),
            DeploymentVerdict::Deferred(_) => Expect::Deferred,
            DeploymentVerdict::Refused(r) => Expect::Refused(r.code.as_u16(), r.reason),
            DeploymentVerdict::Admitted(offer) => Expect::Admitted(offer.success_detail),
        }
    }

    const INVALID: Expect = Expect::Refused(1006, FailReason::ManifestInvalid);
    const FOREIGN: Expect = Expect::Refused(1006, FailReason::Ota(OtaError::ForeignOrigin));
    const TARGET: Expect = Expect::Refused(1007, FailReason::TargetMismatch);
    const BELOW_FLOOR: Expect = Expect::Refused(1007, FailReason::BelowFloor);
    const ROLLED_BACK: Expect = Expect::Refused(1007, FailReason::PreviouslyRolledBack);
    const DOWNGRADE: Expect = Expect::Refused(1007, FailReason::Downgrade);
    const UP_TO_DATE: Expect = Expect::Refused(1007, FailReason::UpToDate);
    const ADMITTED: Expect = Expect::Admitted(None);
    const ADMITTED_DOWNGRADE: Expect = Expect::Admitted(Some(FailReason::Downgrade));
    const ADMITTED_ROLLED_BACK: Expect = Expect::Admitted(Some(FailReason::PreviouslyRolledBack));

    fn run(cases: &[(&str, Spec, AdmissionFacts, Expect)]) {
        for (name, spec, facts, expected) in cases {
            assert_eq!(summary(&judge(spec, facts)), *expected, "{name}");
        }
    }

    #[test]
    fn document_rules_refuse_with_manifest_invalid() {
        let f = facts;
        run(&[
            ("valid upgrade", spec(), f(), ADMITTED),
            (
                "no chunk",
                Spec {
                    chunks: 0,
                    ..spec()
                },
                f(),
                INVALID,
            ),
            (
                "two chunks",
                Spec {
                    chunks: 2,
                    ..spec()
                },
                f(),
                INVALID,
            ),
            (
                "no artifact",
                Spec {
                    artifacts: 0,
                    ..spec()
                },
                f(),
                INVALID,
            ),
            (
                "two artifacts",
                Spec {
                    artifacts: 2,
                    ..spec()
                },
                f(),
                INVALID,
            ),
            (
                "encrypted",
                Spec {
                    encrypted: true,
                    ..spec()
                },
                f(),
                INVALID,
            ),
            (
                "no sha256",
                Spec {
                    sha256: false,
                    ..spec()
                },
                f(),
                INVALID,
            ),
            (
                "no download-http",
                Spec {
                    href: None,
                    ..spec()
                },
                f(),
                INVALID,
            ),
            ("version not strict", spec().version("2.01.0"), f(), INVALID),
            ("version two parts", spec().version("2.1"), f(), INVALID),
            (
                "version with suffix",
                spec().version("2.1.0-rc1"),
                f(),
                INVALID,
            ),
            ("chip missing", spec().metadata(&[]), f(), INVALID),
            ("chip empty", spec().metadata(&[("chip", "")]), f(), INVALID),
            (
                "chip upper case",
                spec().metadata(&[("chip", "ESP32C3")]),
                f(),
                INVALID,
            ),
            (
                "chip with dash",
                spec().metadata(&[("chip", "esp32-c3")]),
                f(),
                INVALID,
            ),
            (
                "chip 33 bytes",
                spec().metadata(&[("chip", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")]),
                f(),
                INVALID,
            ),
            (
                "chip duplicated, same value",
                spec().metadata(&[("chip", "esp32c3"), ("chip", "esp32c3")]),
                f(),
                INVALID,
            ),
            (
                "marker value Allowed",
                spec().metadata(&[("chip", "esp32c3"), ("downgrade", "Allowed")]),
                f(),
                INVALID,
            ),
            (
                "marker value empty",
                spec().metadata(&[("chip", "esp32c3"), ("downgrade", "")]),
                f(),
                INVALID,
            ),
            (
                "marker duplicated",
                spec().metadata(&[
                    ("chip", "esp32c3"),
                    ("downgrade", "allowed"),
                    ("downgrade", "allowed"),
                ]),
                f(),
                INVALID,
            ),
            ("marker present", spec().marked(), f(), ADMITTED),
        ]);
    }

    #[test]
    fn size_is_checked_against_the_update_partition() {
        let f = facts;
        run(&[
            (
                "size equals capacity",
                Spec {
                    size: CAPACITY,
                    ..spec()
                },
                f(),
                ADMITTED,
            ),
            ("size zero", Spec { size: 0, ..spec() }, f(), INVALID),
            (
                "size above capacity",
                Spec {
                    size: CAPACITY + 1,
                    ..spec()
                },
                f(),
                INVALID,
            ),
            (
                "size u64::MAX",
                Spec {
                    size: u64::MAX,
                    ..spec()
                },
                f(),
                INVALID,
            ),
            (
                "no capacity",
                spec(),
                AdmissionFacts {
                    partition_capacity: 0,
                    ..f()
                },
                INVALID,
            ),
        ]);
    }

    #[test]
    fn artifact_link_must_be_on_the_configured_origin() {
        let f = facts;
        run(&[
            (
                "other host",
                Spec {
                    href: Some("http://evil.lab:8080/fw.bin"),
                    ..spec()
                },
                f(),
                FOREIGN,
            ),
            (
                "other port",
                Spec {
                    href: Some("http://ddi.lab:8081/fw.bin"),
                    ..spec()
                },
                f(),
                FOREIGN,
            ),
            (
                "other scheme",
                Spec {
                    href: Some("https://ddi.lab:8080/fw.bin"),
                    ..spec()
                },
                f(),
                FOREIGN,
            ),
            (
                "userinfo",
                Spec {
                    href: Some("http://u:p@ddi.lab:8080/fw.bin"),
                    ..spec()
                },
                f(),
                FOREIGN,
            ),
            (
                "relative",
                Spec {
                    href: Some("/DEFAULT/fw.bin"),
                    ..spec()
                },
                f(),
                FOREIGN,
            ),
            (
                "percent",
                Spec {
                    href: Some("http://ddi.lab:8080/trap/%2e%2e/fw.bin"),
                    ..spec()
                },
                f(),
                FOREIGN,
            ),
            (
                "ip literal for the name",
                Spec {
                    href: Some("http://127.0.0.1:8080/fw.bin"),
                    ..spec()
                },
                f(),
                FOREIGN,
            ),
            (
                "host case",
                Spec {
                    href: Some("http://DDI.lab:8080/fw.bin"),
                    ..spec()
                },
                f(),
                ADMITTED,
            ),
        ]);
    }

    #[test]
    fn a_refused_artifact_link_carries_the_case_for_the_log() {
        for (link, case) in [
            ("http://u@ddi.lab:8080/fw.bin", UrlRefusal::ForbiddenChar),
            ("http://evil.lab:8080/fw.bin", UrlRefusal::Origin),
        ] {
            let spec = Spec {
                href: Some(link),
                ..spec()
            };
            let DeploymentVerdict::Refused(refusal) = judge(&spec, &facts()) else {
                panic!("{link}");
            };
            assert_eq!(
                refusal.reason,
                FailReason::Ota(OtaError::ForeignOrigin),
                "{link}"
            );
            assert_eq!(refusal.url_case, Some(case), "{link}");
        }
        let DeploymentVerdict::Refused(refusal) = judge(&spec().version("2.0.0"), &facts()) else {
            panic!("up to date");
        };
        assert_eq!(refusal.reason, FailReason::UpToDate);
        assert_eq!(refusal.url_case, None);
    }

    #[test]
    fn one_row_per_refusal_code() {
        let f = facts;
        run(&[
            (
                "1006 manifest_invalid",
                Spec {
                    chunks: 2,
                    ..spec()
                },
                f(),
                INVALID,
            ),
            (
                "1006 foreign_origin",
                Spec {
                    href: Some("http://evil/x"),
                    ..spec()
                },
                f(),
                FOREIGN,
            ),
            (
                "1007 target_mismatch",
                spec().metadata(&[("chip", "esp32c6")]),
                f(),
                TARGET,
            ),
            (
                "1007 below_floor",
                spec(),
                AdmissionFacts {
                    floor: Some(v(3, 0, 0)),
                    ..f()
                },
                BELOW_FLOOR,
            ),
            (
                "1007 previously_rolled_back",
                spec(),
                AdmissionFacts {
                    rolled_back: Some(v(2, 1, 0)),
                    ..f()
                },
                ROLLED_BACK,
            ),
            ("1007 downgrade", spec().version("1.9.9"), f(), DOWNGRADE),
            ("1007 up_to_date", spec().version("2.0.0"), f(), UP_TO_DATE),
        ]);
        let link = href(LINK);
        let redirect = admit_retrieved(
            &link,
            &RequestOutcome::Redirect { status: 302 },
            &facts(),
            &endpoint(),
        );
        assert_eq!(
            summary(&redirect),
            Expect::Refused(1006, FailReason::Ota(OtaError::Redirect { status: 302 }))
        );
    }

    #[test]
    fn refusal_precedence_of_the_version_gate() {
        let f = facts;
        let floor_3 = || AdmissionFacts {
            floor: Some(v(3, 0, 0)),
            ..f()
        };
        let remembered = |rb| AdmissionFacts {
            rolled_back: Some(rb),
            ..f()
        };
        let both = |rb| AdmissionFacts {
            rolled_back: Some(rb),
            floor: Some(v(3, 0, 0)),
            ..f()
        };
        run(&[
            // below_floor beats previously_rolled_back.
            (
                "floor over rolled back",
                spec(),
                both(v(2, 1, 0)),
                BELOW_FLOOR,
            ),
            (
                "floor over rolled back, marker",
                spec().marked(),
                both(v(2, 1, 0)),
                BELOW_FLOOR,
            ),
            // below_floor beats downgrade and up_to_date.
            (
                "floor over downgrade",
                spec().version("1.0.0"),
                floor_3(),
                BELOW_FLOOR,
            ),
            (
                "floor over up_to_date",
                spec().version("2.0.0"),
                floor_3(),
                BELOW_FLOOR,
            ),
            (
                "floor over marker",
                spec().version("1.0.0").marked(),
                floor_3(),
                BELOW_FLOOR,
            ),
            // previously_rolled_back beats downgrade and up_to_date.
            (
                "rolled back over downgrade",
                spec().version("1.0.0"),
                remembered(v(1, 0, 0)),
                ROLLED_BACK,
            ),
            (
                "rolled back over up_to_date",
                spec().version("2.0.0"),
                remembered(v(2, 0, 0)),
                ROLLED_BACK,
            ),
            // Another remembered version does not touch the offer.
            (
                "other remembered, upgrade",
                spec(),
                remembered(v(2, 2, 0)),
                ADMITTED,
            ),
            (
                "other remembered, downgrade",
                spec().version("1.0.0"),
                remembered(v(2, 2, 0)),
                DOWNGRADE,
            ),
            // A floor the offer meets does not touch it.
            (
                "floor met",
                spec(),
                AdmissionFacts {
                    floor: Some(v(2, 1, 0)),
                    ..f()
                },
                ADMITTED,
            ),
        ]);
    }

    #[test]
    fn the_marker_admits_and_names_the_success_detail() {
        let f = facts;
        let remembered = |rb| AdmissionFacts {
            rolled_back: Some(rb),
            ..f()
        };
        run(&[
            ("marked upgrade is plain", spec().marked(), f(), ADMITTED),
            (
                "marked downgrade",
                spec().version("1.0.0").marked(),
                f(),
                ADMITTED_DOWNGRADE,
            ),
            (
                "marked same version",
                spec().version("2.0.0").marked(),
                f(),
                ADMITTED_DOWNGRADE,
            ),
            (
                "marked remembered, newer",
                spec().marked(),
                remembered(v(2, 1, 0)),
                ADMITTED_ROLLED_BACK,
            ),
            (
                "marked remembered, older: rolled back wins",
                spec().version("1.0.0").marked(),
                remembered(v(1, 0, 0)),
                ADMITTED_ROLLED_BACK,
            ),
            (
                "marked remembered, same: rolled back wins",
                spec().version("2.0.0").marked(),
                remembered(v(2, 0, 0)),
                ADMITTED_ROLLED_BACK,
            ),
            // The marker does not lift the chip gate.
            (
                "marker and wrong chip",
                spec()
                    .version("1.0.0")
                    .metadata(&[("chip", "esp32"), ("downgrade", "allowed")]),
                f(),
                TARGET,
            ),
        ]);
    }

    #[test]
    fn evaluation_order_validity_skip_chip_version() {
        let f = facts;
        let wrong_chip = || spec().metadata(&[("chip", "esp32s3")]);
        run(&[
            // Validity before skip.
            (
                "invalid and skip",
                Spec {
                    chunks: 2,
                    ..spec()
                }
                .skip(),
                f(),
                INVALID,
            ),
            (
                "oversize and skip",
                Spec {
                    size: CAPACITY + 1,
                    ..spec()
                }
                .skip(),
                f(),
                INVALID,
            ),
            (
                "foreign and skip",
                Spec {
                    href: Some("http://evil/x"),
                    ..spec()
                }
                .skip(),
                f(),
                FOREIGN,
            ),
            (
                "chip missing and skip",
                spec().metadata(&[]).skip(),
                f(),
                INVALID,
            ),
            // Document rules before the artifact origin.
            (
                "invalid and foreign",
                Spec {
                    encrypted: true,
                    href: Some("http://evil/x"),
                    ..spec()
                },
                f(),
                INVALID,
            ),
            // Skip in either mode defers a valid document, before chip and version.
            ("skip update", spec().skip(), f(), Expect::Deferred),
            (
                "skip download",
                Spec {
                    download: "skip",
                    ..spec()
                },
                f(),
                Expect::Deferred,
            ),
            (
                "skip both",
                Spec {
                    download: "skip",
                    ..spec()
                }
                .skip(),
                f(),
                Expect::Deferred,
            ),
            (
                "skip and wrong chip",
                wrong_chip().skip(),
                f(),
                Expect::Deferred,
            ),
            (
                "skip and up_to_date",
                spec().version("2.0.0").skip(),
                f(),
                Expect::Deferred,
            ),
            (
                "attempt admits",
                Spec {
                    download: "attempt",
                    update: "attempt",
                    ..spec()
                },
                f(),
                ADMITTED,
            ),
            // Chip before version.
            (
                "wrong chip and downgrade",
                wrong_chip().version("1.0.0"),
                f(),
                TARGET,
            ),
            (
                "wrong chip and below floor",
                wrong_chip(),
                AdmissionFacts {
                    floor: Some(v(9, 0, 0)),
                    ..f()
                },
                TARGET,
            ),
            // Validity before chip.
            (
                "invalid and wrong chip",
                Spec {
                    sha256: false,
                    ..wrong_chip()
                },
                f(),
                INVALID,
            ),
            // Unknown mode is a document error, not a skip.
            (
                "unknown mode",
                Spec {
                    update: "later",
                    ..spec()
                },
                f(),
                INVALID,
            ),
        ]);
    }

    #[test]
    fn admitted_offer_carries_the_download_facts() {
        let DeploymentVerdict::Admitted(offer) = judge(&spec(), &facts()) else {
            panic!("not admitted");
        };
        assert_eq!(offer.action.as_str(), "12");
        assert_eq!(offer.version, v(2, 1, 0));
        assert_eq!(offer.artifact.as_str(), ARTIFACT);
        assert_eq!(offer.sha256, hex_to_bytes(HASH).unwrap());
        assert_eq!(offer.size, 1234);
        assert_eq!(offer.success_detail, None);
        assert!(!format!("{offer:?}").contains("softwaremodules"));
    }

    #[test]
    fn refusals_and_deferrals_go_to_the_document_action() {
        let DeploymentVerdict::Refused(r) = judge(&spec().version("2.0.0"), &facts()) else {
            panic!("not refused");
        };
        assert_eq!(r.action.as_str(), "12");
        assert_eq!(
            judge(&spec().skip(), &facts()),
            DeploymentVerdict::Deferred(ActionId::parse("12").unwrap())
        );
    }

    #[test]
    fn a_document_id_that_differs_from_the_link_is_refused_to_the_link_action() {
        for spec in [
            Spec { id: "77", ..spec() },
            Spec { id: "77", ..spec() }.skip(),
        ] {
            let DeploymentVerdict::Refused(r) = judge(&spec, &facts()) else {
                panic!("not refused");
            };
            assert_eq!(r.action.as_str(), "12");
            assert_eq!(r.reason, FailReason::ManifestInvalid);
            assert_eq!(r.code, ProfileCode::Deployment);
        }
    }

    fn outcome(link: &str, outcome: RequestOutcome) -> DeploymentVerdict {
        admit_retrieved(&href(link), &outcome, &facts(), &endpoint())
    }

    #[test]
    fn get_outcomes_without_a_document() {
        let body_failed = |status, failure| RequestOutcome::BodyFailed {
            status,
            retry_after: None,
            failure,
        };
        let status = |status: u16| {
            RequestOutcome::Response(DdiResponse {
                status,
                retry_after: None,
                body: Vec::from(&b"{}"[..]),
            })
        };
        let over_cap = format!("{}{}", spec().body(), " ".repeat(MAX_RESPONSE_BYTES));
        let cases = [
            (
                "redirect",
                RequestOutcome::Redirect { status: 307 },
                Expect::Refused(1006, FailReason::Ota(OtaError::Redirect { status: 307 })),
            ),
            (
                "200 over the cap, not read",
                body_failed(200, BodyFailure::TooLarge),
                INVALID,
            ),
            ("200 over the cap, read", ok(&over_cap), INVALID),
            ("200 not JSON", ok("not json"), INVALID),
            ("200 array", ok("[]"), INVALID),
            (
                "200 bad action id",
                ok(&Spec { id: "x", ..spec() }.body()),
                INVALID,
            ),
            (
                "200 body timed out",
                body_failed(200, BodyFailure::Timeout),
                Expect::NotRetrieved(Transient::Body(200)),
            ),
            (
                "200 body cut short",
                body_failed(200, BodyFailure::LengthMismatch),
                Expect::NotRetrieved(Transient::Body(200)),
            ),
            (
                "200 connection lost",
                body_failed(200, BodyFailure::ServerUnreachable),
                Expect::NotRetrieved(Transient::Body(200)),
            ),
            (
                "500 over the cap",
                body_failed(500, BodyFailure::TooLarge),
                Expect::NotRetrieved(Transient::Body(500)),
            ),
            (
                "404",
                status(404),
                Expect::NotRetrieved(Transient::Status(404)),
            ),
            (
                "401",
                status(401),
                Expect::NotRetrieved(Transient::Status(401)),
            ),
            (
                "410",
                status(410),
                Expect::NotRetrieved(Transient::Status(410)),
            ),
            (
                "429",
                status(429),
                Expect::NotRetrieved(Transient::Status(429)),
            ),
            (
                "503",
                status(503),
                Expect::NotRetrieved(Transient::Status(503)),
            ),
            (
                "204",
                status(204),
                Expect::NotRetrieved(Transient::Status(204)),
            ),
            (
                "timeout",
                RequestOutcome::Timeout,
                Expect::NotRetrieved(Transient::Timeout),
            ),
            (
                "unreachable",
                RequestOutcome::Unreachable,
                Expect::NotRetrieved(Transient::Unreachable),
            ),
            (
                "foreign origin",
                RequestOutcome::ForeignOrigin,
                Expect::NotRetrieved(Transient::ForeignOrigin),
            ),
        ];
        for (name, o, expected) in cases {
            assert_eq!(summary(&outcome(LINK, o)), expected, "{name}");
        }
    }

    #[test]
    fn an_unparsed_body_is_reported_to_the_link_action() {
        let DeploymentVerdict::Refused(r) = outcome(LINK, ok("garbage")) else {
            panic!("not refused");
        };
        assert_eq!(r.action.as_str(), "12");
        let DeploymentVerdict::Refused(r) = outcome(LINK, RequestOutcome::Redirect { status: 301 })
        else {
            panic!("not refused");
        };
        assert_eq!(r.action.as_str(), "12");
    }

    #[test]
    fn a_link_without_an_action_id_makes_a_refusal_unreportable() {
        let link = "http://ddi.lab:8080/DEFAULT/controller/v1/dev-1/somewhere";
        assert_eq!(
            outcome(link, ok("garbage")),
            DeploymentVerdict::Unreportable(FailReason::ManifestInvalid)
        );
        assert_eq!(
            outcome(link, RequestOutcome::Redirect { status: 302 }),
            DeploymentVerdict::Unreportable(FailReason::Ota(OtaError::Redirect { status: 302 }))
        );
        let other_controller = "http://ddi.lab:8080/DEFAULT/controller/v1/dev-2/deploymentBase/12";
        assert_eq!(
            outcome(other_controller, ok("garbage")),
            DeploymentVerdict::Unreportable(FailReason::ManifestInvalid)
        );
        // A parsed document names its own action, whatever the link.
        assert!(matches!(
            outcome(link, ok(&spec().version("2.0.0").body())),
            DeploymentVerdict::Refused(Refusal {
                code: ProfileCode::Policy,
                ..
            })
        ));
    }

    #[test]
    fn every_refusal_reason_has_its_code() {
        let reasons = [
            (FailReason::ManifestInvalid, ProfileCode::Deployment),
            (
                FailReason::Ota(OtaError::ForeignOrigin),
                ProfileCode::Deployment,
            ),
            (
                FailReason::Ota(OtaError::Redirect { status: 302 }),
                ProfileCode::Deployment,
            ),
            (FailReason::TargetMismatch, ProfileCode::Policy),
            (FailReason::BelowFloor, ProfileCode::Policy),
            (FailReason::PreviouslyRolledBack, ProfileCode::Policy),
            (FailReason::Downgrade, ProfileCode::Policy),
            (FailReason::UpToDate, ProfileCode::Policy),
        ];
        for (reason, code) in reasons {
            assert_eq!(ProfileCode::for_reason(reason), Some(code), "{reason:?}");
            assert_eq!(
                Refusal::new(ActionId::parse("1").unwrap(), reason).code,
                code
            );
        }
    }
}
