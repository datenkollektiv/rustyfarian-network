//! The DDI cancel action resource.

use serde::Deserialize;

use super::bounded::Obj;
use super::deployment::ActionId;
use super::error::DdiError;
use super::parse_object;

/// Experimental: API may change before 1.0.
///
/// The hawkBit DDI v1 `cancelAction` resource: which action to stop.
/// Unknown fields are ignored; both ids are validated as action ids.
///
/// # Examples
///
/// ```
/// use juggler::ota::ddi::wire::CancelAction;
///
/// let cancel = CancelAction::parse(br#"{"id":"11","cancelAction":{"stopId":"10"}}"#).unwrap();
/// assert_eq!(cancel.id.as_str(), "11");
/// assert_eq!(cancel.stop_id.as_str(), "10");
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CancelAction {
    /// The id of the cancel action itself.
    pub id: ActionId,
    /// `cancelAction.stopId`: the id of the action to stop.
    pub stop_id: ActionId,
}

#[derive(Deserialize)]
struct RawCancel {
    id: alloc::string::String,
    #[serde(rename = "cancelAction")]
    cancel_action: Obj<RawCancelBody>,
}

#[derive(Deserialize)]
struct RawCancelBody {
    #[serde(rename = "stopId")]
    stop_id: alloc::string::String,
}

impl CancelAction {
    /// Parses a cancel body.
    ///
    /// # Errors
    ///
    /// [`DdiError::TooLarge`] for more than [`MAX_RESPONSE_BYTES`](super::MAX_RESPONSE_BYTES) bytes (checked before parsing),
    /// [`DdiError::Malformed`] for anything that is not a JSON object with `id` and `cancelAction.stopId` strings,
    /// [`DdiError::InvalidActionId`] when either id is not a canonical `i64` ([`ActionId::parse`]).
    pub fn parse(body: &[u8]) -> Result<Self, DdiError> {
        let raw: RawCancel = parse_object(body)?;
        Ok(CancelAction {
            id: ActionId::parse(&raw.id)?,
            stop_id: ActionId::parse(&raw.cancel_action.0.stop_id)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::format;

    #[test]
    fn parses_both_ids() {
        let c = CancelAction::parse(br#"{"id":"11","cancelAction":{"stopId":"9"}}"#).unwrap();
        assert_eq!(c.id.as_str(), "11");
        assert_eq!(c.stop_id.as_str(), "9");
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let c = CancelAction::parse(
            br#"{"id":"1","x":1,"cancelAction":{"stopId":"2","y":[]},"_links":{}}"#,
        )
        .unwrap();
        assert_eq!(c.stop_id.as_str(), "2");
    }

    #[test]
    fn both_ids_are_validated() {
        for body in [
            r#"{"id":"","cancelAction":{"stopId":"1"}}"#,
            r#"{"id":"1","cancelAction":{"stopId":"1a"}}"#,
            r#"{"id":"1","cancelAction":{"stopId":"12345678901234567890"}}"#,
            r#"{"id":"-1","cancelAction":{"stopId":"1"}}"#,
            r#"{"id":"1","cancelAction":{"stopId":"09"}}"#,
        ] {
            assert_eq!(
                CancelAction::parse(body.as_bytes()),
                Err(DdiError::InvalidActionId),
                "{body}"
            );
        }
    }

    #[test]
    fn array_body_is_malformed_at_the_first_token() {
        assert_eq!(
            CancelAction::parse(b" [{}]"),
            Err(DdiError::Malformed { line: 1, column: 2 })
        );
    }

    #[test]
    fn missing_or_mistyped_fields_are_malformed() {
        for body in [
            r#"{"id":"1"}"#,
            r#"{"cancelAction":{"stopId":"1"}}"#,
            r#"{"id":"1","cancelAction":{}}"#,
            r#"{"id":"1","cancelAction":null}"#,
            r#"{"id":1,"cancelAction":{"stopId":"1"}}"#,
            r#"{"id":"1","cancelAction":{"stopId":1}}"#,
            r#"{"id":"1","id":"2","cancelAction":{"stopId":"1"}}"#,
        ] {
            assert!(
                matches!(
                    CancelAction::parse(body.as_bytes()),
                    Err(DdiError::Malformed { .. })
                ),
                "{body}"
            );
        }
    }

    #[test]
    fn nested_object_in_array_form_is_malformed() {
        assert!(matches!(
            CancelAction::parse(br#"{"id":"1","cancelAction":["2"]}"#),
            Err(DdiError::Malformed { .. })
        ));
    }

    #[test]
    fn oversized_body_is_too_large() {
        let body = alloc::vec![b' '; crate::ota::ddi::wire::MAX_RESPONSE_BYTES + 1];
        assert_eq!(CancelAction::parse(&body), Err(DdiError::TooLarge));
    }

    #[test]
    fn errors_never_contain_payload_text() {
        let e = CancelAction::parse(br#"{"id":"secretpass","cancelAction":{"stopId":"1"}}"#)
            .unwrap_err();
        assert!(!format!("{e} {e:?}").contains("secretpass"));
    }
}
