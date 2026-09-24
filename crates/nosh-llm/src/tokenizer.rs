//! Tokenizer wrapper: segment-aware encoding (special tokens only from trusted
//! text), a per-id byte table and incremental UTF-8 decoding.

use std::collections::HashMap;
use std::path::Path;

use tokenizers::Tokenizer;

use crate::LlmError;
use crate::template::Seg;

pub struct Tok {
    inner: Tokenizer,
    /// Raw bytes of every token id.
    bytes: Vec<Vec<u8>>,
    /// Added tokens (content, id), longest first, for splitting trusted text.
    added: Vec<(String, u32)>,
    vocab: HashMap<String, u32>,
}

/// GPT-2 byte-level alphabet: maps each byte to a printable char.
fn bytes_to_unicode() -> [char; 256] {
    let mut table = ['\0'; 256];
    let mut n = 0u32;
    for b in 0..256u32 {
        let printable =
            (33..=126).contains(&b) || (161..=172).contains(&b) || (174..=255).contains(&b);
        table[b as usize] = if printable {
            char::from_u32(b).unwrap()
        } else {
            let c = char::from_u32(256 + n).unwrap();
            n += 1;
            c
        };
    }
    table
}

impl Tok {
    pub fn load(path: &Path) -> Result<Self, LlmError> {
        let inner = Tokenizer::from_file(path)
            .map_err(|e| LlmError::Tokenizer(format!("{}: {e}", path.display())))?;
        Ok(Self::from_tokenizer(inner))
    }

    pub fn from_tokenizer(mut inner: Tokenizer) -> Self {
        let _ = inner.with_truncation(None);
        inner.with_padding(None);
        let vocab = inner.get_vocab(true);
        let size = vocab.values().copied().max().map_or(0, |m| m as usize + 1);
        let added_dec = inner.get_added_tokens_decoder();
        let mut rev = [0u8; 0x200];
        let mut rev_ok = [false; 0x200];
        for (b, c) in bytes_to_unicode().iter().enumerate() {
            rev[*c as usize] = b as u8;
            rev_ok[*c as usize] = true;
        }
        let mut bytes = vec![Vec::new(); size];
        for (s, &id) in &vocab {
            let v = if let Some(a) = added_dec.get(&id) {
                a.content.as_bytes().to_vec()
            } else {
                let mut out = Vec::with_capacity(s.len());
                for ch in s.chars() {
                    let cp = ch as usize;
                    if cp < rev.len() && rev_ok[cp] {
                        out.push(rev[cp]);
                    } else {
                        let mut buf = [0u8; 4];
                        out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                    }
                }
                out
            };
            bytes[id as usize] = v;
        }
        let mut added: Vec<(String, u32)> = added_dec
            .iter()
            .map(|(id, t)| (t.content.clone(), *id))
            .filter(|(c, _)| !c.is_empty())
            .collect();
        added.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then(a.1.cmp(&b.1)));
        Self {
            inner,
            bytes,
            added,
            vocab,
        }
    }

    pub fn vocab_size(&self) -> usize {
        self.bytes.len()
    }

    pub fn token_id(&self, s: &str) -> Option<u32> {
        self.vocab.get(s).copied()
    }

    pub fn token_bytes(&self, id: u32) -> &[u8] {
        self.bytes
            .get(id as usize)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Encodes text; `allow_special` lets added tokens in the text be matched.
    pub fn encode(&mut self, text: &str, allow_special: bool) -> Result<Vec<u32>, LlmError> {
        if text.is_empty() {
            return Ok(Vec::new());
        }
        self.inner.set_encode_special_tokens(!allow_special);
        let enc = self
            .inner
            .encode_fast(text, false)
            .map_err(|e| LlmError::Tokenizer(e.to_string()))?;
        Ok(enc.get_ids().to_vec())
    }

    /// Splits trusted text at added-token occurrences (leftmost-longest).
    fn split_added<'a>(&self, text: &'a str) -> Vec<Result<&'a str, u32>> {
        let mut out = Vec::new();
        let mut last = 0;
        let mut i = 0;
        let b = text.as_bytes();
        while i < b.len() {
            if b[i] == b'<' || b[i] == b'/' {
                let rest = &text[i..];
                if let Some((content, id)) = self
                    .added
                    .iter()
                    .find(|(c, _)| rest.starts_with(c.as_str()))
                {
                    if last < i {
                        out.push(Ok(&text[last..i]));
                    }
                    out.push(Err(*id));
                    i += content.len();
                    last = i;
                    continue;
                }
            }
            i += 1;
        }
        if last < text.len() {
            out.push(Ok(&text[last..]));
        }
        out
    }

    /// Encodes template segments. Special tokens come only from trusted
    /// segments; text between them (trusted and untrusted alike) is encoded in
    /// one piece, as HF does, but without special-token matching.
    pub fn encode_segments(&mut self, segs: &[Seg]) -> Result<Vec<u32>, LlmError> {
        let mut ids = Vec::new();
        let mut pending = String::new();
        for seg in segs {
            if seg.trusted {
                for part in self.split_added(&seg.text) {
                    match part {
                        Ok(t) => pending.push_str(t),
                        Err(id) => {
                            let text = std::mem::take(&mut pending);
                            ids.extend(self.encode(&text, false)?);
                            ids.push(id);
                        }
                    }
                }
            } else {
                pending.push_str(&seg.text);
            }
        }
        let text = std::mem::take(&mut pending);
        ids.extend(self.encode(&text, false)?);
        Ok(ids)
    }

    pub fn decode_lossy(&self, ids: &[u32]) -> String {
        let mut d = Utf8Stream::default();
        let mut s = String::new();
        for &id in ids {
            s.push_str(&d.push(self.token_bytes(id)));
        }
        s.push_str(&d.finish());
        s
    }
}

