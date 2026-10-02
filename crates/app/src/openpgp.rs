//! Detached OpenPGP signatures (RFC 9580), only as far as checking yt-dlp's releases needs: a
//! version-4 signature of a binary document by an RSA key, over SHA-256 or SHA-512. The RSA
//! arithmetic is `ring`'s (the TLS library's own); this file only reads the packet.

/// An RSA public key: modulus and public exponent, big-endian.
pub struct RsaKey {
    pub n: &'static [u8],
    pub e: &'static [u8],
}

/// Whether `signature` (the binary `.sig` file, not armored) is `key`'s signature of `document`.
pub fn verify(key: &RsaKey, document: &[u8], signature: &[u8]) -> bool {
    let Some((hash, hashed, value)) = signed_parts(signature) else { return false };
    let params = match hash {
        8 => &ring::signature::RSA_PKCS1_2048_8192_SHA256,
        10 => &ring::signature::RSA_PKCS1_2048_8192_SHA512,
        _ => return false,
    };
    let Ok(hashed_len) = u32::try_from(hashed.len()) else { return false };
    // What was signed: the document, the signature's hashed part, then that part's length.
    let message = [document, hashed, &[4, 0xff], &hashed_len.to_be_bytes()].concat();
    // The value is an MPI, leading zeros dropped: back to the modulus' length.
    let Some(pad) = key.n.len().checked_sub(value.len()) else { return false };
    let padded = [&vec![0; pad][..], value].concat();
    ring::signature::RsaPublicKeyComponents { n: key.n, e: key.e }.verify(params, &message, &padded).is_ok()
}

/// (hash algorithm, hashed part, signature value) of a version-4 RSA signature of a binary
/// document; `None` for anything else.
fn signed_parts(packet: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let body = signature_body(packet)?;
    let u16_at = |at: usize| Some(usize::from(u16::from_be_bytes([*body.get(at)?, *body.get(at.checked_add(1)?)?])));
    // Version 4, a signature of a binary document (0x00), by an RSA key (1).
    if body.get(..3)? != [4, 0, 1] {
        return None;
    }
    let hash = *body.get(3)?;
    let hashed_end = 6 + u16_at(4)?;
    let hashed = body.get(..hashed_end)?;
    // The unhashed subpackets, then the first two bytes of the hash: skipped.
    let value_at = hashed_end + 2 + u16_at(hashed_end)? + 2;
    let bits = u16_at(value_at)?;
    let value = body.get(value_at + 2..value_at + 2 + bits.div_ceil(8))?;
    Some((hash, hashed, value))
}

/// The body of the signature packet (tag 2) at the start of `packet`, whatever its header format.
fn signature_body(packet: &[u8]) -> Option<&[u8]> {
    let (&first, rest) = packet.split_first()?;
    let (tag, len, rest) = if first & 0xc0 == 0xc0 {
        // New format: the length in one, two or five octets.
        let (&octet, rest) = rest.split_first()?;
        match octet {
            0..=191 => (first & 0x3f, usize::from(octet), rest),
            192..=223 => {
                let (&second, rest) = rest.split_first()?;
                (first & 0x3f, ((usize::from(octet) - 192) << 8) + usize::from(second) + 192, rest)
            }
            255 => (first & 0x3f, usize::try_from(u32::from_be_bytes(rest.get(..4)?.try_into().ok()?)).ok()?, rest.get(4..)?),
            _ => return None, // partial lengths: never a signature
        }
    } else if first & 0x80 != 0 {
        // Old format: the size of the length in the two low bits.
        let size = match first & 3 {
            0 => 1,
            1 => 2,
            2 => 4,
            _ => return None,
        };
        let len = rest.get(..size)?.iter().fold(0usize, |n, &b| n << 8 | usize::from(b));
        ((first >> 2) & 0x0f, len, rest.get(size..)?)
    } else {
        return None;
    };
    if tag != 2 {
        return None;
    }
    rest.get(..len)
}

