//! Comparing SpiceDB revisions, read from ZedTokens, so the Watch loop can tell when a resumed stream has delivered
//! everything up to a revision read at the head (`SpiceDb::head_revision`).
//!
//! A ZedToken is base64 of a `DecodedZedToken` protobuf whose `v1` (field 3) holds the datastore's revision string
//! (field 1). Within one datastore revisions are ordered, and their strings take one of two shapes:
//!
//! - a DECIMAL (`1795557137917555507`, or `1795557137917555507.0000000001`): the in-memory datastore, and the shape
//!   MySQL, CockroachDB and Spanner write. Ordered as numbers.
//! - base64 of a Postgres snapshot (`xmin` field 1, `relative_xmax` field 2, the in-progress list after): the Postgres
//!   datastore. A snapshot S has seen every transaction that a head snapshot H had seen once `S.xmin >= H.xmax`: every
//!   transaction below S.xmin has finished, and every one H had seen is below H.xmax. That is conservative (S may have
//!   seen them all a little earlier) and sound.
//!
//! Anything else is incomparable, and the caller must not treat the stream as caught up on a guess.

use base64::{engine::general_purpose::STANDARD, Engine};
use std::cmp::Ordering;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Revision {
    /// The integer part and the fractional digits (trailing zeros removed), both without leading signs.
    Decimal(String, String),
    /// A Postgres snapshot's bounds.
    Snapshot { xmin: u64, xmax: u64 },
}

/// The revision a ZedToken names, or None for a token this module cannot read.
pub fn of_token(token: &str) -> Option<Revision> {
    let bytes = STANDARD.decode(token.trim()).ok()?;
    let v1 = field_bytes(&bytes, 3)?;
    let rev = std::str::from_utf8(field_bytes(v1, 1)?).ok()?;
    parse(rev)
}

/// A datastore revision string.
pub fn parse(rev: &str) -> Option<Revision> {
    let (int, frac) = rev.split_once('.').unwrap_or((rev, ""));
    if !int.is_empty()
        && int.bytes().all(|b| b.is_ascii_digit())
        && frac.bytes().all(|b| b.is_ascii_digit())
    {
        let int = int.trim_start_matches('0');
        let frac = frac.trim_end_matches('0');
        return Some(Revision::Decimal(int.to_string(), frac.to_string()));
    }
    let snap = STANDARD.decode(rev).ok()?;
    let xmin = field_varint(&snap, 1)?;
    let rel = field_varint(&snap, 2).unwrap_or(0) as i64;
    let xmax = if rel >= 0 {
        xmin.checked_add(rel as u64)?
    } else {
        return None;
    };
    Some(Revision::Snapshot { xmin, xmax })
}

/// Whether a stream at `through` has delivered everything up to `head`: Some(true) or Some(false), or None when the
/// two cannot be compared (different shapes, or an unreadable token).
pub fn covers(through: &Revision, head: &Revision) -> Option<bool> {
    match (through, head) {
        (Revision::Decimal(ti, tf), Revision::Decimal(hi, hf)) => {
            let ord = ti
                .len()
                .cmp(&hi.len())
                .then_with(|| ti.cmp(hi))
                .then_with(|| tf.cmp(hf));
            Some(ord != Ordering::Less)
        }
        (Revision::Snapshot { xmin, .. }, Revision::Snapshot { xmax, .. }) => Some(xmin >= xmax),
        _ => None,
    }
}

fn varint(b: &[u8], i: &mut usize) -> Option<u64> {
    let mut v = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *b.get(*i)?;
        *i += 1;
        v |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(v);
        }
    }
    None
}

/// Walk a protobuf message's top-level fields, calling `f(field, wire type, varint value, bytes)`.
fn walk<'a>(b: &'a [u8], mut f: impl FnMut(u64, u64, u64, &'a [u8]) -> bool) -> Option<()> {
    let mut i = 0;
    while i < b.len() {
        let key = varint(b, &mut i)?;
        let (field, wire) = (key >> 3, key & 7);
        let stop = match wire {
            0 => {
                let v = varint(b, &mut i)?;
                f(field, wire, v, &[])
            }
            1 => {
                i = i.checked_add(8)?;
                false
            }
            2 => {
                let n = usize::try_from(varint(b, &mut i)?).ok()?;
                let end = i.checked_add(n)?;
                let s = b.get(i..end)?;
                i = end;
                f(field, wire, 0, s)
            }
            5 => {
                i = i.checked_add(4)?;
                false
            }
            _ => return None,
        };
        if stop {
            return Some(());
        }
    }
    (i == b.len()).then_some(())
}

