use sha2::{Digest, Sha384};

pub const CHECKSUM_LEN: usize = 48;

pub fn checksum(bytes: &[u8]) -> [u8; CHECKSUM_LEN] {
    Sha384::digest(bytes).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vector() {
        // SHA-384 of "abc" (FIPS 180-2 test vector)
        let expected = "cb00753f45a35e8bb5a03d699ac65007272c32ab0eded1631a8b605a43ff5bed\
                        8086072ba1e7cc2358baeca134c825a7";
        assert_eq!(hex::encode(checksum(b"abc")), expected);
    }

    #[test]
    fn empty_input() {
        let expected = "38b060a751ac96384cd9327eb1b1e36a21fdb71114be07434c0cc7bf63f6e1da\
                        274edebfe76f65fbd51ad2f14898b95b";
        assert_eq!(hex::encode(checksum(b"")), expected);
    }
}
