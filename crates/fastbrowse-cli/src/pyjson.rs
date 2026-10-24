//! `json.loads(line)` on bytes, with CPython's error text, which a parse error reply carries as its message.
//!
//! serde_json reads the line. Only when it refuses one is the line scanned again the way CPython's scanner walks it,
//! to name the same fault at the same character.

use serde_json::Value;

pub fn loads(line: &[u8]) -> Result<Value, String> {
    let text = decode(line)?;
    serde_json::from_str(&text).map_err(|error| scan_error(&text).unwrap_or_else(|| error.to_string()))
}

/// `bytes.decode(json.detect_encoding(b))`, for the encodings a line can start with.
fn decode(line: &[u8]) -> Result<String, String> {
    if let Some(rest) = line.strip_prefix(b"\xef\xbb\xbf") {
        return utf8(rest);
    }
    let utf16 = |rest: &[u8], little: bool| {
        let units = rest.chunks(2).map(|pair| {
            let pair = [pair[0], *pair.get(1).unwrap_or(&0)];
            if little { u16::from_le_bytes(pair) } else { u16::from_be_bytes(pair) }
        });
        char::decode_utf16(units)
            .collect::<Result<String, _>>()
            .map_err(|_| "'utf-16' codec can't decode the input: illegal encoding".to_string())
    };
    if let Some(rest) = line.strip_prefix(b"\xff\xfe") {
        return utf16(rest, true);
    }
    if let Some(rest) = line.strip_prefix(b"\xfe\xff") {
        return utf16(rest, false);
    }
    utf8(line)
}

fn utf8(bytes: &[u8]) -> Result<String, String> {
    match std::str::from_utf8(bytes) {
        Ok(text) => Ok(text.to_string()),
        Err(error) => {
            let start = error.valid_up_to();
            let (length, reason) = match error.error_len() {
                Some(length) if length == 1 && !matches!(bytes[start], 0xc2..=0xf4) => (1, "invalid start byte"),
                Some(length) => (length, "invalid continuation byte"),
                None => (bytes.len() - start, "unexpected end of data"),
            };
            let at = if length == 1 {
                format!("byte 0x{:02x} in position {start}", bytes[start])
            } else {
                format!("bytes in position {start}-{}", start + length - 1)
            };
            Err(format!("'utf-8' codec can't decode {at}: {reason}"))
        }
    }
}

/// `JSONDecodeError(msg, doc, pos)` as `str()` writes it.
fn located(chars: &[char], message: &str, pos: usize) -> String {
    let line = chars[..pos].iter().filter(|c| **c == '\n').count() + 1;
    let column = match chars[..pos].iter().rposition(|c| *c == '\n') {
        Some(newline) => pos - newline,
        None => pos + 1,
    };
    format!("{message}: line {line} column {column} (char {pos})")
}

struct Fault(&'static str, usize);

/// Where CPython's scanner stops on `text`, or None where it reads what serde_json refused (NaN, Infinity).
fn scan_error(text: &str) -> Option<String> {
    let chars: Vec<char> = text.chars().collect();
    let scanner = Scanner { chars: &chars };
    let start = scanner.skip(0);
    let outcome = scanner.value(start).and_then(|end| {
        let end = scanner.skip(end);
        if end == chars.len() { Ok(end) } else { Err(Fault("Extra data", end)) }
    });
    outcome.err().map(|Fault(message, pos)| located(&chars, message, pos))
}

struct Scanner<'a> {
    chars: &'a [char],
}

