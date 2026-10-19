//! Turns an `SdkError` into a response. This is the only place that does it.
//!
//! An engine answer is a response value and never a gRPC error status. That
//! rule is easy to break one call site at a time, so no other module may build
//! a `Failed` or a `Refused`.

use crate::pb;
use orvu_sdk::SdkError;

pub fn ack() -> pb::Response {
    pb::Response { result: Some(pb::response::Result::Ack(pb::Ack {})) }
}

pub fn value(v: pb::read_value::V) -> pb::Response {
    pb::Response {
        result: Some(pb::response::Result::Value(pb::ReadValue { v: Some(v) })),
    }
}

pub fn int(v: i64) -> pb::Response {
    value(pb::read_value::V::IntValue(v))
}

pub fn boolean(v: bool) -> pb::Response {
    value(pb::read_value::V::BoolValue(v))
}

pub fn opt_string(v: Option<String>) -> pb::Response {
    match v {
        Some(s) => value(pb::read_value::V::StringValue(s)),
        // Not the empty string. A client must be able to tell unset from empty.
        None => value(pb::read_value::V::Absent(pb::Absent {})),
    }
}

pub fn strings(v: Vec<String>) -> pb::Response {
    value(pb::read_value::V::Strings(pb::StringList { values: v }))
}

pub fn json(s: String) -> pb::Response {
    value(pb::read_value::V::Json(s))
}

pub fn pairs(v: Vec<(String, String)>) -> pb::Response {
    value(pb::read_value::V::Pairs(pb::PairList {
        pairs: v.into_iter().map(|(key, value)| pb::Pair { key, value }).collect(),
    }))
}

/// Each key with all its values, for a map that keeps clashing writes.
pub fn multi_pairs(v: Vec<(String, Vec<String>)>) -> pb::Response {
    value(pb::read_value::V::MultiPairs(pb::MultiPairList {
        pairs: v.into_iter().map(|(key, values)| pb::MultiPair { key, values }).collect(),
    }))
}

/// `qty` stays signed all the way through, because a quantity can really be
/// negative.
pub fn counted(v: Vec<(String, i64)>) -> pb::Response {
    value(pb::read_value::V::Counted(pb::CountedList {
        items: v.into_iter().map(|(item, qty)| pb::CountedItem { item, qty }).collect(),
    }))
}

/// The response for JSON text that did not parse. It lives here because only
/// this module may build a `Failed`.
pub fn bad_json(detail: String) -> pb::Response {
    failed(pb::Code::Json, detail)
}

/// The reply for a batch operation that was not run because the batch's
/// replies had already grown past the limit.
pub fn too_large() -> pb::Response {
    failed(
        pb::Code::TooLarge,
        "the batch's replies passed the size limit; this operation was not run".into(),
    )
}

/// The reply for a batch operation that was not run because the sidecar
/// started stopping.
pub fn stopping() -> pb::Response {
    failed(pb::Code::Engine, "the sidecar is stopping; this operation was not run".into())
}

/// The response for a search that could not be done, and why.
pub fn no_search(detail: String) -> pb::Response {
    failed(pb::Code::NoSearch, detail)
}

/// The response for an engine failure that has no more specific code. It
/// lives here because only this module may build a `Failed`.
pub fn engine_failed(detail: String) -> pb::Response {
    failed(pb::Code::Engine, detail)
}

fn failed(code: pb::Code, detail: String) -> pb::Response {
    pb::Response {
        result: Some(pb::response::Result::Failed(pb::Failed {
            code: code as i32,
            detail,
        })),
    }
}

/// The usual way to answer: `Ok` goes through `ok`, and `Err` goes through
/// `to_response`. Written this way, an `SdkError` cannot leave as a `Status`.
pub fn reply<T>(r: Result<T, SdkError>, ok: impl FnOnce(T) -> pb::Response) -> pb::Response {
    match r {
        Ok(v) => ok(v),
        Err(e) => to_response(e),
    }
}

/// Gets a handle, then runs the op on it. If either step fails, the
/// `SdkError` becomes a response.
pub async fn on<H, T, Fut>(
    handle: Result<H, SdkError>,
    op: impl FnOnce(H) -> Fut,
    ok: impl FnOnce(T) -> pb::Response,
) -> pb::Response
where
    Fut: std::future::Future<Output = Result<T, SdkError>>,
{
    match handle {
        Ok(h) => reply(op(h).await, ok),
        Err(e) => to_response(e),
    }
}

