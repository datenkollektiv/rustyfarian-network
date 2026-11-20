//! The DDI deployment base resource: the action id, the update modes and the chunks with their artifacts.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use serde::de::{Deserializer, Error, SeqAccess, Visitor};
use serde::Deserialize;

use super::bounded::{BoundedString, BoundedVec, Href, Obj};
use super::error::DdiError;
use super::parse_object;
use super::{
    MAX_ARTIFACTS, MAX_CHUNKS, MAX_FILENAME_BYTES, MAX_METADATA, MAX_METADATA_VALUE_BYTES,
    MAX_VERSION_BYTES,
};
use crate::ota::action_id::is_canonical_action_id;
use crate::ota::hex_to_bytes;

/// Experimental: API may change before 1.0.
///
/// A hawkBit action id in canonical `i64` form: 1 to 19 ASCII digits, no leading zero unless the id is `0`, at most `i64::MAX`.
/// hawkBit ids are Java `long` values, so a server never sends another spelling, and text and numeric equality agree: the loop guard compares ids as text and persists them as these digits.
/// `Display` writes the digits and is safe to log.
///
/// # Examples
///
/// ```
/// use juggler::ota::ddi::wire::{ActionId, DdiError};
///
/// assert_eq!(ActionId::parse("12").unwrap().as_str(), "12");
/// assert_eq!(ActionId::parse("012"), Err(DdiError::InvalidActionId));
/// assert_eq!(ActionId::parse("12a"), Err(DdiError::InvalidActionId));
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionId(String);

impl ActionId {
    /// The digits.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Validates `text` as an action id.
    ///
    /// # Errors
    ///
    /// [`DdiError::InvalidActionId`] unless `text` is a canonical `i64`: 1 to [`MAX_ACTION_ID_DIGITS`](super::MAX_ACTION_ID_DIGITS) ASCII digits, no leading zero unless the id is `0`, at most `i64::MAX`.
    pub fn parse(text: &str) -> Result<Self, DdiError> {
        if !is_canonical_action_id(text) {
            return Err(DdiError::InvalidActionId);
        }
        Ok(ActionId(String::from(text)))
    }
}

impl fmt::Display for ActionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Experimental: API may change before 1.0.
///
/// A hawkBit update mode, the wire strings `forced`, `attempt` and `skip` (exact lowercase).
/// Any other string is [`DdiError::Malformed`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateMode {
    /// `forced`.
    Forced,
    /// `attempt`.
    Attempt,
    /// `skip`.
    Skip,
}

impl<'de> Deserialize<'de> for UpdateMode {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ModeVisitor;
        impl Visitor<'_> for ModeVisitor {
            type Value = UpdateMode;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an update mode")
            }
            fn visit_str<E: Error>(self, v: &str) -> Result<Self::Value, E> {
                match v {
                    "forced" => Ok(UpdateMode::Forced),
                    "attempt" => Ok(UpdateMode::Attempt),
                    "skip" => Ok(UpdateMode::Skip),
                    _ => Err(E::custom("unknown update mode")),
                }
            }
        }
        deserializer.deserialize_str(ModeVisitor)
    }
}

/// Experimental: API may change before 1.0.
///
/// The hawkBit DDI v1 `deploymentBase` resource, reduced to what the device acts on.
/// It is only syntax-checked: a deployment with no chunks, an artifact without a digest or without a download link parses, and admission refuses it afterwards.
/// Unknown fields (`maintenanceWindow`, `actionHistory`, links, a chunk's `part` and `name`) are ignored.
///
/// `Debug` redacts every download link because it may carry credentials or a token.
///
/// # Examples
///
/// ```
/// use juggler::ota::ddi::wire::{DeploymentBase, UpdateMode};
///
/// let body = br#"{"id":"12","deployment":{"download":"forced","update":"forced","chunks":[
///     {"part":"os","version":"1.2.3","name":"firmware","artifacts":[
///         {"filename":"fw.bin","size":1234,
///          "hashes":{"sha256":"2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"},
///          "_links":{"download-http":{"href":"http://h/fw.bin"}}}]}]}}"#;
/// let deployment = DeploymentBase::parse(body).unwrap();
/// assert_eq!(deployment.id.as_str(), "12");
/// assert_eq!(deployment.update, UpdateMode::Forced);
/// let artifact = &deployment.chunks[0].artifacts[0];
/// assert_eq!(artifact.size, 1234);
/// assert_eq!(artifact.download_http.as_ref().unwrap().as_str(), "http://h/fw.bin");
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeploymentBase {
    /// The action id.
    pub id: ActionId,
    /// `deployment.download`.
    pub download: UpdateMode,
    /// `deployment.update`.
    pub update: UpdateMode,
    /// `deployment.chunks`, at most [`MAX_CHUNKS`]; may be empty (admission refuses it later).
    pub chunks: Vec<Chunk>,
}

/// Experimental: API may change before 1.0.
///
/// One software module of a deployment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    /// The version text, at most [`MAX_VERSION_BYTES`] bytes; required and not parsed as a version here.
    pub version: String,
    /// `encrypted`; `false` when absent, and a value that is not a boolean is [`DdiError::Malformed`].
    /// Admission refuses `true`.
    pub encrypted: bool,
    /// The `downgrade` and `chip` metadata entries in document order, at most [`MAX_METADATA`]; empty when absent.
    /// Every other entry is skipped, while a duplicated or invalid `downgrade` or `chip` entry is kept for admission to refuse.
    pub metadata: Vec<Metadata>,
    /// The artifacts, at most [`MAX_ARTIFACTS`]; empty when absent.
    pub artifacts: Vec<Artifact>,
}

