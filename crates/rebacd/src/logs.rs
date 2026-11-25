//! WHAT A LOG LINE MAY SAY ABOUT A FAILURE: its kind, never its text. A database's or SpiceDB's message can quote the
//! value it refused (a key, a subject, an object id), and a log line leaves the machine for whoever collects it. So a
//! line carries a bounded code — `postgres:<SQLSTATE>`, `grpc:<code>`, `pool`, `io` or `other` — and the full text
//! stays where it already is: returned to the caller, or recorded on the queue row (`rebac.refresh.last_error`).

use std::error::Error as StdError;

/// The kind of the first error in the chain this daemon can name.
#[must_use]
pub fn kind(e: &(dyn StdError + 'static)) -> String {
    std::iter::successors(Some(e), |&err| err.source())
        .find_map(classify)
        .unwrap_or_else(|| "other".into())
}

/// The same for an `anyhow::Error`, which is not itself a `std::error::Error`.
#[must_use]
pub fn kind_of(e: &anyhow::Error) -> String {
    e.chain()
        .find_map(classify)
        .unwrap_or_else(|| "other".into())
}

/// The kind of this one error, when it is one the daemon can name.
fn classify(err: &(dyn StdError + 'static)) -> Option<String> {
    if let Some(pg) = err.downcast_ref::<tokio_postgres::Error>() {
        return Some(pg.code().map_or_else(
            || "postgres".into(),
            |code| format!("postgres:{}", code.code()),
        ));
    }
    if let Some(st) = err.downcast_ref::<tonic::Status>() {
        return Some(format!("grpc:{:?}", st.code()));
    }
    if err.is::<deadpool_postgres::PoolError>() {
        return Some("pool".into());
    }
    if err.is::<std::io::Error>() {
        return Some("io".into());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_the_kind_never_the_text() {
        let st = tonic::Status::failed_precondition(
            "object definition `container` not found: acme/secret-space",
        );
        let e = anyhow::Error::new(st).context("lookup");
        let k = kind_of(&e);
        assert_eq!(k, "grpc:FailedPrecondition");
        assert!(!k.contains("secret"));
        assert_eq!(
            kind_of(&anyhow::anyhow!("anything at all: priya@acme")),
            "other"
        );
        let io = std::io::Error::other("connection reset by 10.0.0.1");
        assert_eq!(kind(&io), "io");
    }
}