/// Buffers incomplete UTF-8 sequences across tokens.
#[derive(Debug, Default)]
pub struct Utf8Stream {
    buf: Vec<u8>,
}

impl Utf8Stream {
    pub fn push(&mut self, bytes: &[u8]) -> String {
        self.buf.extend_from_slice(bytes);
        let mut out = String::new();
        loop {
            match std::str::from_utf8(&self.buf) {
                Ok(s) => {
                    out.push_str(s);
                    self.buf.clear();
                    return out;
                }
                Err(e) => {
                    let valid = e.valid_up_to();
                    out.push_str(std::str::from_utf8(&self.buf[..valid]).expect("validated"));
                    match e.error_len() {
                        None => {
                            self.buf.drain(..valid);
                            return out;
                        }
                        Some(bad) => {
                            out.push('\u{FFFD}');
                            self.buf.drain(..valid + bad);
                        }
                    }
                }
            }
        }
    }

    /// Flushes whatever is left (invalid tails become U+FFFD).
    pub fn finish(&mut self) -> String {
        let s = String::from_utf8_lossy(&self.buf).into_owned();
        self.buf.clear();
        s
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_alphabet_roundtrip() {
        let t = bytes_to_unicode();
        assert_eq!(t[b'A' as usize], 'A');
        assert_eq!(t[b' ' as usize], 'Ġ');
        assert_eq!(t[b'\n' as usize], 'Ċ');
        let mut seen = std::collections::HashSet::new();
        assert!(t.iter().all(|c| seen.insert(*c)));
    }

    #[test]
    fn utf8_stream_buffers_partial_chars() {
        let mut s = Utf8Stream::default();
        let bytes = "你好".as_bytes();
        assert_eq!(s.push(&bytes[..2]), "");
        assert_eq!(s.push(&bytes[2..4]), "你");
        assert_eq!(s.push(&bytes[4..]), "好");
        assert!(s.is_empty());
        assert_eq!(s.push(&[b'a', 0xff, b'b']), "a\u{FFFD}b");
        assert_eq!(s.push(&[0xe4]), "");
        assert_eq!(s.finish(), "\u{FFFD}");
    }
}
