//! Length-bounded building blocks for the strict parsers: a string and a vector that refuse, never truncate.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;
use core::marker::PhantomData;

use serde::de::value::MapAccessDeserializer;
use serde::de::{Deserializer, Error, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::Deserialize;

use super::MAX_HREF_BYTES;

/// A string of at most `N` bytes; a longer one fails with fixed text that never quotes the input.
pub(super) struct BoundedString<const N: usize>(String);

impl<const N: usize> BoundedString<N> {
    pub(super) fn into_inner(self) -> String {
        self.0
    }
}

impl<'de, const N: usize> Deserialize<'de> for BoundedString<N> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct StrVisitor<const N: usize>;
        impl<const N: usize> Visitor<'_> for StrVisitor<N> {
            type Value = BoundedString<N>;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a string")
            }
            fn visit_str<E: Error>(self, v: &str) -> Result<Self::Value, E> {
                if v.len() > N {
                    return Err(E::custom("string too long"));
                }
                Ok(BoundedString(String::from(v)))
            }
        }
        deserializer.deserialize_str(StrVisitor::<N>)
    }
}

/// A nested JSON object.
///
/// serde's derived struct deserializer also accepts the positional array form, so every nested raw struct is wrapped in `Obj` to demand a map.
/// The map is forwarded to the derive, which keeps `serde_json` positions and duplicate-field detection.
pub(super) struct Obj<T>(pub(super) T);

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Obj<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct MapVisitor<T>(PhantomData<T>);
        impl<'de, T: Deserialize<'de>> Visitor<'de> for MapVisitor<T> {
            type Value = Obj<T>;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an object")
            }
            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
                T::deserialize(MapAccessDeserializer::new(map)).map(Obj)
            }
        }
        deserializer.deserialize_map(MapVisitor(PhantomData))
    }
}

/// A vector of at most `N` elements; one more fails with fixed text.
pub(super) struct BoundedVec<T, const N: usize>(Vec<T>);

impl<T, const N: usize> BoundedVec<T, N> {
    pub(super) fn into_inner(self) -> Vec<T> {
        self.0
    }
}

impl<T, const N: usize> Default for BoundedVec<T, N> {
    fn default() -> Self {
        BoundedVec(Vec::new())
    }
}

impl<'de, T: Deserialize<'de>, const N: usize> Deserialize<'de> for BoundedVec<T, N> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct SeqVisitor<T, const N: usize>(PhantomData<T>);
        impl<'de, T: Deserialize<'de>, const N: usize> Visitor<'de> for SeqVisitor<T, N> {
            type Value = BoundedVec<T, N>;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an array")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut items = Vec::with_capacity(seq.size_hint().unwrap_or(0).min(N));
                while items.len() < N {
                    match seq.next_element::<T>()? {
                        Some(item) => items.push(item),
                        None => return Ok(BoundedVec(items)),
                    }
                }
                // Full: probe without building the next element, so an over-long array never allocates for it.
                if seq.next_element::<IgnoredAny>()?.is_some() {
                    return Err(A::Error::custom("too many elements"));
                }
                Ok(BoundedVec(items))
            }
        }
        deserializer.deserialize_seq(SeqVisitor::<T, N>(PhantomData))
    }
}

/// Experimental: API may change before 1.0.
///
/// A link from a DDI document, 1 to [`MAX_HREF_BYTES`] bytes.
/// Not validated as a URL here: admission checks the origin.
///
/// A link may carry credentials or a token, so `Debug` prints `Href(<redacted>)` and there is deliberately no `Display`.
/// Read it with [`as_str`](Href::as_str) only to use it, never to log it.
#[derive(Clone, PartialEq, Eq)]
pub struct Href(String);

impl Href {
    /// The link text.
    /// Do not log it.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Takes the link text.
    /// Do not log it.
    pub fn into_string(self) -> String {
        self.0
    }

    /// `text` as a link, `None` when it is empty or longer than [`MAX_HREF_BYTES`].
    pub(super) fn from_text(text: String) -> Option<Self> {
        (!text.is_empty() && text.len() <= MAX_HREF_BYTES).then_some(Href(text))
    }
}

