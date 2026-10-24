//! Base64 as Python's `base64` module writes and reads it.
//!
//! Writing is the standard alphabet with padding, what a browser's `atob` reads. Reading is `b64decode` without
//! `validate`, which pydantic's `Base64Bytes` uses: characters outside the alphabet are skipped, and padding ends the
//! input only when nothing of the alphabet follows it.

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for (index, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            if index <= chunk.len() {
                out.push(ALPHABET[(n >> shift & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn sextet(byte: u8) -> Option<u8> {
    ALPHABET.iter().position(|&a| a == byte).map(|p| p as u8)
}

/// `base64.b64decode(text)`, with its error messages.
pub fn decode(text: &str) -> Result<Vec<u8>, String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    let mut quad = 0;
    let mut pads = 0;
    let mut left: u32 = 0;
    let mut data_chars = 0usize;
    for (index, &byte) in bytes.iter().enumerate() {
        if byte == b'=' {
            pads += 1;
            if quad >= 2 && quad + pads >= 4 && !bytes[index + 1..].iter().any(|&b| sextet(b).is_some()) {
                return Ok(out);
            }
            continue;
        }
        let Some(value) = sextet(byte) else { continue };
        pads = 0;
        data_chars += 1;
        let value = u32::from(value);
        match quad {
            0 => left = value,
            1 => {
                out.push(((left << 2) | (value >> 4)) as u8);
                left = value & 0x0f;
            }
            2 => {
                out.push(((left << 4) | (value >> 2)) as u8);
                left = value & 0x03;
            }
            _ => {
                out.push(((left << 6) | value) as u8);
                left = 0;
            }
        }
        quad = (quad + 1) % 4;
    }
    match quad {
        0 => Ok(out),
        1 => Err(format!(
            "Invalid base64-encoded string: number of data characters ({data_chars}) cannot be 1 more than a multiple of 4"
        )),
        _ => Err("Incorrect padding".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decoding_follows_cpython() {
        for (input, expected) in [
            ("aGVsbG8=", Ok(b"hello".to_vec())),
            ("aGVsbG8", Err("Incorrect padding")),
            ("aGVs bG8=", Ok(b"hello".to_vec())),
            ("aGVsbG8=junk!", Err("Incorrect padding")),
            ("aGVsbG8=ju", Err("(9)")),
            ("aGVsbA=x=", Ok(b"hell\x0c".to_vec())),
            ("aGVsbA=", Err("Incorrect padding")),
            ("=aGVs", Ok(b"hel".to_vec())),
            ("aGVsbG8=Zg==", Err("(9)")),
            ("aGVsbG8=!", Ok(b"hello".to_vec())),
            ("aGVsbG8=a", Ok(b"hello\x1a".to_vec())),
            ("a===", Err("(1)")),
            ("", Ok(vec![])),
            ("+/8=", Ok(vec![0xfb, 0xff])),
        ] {
            match (decode(input), expected) {
                (Ok(got), Ok(want)) => assert_eq!(got, want, "{input}"),
                (Err(got), Err(want)) => assert!(got.contains(want), "{input}: {got}"),
                (got, want) => panic!("{input}: {got:?} vs {want:?}"),
            }
        }
    }

    #[test]
    fn encoding_is_the_standard_alphabet() {
        assert_eq!(encode(b"hello"), "aGVsbG8=");
        assert_eq!(encode(&[0xfb, 0xff]), "+/8=");
        assert_eq!(encode(b""), "");
    }
}
