//! Bounded, write-only OSC 52 clipboard requests from terminal applications.

use base64::{Engine, engine::general_purpose::STANDARD};

const MAX_BYTES: usize = 100_000;

pub(crate) fn decode(data: &str) -> Option<String> {
    let (selection, encoded) = data.split_once(';')?;
    // c = clipboard, p = primary selection, empty = default selection.
    // Queries ("?") are never answered: terminal applications cannot read the
    // browser clipboard. Malformed and empty requests leave it unchanged.
    if !selection.chars().all(|c| matches!(c, 'c' | 'p'))
        || encoded.is_empty()
        || encoded.len() > MAX_BYTES.div_ceil(3) * 4
    {
        return None;
    }
    let bytes = STANDARD.decode(encoded).ok()?;
    if bytes.is_empty() || bytes.len() > MAX_BYTES {
        return None;
    }
    String::from_utf8(bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipboard_requests_preserve_unicode() {
        let text = "Van-Thanh Pham — tiếng Việt 🦀\nline two";
        for selection in ["", "c", "p", "cp"] {
            assert_eq!(
                decode(&format!("{selection};{}", STANDARD.encode(text))),
                Some(text.into())
            );
        }
    }

    #[test]
    fn reject_reads_invalid_and_oversized_payloads() {
        for data in [
            "c;?",
            "p;?",
            "c;",
            "c;%%%",
            "c;/w==",
            "s;aGk=",
            "c;aGk=;extra",
            "missing separator",
        ] {
            assert_eq!(decode(data), None, "{data}");
        }
        assert!(decode(&format!("c;{}", STANDARD.encode(vec![b'x'; MAX_BYTES]))).is_some());
        assert!(decode(&format!("c;{}", STANDARD.encode(vec![b'x'; MAX_BYTES + 1]))).is_none());
    }
}
