//! Port of `src/fastbrowse/adapters/totp.py`: authenticator codes computed locally when a secret is typed.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use fastbrowse_core::models::SecretValue;
use sha1::Sha1;
use sha2::digest::{Digest, FixedOutputReset};
use sha2::{Sha256, Sha512};

/// The stored authenticator key cannot produce a code. The message never contains the key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TotpError(pub &'static str);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Algorithm {
    Sha1,
    Sha256,
    Sha512,
}

#[derive(Clone)]
struct Totp {
    secret: Vec<u8>,
    algorithm: Algorithm,
    digits: u32,
    period: u64,
}

fn hmac<D: Digest + FixedOutputReset>(key: &[u8], message: &[u8], block: usize) -> Vec<u8> {
    let mut key = if key.len() > block { D::digest(key).to_vec() } else { key.to_vec() };
    key.resize(block, 0);
    let inner: Vec<u8> = key.iter().map(|b| b ^ 0x36).collect();
    let outer: Vec<u8> = key.iter().map(|b| b ^ 0x5c).collect();
    let mut hasher = D::new();
    Digest::update(&mut hasher, &inner);
    Digest::update(&mut hasher, message);
    let inner_digest = hasher.finalize_reset();
    Digest::update(&mut hasher, &outer);
    Digest::update(&mut hasher, &inner_digest);
    hasher.finalize().to_vec()
}

impl Totp {
    fn at(&self, when: f64) -> String {
        let counter = ((when / self.period as f64).floor() as u64).to_be_bytes();
        let digest = match self.algorithm {
            Algorithm::Sha1 => hmac::<Sha1>(&self.secret, &counter, 64),
            Algorithm::Sha256 => hmac::<Sha256>(&self.secret, &counter, 64),
            Algorithm::Sha512 => hmac::<Sha512>(&self.secret, &counter, 128),
        };
        let offset = usize::from(digest[digest.len() - 1] & 0x0f);
        let truncated =
            u32::from_be_bytes([digest[offset], digest[offset + 1], digest[offset + 2], digest[offset + 3]])
                & 0x7fff_ffff;
        format!("{:0width$}", truncated % 10u32.pow(self.digits), width = self.digits as usize)
    }
}

/// `base64.b32decode(..., casefold=True)` after padding, or None for anything that is not base32.
fn base32(text: &str) -> Option<Vec<u8>> {
    let mut bits: u64 = 0;
    let mut count = 0;
    let mut out = Vec::new();
    let trimmed = text.trim_end_matches('=');
    if !text.len().is_multiple_of(8) || trimmed.contains('=') {
        return None;
    }
    for c in trimmed.chars() {
        let value = match c.to_ascii_uppercase() {
            c @ 'A'..='Z' => c as u64 - 'A' as u64,
            c @ '2'..='7' => c as u64 - '2' as u64 + 26,
            _ => return None,
        };
        bits = (bits << 5) | value;
        count += 5;
        if count >= 8 {
            count -= 8;
            out.push((bits >> count) as u8);
            bits &= (1 << count) - 1;
        }
    }
    Some(out)
}

