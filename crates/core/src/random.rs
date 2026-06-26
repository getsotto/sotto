//! CSPRNG helpers (libsodium `randombytes`, via dryoc).

use dryoc::rng::copy_randombytes;

/// Fill `buf` with cryptographically secure random bytes.
pub fn fill(buf: &mut [u8]) {
    copy_randombytes(buf);
}

/// Generate `N` cryptographically secure random bytes.
pub fn bytes<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    copy_randombytes(&mut b);
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distinct_draws() {
        // Astronomically unlikely to collide; catches a stubbed/zeroed RNG.
        assert_ne!(bytes::<32>(), bytes::<32>());
    }
}
