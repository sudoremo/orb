//! Modified UTF-7 (RFC 3501 §5.1.3), the encoding of mailbox names on the
//! wire: `Entwürfe` travels as `Entw&APw-rfe`.
//!
//! Printable ASCII stands for itself, `&` opens a run of UTF-16BE in a base64
//! that uses `,` for `/`, `-` closes it, and `&-` is a literal `&`.

/// The readable name of a mailbox the server named. A name that is not
/// valid modified UTF-7 is returned as it came, rather than lost.
pub fn decode(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut rest = name;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at + 1..];
        let Some(end) = rest.find('-') else {
            // An unterminated run: not modified UTF-7 at all.
            out.push('&');
            out.push_str(rest);
            return out;
        };
        let run = &rest[..end];
        rest = &rest[end + 1..];
        if run.is_empty() {
            out.push('&');
            continue;
        }
        match decode_run(run) {
            Some(text) => out.push_str(&text),
            None => {
                out.push('&');
                out.push_str(run);
                out.push('-');
            }
        }
    }
    out.push_str(rest);
    out
}

/// The wire form of a mailbox name typed by a person.
pub fn encode(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut pending: Vec<u16> = Vec::new();
    let flush = |pending: &mut Vec<u16>, out: &mut String| {
        if pending.is_empty() {
            return;
        }
        let bytes: Vec<u8> = pending.iter().flat_map(|unit| unit.to_be_bytes()).collect();
        out.push('&');
        out.push_str(&base64_encode(&bytes));
        out.push('-');
        pending.clear();
    };
    for c in name.chars() {
        if c == '&' {
            flush(&mut pending, &mut out);
            out.push_str("&-");
        } else if (' '..='~').contains(&c) {
            flush(&mut pending, &mut out);
            out.push(c);
        } else {
            let mut units = [0u16; 2];
            pending.extend_from_slice(c.encode_utf16(&mut units));
        }
    }
    flush(&mut pending, &mut out);
    out
}

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+,";

fn decode_run(run: &str) -> Option<String> {
    let mut bits: u32 = 0;
    let mut have = 0;
    let mut bytes = Vec::with_capacity(run.len());
    for c in run.bytes() {
        let value = ALPHABET.iter().position(|&a| a == c)? as u32;
        bits = (bits << 6) | value;
        have += 6;
        if have >= 8 {
            have -= 8;
            bytes.push((bits >> have) as u8);
            bits &= (1 << have) - 1;
        }
    }
    if bytes.len() % 2 != 0 {
        return None;
    }
    let units: Vec<u16> = bytes
        .chunks(2)
        .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
        .collect();
    String::from_utf16(&units).ok()
}

fn base64_encode(bytes: &[u8]) -> String {
    let mut out = String::new();
    let mut bits: u32 = 0;
    let mut have = 0;
    for &byte in bytes {
        bits = (bits << 8) | u32::from(byte);
        have += 8;
        while have >= 6 {
            have -= 6;
            out.push(ALPHABET[((bits >> have) & 63) as usize] as char);
        }
    }
    if have > 0 {
        out.push(ALPHABET[((bits << (6 - have)) & 63) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_decode_as_the_user_wrote_them() {
        assert_eq!(decode("Entw&APw-rfe"), "Entwürfe");
        assert_eq!(decode("INBOX.Gel&APY-scht"), "INBOX.Gelöscht");
        assert_eq!(decode("Fish &- Chips"), "Fish & Chips");
        assert_eq!(decode("INBOX"), "INBOX");
        assert_eq!(decode("&ZeVnLIqe-"), "日本語");
        assert_eq!(
            decode("&2D3eAQ-"),
            "😁",
            "a pair of surrogates is one character"
        );
    }

    #[test]
    fn what_is_not_utf7_is_left_alone() {
        assert_eq!(decode("Rock & Roll"), "Rock & Roll");
        assert_eq!(decode("A&!!-B"), "A&!!-B");
        assert_eq!(decode("A&B"), "A&B");
    }

    #[test]
    fn names_encode_to_what_the_server_expects() {
        assert_eq!(encode("Entwürfe"), "Entw&APw-rfe");
        assert_eq!(encode("Fish & Chips"), "Fish &- Chips");
        assert_eq!(encode("INBOX"), "INBOX");
        assert_eq!(encode("日本語"), "&ZeVnLIqe-");
        for name in ["Entwürfe", "Fish & Chips", "😁 mail", "a&b&c", "Ünïcödé"] {
            assert_eq!(decode(&encode(name)), name, "{name}");
        }
    }
}