/// `parse_qs(query, keep_blank_values=True)` for the names a key URI uses.
fn query_values(query: &str, name: &str) -> Vec<String> {
    query
        .split('&')
        .filter_map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            let decode = |text: &str| percent_decode(&text.replace('+', " "));
            (decode(key) == name).then(|| decode(value))
        })
        .collect()
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        if bytes[index] == b'%'
            && let (Some(high), Some(low)) =
                (bytes.get(index + 1).and_then(|b| hex(*b)), bytes.get(index + 2).and_then(|b| hex(*b)))
        {
            out.push((high * 16 + low) as u8);
            index += 3;
            continue;
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn parse_key(key: &str) -> Result<Totp, TotpError> {
    let parts = crate::options::SplitUrl::of(key.trim());
    let mut options: Vec<(&str, String)> = Vec::new();
    let mut secret = key.to_string();
    if !parts.scheme.is_empty() {
        if parts.scheme != "otpauth" || parts.netloc.to_lowercase() != "totp" {
            return Err(TotpError("only TOTP authenticator URIs are supported"));
        }
        for name in ["secret", "algorithm", "digits", "period"] {
            let values = query_values(parts.query, name);
            match values.as_slice() {
                [] => {}
                [one] => options.push((name, one.clone())),
                _ => return Err(TotpError("authenticator URI has repeated parameters")),
            }
        }
        secret = option(&options, "secret").unwrap_or_default();
    }
    let algorithm = match option(&options, "algorithm").unwrap_or_else(|| "SHA1".into()).to_lowercase().as_str() {
        "sha1" => Algorithm::Sha1,
        "sha256" => Algorithm::Sha256,
        "sha512" => Algorithm::Sha512,
        _ => return Err(TotpError("unsupported authenticator algorithm")),
    };
    let digits = match option(&options, "digits").as_deref().unwrap_or("6") {
        "6" => 6,
        "8" => 8,
        _ => return Err(TotpError("authenticator digits must be 6 or 8")),
    };
    let period_text = option(&options, "period").unwrap_or_else(|| "30".into());
    // Bounded before parsing, which a vault URI with thousands of digits would make overflow.
    let period = match period_text.parse::<u64>() {
        Ok(period)
            if period_text.len() <= 5
                && period_text.bytes().all(|b| b.is_ascii_digit())
                && 0 < period
                && period <= 86400 =>
        {
            period
        }
        _ => return Err(TotpError("authenticator period must be a whole number of seconds up to a day")),
    };
    let encoded: String = secret.split_whitespace().collect();
    if !encoded.is_ascii() {
        return Err(TotpError("authenticator secret is not valid base32"));
    }
    let padded = format!("{encoded}{}", "=".repeat((8 - encoded.len() % 8) % 8));
    let decoded = base32(&padded).ok_or(TotpError("authenticator secret is not valid base32"))?;
    if decoded.is_empty() {
        return Err(TotpError("authenticator secret is missing"));
    }
    Ok(Totp { secret: decoded, algorithm, digits, period })
}

fn option(options: &[(&str, String)], name: &str) -> Option<String> {
    options.iter().find(|(key, _)| *key == name).map(|(_, value)| value.clone())
}

/// The code `key` gives at unix time `when`.
pub fn totp_at(key: &str, when: f64) -> Result<String, TotpError> {
    Ok(parse_key(key)?.at(when))
}

fn now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0.0, |elapsed| elapsed.as_secs_f64())
}

/// Validate `key` now and return a value that computes a fresh code each time it is typed.
///
/// Wait for the next period when less than five seconds remain, capped at half the period, then read the clock again.
pub fn one_time_code(key: &str) -> Result<SecretValue, TotpError> {
    let authenticator = parse_key(key)?;
    let guard = (authenticator.period as f64 / 2.0).min(5.0);
    Ok(SecretValue::Computed(Arc::new(move || {
        let authenticator = authenticator.clone();
        Box::pin(async move {
            let mut when = now();
            let remaining = authenticator.period as f64 - when % authenticator.period as f64;
            if remaining < guard {
                tokio::time::sleep(std::time::Duration::from_secs_f64(remaining)).await;
                when = now();
            }
            authenticator.at(when)
        })
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    // RFC 6238's test vectors, the key being "12345678901234567890" repeated to each digest's length.
    #[test]
    fn codes_match_rfc_6238() {
        let sha1 = "otpauth://totp/x?secret=GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ&digits=8";
        assert_eq!(totp_at(sha1, 59.0).unwrap(), "94287082");
        assert_eq!(totp_at(sha1, 1111111109.0).unwrap(), "07081804");
        let sha256 =
            "otpauth://totp/x?secret=GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQGEZA&digits=8&algorithm=SHA256";
        assert_eq!(totp_at(sha256, 59.0).unwrap(), "46119246");
        assert_eq!(totp_at("GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ", 59.0).unwrap(), "287082");
    }

    #[test]
    fn an_unusable_key_is_refused_without_repeating_it() {
        assert_eq!(totp_at("not base32!", 0.0).unwrap_err().0, "authenticator secret is not valid base32");
        assert_eq!(
            totp_at("otpauth://hotp/x?secret=AA", 0.0).unwrap_err().0,
            "only TOTP authenticator URIs are supported"
        );
        assert_eq!(
            totp_at("otpauth://totp/x?secret=GE&period=0", 0.0).unwrap_err().0,
            "authenticator period must be a whole number of seconds up to a day"
        );
    }
}
