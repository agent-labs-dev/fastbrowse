//! Authenticator codes computed locally when a secret provider is awaited (`adapters/totp.py`).

use std::fmt;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fastbrowse_core::BoxFuture;
use fastbrowse_core::models::SecretValue;
use ring::hmac;

use crate::urlsplit::urlsplit;

/// The stored authenticator key cannot produce a code. The message never contains the key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TotpError(&'static str);

impl fmt::Display for TotpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for TotpError {}

/// What a clock read returns: unix seconds, as Python's `time.time`.
pub type Clock = Arc<dyn Fn() -> f64 + Send + Sync>;
/// What waits out the end of a period, as Python's `asyncio.sleep`.
pub type Sleep = Arc<dyn Fn(f64) -> BoxFuture<'static, ()> + Send + Sync>;

#[derive(Clone, Copy)]
enum Algorithm {
    Sha1,
    Sha256,
    Sha512,
}

struct Totp {
    secret: Vec<u8>,
    algorithm: Algorithm,
    digits: u32,
    period: u32,
}

impl Totp {
    fn at(&self, when: f64) -> String {
        // Python packs `int(when // period)` as an unsigned 64-bit counter; a time before the epoch has no code.
        let counter = when.div_euclid(f64::from(self.period)) as u64;
        let algorithm = match self.algorithm {
            Algorithm::Sha1 => hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY,
            Algorithm::Sha256 => hmac::HMAC_SHA256,
            Algorithm::Sha512 => hmac::HMAC_SHA512,
        };
        let tag = hmac::sign(&hmac::Key::new(algorithm, &self.secret), &counter.to_be_bytes());
        let digest = tag.as_ref();
        let offset = usize::from(digest[digest.len() - 1] & 0x0F);
        let window = [digest[offset], digest[offset + 1], digest[offset + 2], digest[offset + 3]];
        let truncated = u32::from_be_bytes(window) & 0x7FFF_FFFF;
        let width = self.digits as usize;
        format!("{:0width$}", truncated % 10u32.pow(self.digits))
    }
}

/// `str.isspace`, which `str.split()` and `str.strip()` break on: Unicode whitespace plus the ASCII separators.
fn py_space(c: char) -> bool {
    c.is_whitespace() || ('\x1c'..='\x1f').contains(&c)
}

/// `urllib.parse.unquote`: `%XX` escapes as UTF-8 bytes, and a byte run that does not decode becomes U+FFFD.
fn unquote(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(high), Some(low)) = (hex_value(bytes[i + 1]), hex_value(bytes[i + 2]))
        {
            out.push(high << 4 | low);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    char::from(byte).to_digit(16).map(|digit| digit as u8)
}

/// `parse_qs(query, keep_blank_values=True)`: pairs split on `&` alone, `+` read as a space, values in order.
fn parse_qs(query: &str) -> Vec<(String, Vec<String>)> {
    let mut parsed: Vec<(String, Vec<String>)> = Vec::new();
    for piece in query.split('&').filter(|piece| !piece.is_empty()) {
        let (name, value) = piece.split_once('=').unwrap_or((piece, ""));
        let name = unquote(&name.replace('+', " "));
        let value = unquote(&value.replace('+', " "));
        match parsed.iter_mut().find(|(known, _)| *known == name) {
            Some((_, values)) => values.push(value),
            None => parsed.push((name, vec![value])),
        }
    }
    parsed
}

/// Whether CPython's `urlsplit` raises on this netloc: an unmatched bracket, or a bracketed host that is neither an
/// IPv6 address nor an IPvFuture literal.
fn invalid_netloc(netloc: &str) -> bool {
    match (netloc.contains('['), netloc.contains(']')) {
        (false, false) => false,
        (true, true) => {
            let host = netloc.split_once('[').map_or("", |(_, rest)| rest.split_once(']').map_or(rest, |(h, _)| h));
            if let Some(future) = host.strip_prefix('v') {
                // `\Av[a-fA-F0-9]+\..+\z`.
                let version = future.chars().take_while(char::is_ascii_hexdigit).count();
                return !(version > 0 && future[version..].strip_prefix('.').is_some_and(|rest| !rest.is_empty()));
            }
            // `ipaddress.ip_address` takes a scope after `%`; an IPv4 address is refused in brackets.
            let address = host.split_once('%').map_or(host, |(address, _)| address);
            address.parse::<std::net::Ipv6Addr>().is_err()
        }
        _ => true,
    }
}

