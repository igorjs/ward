// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Decode a byte stream that arrives in chunks as UTF-8 text.

/// Decodes one stream's chunks, carrying a character cut off at the end of a
/// chunk over to the next so it is not turned into replacement characters.
#[derive(Debug, Default)]
pub struct Utf8Stream {
    pending: Vec<u8>,
}

impl Utf8Stream {
    /// Text for `chunk`, minus a trailing incomplete character that waits for
    /// the next chunk. Bytes that can never be valid become U+FFFD.
    pub fn push(&mut self, chunk: &[u8]) -> String {
        let mut data = std::mem::take(&mut self.pending);
        data.extend_from_slice(chunk);
        let mut text = String::with_capacity(data.len());
        let mut rest = data.as_slice();
        loop {
            match std::str::from_utf8(rest) {
                Ok(valid) => {
                    text.push_str(valid);
                    break;
                }
                Err(e) => {
                    let (valid, after) = rest.split_at(e.valid_up_to());
                    text.push_str(&String::from_utf8_lossy(valid));
                    match e.error_len() {
                        Some(bad) => {
                            text.push(char::REPLACEMENT_CHARACTER);
                            rest = &after[bad..];
                        }
                        None => {
                            self.pending = after.to_vec();
                            break;
                        }
                    }
                }
            }
        }
        text
    }

    /// Text for whatever is still waiting once the stream has ended.
    pub fn finish(&mut self) -> String {
        String::from_utf8_lossy(&std::mem::take(&mut self.pending)).into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EURO: &[u8] = "\u{20ac}".as_bytes();

    #[test]
    fn given_three_byte_char_split_over_two_chunks_when_pushed_then_it_is_intact() {
        // Arrange
        let mut stream = Utf8Stream::default();

        // Act
        let first = stream.push(&EURO[..1]);
        let second = stream.push(&EURO[1..]);

        // Assert
        assert_eq!((first.as_str(), second.as_str()), ("", "\u{20ac}"));
    }

    #[test]
    fn given_invalid_byte_when_pushed_then_it_becomes_one_replacement() {
        // Arrange
        let mut stream = Utf8Stream::default();

        // Act
        let text = stream.push(b"a\xffb");

        // Assert
        assert_eq!(text, "a\u{fffd}b");
    }

    #[test]
    fn given_stream_ending_mid_char_when_finished_then_replacement_is_emitted() {
        // Arrange
        let mut stream = Utf8Stream::default();
        stream.push(&EURO[..2]);

        // Act
        let text = stream.finish();

        // Assert
        assert_eq!(text, "\u{fffd}");
    }
}
