//! Makes sort keys for keeping items in an order you choose. Each key is a
//! short string built to sort strictly between two existing keys, so an item
//! can always be put between two others without touching any other key. The
//! technique is called fractional indexing. The tree uses these keys to order a
//! node's children.
//!
//! The alphabet is in ascending ASCII order, so comparing two keys byte by
//! byte, the way strings normally compare, gives the intended order. Nothing
//! needs a custom comparison. A generated key never ends in the lowest digit,
//! `'0'`. That is what guarantees a new key always fits between any two keys
//! made earlier.
//!
//! `between` promises `a < result < b` only for keys it could have made itself:
//! alphabet characters only, not empty, and not ending in `'0'`. The whole
//! scheme depends on that last rule. Every key this crate makes comes from
//! `between`, so all of them qualify. A key with bytes outside the alphabet can
//! only appear if a peer sends a made-up key and it is merged into local state.
//! Such a key can make the result sort outside the bounds that were asked for,
//! because `digit` treats those bytes as 0. Every machine still works out the
//! same result from the same input, so machines cannot end up disagreeing, and
//! it cannot cause a panic or a loop in the tree. The worst outcome is siblings
//! in an unexpected order.

const ALPHABET: &[u8; 62] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
const BASE: u8 = ALPHABET.len() as u8;

/// The place of an ASCII byte in the alphabet, or 0 for any byte that is not in
/// it. A key merged from a badly behaved peer is just data and must not cause a
/// panic. Falling back to 0 is what lets `between` return a key for any bytes
/// at all. It is also why the `a < result < b` promise only covers keys made of
/// alphabet characters, as the comment at the top of this file explains. A byte
/// outside the alphabet, such as one below `'0'` or above `'z'`, counts as the
/// same digit as `'0'`, so working through the digits no longer matches the
/// order of the raw bytes.
fn digit(b: u8) -> u8 {
    match b {
        b'0'..=b'9' => b - b'0',
        b'A'..=b'Z' => b - b'A' + 10,
        b'a'..=b'z' => b - b'a' + 36,
        _ => 0,
    }
}