/// `base64.b32decode(encoded, casefold=True)` as CPython 3.13 writes it: unused trailing bits are not checked.
fn b32decode(encoded: &[u8]) -> Option<Vec<u8>> {
    if !encoded.len().is_multiple_of(8) {
        return None;
    }
    let upper = encoded.to_ascii_uppercase();
    let stripped = upper.iter().rposition(|&b| b != b'=').map_or(&upper[..0], |last| &upper[..=last]);
    let padchars = upper.len() - stripped.len();
    let mut decoded = Vec::with_capacity(stripped.len() * 5 / 8 + 5);
    let mut acc: u64 = 0;
    for quanta in stripped.chunks(8) {
        acc = 0;
        for &c in quanta {
            let digit = match c {
                b'A'..=b'Z' => c - b'A',
                b'2'..=b'7' => c - b'2' + 26,
                _ => return None,
            };
            acc = (acc << 5) + u64::from(digit);
        }
        decoded.extend_from_slice(&acc.to_be_bytes()[3..]);
    }
    if ![0, 1, 3, 4, 6].contains(&padchars) {
        return None;
    }
    if padchars > 0 && !decoded.is_empty() {
        acc <<= 5 * padchars;
        let last = &acc.to_be_bytes()[3..];
        let leftover = (43 - 5 * padchars) / 8;
        let start = decoded.len() - 5;
        decoded.truncate(start);
        decoded.extend_from_slice(&last[..leftover]);
    }
    Some(decoded)
}

fn parse_key(key: &str) -> Result<Totp, TotpError> {
    let (scheme, netloc, _, query, _) = urlsplit(key.trim_matches(py_space));
    if invalid_netloc(&netloc) {
        // URL parser errors can quote the input, which here contains a secret.
        return Err(TotpError("invalid authenticator URI"));
    }

    let mut options: Vec<(&str, String)> = Vec::new();
    let mut secret = key.to_owned();
    if !scheme.is_empty() {
        if scheme != "otpauth" || netloc.to_lowercase() != "totp" {
            return Err(TotpError("only TOTP authenticator URIs are supported"));
        }
        let query = parse_qs(&query);
        for name in ["secret", "algorithm", "digits", "period"] {
            if let Some((_, values)) = query.iter().find(|(known, _)| known == name) {
                if values.len() != 1 {
                    return Err(TotpError("authenticator URI has repeated parameters"));
                }
                options.push((name, values[0].clone()));
            }
        }
        secret = option(&options, "secret").unwrap_or("").to_owned();
    }

    let algorithm = match option(&options, "algorithm").unwrap_or("SHA1").to_lowercase().as_str() {
        "sha1" => Algorithm::Sha1,
        "sha256" => Algorithm::Sha256,
        "sha512" => Algorithm::Sha512,
        _ => return Err(TotpError("unsupported authenticator algorithm")),
    };
    let digits = match option(&options, "digits").unwrap_or("6") {
        "6" => 6,
        "8" => 8,
        _ => return Err(TotpError("authenticator digits must be 6 or 8")),
    };
    let period_text = option(&options, "period").unwrap_or("30");
    // Bounded before parsing, which a vault URI with thousands of digits would make fail or overflow later.
    let period = (period_text.bytes().all(|b| b.is_ascii_digit()) && (1..=5).contains(&period_text.len()))
        .then(|| period_text.parse::<u32>().ok())
        .flatten()
        .filter(|period| (1..=86400).contains(period))
        .ok_or(TotpError("authenticator period must be a whole number of seconds up to a day"))?;

    let encoded: String = secret.split(py_space).collect();
    if !encoded.is_ascii() {
        return Err(TotpError("authenticator secret is not valid base32"));
    }
    let mut padded = encoded.into_bytes();
    padded.resize(padded.len().div_ceil(8) * 8, b'=');
    let decoded = b32decode(&padded).ok_or(TotpError("authenticator secret is not valid base32"))?;
    if decoded.is_empty() {
        return Err(TotpError("authenticator secret is missing"));
    }
    Ok(Totp { secret: decoded, algorithm, digits, period })
}

fn option<'a>(options: &'a [(&str, String)], name: &str) -> Option<&'a str> {
    options.iter().find(|(known, _)| *known == name).map(|(_, value)| value.as_str())
}

/// The code `key` gives at unix time `when`. Pure; fails with TotpError on an unusable key.
pub fn totp_at(key: &str, when: f64) -> Result<String, TotpError> {
    Ok(parse_key(key)?.at(when))
}

