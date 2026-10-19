//! An id that travels with every delta so that tracing tools can follow one
//! change from machine to machine. It is 16 bytes, in the W3C format. All
//! zeros means "not traced". That is also what you get when the id is missing,
//! for example from a machine that does not send one.

/// A 16-byte trace id. All zeros means not traced, or missing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct TraceId([u8; 16]);

impl TraceId {
    /// Makes an id from its 16 bytes.
    pub fn from_array(bytes: [u8; 16]) -> Self {
        TraceId(bytes)
    }

    /// The all-zero id, which means "not traced".
    pub fn none() -> Self {
        TraceId([0u8; 16])
    }

    /// True if this is the all-zero id.
    pub fn is_none(&self) -> bool {
        self.0 == [0u8; 16]
    }

    /// The 16 bytes of the id.
    pub fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    /// Reads an id from bytes. Exactly 16 bytes gives that id. Any other
    /// length, including none at all, gives `none()`.
    pub fn from_bytes(bytes: &[u8]) -> Self {
        if bytes.len() == 16 {
            let mut a = [0u8; 16];
            a.copy_from_slice(bytes);
            TraceId(a)
        } else {
            TraceId::none()
        }
    }

    /// The id as 32 lowercase hex characters.
    pub fn to_hex(&self) -> String {
        let mut s = String::with_capacity(32);
        for b in &self.0 {
            s.push_str(&format!("{b:02x}"));
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn none_is_none_and_hex_zeroes() {
        let n = TraceId::none();
        assert!(n.is_none());
        assert_eq!(n.to_hex(), "0".repeat(32));
        assert_eq!(TraceId::default(), n);
    }

    #[test]
    fn from_array_round_trips_hex_and_bytes() {
        let id = TraceId::from_array([
            0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55,
            0x66, 0x77,
        ]);
        assert!(!id.is_none());
        assert_eq!(id.to_hex(), "0123456789abcdef0011223344556677");
        assert_eq!(TraceId::from_bytes(id.as_bytes()), id);
    }

    #[test]
    fn from_bytes_rejects_wrong_length() {
        assert!(TraceId::from_bytes(&[]).is_none());
        assert!(TraceId::from_bytes(&[1, 2, 3]).is_none());
        assert!(TraceId::from_bytes(&[9u8; 15]).is_none());
        assert!(!TraceId::from_bytes(&[9u8; 16]).is_none());
    }
}