impl Scanner<'_> {
    fn at(&self, index: usize) -> Option<char> {
        self.chars.get(index).copied()
    }

    fn skip(&self, mut index: usize) -> usize {
        while matches!(self.at(index), Some(' ' | '\t' | '\n' | '\r')) {
            index += 1;
        }
        index
    }

    fn word(&self, index: usize, word: &str) -> bool {
        word.chars().enumerate().all(|(offset, c)| self.at(index + offset) == Some(c))
    }

    /// `scan_once`: the end of the value at `index`.
    fn value(&self, index: usize) -> Result<usize, Fault> {
        let expecting = Err(Fault("Expecting value", index));
        match self.at(index) {
            Some('"') => self.string(index + 1),
            Some('{') => self.object(index + 1),
            Some('[') => self.array(index + 1),
            Some('n') if self.word(index, "null") => Ok(index + 4),
            Some('t') if self.word(index, "true") => Ok(index + 4),
            Some('f') if self.word(index, "false") => Ok(index + 5),
            Some('N') if self.word(index, "NaN") => Ok(index + 3),
            Some('I') if self.word(index, "Infinity") => Ok(index + 8),
            Some('-') if self.word(index, "-Infinity") => Ok(index + 9),
            Some('-' | '0'..='9') => self.number(index).map_or(expecting, Ok),
            _ => expecting,
        }
    }

    /// `-?(?:0|[1-9]\d*)(\.\d+)?([eE][-+]?\d+)?`, or None when it does not match.
    fn number(&self, mut index: usize) -> Option<usize> {
        let digits = |mut i: usize| {
            let from = i;
            while matches!(self.at(i), Some('0'..='9')) {
                i += 1;
            }
            (i > from).then_some(i)
        };
        if self.at(index) == Some('-') {
            index += 1;
        }
        index = match self.at(index) {
            Some('0') => index + 1,
            Some('1'..='9') => digits(index)?,
            _ => return None,
        };
        if self.at(index) == Some('.')
            && let Some(end) = digits(index + 1)
        {
            index = end;
        }
        if matches!(self.at(index), Some('e' | 'E')) {
            let sign = usize::from(matches!(self.at(index + 1), Some('+' | '-')));
            if let Some(end) = digits(index + 1 + sign) {
                index = end;
            }
        }
        Some(index)
    }

    /// `scanstring`, from just past the opening quote.
    fn string(&self, mut index: usize) -> Result<usize, Fault> {
        let begin = index - 1;
        loop {
            match self.at(index) {
                None => return Err(Fault("Unterminated string starting at", begin)),
                Some('"') => return Ok(index + 1),
                Some('\\') => match self.at(index + 1) {
                    None => return Err(Fault("Unterminated string starting at", begin)),
                    Some('u') => {
                        let hex = (index + 2..index + 6).all(|i| self.at(i).is_some_and(|c| c.is_ascii_hexdigit()));
                        if !hex {
                            return Err(Fault("Invalid \\uXXXX escape", index + 1));
                        }
                        index += 6;
                    }
                    Some('"' | '\\' | '/' | 'b' | 'f' | 'n' | 'r' | 't') => index += 2,
                    Some(_) => return Err(Fault("Invalid \\escape", index)),
                },
                Some(c) if c < ' ' => return Err(Fault("Invalid control character at", index)),
                Some(_) => index += 1,
            }
        }
    }

    fn object(&self, index: usize) -> Result<usize, Fault> {
        let mut index = self.skip(index);
        if self.at(index) == Some('}') {
            return Ok(index + 1);
        }
        loop {
            if self.at(index) != Some('"') {
                return Err(Fault("Expecting property name enclosed in double quotes", index));
            }
            index = self.skip(self.string(index + 1)?);
            if self.at(index) != Some(':') {
                return Err(Fault("Expecting ':' delimiter", index));
            }
            index = self.skip(self.value(self.skip(index + 1))?);
            match self.at(index) {
                Some('}') => return Ok(index + 1),
                Some(',') => {
                    let comma = index;
                    index = self.skip(index + 1);
                    if self.at(index) == Some('}') {
                        return Err(Fault("Illegal trailing comma before end of object", comma));
                    }
                }
                _ => return Err(Fault("Expecting ',' delimiter", index)),
            }
        }
    }

    fn array(&self, index: usize) -> Result<usize, Fault> {
        let mut index = self.skip(index);
        if self.at(index) == Some(']') {
            return Ok(index + 1);
        }
        loop {
            index = self.skip(self.value(index)?);
            match self.at(index) {
                Some(']') => return Ok(index + 1),
                Some(',') => {
                    let comma = index;
                    index = self.skip(index + 1);
                    if self.at(index) == Some(']') {
                        return Err(Fault("Illegal trailing comma before end of array", comma));
                    }
                }
                _ => return Err(Fault("Expecting ',' delimiter", index)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_read_as_cpython_writes_them() {
        let cases: [(&[u8], &str); 21] = [
            (b"{\"jsonrpc\": \"2.0\", \"id\": 1, \"method\"", "Expecting ':' delimiter: line 1 column 37 (char 36)"),
            (b"\xff\xfe", "Expecting value: line 1 column 1 (char 0)"),
            (b"", "Expecting value: line 1 column 1 (char 0)"),
            (b"   ", "Expecting value: line 1 column 4 (char 3)"),
            (b"{", "Expecting property name enclosed in double quotes: line 1 column 2 (char 1)"),
            (b"[1,]", "Illegal trailing comma before end of array: line 1 column 3 (char 2)"),
            (b"{\"a\":1,}", "Illegal trailing comma before end of object: line 1 column 7 (char 6)"),
            (b"{\"a\":1 \"b\":2}", "Expecting ',' delimiter: line 1 column 8 (char 7)"),
            (b"\"abc", "Unterminated string starting at: line 1 column 1 (char 0)"),
            (b"\"a\\x\"", "Invalid \\escape: line 1 column 3 (char 2)"),
            (b"\"a\\u12zz\"", "Invalid \\uXXXX escape: line 1 column 4 (char 3)"),
            (b"\"a\x01b\"", "Invalid control character at: line 1 column 3 (char 2)"),
            (b"1 2", "Extra data: line 1 column 3 (char 2)"),
            (b"\n\n  x", "Expecting value: line 3 column 3 (char 4)"),
            (b"{\"a\":\n  }", "Expecting value: line 2 column 3 (char 8)"),
            (b"\xc3\x28", "'utf-8' codec can't decode byte 0xc3 in position 0: invalid continuation byte"),
            (b"\xe2\x82\x28", "'utf-8' codec can't decode bytes in position 0-1: invalid continuation byte"),
            (b"{\"a\":\"\xe2\x82", "'utf-8' codec can't decode bytes in position 6-7: unexpected end of data"),
            (b"\x80abc", "'utf-8' codec can't decode byte 0x80 in position 0: invalid start byte"),
            (b"01", "Extra data: line 1 column 2 (char 1)"),
            ("[\"\u{e9}\", x]".as_bytes(), "Expecting value: line 1 column 7 (char 6)"),
        ];
        for (line, message) in cases {
            assert_eq!(loads(line).unwrap_err(), message, "{line:?}");
        }
        assert!(loads(b"\xef\xbb\xbf{}").is_ok());
    }
}