/// `N` bytes from `2 × N` lower-case hexadecimal digits (keys written in the source).
pub const fn hex<const N: usize>(s: &str) -> [u8; N] {
    const fn nibble(c: u8) -> u8 {
        match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            _ => panic!("bad hex"),
        }
    }
    let b = s.as_bytes();
    assert!(b.len() == 2 * N);
    let mut out = [0u8; N];
    let mut i = 0;
    while i < N {
        out[i] = nibble(b[2 * i]) << 4 | nibble(b[2 * i + 1]);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// yt-dlp 2026.08.19's `SHA2-256SUMS` and its signature, as published.
    const SUMS: &[u8] = include_bytes!("../testdata/yt-dlp-2026.08.19-SHA2-256SUMS");
    const SIG: &[u8] = include_bytes!("../testdata/yt-dlp-2026.08.19-SHA2-256SUMS.sig");

    #[test]
    fn checks_yt_dlps_signed_checksums() {
        let key = &crate::ytdlp::SIGNING_KEY;
        assert!(verify(key, SUMS, SIG));
        let mut sums = SUMS.to_vec();
        sums[0] ^= 1;
        assert!(!verify(key, &sums, SIG), "another file");
        // Outside the signature's coverage, by design: the unhashed subpackets (hints such as the
        // issuer's key ID) and the hash's first two bytes (a quick check, not a proof).
        let hashed_end = 3 + 6 + usize::from(u16::from_be_bytes([SIG[7], SIG[8]]));
        let unhashed = usize::from(u16::from_be_bytes([SIG[hashed_end], SIG[hashed_end + 1]]));
        let unsigned = hashed_end + 2..hashed_end + 2 + unhashed + 2;
        for i in 0..SIG.len() {
            let mut sig = SIG.to_vec();
            sig[i] ^= 0x40;
            assert_eq!(verify(key, SUMS, &sig), unsigned.contains(&i), "byte {i} changed");
        }
        for len in 0..SIG.len() {
            assert!(!verify(key, SUMS, &SIG[..len]), "cut at {len}");
        }
        let other = RsaKey { n: &[0xc5; 512], e: &[1, 0, 1] };
        assert!(!verify(&other, SUMS, SIG), "another key");
    }

    /// The key written in `ytdlp` is the one keys.openpgp.org and yt-dlp's `public.key` (unchanged
    /// since 2023) give: its version-4 fingerprint, created 2023-02-27 (RSA 4096).
    #[test]
    fn the_embedded_key_is_yt_dlps() {
        use sha1::{Digest, Sha1};
        let key = &crate::ytdlp::SIGNING_KEY;
        let mpi = |v: &[u8]| {
            let bits = u16::try_from(v.len() * 8 - v[0].leading_zeros() as usize).unwrap();
            [&bits.to_be_bytes()[..], v].concat()
        };
        let body = [&[4u8, 0x63, 0xfb, 0xf0, 0x2e, 1][..], &mpi(key.n), &mpi(key.e)].concat();
        let len = u16::try_from(body.len()).unwrap().to_be_bytes();
        let fingerprint = Sha1::digest([&[0x99, len[0], len[1]][..], &body].concat());
        assert_eq!(crate::manager::checksum::hex(&fingerprint), "ac0cbbe6848d6a873464af4e57cf65933b5a7581");
    }

    #[test]
    fn reads_both_header_formats() {
        let body = [4u8, 0, 1, 10, 0, 0, 0, 0, 0xab, 0xcd, 0, 9, 0x01, 0xff];
        let old = [&[0x88, body.len() as u8][..], &body].concat();
        let new = [&[0xc2, body.len() as u8][..], &body].concat();
        for packet in [old, new] {
            let (hash, hashed, value) = signed_parts(&packet).unwrap();
            assert_eq!((hash, hashed.len(), value), (10, 6, &[0x01, 0xff][..]));
        }
        assert!(signed_parts(&[0xc6, 1, 4]).is_none(), "not a signature packet");
    }
}