fn field_bytes(b: &[u8], want: u64) -> Option<&[u8]> {
    let mut out = None;
    walk(b, |field, wire, _, s| {
        if field == want && wire == 2 {
            out = Some(s);
            return true;
        }
        false
    })?;
    out
}

fn field_varint(b: &[u8], want: u64) -> Option<u64> {
    let mut out = None;
    walk(b, |field, wire, v, _| {
        if field == want && wire == 0 {
            out = Some(v);
            return true;
        }
        false
    })?;
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // Tokens as SpiceDB v1.56.2 returned them from the in-memory datastore.
    const MEM_A: &str = "Gh8KEzE3OTU1NTcxMzc5MTc1NTU1MDcSCGU2ZGQyYzM2";
    const MEM_B: &str = "Gh8KEzE3OTU1NTcxMzg0NTY5NTYxMTESCGU2ZGQyYzM2";

    /// A ZedToken around a revision string, as SpiceDB encodes one (v1, revision, a schema hash).
    fn tok(rev: &str) -> String {
        let mut v1 = vec![0x0a, rev.len() as u8];
        v1.extend_from_slice(rev.as_bytes());
        v1.extend_from_slice(&[0x12, 8]);
        v1.extend_from_slice(b"e6238e7f");
        let mut t = vec![0x1a, v1.len() as u8];
        t.extend_from_slice(&v1);
        STANDARD.encode(t)
    }

    // Revisions the Postgres datastore returned (track_commit_timestamp on): a fully consistent check's checked_at,
    // and two Watch positions after it.
    const PG_HEAD: &str = "COIe"; // xmin 3938
    const PG_THROUGH_OLD: &str = "COEeIOAeKKiU3pDnw8b1GA=="; // xmin 3937, txid 3936
    const PG_THROUGH_NEW: &str = "COIeIOEeKNjIzZznw8b1GA=="; // xmin 3938, txid 3937

    #[test]
    fn memory_tokens_order_as_numbers() {
        let a = of_token(MEM_A).unwrap();
        let b = of_token(MEM_B).unwrap();
        assert_eq!(covers(&b, &a), Some(true));
        assert_eq!(covers(&a, &b), Some(false));
        assert_eq!(covers(&a, &a), Some(true));
    }

    #[test]
    fn decimals_compare_by_magnitude_not_text() {
        let p = |s| parse(s).unwrap();
        assert_eq!(covers(&p("10"), &p("9")), Some(true));
        assert_eq!(covers(&p("9"), &p("10")), Some(false));
        assert_eq!(covers(&p("5.0000000002"), &p("5.0000000001")), Some(true));
        assert_eq!(covers(&p("5.1"), &p("5.10")), Some(true));
        assert_eq!(covers(&p("5"), &p("5.0000000001")), Some(false));
        assert_eq!(covers(&p("007"), &p("7")), Some(true));
    }

    #[test]
    fn postgres_snapshots_compare_by_xmin_against_xmax() {
        let head = of_token(&tok(PG_HEAD)).unwrap();
        assert_eq!(
            head,
            Revision::Snapshot {
                xmin: 3938,
                xmax: 3938
            }
        );
        assert_eq!(
            covers(&of_token(&tok(PG_THROUGH_OLD)).unwrap(), &head),
            Some(false)
        );
        assert_eq!(
            covers(&of_token(&tok(PG_THROUGH_NEW)).unwrap(), &head),
            Some(true)
        );
        // A head with transactions in flight (relative_xmax 5) is covered only once all of them have finished.
        let busy = Revision::Snapshot {
            xmin: 100,
            xmax: 105,
        };
        assert_eq!(
            covers(
                &Revision::Snapshot {
                    xmin: 104,
                    xmax: 104
                },
                &busy
            ),
            Some(false)
        );
        assert_eq!(
            covers(
                &Revision::Snapshot {
                    xmin: 105,
                    xmax: 105
                },
                &busy
            ),
            Some(true)
        );
    }

    #[test]
    fn unreadable_or_mixed_is_incomparable() {
        assert_eq!(of_token("not base64!"), None);
        assert_eq!(of_token(""), None);
        let mem = of_token(MEM_A).unwrap();
        let pg = of_token(&tok(PG_HEAD)).unwrap();
        assert_eq!(covers(&mem, &pg), None);
    }
}