/// Experimental: API may change before 1.0.
///
/// One key and value of a chunk's metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Metadata {
    /// The key, `downgrade` or `chip` exactly (case-sensitive).
    pub key: String,
    /// The value, at most [`MAX_METADATA_VALUE_BYTES`] bytes; not checked against the key's grammar here.
    pub value: String,
}

/// Experimental: API may change before 1.0.
///
/// One downloadable file of a chunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Artifact {
    /// The file name, at most [`MAX_FILENAME_BYTES`] bytes; required.
    pub filename: String,
    /// The size in bytes; required, a non-negative integer.
    pub size: u64,
    /// `hashes.sha256` decoded; `None` when `hashes` or `sha256` is absent or `null` (admission refuses that later).
    /// The `sha1` and `md5` hashes are ignored.
    pub sha256: Option<[u8; 32]>,
    /// `_links.download-http.href`; `None` when `_links` or the link is absent or `null`.
    pub download_http: Option<Href>,
}

#[derive(Deserialize)]
struct RawDeployment {
    id: String,
    deployment: Obj<RawBody>,
}

#[derive(Deserialize)]
struct RawBody {
    download: UpdateMode,
    update: UpdateMode,
    chunks: BoundedVec<Obj<RawChunk>, MAX_CHUNKS>,
}

#[derive(Deserialize)]
struct RawChunk {
    version: BoundedString<MAX_VERSION_BYTES>,
    #[serde(default)]
    encrypted: bool,
    #[serde(default)]
    metadata: RawMetadataList,
    #[serde(default)]
    artifacts: BoundedVec<Obj<RawArtifact>, MAX_ARTIFACTS>,
}

/// The metadata keys of the fleet DDI profile; an entry with any other key is skipped.
const PROFILE_METADATA_KEYS: [&str; 2] = ["downgrade", "chip"];

/// A metadata key: one of [`PROFILE_METADATA_KEYS`], or any other text, which is neither stored nor capped.
enum RawKey {
    Profile(&'static str),
    Other,
}

impl<'de> Deserialize<'de> for RawKey {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct KeyVisitor;
        impl Visitor<'_> for KeyVisitor {
            type Value = RawKey;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a string")
            }
            fn visit_str<E: Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(PROFILE_METADATA_KEYS
                    .iter()
                    .find(|k| **k == v)
                    .map_or(RawKey::Other, |k| RawKey::Profile(k)))
            }
        }
        deserializer.deserialize_str(KeyVisitor)
    }
}

/// A metadata value of at most [`MAX_METADATA_VALUE_BYTES`] bytes, or only the mark that it was longer.
///
/// The value may precede the key, so an over-long one cannot fail here: it fails only once its entry turns out to have a profile key, and a skipped entry never allocates for it.
enum RawValue {
    Text(String),
    TooLong,
}

impl<'de> Deserialize<'de> for RawValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ValueVisitor;
        impl Visitor<'_> for ValueVisitor {
            type Value = RawValue;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a string")
            }
            fn visit_str<E: Error>(self, v: &str) -> Result<Self::Value, E> {
                if v.len() > MAX_METADATA_VALUE_BYTES {
                    return Ok(RawValue::TooLong);
                }
                Ok(RawValue::Text(String::from(v)))
            }
        }
        deserializer.deserialize_str(ValueVisitor)
    }
}

/// One metadata entry; every entry, skipped or not, must be an object with a string `key` and a string `value`.
#[derive(Deserialize)]
struct RawMetadata {
    key: RawKey,
    value: RawValue,
}

/// A chunk's metadata array reduced to its profile entries, in document order.
///
/// Fail, never truncate: a profile entry with an over-long value or one more than [`MAX_METADATA`] profile entries is an error.
#[derive(Default)]
struct RawMetadataList(Vec<Metadata>);

impl<'de> Deserialize<'de> for RawMetadataList {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct SeqVisitor;
        impl<'de> Visitor<'de> for SeqVisitor {
            type Value = RawMetadataList;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an array")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut kept = Vec::new();
                while let Some(Obj(entry)) = seq.next_element::<Obj<RawMetadata>>()? {
                    let RawKey::Profile(key) = entry.key else {
                        continue;
                    };
                    let RawValue::Text(value) = entry.value else {
                        return Err(A::Error::custom("string too long"));
                    };
                    if kept.len() == MAX_METADATA {
                        return Err(A::Error::custom("too many elements"));
                    }
                    kept.push(Metadata {
                        key: String::from(key),
                        value,
                    });
                }
                Ok(RawMetadataList(kept))
            }
        }
        deserializer.deserialize_seq(SeqVisitor)
    }
}

#[derive(Deserialize)]
struct RawArtifact {
    filename: BoundedString<MAX_FILENAME_BYTES>,
    size: u64,
    hashes: Option<Obj<RawHashes>>,
    #[serde(rename = "_links")]
    links: Option<Obj<RawArtifactLinks>>,
}