/// The full mapping from `SdkError` to a response. `Exhausted` becomes a
/// `Refused` value and not a gRPC status. That keeps "the rule held and your
/// state is intact" apart from "the call failed" in a language whose generated
/// client raises an exception on a non-OK status.
pub fn to_response(e: SdkError) -> pb::Response {
    let detail = e.to_string();
    match e {
        // A write a rule turned away when it came to be made, because the
        // object had changed since it was looked at, changed nothing.
        SdkError::Exhausted | SdkError::NotAllowed(_) => pb::Response {
            result: Some(pb::response::Result::Refused(pb::Refused { detail })),
        },
        SdkError::TypeMismatch { .. } => failed(pb::Code::TypeMismatch, detail),
        SdkError::ClassMismatch { .. } => failed(pb::Code::ClassMismatch, detail),
        SdkError::OutOfBounds { .. } => failed(pb::Code::OutOfBounds, detail),
        SdkError::Json(_) => failed(pb::Code::Json, detail),
        SdkError::Config(_) => failed(pb::Code::Config, detail),
        // The request check turns an empty mark value away before it gets
        // here. Should one still come through, the detail says what was
        // wrong with it.
        SdkError::EmptyValue => failed(pb::Code::Engine, detail),
        // A node that can no longer save gives a failure with the reason in
        // the detail, so a change it did not save is never answered as done.
        // A write the object turned away says why in the detail too.
        SdkError::Storage(_) | SdkError::Engine | SdkError::Transport(_) | SdkError::Write(_) => {
            failed(pb::Code::Engine, detail)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orvu_sdk::ConsistencyClass;

    fn failed_code(e: SdkError) -> pb::Code {
        match to_response(e).result {
            Some(pb::response::Result::Failed(f)) => f.code(),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn exhausted_is_a_refusal_value_and_never_a_failure() {
        let r = to_response(SdkError::Exhausted);
        assert!(matches!(r.result, Some(pb::response::Result::Refused(_))), "got {r:?}");
        assert!(!matches!(r.result, Some(pb::response::Result::Failed(_))));
    }

    #[test]
    fn every_other_variant_maps_to_its_specific_code() {
        let tm = SdkError::TypeMismatch { id: "x".into(), expected: "set" };
        assert_eq!(failed_code(tm), pb::Code::TypeMismatch);
        let cm = SdkError::ClassMismatch { id: "x".into(), declared: ConsistencyClass::Escrow };
        assert_eq!(failed_code(cm), pb::Code::ClassMismatch);
        assert_eq!(failed_code(SdkError::OutOfBounds { index: 3, len: 1 }), pb::Code::OutOfBounds);
        assert_eq!(failed_code(SdkError::Config("c".into())), pb::Code::Config);
        assert_eq!(failed_code(SdkError::Engine), pb::Code::Engine);
        let j = SdkError::Json(orvu_crdt::JsonError::EmptyPath);
        assert_eq!(failed_code(j), pb::Code::Json);
        let t = SdkError::Transport(orvu_transport::TransportError::Closed);
        assert_eq!(failed_code(t), pb::Code::Engine);
        let w = SdkError::Write(orvu_crdt::WriteError::NoFreshDot);
        assert_eq!(failed_code(w), pb::Code::Engine);
    }

    #[test]
    fn an_empty_mark_value_says_what_was_wrong() {
        match to_response(SdkError::EmptyValue).result {
            Some(pb::response::Result::Failed(f)) => {
                assert!(f.detail.contains("mark needs a value") && f.detail.contains("unformat"), "{}", f.detail);
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    // A write the object turned away reaches the client with the reason, so
    // the Python and TypeScript libraries raise an error that says it.
    #[test]
    fn a_write_the_object_turned_away_says_why() {
        for (why, says) in [
            (orvu_crdt::WriteError::NoFreshDot, "no change numbers left for this machine; it can only be read"),
            (orvu_crdt::WriteError::TooManyWriters, "already has changes from the most machines it takes"),
        ] {
            match to_response(SdkError::Write(why)).result {
                Some(pb::response::Result::Failed(f)) => {
                    assert_eq!(f.code(), pb::Code::Engine);
                    assert!(f.detail.contains("write refused") && f.detail.contains(says), "{}", f.detail);
                }
                other => panic!("expected Failed, got {other:?}"),
            }
        }
    }

    #[test]
    fn a_storage_failure_is_a_failure_that_says_why() {
        match to_response(SdkError::Storage("disk full".into())).result {
            Some(pb::response::Result::Failed(f)) => {
                assert_eq!(f.code(), pb::Code::Engine);
                assert!(f.detail.contains("cannot save changes") && f.detail.contains("disk full"), "{}", f.detail);
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_handle_error_in_on_is_a_response_not_a_panic_or_status() {
        let r = on(
            Err::<(), _>(SdkError::Exhausted),
            |()| async { Ok::<(), SdkError>(()) },
            |()| ack(),
        )
        .await;
        assert!(matches!(r.result, Some(pb::response::Result::Refused(_))));
    }
}
