//! A handler payload that tolerates an absent body.

use bytes::Bytes;
use restate_sdk::serde::{InputMetadata, PayloadMetadata};

/// An optional handler payload: `null`, or no body at all.
///
/// A handler that takes `Json<Option<T>>` is sent `null` by the generated client and is
/// happy, but a bare `POST` to its ingress path — from `curl`, the `restate` CLI, or a
/// scheduler kick that has nothing to say — arrives with no body, and Restate refuses it
/// for the missing content type before the handler sees it. This wrapper takes both: an
/// empty body deserializes as `None`, and the input metadata says a content type is not
/// required.
///
/// A chain that arms itself is the usual reason to want it: the first tick has no argument
/// and is sent by hand, later ticks carry the generation they belong to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Optional<T>(pub Option<T>);

impl<T> Optional<T> {
    pub fn into_inner(self) -> Option<T> {
        self.0
    }
}

impl<T> From<Option<T>> for Optional<T> {
    fn from(value: Option<T>) -> Self {
        Self(value)
    }
}

impl<T> From<Optional<T>> for Option<T> {
    fn from(value: Optional<T>) -> Self {
        value.0
    }
}

impl<T: serde::Serialize> restate_sdk::serde::Serialize for Optional<T> {
    type Error = serde_json::Error;

    fn serialize(&self) -> Result<Bytes, Self::Error> {
        serde_json::to_vec(&self.0).map(Bytes::from)
    }
}

impl<T: serde::de::DeserializeOwned> restate_sdk::serde::Deserialize for Optional<T> {
    type Error = serde_json::Error;

    fn deserialize(bytes: &mut Bytes) -> Result<Self, Self::Error> {
        if bytes.is_empty() {
            Ok(Self(None))
        } else {
            serde_json::from_slice(bytes).map(Self)
        }
    }
}

impl<T> PayloadMetadata for Optional<T> {
    /// No schema: the payload is the inner type or nothing, and this wrapper cannot say
    /// what the inner type looks like without a `schemars` bound the SDK only asks for
    /// under its own feature. A handler that wants one in Restate's catalogue implements
    /// [`PayloadMetadata`] on its own named payload type, as the SDK documents.
    fn json_schema() -> Option<serde_json::Value> {
        None
    }

    /// The point of the type: Restate itself rejects a request with no content type when
    /// the input is required, so the absent body would never reach
    /// [`Deserialize::deserialize`](restate_sdk::serde::Deserialize::deserialize).
    fn input_metadata() -> InputMetadata {
        InputMetadata {
            accept_content_type: "*/*",
            is_required: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use restate_sdk::serde::{Deserialize as _, Serialize as _};

    use super::*;

    /// The case the wrapper exists for: a bare `POST` with no body, which a plain
    /// `Json<Option<u64>>` would fail to deserialize.
    #[test]
    fn an_absent_body_deserializes_as_nothing() {
        let mut empty = Bytes::new();

        assert_eq!(
            Optional::<u64>::deserialize(&mut empty).unwrap(),
            Optional(None)
        );
    }

    #[test]
    fn a_value_and_an_explicit_null_both_round_trip() {
        for value in [Optional(Some(7u64)), Optional(None)] {
            let mut bytes = value.serialize().unwrap();

            assert_eq!(Optional::<u64>::deserialize(&mut bytes).unwrap(), value);
        }
    }

    /// Restate is told a content type is not required, or it would refuse the bodyless
    /// request before the handler could read it as nothing.
    #[test]
    fn the_input_is_not_required() {
        assert!(!Optional::<u64>::input_metadata().is_required);
    }

    /// A body that is neither empty nor the inner type is a mistake, not a `None`: a typo
    /// in a hand-sent payload must fail the invocation rather than silently arm a tick
    /// with nothing.
    #[test]
    fn a_body_that_is_not_the_inner_type_fails() {
        let mut bytes = Bytes::from_static(br#""seven""#);

        assert!(Optional::<u64>::deserialize(&mut bytes).is_err());
    }
}