#[derive(Deserialize)]
struct RawHashes {
    sha256: Option<String>,
}

#[derive(Deserialize)]
struct RawArtifactLinks {
    #[serde(rename = "download-http")]
    download_http: Option<Obj<RawLink>>,
}

#[derive(Deserialize)]
struct RawLink {
    href: Href,
}

impl RawArtifact {
    fn into_artifact(self) -> Result<Artifact, DdiError> {
        let sha256 = match self.hashes.and_then(|h| h.0.sha256) {
            Some(hex) => Some(hex_to_bytes(&hex).map_err(|_| DdiError::InvalidDigest)?),
            None => None,
        };
        Ok(Artifact {
            filename: self.filename.into_inner(),
            size: self.size,
            sha256,
            download_http: self.links.and_then(|l| l.0.download_http).map(|l| l.0.href),
        })
    }
}

impl RawChunk {
    fn into_chunk(self) -> Result<Chunk, DdiError> {
        let artifacts = self
            .artifacts
            .into_inner()
            .into_iter()
            .map(|a| a.0.into_artifact())
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Chunk {
            version: self.version.into_inner(),
            encrypted: self.encrypted,
            metadata: self.metadata.0,
            artifacts,
        })
    }
}

impl DeploymentBase {
    /// Parses a deployment base body.
    ///
    /// # Errors
    ///
    /// [`DdiError::TooLarge`] for more than [`MAX_RESPONSE_BYTES`](super::MAX_RESPONSE_BYTES) bytes (checked before parsing),
    /// [`DdiError::Malformed`] for anything that is not a JSON object of the expected shape, including an unknown update mode, a string or array over its cap, a numeric `id`, a negative or fractional `size`, and a non-boolean `encrypted`,
    /// [`DdiError::InvalidActionId`] when `id` is not a canonical `i64` ([`ActionId::parse`]),
    /// [`DdiError::InvalidDigest`] when a `sha256` is present but not 64 hex characters.
    pub fn parse(body: &[u8]) -> Result<Self, DdiError> {
        let raw: RawDeployment = parse_object(body)?;
        let id = ActionId::parse(&raw.id)?;
        let chunks = raw
            .deployment
            .0
            .chunks
            .into_inner()
            .into_iter()
            .map(|c| c.0.into_chunk())
            .collect::<Result<Vec<_>, _>>()?;
        Ok(DeploymentBase {
            id,
            download: raw.deployment.0.download,
            update: raw.deployment.0.update,
            chunks,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ota::ddi::wire::{MAX_HREF_BYTES, MAX_RESPONSE_BYTES};
    use alloc::format;
    use alloc::vec;

    const HASH: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

    fn hash_bytes() -> [u8; 32] {
        hex_to_bytes(HASH).unwrap()
    }

    fn art(filename: &str, size: &str, hashes: &str, links: &str) -> String {
        format!(r#"{{"filename":"{filename}","size":{size},"hashes":{hashes},"_links":{links}}}"#)
    }

    fn good_art() -> String {
        art(
            "fw.bin",
            "1234",
            &format!(r#"{{"sha1":"aa","md5":"bb","sha256":"{HASH}"}}"#),
            r#"{"download-http":{"href":"http://h/fw.bin"},"md5sum-http":{"href":"http://h/fw.md5"}}"#,
        )
    }

    fn chunk(version: &str, metadata: &str, artifacts: &str) -> String {
        format!(r#"{{"version":"{version}","metadata":{metadata},"artifacts":{artifacts}}}"#)
    }

    fn good_chunk() -> String {
        chunk(
            "1.2.3",
            r#"[{"key":"chip","value":"esp32c3"}]"#,
            &format!("[{}]", good_art()),
        )
    }

    fn doc(id: &str, download: &str, update: &str, chunks: &str) -> String {
        format!(
            r#"{{"id":"{id}","deployment":{{"download":"{download}","update":"{update}","chunks":[{chunks}]}},"_links":{{"self":{{"href":"http://h/s"}}}}}}"#
        )
    }

    fn good() -> String {
        doc("12", "forced", "forced", &good_chunk())
    }

    fn parse(s: &str) -> Result<DeploymentBase, DdiError> {
        DeploymentBase::parse(s.as_bytes())
    }

    fn is_malformed(s: &str) -> bool {
        matches!(parse(s), Err(DdiError::Malformed { .. }))
    }

    fn join(item: &str, n: usize) -> String {
        vec![item; n].join(",")
    }

    #[test]
    fn full_document_with_two_chunks_parses_every_field() {
        let second = chunk(
            "9.9.9",
            "[]",
            &format!(
                "[{}]",
                art(
                    "x.bin",
                    "0",
                    &format!(r#"{{"sha256":"{}"}}"#, HASH.to_uppercase()),
                    "{}"
                )
            ),
        );
        let body = doc(
            "12",
            "attempt",
            "skip",
            &format!("{},{second}", good_chunk()),
        );
        let d = parse(&body).unwrap();
        assert_eq!(d.id.as_str(), "12");
        assert_eq!(d.download, UpdateMode::Attempt);
        assert_eq!(d.update, UpdateMode::Skip);
        assert_eq!(d.chunks.len(), 2);
        let c = &d.chunks[0];
        assert_eq!((c.version.as_str(), c.encrypted), ("1.2.3", false));
        assert_eq!(
            c.metadata,
            [Metadata {
                key: "chip".into(),
                value: "esp32c3".into()
            }]
        );
        let a = &c.artifacts[0];
        assert_eq!(a.filename, "fw.bin");
        assert_eq!(a.size, 1234);
        assert_eq!(a.sha256, Some(hash_bytes()));
        assert_eq!(
            a.download_http.as_ref().unwrap().as_str(),
            "http://h/fw.bin"
        );
        let c2 = &d.chunks[1];
        assert_eq!(c2.version, "9.9.9");
        assert!(c2.metadata.is_empty());
        let a2 = &c2.artifacts[0];
        assert_eq!(a2.size, 0);
        assert_eq!(a2.sha256, Some(hash_bytes()));
        assert!(a2.download_http.is_none());
    }

    #[test]
    fn realistic_hawkbit_extras_are_ignored() {
        let body = r#"{"id":"7","deployment":{"download":"forced","update":"forced","maintenanceWindow":"available","chunks":[]},"actionHistory":{"status":"RUNNING","messages":["a","b"]},"_links":{}}"#;
        let d = parse(body).unwrap();
        assert!(d.chunks.is_empty());
    }

    #[test]
    fn modes_accept_only_the_three_lowercase_strings() {
        for (text, mode) in [
            ("forced", UpdateMode::Forced),
            ("attempt", UpdateMode::Attempt),
            ("skip", UpdateMode::Skip),
        ] {
            let d = parse(&doc("1", text, text, "")).unwrap();
            assert_eq!((d.download, d.update), (mode, mode));
        }
        for text in ["Forced", "", "install", "FORCED", "forced "] {
            assert!(is_malformed(&doc("1", text, "forced", "")), "{text:?}");
            assert!(is_malformed(&doc("1", "forced", text, "")), "{text:?}");
        }
    }

    #[test]
    fn mode_that_is_not_a_string_is_malformed() {
        let body = r#"{"id":"1","deployment":{"download":1,"update":"forced","chunks":[]}}"#;
        assert!(is_malformed(body));
        let body = r#"{"id":"1","deployment":{"download":null,"update":"forced","chunks":[]}}"#;
        assert!(is_malformed(body));
    }

    #[test]
    fn action_id_bounds() {
        for id in ["1", "0", "1234567890123456789", "9223372036854775807"] {
            assert_eq!(
                parse(&doc(id, "forced", "forced", "")).unwrap().id.as_str(),
                id
            );
        }
        for id in [
            "",
            "012",
            "00",
            "9223372036854775808",
            "9999999999999999999",
            "12345678901234567890",
            "12a",
            "-1",
            " 12",
            "12 ",
            "+1",
            "1.0",
        ] {
            assert_eq!(
                parse(&doc(id, "forced", "forced", "")),
                Err(DdiError::InvalidActionId),
                "{id:?}"
            );
        }
    }

    #[test]
    fn numeric_or_missing_id_is_malformed() {
        assert!(is_malformed(
            r#"{"id":12,"deployment":{"download":"forced","update":"forced","chunks":[]}}"#
        ));
        assert!(is_malformed(
            r#"{"deployment":{"download":"forced","update":"forced","chunks":[]}}"#
        ));
        assert!(is_malformed(
            r#"{"id":null,"deployment":{"download":"forced","update":"forced","chunks":[]}}"#
        ));
    }

    #[test]
    fn action_id_display_is_the_digits() {
        assert_eq!(format!("{}", ActionId::parse("7").unwrap()), "7");
    }

    #[test]
    fn duplicate_keys_are_malformed() {
        assert!(is_malformed(
            r#"{"id":"1","id":"2","deployment":{"download":"forced","update":"forced","chunks":[]}}"#
        ));
        let dup = art(
            "f",
            "1",
            &format!(r#"{{"sha256":"{HASH}","sha256":"{HASH}"}}"#),
            "{}",
        );
        assert!(is_malformed(&doc(
            "1",
            "forced",
            "forced",
            &chunk("v", "[]", &format!("[{dup}]"))
        )));
    }

    fn with_hashes(hashes: &str) -> Result<DeploymentBase, DdiError> {
        let a = art("f", "1", hashes, "{}");
        parse(&doc(
            "1",
            "forced",
            "forced",
            &chunk("v", "[]", &format!("[{a}]")),
        ))
    }

    #[test]
    fn missing_hashes_or_sha256_is_none() {
        let sha = |r: Result<DeploymentBase, DdiError>| r.unwrap().chunks[0].artifacts[0].sha256;
        assert_eq!(sha(with_hashes("{}")), None);
        assert_eq!(sha(with_hashes(r#"{"sha1":"aa","md5":"bb"}"#)), None);
        assert_eq!(sha(with_hashes("null")), None);
        let no_hashes = r#"{"id":"1","deployment":{"download":"forced","update":"forced","chunks":[{"version":"v","artifacts":[{"filename":"f","size":1}]}]}}"#;
        let d = parse(no_hashes).unwrap();
        assert_eq!(d.chunks[0].artifacts[0].sha256, None);
        assert!(d.chunks[0].metadata.is_empty());
    }

    #[test]
    fn sha256_accepts_either_case_and_rejects_bad_digests() {
        let lower = with_hashes(&format!(r#"{{"sha256":"{HASH}"}}"#)).unwrap();
        let upper = with_hashes(&format!(r#"{{"sha256":"{}"}}"#, HASH.to_uppercase())).unwrap();
        assert_eq!(lower, upper);
        assert_eq!(lower.chunks[0].artifacts[0].sha256, Some(hash_bytes()));
        let g = format!("g{}", &HASH[1..]);
        for bad in [
            &HASH[..63],
            &format!("{HASH}0"),
            "",
            &g,
            "\u{e9}0000000000000000000000000000000000000000000000000000000000000",
        ] {
            assert_eq!(
                with_hashes(&format!(r#"{{"sha256":"{bad}"}}"#)),
                Err(DdiError::InvalidDigest),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn size_must_be_a_non_negative_integer() {
        let with_size = |s: &str| {
            let a = art("f", s, "{}", "{}");
            parse(&doc(
                "1",
                "forced",
                "forced",
                &chunk("v", "[]", &format!("[{a}]")),
            ))
        };
        assert_eq!(
            with_size("18446744073709551615").unwrap().chunks[0].artifacts[0].size,
            u64::MAX
        );
        assert_eq!(with_size("0").unwrap().chunks[0].artifacts[0].size, 0);
        for bad in [
            "-1",
            "1.5",
            "1.0",
            r#""12""#,
            "null",
            "true",
            "18446744073709551616",
            "1e3",
        ] {
            assert!(
                matches!(with_size(bad), Err(DdiError::Malformed { .. })),
                "{bad}"
            );
        }
    }

    #[test]
    fn count_bounds_are_exact() {
        let a = good_art();
        let a_n = |n: usize| {
            doc(
                "1",
                "forced",
                "forced",
                &chunk("v", "[]", &format!("[{}]", join(&a, n))),
            )
        };
        assert_eq!(
            parse(&a_n(MAX_ARTIFACTS)).unwrap().chunks[0]
                .artifacts
                .len(),
            MAX_ARTIFACTS
        );
        assert!(is_malformed(&a_n(MAX_ARTIFACTS + 1)));

        let m = r#"{"key":"chip","value":"v"}"#;
        let m_n = |n: usize| {
            doc(
                "1",
                "forced",
                "forced",
                &chunk("v", &format!("[{}]", join(m, n)), "[]"),
            )
        };
        assert_eq!(
            parse(&m_n(MAX_METADATA)).unwrap().chunks[0].metadata.len(),
            MAX_METADATA
        );
        assert!(is_malformed(&m_n(MAX_METADATA + 1)));

        let c = good_chunk();
        let c_n = |n: usize| doc("1", "forced", "forced", &join(&c, n));
        assert_eq!(parse(&c_n(MAX_CHUNKS)).unwrap().chunks.len(), MAX_CHUNKS);
        assert!(is_malformed(&c_n(MAX_CHUNKS + 1)));
        assert!(parse(&c_n(0)).unwrap().chunks.is_empty());
    }

    fn cap_case(cap: usize, build: impl Fn(&str) -> String) {
        let at = "a".repeat(cap);
        assert!(parse(&build(&at)).is_ok(), "cap {cap} should pass");
        let over = "a".repeat(cap + 1);
        assert!(is_malformed(&build(&over)), "cap {cap} + 1 should fail");
    }

    #[test]
    fn string_caps_are_exact() {
        let wrap = |c: String| doc("1", "forced", "forced", &c);
        cap_case(crate::ota::ddi::wire::MAX_VERSION_BYTES, |s| {
            wrap(chunk(s, "[]", "[]"))
        });
        cap_case(crate::ota::ddi::wire::MAX_METADATA_VALUE_BYTES, |s| {
            wrap(chunk(
                "v",
                &format!(r#"[{{"key":"chip","value":"{s}"}}]"#),
                "[]",
            ))
        });
        cap_case(crate::ota::ddi::wire::MAX_FILENAME_BYTES, |s| {
            wrap(chunk("v", "[]", &format!("[{}]", art(s, "1", "{}", "{}"))))
        });
        cap_case(MAX_HREF_BYTES, |s| {
            let links = format!(r#"{{"download-http":{{"href":"{s}"}}}}"#);
            wrap(chunk(
                "v",
                "[]",
                &format!("[{}]", art("f", "1", "{}", &links)),
            ))
        });
    }

    #[test]
    fn nested_objects_in_array_form_are_malformed() {
        for body in [
            r#"{"id":"1","deployment":["forced","forced",[["os","1","n",[],[["f.bin",1,null,null]]]]]}"#,
            r#"{"id":"1","deployment":{"download":"forced","update":"forced","chunks":[["os","1","n",[],[]]]}}"#,
            r#"{"id":"1","deployment":{"download":"forced","update":"forced","chunks":[{"version":"v","metadata":[["k","v"]]}]}}"#,
            r#"{"id":"1","deployment":{"download":"forced","update":"forced","chunks":[{"version":"v","artifacts":[["f",1,null,null]]}]}}"#,
            r#"{"id":"1","deployment":{"download":"forced","update":"forced","chunks":[{"version":"v","artifacts":[{"filename":"f","size":1,"hashes":["x"]}]}]}}"#,
            r#"{"id":"1","deployment":{"download":"forced","update":"forced","chunks":[{"version":"v","artifacts":[{"filename":"f","size":1,"_links":["x"]}]}]}}"#,
            r#"{"id":"1","deployment":{"download":"forced","update":"forced","chunks":[{"version":"v","artifacts":[{"filename":"f","size":1,"_links":{"download-http":["x"]}}]}]}}"#,
        ] {
            assert!(is_malformed(body), "{body}");
        }
    }

    #[test]
    fn empty_href_is_malformed() {
        let links = r#"{"download-http":{"href":""}}"#;
        let a = art("f", "1", "{}", links);
        assert!(is_malformed(&doc(
            "1",
            "forced",
            "forced",
            &chunk("v", "[]", &format!("[{a}]"))
        )));
    }

    #[test]
    fn missing_required_fields_are_malformed() {
        for body in [
            r#"{"id":"1"}"#,
            r#"{"id":"1","deployment":{}}"#,
            r#"{"id":"1","deployment":{"update":"forced","chunks":[]}}"#,
            r#"{"id":"1","deployment":{"download":"forced","chunks":[]}}"#,
            r#"{"id":"1","deployment":{"download":"forced","update":"forced"}}"#,
            r#"{"id":"1","deployment":{"download":"forced","update":"forced","chunks":[{"part":"p","name":"n"}]}}"#,
            r#"{"id":"1","deployment":{"download":"forced","update":"forced","chunks":[{"version":"v","artifacts":[{"size":1}]}]}}"#,
            r#"{"id":"1","deployment":{"download":"forced","update":"forced","chunks":[{"version":"v","artifacts":[{"filename":"f"}]}]}}"#,
            r#"{"id":"1","deployment":{"download":"forced","update":"forced","chunks":[{"version":"v","metadata":[{"key":"k"}]}]}}"#,
            r#"{"id":"1","deployment":{"download":"forced","update":"forced","chunks":[{"version":"v","metadata":[{"value":"v"}]}]}}"#,
        ] {
            assert!(is_malformed(body), "{body}");
        }
    }

    #[test]
    fn null_required_fields_are_malformed() {
        for body in [
            r#"{"id":"1","deployment":null}"#,
            r#"{"id":"1","deployment":{"download":"forced","update":"forced","chunks":null}}"#,
            r#"{"id":"1","deployment":{"download":"forced","update":"forced","chunks":[{"version":null}]}}"#,
            r#"{"id":"1","deployment":{"download":"forced","update":"forced","chunks":[{"version":"v","metadata":null}]}}"#,
            r#"{"id":"1","deployment":{"download":"forced","update":"forced","chunks":[{"version":"v","artifacts":[{"filename":null,"size":1}]}]}}"#,
            r#"{"id":"1","deployment":{"download":"forced","update":"forced","chunks":[{"version":"v","artifacts":null}]}}"#,
        ] {
            assert!(is_malformed(body), "{body}");
        }
    }

    #[test]
    fn missing_metadata_and_artifacts_are_empty() {
        let body = r#"{"id":"1","deployment":{"download":"forced","update":"forced","chunks":[{"version":"v"}]}}"#;
        let c = &parse(body).unwrap().chunks[0];
        assert!(c.metadata.is_empty());
        assert!(c.artifacts.is_empty());
        assert!(!c.encrypted);
    }

    fn one_chunk(chunk_json: &str) -> Result<DeploymentBase, DdiError> {
        parse(&doc("1", "forced", "forced", chunk_json))
    }

    fn metadata_of(entries: &str) -> Result<Vec<Metadata>, DdiError> {
        one_chunk(&chunk("v", entries, "[]")).map(|mut d| d.chunks.remove(0).metadata)
    }

    fn md(key: &str, value: &str) -> Metadata {
        Metadata {
            key: key.into(),
            value: value.into(),
        }
    }

    #[test]
    fn encrypted_is_a_boolean_and_false_when_absent() {
        let enc = |v: &str| one_chunk(&format!(r#"{{"version":"v","encrypted":{v}}}"#));
        assert!(enc("true").unwrap().chunks[0].encrypted);
        assert!(!enc("false").unwrap().chunks[0].encrypted);
        assert!(!one_chunk(r#"{"version":"v"}"#).unwrap().chunks[0].encrypted);
        for bad in [r#""true""#, "1", "0", "null", "{}", "[]"] {
            assert!(matches!(enc(bad), Err(DdiError::Malformed { .. })), "{bad}");
        }
        assert!(is_malformed(&doc(
            "1",
            "forced",
            "forced",
            r#"{"version":"v","encrypted":true,"encrypted":false}"#
        )));
    }

    #[test]
    fn part_and_name_are_ignored_at_any_length_or_type() {
        let long = "x".repeat(1000);
        for c in [
            format!(r#"{{"part":"{long}","version":"v","name":"{long}"}}"#),
            String::from(r#"{"part":7,"version":"v","name":null}"#),
            String::from(r#"{"part":{"a":[1]},"version":"v","name":["n"]}"#),
        ] {
            assert_eq!(one_chunk(&c).unwrap().chunks[0].version, "v", "{c}");
        }
    }

    #[test]
    fn unknown_metadata_entries_are_skipped_uncounted_and_uncapped() {
        let long = "x".repeat(2000);
        let unknown = format!(
            r#"{{"key":"{long}","value":"{long}"}},{{"key":"k","value":"{long}"}},{{"key":"","value":""}},{{"key":"description","value":"v","targetVisible":true}}"#
        );
        let many = join(r#"{"key":"k","value":"v"}"#, MAX_METADATA * 3);
        let entries = format!(r#"[{unknown},{many},{{"key":"chip","value":"esp32c3"}}]"#);
        assert_eq!(metadata_of(&entries).unwrap(), [md("chip", "esp32c3")]);
        assert!(metadata_of(&format!("[{many}]")).unwrap().is_empty());
    }

    #[test]
    fn profile_metadata_keys_are_exact_and_case_sensitive() {
        let entries = r#"[{"key":"Chip","value":"a"},{"key":"CHIP","value":"a"},{"key":"chip ","value":"a"},{"key":"Downgrade","value":"allowed"},{"key":"downgrade_","value":"allowed"}]"#;
        assert!(metadata_of(entries).unwrap().is_empty());
    }

    #[test]
    fn duplicate_empty_and_odd_profile_entries_are_kept_for_admission() {
        let entries = r#"[{"key":"downgrade","value":"allowed"},{"key":"chip","value":""},{"key":"other","value":"x"},{"key":"downgrade","value":"Allowed"},{"key":"chip","value":"ESP32-C3"}]"#;
        assert_eq!(
            metadata_of(entries).unwrap(),
            [
                md("downgrade", "allowed"),
                md("chip", ""),
                md("downgrade", "Allowed"),
                md("chip", "ESP32-C3"),
            ]
        );
    }

    #[test]
    fn metadata_value_may_precede_the_key() {
        let entries = r#"[{"value":"esp32c6","key":"chip"},{"value":"allowed","key":"downgrade"}]"#;
        assert_eq!(
            metadata_of(entries).unwrap(),
            [md("chip", "esp32c6"), md("downgrade", "allowed")]
        );
    }

    #[test]
    fn profile_metadata_is_capped_and_counted_exactly() {
        let over = "a".repeat(MAX_METADATA_VALUE_BYTES + 1);
        for entries in [
            format!(r#"[{{"key":"chip","value":"{over}"}}]"#),
            format!(r#"[{{"value":"{over}","key":"downgrade"}}]"#),
        ] {
            assert!(
                matches!(metadata_of(&entries), Err(DdiError::Malformed { .. })),
                "{entries}"
            );
        }
        let filler = join(r#"{"key":"k","value":"v"}"#, MAX_METADATA);
        let known = |n: usize| {
            format!(
                "[{filler},{}]",
                join(r#"{"key":"downgrade","value":"allowed"}"#, n)
            )
        };
        assert_eq!(
            metadata_of(&known(MAX_METADATA)).unwrap().len(),
            MAX_METADATA
        );
        assert!(matches!(
            metadata_of(&known(MAX_METADATA + 1)),
            Err(DdiError::Malformed { .. })
        ));
    }

    #[test]
    fn skipped_metadata_entries_still_need_the_entry_shape() {
        for entries in [
            r#"[{"key":"k"}]"#,
            r#"[{"value":"v"}]"#,
            r#"[{"key":"k","value":1}]"#,
            r#"[{"key":"k","value":null}]"#,
            r#"[{"key":1,"value":"v"}]"#,
            r#"[{"key":"k","key":"k","value":"v"}]"#,
            r#"[["k","v"]]"#,
            r#"["k"]"#,
            r#"[null]"#,
            r#"{}"#,
        ] {
            assert!(
                matches!(metadata_of(entries), Err(DdiError::Malformed { .. })),
                "{entries}"
            );
        }
    }

    #[test]
    fn realistic_hawkbit_document_with_many_unknown_metadata_entries() {
        let unknown: Vec<String> = (0..20)
            .map(|i| match i % 4 {
                0 => format!(
                    r#"{{"key":"release.notes.{i}","value":"{}"}}"#,
                    "n".repeat(300)
                ),
                1 => format!(r#"{{"key":"{}{i}","value":"x"}}"#, "k".repeat(60)),
                2 => format!(r#"{{"key":"Chip{i}","value":"ESP32-C3 rev {i}"}}"#),
                _ => format!(r#"{{"key":"owner","value":"team {i}"}}"#),
            })
            .collect();
        let body = format!(
            r#"{{
  "id": "8",
  "deployment": {{
    "download": "forced",
    "update": "attempt",
    "maintenanceWindow": "available",
    "chunks": [ {{
      "metadata": [ {}, {{ "key": "chip", "value": "esp32c3" }}, {{ "key": "downgrade", "value": "allowed" }} ],
      "version": "1.4.0",
      "encrypted": false,
      "artifacts": [ {{
        "filename": "firmware.bin",
        "hashes": {{
          "sha1": "aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d",
          "md5": "5d41402abc4b2a76b9719d911017c592",
          "sha256": "{HASH}"
        }},
        "size": 1048576,
        "_links": {{
          "download": {{ "href": "https://ddi.example/DEFAULT/controller/v1/dev-1/softwaremodules/3/artifacts/firmware.bin" }},
          "download-http": {{ "href": "http://ddi.example/DEFAULT/controller/v1/dev-1/softwaremodules/3/artifacts/firmware.bin" }},
          "md5sum": {{ "href": "https://ddi.example/DEFAULT/controller/v1/dev-1/softwaremodules/3/artifacts/firmware.bin.MD5SUM" }},
          "md5sum-http": {{ "href": "http://ddi.example/DEFAULT/controller/v1/dev-1/softwaremodules/3/artifacts/firmware.bin.MD5SUM" }}
        }}
      }} ]
    }} ]
  }},
  "actionHistory": {{ "status": "RUNNING", "messages": [ "Assignment initiated by user 'admin'", "Update Server: Target retrieved update action" ] }}
}}"#,
            unknown.join(", ")
        );
        assert!(body.len() <= MAX_RESPONSE_BYTES, "{}", body.len());
        let d = parse(&body).unwrap();
        assert_eq!(d.id.as_str(), "8");
        assert_eq!(
            (d.download, d.update),
            (UpdateMode::Forced, UpdateMode::Attempt)
        );
        let c = &d.chunks[0];
        assert_eq!(c.version, "1.4.0");
        assert!(!c.encrypted);
        assert_eq!(
            c.metadata,
            [md("chip", "esp32c3"), md("downgrade", "allowed")]
        );
        let a = &c.artifacts[0];
        assert_eq!((a.filename.as_str(), a.size), ("firmware.bin", 1_048_576));
        assert_eq!(a.sha256, Some(hash_bytes()));
        assert_eq!(
            a.download_http.as_ref().unwrap().as_str(),
            "http://ddi.example/DEFAULT/controller/v1/dev-1/softwaremodules/3/artifacts/firmware.bin"
        );
    }

    fn padded(len: usize) -> String {
        let mut s = good();
        assert!(s.len() <= len);
        s.push_str(&" ".repeat(len - s.len()));
        s
    }

    #[test]
    fn body_size_limit_is_exact_and_checked_before_parsing() {
        assert!(parse(&padded(MAX_RESPONSE_BYTES)).is_ok());
        assert_eq!(
            parse(&padded(MAX_RESPONSE_BYTES + 1)),
            Err(DdiError::TooLarge)
        );
        let garbage = "x".repeat(MAX_RESPONSE_BYTES + 1);
        assert_eq!(parse(&garbage), Err(DdiError::TooLarge));
    }

    #[test]
    fn array_body_is_malformed_at_the_first_token() {
        assert_eq!(
            parse("[1]"),
            Err(DdiError::Malformed { line: 1, column: 1 })
        );
        assert_eq!(
            parse("\n\n  [{}]"),
            Err(DdiError::Malformed { line: 3, column: 3 })
        );
    }

    #[test]
    fn leading_whitespace_is_accepted() {
        assert!(parse(&format!(" \r\n\t{}", good())).is_ok());
    }

    #[test]
    fn trailing_garbage_is_malformed() {
        assert!(is_malformed(&format!("{} x", good())));
    }

    #[test]
    fn garbage_never_panics() {
        let corpus: [&[u8]; 10] = [
            b"",
            b"{",
            b"{\"id\":\"1\",\"deployment\":",
            b"\xff\xfe",
            b"{\"id\":\"\xff\"}",
            b"{\"id\":\"1\",\"deployment\":{\"download\":\"\xc3\",\"update\":\"x\"}}",
            b"{\"id\":\"1\"}\x00",
            b"{\"a\":[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[",
            b"null",
            b"\x00\x01\x02",
        ];
        for body in corpus {
            let _ = DeploymentBase::parse(body);
        }
        let nested = format!("{{\"a\":{}{}}}", "[".repeat(5000), "]".repeat(5000));
        assert!(DeploymentBase::parse(nested.as_bytes()).is_err());
    }

    #[test]
    fn debug_and_errors_never_contain_payload_text() {
        let links = r#"{"download-http":{"href":"http://u:secretpass@h/fw.bin"}}"#;
        let a = art("f", "1", "{}", links);
        let d = parse(&doc(
            "1",
            "forced",
            "forced",
            &chunk("v", "[]", &format!("[{a}]")),
        ))
        .unwrap();
        assert!(!format!("{d:?}").contains("secretpass"));
        assert!(!format!("{:?}", d.chunks[0].artifacts[0]).contains("secretpass"));

        let long_href = format!("secretpass{}", "x".repeat(MAX_HREF_BYTES));
        let long_links = format!(r#"{{"download-http":{{"href":"{long_href}"}}}}"#);
        let long = art("f", "1", "{}", &long_links);
        let bad = [
            doc("secretpass", "forced", "forced", ""),
            doc("1", "secretpass", "forced", ""),
            doc(
                "1",
                "forced",
                "forced",
                &chunk("v", "[]", &format!("[{long}]")),
            ),
            doc(
                "1",
                "forced",
                "forced",
                &chunk(
                    "v",
                    "[]",
                    &format!("[{}]", art("f", "1", r#"{"sha256":"secretpass"}"#, "{}")),
                ),
            ),
            String::from("secretpass"),
            String::from(r#"{"id":"1","secretpass":}"#),
        ];
        for body in bad {
            let e = parse(&body).unwrap_err();
            assert!(!format!("{e}").contains("secretpass"));
            assert!(!format!("{e:?}").contains("secretpass"));
        }
    }
}