impl fmt::Debug for Href {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Href(<redacted>)")
    }
}

impl<'de> Deserialize<'de> for Href {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = BoundedString::<MAX_HREF_BYTES>::deserialize(deserializer)?.into_inner();
        if text.is_empty() {
            return Err(D::Error::custom("empty href"));
        }
        Ok(Href(text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::format;

    #[derive(Deserialize)]
    struct S {
        s: BoundedString<3>,
    }
    #[derive(Deserialize)]
    struct V {
        v: BoundedVec<u8, 2>,
    }
    #[derive(Deserialize)]
    struct H {
        h: Href,
    }

    #[test]
    fn string_at_the_cap_is_ok_and_one_over_fails() {
        assert_eq!(
            serde_json::from_str::<S>(r#"{"s":"abc"}"#)
                .unwrap()
                .s
                .into_inner(),
            "abc"
        );
        let err = serde_json::from_str::<S>(r#"{"s":"abcd"}"#).err().unwrap();
        assert!(format!("{err}").starts_with("string too long"));
        assert!(!format!("{err}").contains("abcd"));
    }

    #[test]
    fn string_cap_counts_bytes_not_chars() {
        assert!(serde_json::from_str::<S>("{\"s\":\"\u{e9}a\"}").is_ok());
        assert!(serde_json::from_str::<S>("{\"s\":\"\u{e9}\u{e9}\"}").is_err());
    }

    #[test]
    fn string_cap_applies_to_the_decoded_text() {
        assert!(serde_json::from_str::<S>(r#"{"s":"\u0041\u0041\u0041"}"#).is_ok());
        assert!(serde_json::from_str::<S>(r#"{"s":"\u0041\u0041\u0041\u0041"}"#).is_err());
    }

    #[test]
    fn vec_at_the_cap_is_ok_and_one_over_fails() {
        assert_eq!(
            serde_json::from_str::<V>(r#"{"v":[1,2]}"#)
                .unwrap()
                .v
                .into_inner(),
            [1, 2]
        );
        assert!(serde_json::from_str::<V>(r#"{"v":[]}"#).is_ok());
        let err = serde_json::from_str::<V>(r#"{"v":[1,2,3]}"#).err().unwrap();
        assert!(format!("{err}").starts_with("too many elements"));
    }

    #[test]
    fn obj_accepts_a_map_and_rejects_the_positional_array() {
        #[derive(Deserialize)]
        struct P {
            a: u8,
        }
        #[derive(Deserialize)]
        struct W {
            p: Obj<P>,
        }
        assert_eq!(
            serde_json::from_str::<W>(r#"{"p":{"a":1}}"#).unwrap().p.0.a,
            1
        );
        assert!(serde_json::from_str::<W>(r#"{"p":[1]}"#).is_err());
        assert!(serde_json::from_str::<W>(r#"{"p":{"a":1,"a":2}}"#).is_err());
    }

    #[test]
    fn vec_stops_before_deserializing_the_extra_element() {
        #[derive(Deserialize)]
        struct W {
            #[serde(rename = "v")]
            _items: BoundedVec<u8, 1>,
        }
        let err = serde_json::from_str::<W>(r#"{"v":[1,"not a u8"]}"#)
            .err()
            .unwrap();
        assert!(format!("{err}").starts_with("too many elements"));
    }

    #[test]
    fn vec_rejects_a_non_array() {
        assert!(serde_json::from_str::<V>(r#"{"v":null}"#).is_err());
        assert!(serde_json::from_str::<V>(r#"{"v":{}}"#).is_err());
    }

    #[test]
    fn href_rejects_empty_and_redacts_debug() {
        let err = serde_json::from_str::<H>(r#"{"h":""}"#).err().unwrap();
        assert!(format!("{err}").starts_with("empty href"));
        let h = serde_json::from_str::<H>(r#"{"h":"http://u:secretpass@h/x"}"#)
            .unwrap()
            .h;
        assert_eq!(format!("{h:?}"), "Href(<redacted>)");
        assert_eq!(h.as_str(), "http://u:secretpass@h/x");
        assert_eq!(h.into_string(), "http://u:secretpass@h/x");
    }
}