/// Validate `key` now and return a provider that computes a fresh code on each await.
///
/// Wait for the next period when less than five seconds remain, capped at half the period, then read the clock
/// again.
pub fn one_time_code(key: &str) -> Result<SecretValue, TotpError> {
    let clock: Clock = Arc::new(|| SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs_f64());
    let sleep: Sleep = Arc::new(|seconds| Box::pin(tokio::time::sleep(Duration::from_secs_f64(seconds.max(0.0)))));
    one_time_code_with(key, clock, sleep)
}

/// `one_time_code` with the clock and sleep swapped, so a test checks the wait without waiting it out.
pub fn one_time_code_with(key: &str, clock: Clock, sleep: Sleep) -> Result<SecretValue, TotpError> {
    let authenticator = Arc::new(parse_key(key)?);
    let period = f64::from(authenticator.period);
    let guard = 5.0_f64.min(period / 2.0);
    Ok(SecretValue::Computed(Arc::new(move || {
        let (authenticator, clock, sleep) = (authenticator.clone(), clock.clone(), sleep.clone());
        Box::pin(async move {
            let mut now = clock();
            let remaining = period - now.rem_euclid(period);
            if remaining < guard {
                sleep(remaining).await;
                now = clock();
            }
            authenticator.at(now)
        })
    })))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    const SEED: &[u8] = b"12345678901234567890";

    fn b32(bytes: &[u8]) -> String {
        data_encoding::BASE32.encode(bytes)
    }

    fn key() -> String {
        b32(SEED)
    }

    fn uri() -> String {
        format!("otpauth://totp/example?secret={}", key())
    }

    async fn provide(value: &SecretValue) -> String {
        match value {
            SecretValue::Computed(provider) => provider().await,
            SecretValue::Plain(_) => panic!("a one-time code is computed when it is typed"),
        }
    }

    #[test]
    fn rfc_6238_appendix_b() {
        let rows: [(f64, [&str; 3]); 6] = [
            (59.0, ["94287082", "46119246", "90693936"]),
            (1111111109.0, ["07081804", "68084774", "25091201"]),
            (1111111111.0, ["14050471", "67062674", "99943326"]),
            (1234567890.0, ["89005924", "91819424", "93441116"]),
            (2000000000.0, ["69279037", "90698825", "38618901"]),
            (20000000000.0, ["65353130", "77737706", "47863826"]),
        ];
        for (column, (algorithm, size)) in [("SHA1", 20), ("SHA256", 32), ("SHA512", 64)].into_iter().enumerate() {
            let seed: Vec<u8> = SEED.iter().cycle().take(size).copied().collect();
            let uri = format!("otpauth://totp/example?secret={}&algorithm={algorithm}&digits=8", b32(&seed));
            for (when, expected) in rows {
                assert_eq!(totp_at(&uri, when).unwrap(), expected[column], "{algorithm} at {when}");
            }
        }
    }

    #[test]
    fn bare_keys_and_uris_share_defaults_and_accept_base32_variants() {
        for seed in [&b"12345678901234567890"[..], &b"123456789012345678901"[..]] {
            let key = b32(seed);
            let expected = totp_at(&key, 59.0).unwrap();
            assert_eq!(expected.len(), 6);
            let spaced = key.to_lowercase().chars().map(String::from).collect::<Vec<_>>().join(" ");
            for variant in [key.clone(), key.trim_end_matches('=').to_owned(), spaced] {
                assert_eq!(totp_at(&variant, 59.0).unwrap(), expected);
                assert_eq!(totp_at(&format!("otpauth://totp/example?secret={variant}"), 59.0).unwrap(), expected);
            }
        }
    }

    #[test]
    fn defaults_zero_pad_six_digits() {
        assert_eq!(totp_at(&key(), 1111111109.0).unwrap(), "081804");
    }

    #[test]
    fn unusable_keys_fail_before_use_without_disclosing_secrets() {
        let (key, uri) = (key(), uri());
        let keys = [
            String::new(),
            "   ".into(),
            "========".into(),
            "A".into(),
            "INVALID!SECRET".into(),
            "MZXW6===A".into(),
            "\u{2603}".into(),
            format!("otpauth://hotp/example?secret={key}"),
            format!("steam://{key}"),
            format!("https://example.com/?secret={key}"),
            format!("otpauth://[{key}]/example?secret={key}"),
            "otpauth://totp/example".into(),
            "otpauth://totp/example?secret=".into(),
            "otpauth://totp/example?secret=INVALID!SECRET".into(),
            format!("{uri}&secret={key}"),
            format!("{uri}&algorithm=MD5"),
            format!("{uri}&algorithm="),
            format!("{uri}&algorithm={key}"),
            format!("{uri}&digits=7"),
            format!("{uri}&digits=6.0"),
            format!("{uri}&digits="),
            format!("{uri}&period=0"),
            format!("{uri}&period=-30"),
            format!("{uri}&period=1.5"),
            format!("{uri}&period=invalid"),
            format!("{uri}&period="),
            format!("{uri}&period={key}"),
            format!("{uri}&period=86401"),
            format!("{uri}&period={}", "9".repeat(400)),
        ];
        for candidate in &keys {
            let errors = [totp_at(candidate, 59.0).unwrap_err(), one_time_code(candidate).unwrap_err()];
            for error in errors {
                let rendered = format!("{error}{error:?}");
                for sensitive in [candidate.as_str(), key.as_str(), "INVALID!SECRET"] {
                    if !sensitive.trim().is_empty() {
                        assert!(!rendered.contains(sensitive), "{candidate:?} rendered {rendered:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn errors_name_the_rule_they_broke() {
        let uri = uri();
        let cases = [
            (format!("otpauth://[{}]/x", key()), "invalid authenticator URI"),
            (format!("steam://{}", key()), "only TOTP authenticator URIs are supported"),
            (format!("{uri}&secret=x"), "authenticator URI has repeated parameters"),
            (format!("{uri}&algorithm=MD5"), "unsupported authenticator algorithm"),
            (format!("{uri}&digits=7"), "authenticator digits must be 6 or 8"),
            (format!("{uri}&period=0"), "authenticator period must be a whole number of seconds up to a day"),
            ("INVALID!SECRET".into(), "authenticator secret is not valid base32"),
            ("\u{2603}".into(), "authenticator secret is not valid base32"),
            ("".into(), "authenticator secret is missing"),
        ];
        for (candidate, message) in cases {
            assert_eq!(totp_at(&candidate, 59.0).unwrap_err().to_string(), message, "{candidate:?}");
        }
    }

    #[test]
    fn query_values_are_unquoted_as_parse_qs_does() {
        let spaced = key().chars().map(String::from).collect::<Vec<_>>().join("+");
        let expected = totp_at(&key(), 59.0).unwrap();
        assert_eq!(totp_at(&format!("otpauth://TOTP/x?secret={spaced}"), 59.0).unwrap(), expected);
        assert_eq!(totp_at(&format!("otpauth://totp/x?secret={}&algorithm=sha%31", key()), 59.0).unwrap(), expected);
    }

    #[tokio::test]
    async fn provider_waits_only_inside_the_guard_and_reads_the_clock_again() {
        let cases = [(30, 20.0, 0.0), (30, 25.0, 0.0), (30, 29.5, 0.5), (8, 3.5, 0.0), (8, 4.5, 3.5), (1, 0.75, 0.25)];
        for (period, start, delay) in cases {
            let now = Arc::new(Mutex::new(start));
            let slept = Arc::new(Mutex::new(Vec::<f64>::new()));
            let clock: Clock = {
                let now = now.clone();
                Arc::new(move || *now.lock().unwrap())
            };
            let sleep: Sleep = {
                let (now, slept) = (now.clone(), slept.clone());
                Arc::new(move |seconds| {
                    slept.lock().unwrap().push(seconds);
                    // A busy event loop may resume us more than one period after the requested wake time.
                    *now.lock().unwrap() += seconds + f64::from(period);
                    Box::pin(async {})
                })
            };
            let provider = one_time_code_with(&format!("{}&period={period}", uri()), clock, sleep).unwrap();
            let code = provide(&provider).await;
            let at = *now.lock().unwrap();
            assert_eq!(code, totp_at(&key(), (at / f64::from(period)).floor() * 30.0).unwrap());
            let expected: Vec<f64> = if delay > 0.0 { vec![delay] } else { vec![] };
            assert_eq!(*slept.lock().unwrap(), expected, "period {period} at {start}");
        }
    }

    #[tokio::test]
    async fn provider_computes_afresh_and_its_debug_keeps_the_key_private() {
        for candidate in [key(), uri()] {
            let times = Arc::new(Mutex::new(vec![60.0, 30.0, 0.0]));
            let clock: Clock = Arc::new(move || times.lock().unwrap().pop().unwrap());
            let sleep: Sleep = Arc::new(|_| panic!("a code at the start of its period must not wait"));
            let provider = one_time_code_with(&candidate, clock, sleep).unwrap();
            let rendered = format!("{provider:?}");
            assert!(!rendered.contains(&candidate) && !rendered.contains(&key()));
            let mut codes = vec![];
            for _ in 0..3 {
                codes.push(provide(&provider).await);
            }
            assert_eq!(codes, ["755224", "287082", "359152"]);
        }
    }
}