/// Returns a key that sorts strictly between `a` and `b`. Pass `None` for `a`
/// when there is no lower bound, and `None` for `b` when there is no upper
/// bound. If `a >= b` no key can sit between them, so `b` is ignored and the
/// key lands after `a`.
pub(crate) fn between(a: Option<&str>, b: Option<&str>) -> String {
    let b = match (a, b) {
        (Some(x), Some(y)) if x >= y => None,
        _ => b,
    };
    let mut out = String::new();
    let mut i = 0usize;
    let mut bounded = true; // true while `b` still limits the digits we emit
    loop {
        let da = a.and_then(|s| s.as_bytes().get(i)).map(|&c| digit(c)).unwrap_or(0);
        let db = if bounded {
            match b.and_then(|s| s.as_bytes().get(i)) {
                Some(&c) => digit(c),
                None => BASE,
            }
        } else {
            BASE
        };
        if db > da + 1 {
            out.push(ALPHABET[((da + db) / 2) as usize] as char);
            return out;
        }
        if db == da + 1 {
            // The digit we are about to emit is already strictly below `b`'s
            // digit here, so `b` cannot limit anything after this position.
            bounded = false;
        }
        out.push(ALPHABET[da as usize] as char);
        i += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Every key the generator makes must be non-empty and must never end in the
    // lowest digit. The whole scheme depends on that.
    fn assert_wellformed(k: &str) {
        assert!(!k.is_empty(), "a generated key is never empty");
        assert!(!k.ends_with('0'), "a generated key never ends in the minimum digit: {k}");
    }

    #[test]
    fn generates_a_key_from_nothing() {
        let k = between(None, None);
        assert_wellformed(&k);
    }

    #[test]
    fn orders_against_an_open_boundary() {
        let mid = between(None, None);
        let before = between(None, Some(&mid));
        let after = between(Some(&mid), None);
        assert_wellformed(&before);
        assert_wellformed(&after);
        assert!(before < mid, "{before} < {mid}");
        assert!(mid < after, "{mid} < {after}");
    }

    #[test]
    fn fits_strictly_between_two_keys() {
        let a = between(None, None);
        let c = between(Some(&a), None);
        let b = between(Some(&a), Some(&c));
        assert_wellformed(&b);
        assert!(a < b && b < c, "{a} < {b} < {c}");
    }

    #[test]
    fn repeated_insertion_into_the_same_gap_stays_ordered() {
        // This is what breaks if the step that carries on to the next digit is
        // wrong. Keep inserting into the same gap, check every new key sorts
        // strictly inside it, and check at the end that all the keys are
        // different.
        let left = between(None, None);
        let right = between(Some(&left), None);
        let mut keys = vec![left.clone(), right.clone()];
        let mut hi = right;
        for _ in 0..64 {
            let k = between(Some(&left), Some(&hi));
            assert_wellformed(&k);
            assert!(left < k && k < hi, "{left} < {k} < {hi}");
            keys.push(k.clone());
            hi = k;
        }
        let mut sorted = keys.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), keys.len(), "every generated key is distinct");
    }

    #[test]
    fn repeated_appending_stays_ordered() {
        let mut prev = between(None, None);
        for _ in 0..64 {
            let next = between(Some(&prev), None);
            assert_wellformed(&next);
            assert!(prev < next, "{prev} < {next}");
            prev = next;
        }
    }

    #[test]
    fn a_cleared_bound_stops_constraining_the_key() {
        // The `bounded` flag is what keeps generated keys short. Once the digit
        // just emitted is strictly below `b`'s, `b` must stop limiting the
        // rest. Without the flag, the low digits at the end of `b` get copied
        // into the result, and this call would return "10000V" instead of "1V".
        // This is the test that fails if the flag is removed.
        let k = between(Some("1"), Some("20001"));
        assert_wellformed(&k);
        assert!("1" < k.as_str() && k.as_str() < "20001", "1 < {k} < 20001");
        assert!(k.len() <= 2, "keys stay compact, got {k:?} ({} bytes)", k.len());
    }

    #[test]
    fn non_ordered_input_lands_after_the_left_key_without_looping() {
        // No key can sort between two equal keys, so `b` is ignored and the
        // result lands after `a`.
        let a = between(None, None);
        let k = between(Some(&a), Some(&a));
        assert_wellformed(&k);
        assert!(k > a, "{k} > {a}");

        let hi = between(Some(&a), None);
        let swapped = between(Some(&hi), Some(&a)); // the bounds are the wrong way round, so a >= b
        assert_wellformed(&swapped);
        assert!(swapped > hi, "{swapped} > {hi}");
    }

    #[test]
    fn empty_keys_terminate() {
        // move_node_at passes None, not Some(""), for a neighbour that has no
        // position, so it never makes up an empty key itself. But "" can still
        // get here. A peer can send it as a stored key, which is then merged
        // and passed on as it is. It is also the sort key children() uses for a
        // sibling with no position. So the generator must accept "" as input
        // without looping forever.
        let k = between(Some(""), Some(""));
        assert_wellformed(&k);
        assert!(k.as_str() > "", "a generated key sorts after the empty key");
        let after_empty = between(Some(""), None);
        assert_wellformed(&after_empty);
    }

    #[test]
    fn hostile_non_alphabet_keys_stay_total_and_deterministic() {
        // The known limit: a made-up key with bytes outside the alphabet can
        // make the result sort outside the bounds asked for, because digit()
        // treats unknown bytes as 0. So this checks only what is promised. The
        // call returns, it gives the same answer every time, and the key it
        // makes is well formed. It does not check that the result sorts between
        // its arguments.
        let hostile = ["!", "{", "é", "🙂", "!{"];
        for h in &hostile {
            let k1 = between(Some(h), None);
            let k2 = between(Some(h), None);
            assert_wellformed(&k1);
            assert_eq!(k1, k2, "deterministic with hostile left bound {h:?}");
            let k3 = between(None, Some(h));
            let k4 = between(None, Some(h));
            assert_wellformed(&k3);
            assert_eq!(k3, k4, "deterministic with hostile right bound {h:?}");
        }
        let both1 = between(Some("!"), Some("{"));
        let both2 = between(Some("!"), Some("{"));
        assert_wellformed(&both1);
        assert_eq!(both1, both2, "deterministic with both bounds hostile");
    }
}
