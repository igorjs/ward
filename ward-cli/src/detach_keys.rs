// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! The key sequence that detaches from an attached sandbox, in Docker's
//! `--detach-keys` syntax: comma separated keys, each one character or
//! `ctrl-` plus a letter or one of `@ [ \ ] ^ _`.

use anyhow::{Result, bail};

/// What Docker uses when `--detach-keys` is not given.
pub const DEFAULT_SEQUENCE: &str = "ctrl-p,ctrl-q";
const CTRL_PREFIX: &str = "ctrl-";
/// Control codes are the key's code with the top three bits cleared.
const CONTROL_MASK: u8 = 0x1f;
const CONTROL_SYMBOLS: [char; 6] = ['@', '[', '\\', ']', '^', '_'];

/// A parsed detach sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetachKeys(Vec<u8>);

impl DetachKeys {
    /// # Errors
    ///
    /// Fails on an empty sequence or a key Docker does not know.
    pub fn parse(text: &str) -> Result<Self> {
        let keys = text
            .split(',')
            .map(parse_key)
            .collect::<Result<Vec<u8>>>()
            .map_err(|e| anyhow::anyhow!("invalid detach keys '{text}': {e}"))?;
        Ok(Self(keys))
    }

    pub fn filter(&self) -> DetachFilter {
        DetachFilter {
            keys: self.0.clone(),
            matched: 0,
        }
    }
}

fn parse_key(key: &str) -> Result<u8> {
    if let Some(rest) = key.strip_prefix(CTRL_PREFIX) {
        let mut chars = rest.chars();
        return match (chars.next(), chars.next()) {
            (Some(c), None) if c.is_ascii_alphabetic() => {
                Ok(c.to_ascii_lowercase() as u8 & CONTROL_MASK)
            }
            (Some(c), None) if CONTROL_SYMBOLS.contains(&c) => Ok(c as u8 & CONTROL_MASK),
            _ => bail!("unknown key '{key}'"),
        };
    }
    let mut chars = key.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) if c.is_ascii() => Ok(c as u8),
        _ => bail!("unknown key '{key}'"),
    }
}

/// What to do with a piece of stdin.
#[derive(Debug, PartialEq, Eq)]
pub enum Filtered {
    /// Send these bytes on.
    Pass(Vec<u8>),
    /// The whole sequence was typed.
    Detach,
}

/// Holds back the bytes that might start the sequence, as Docker does, and
/// sends them on when the next byte shows they did not.
#[derive(Debug)]
pub struct DetachFilter {
    keys: Vec<u8>,
    matched: usize,
}

impl DetachFilter {
    pub fn feed(&mut self, input: &[u8]) -> Filtered {
        let mut out = Vec::with_capacity(input.len());
        for &byte in input {
            if byte == self.keys[self.matched] {
                self.matched += 1;
                if self.matched == self.keys.len() {
                    return Filtered::Detach;
                }
                continue;
            }
            out.extend_from_slice(&self.keys[..self.matched]);
            self.matched = 0;
            out.push(byte);
        }
        Filtered::Pass(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CTRL_P: u8 = 0x10;
    const CTRL_Q: u8 = 0x11;

    #[rstest::rstest]
    #[case::default(DEFAULT_SEQUENCE, &[CTRL_P, CTRL_Q])]
    #[case::letters("a,b", b"ab".as_slice())]
    #[case::mixed("ctrl-a,x", &[0x01, b'x'])]
    #[case::symbols("ctrl-[,ctrl-],ctrl-_", &[0x1b, 0x1d, 0x1f])]
    #[case::upper_letter("ctrl-Z", &[0x1a])]
    fn given_sequence_when_parsed_then_bytes_follow_docker(
        #[case] text: &str,
        #[case] want: &[u8],
    ) {
        assert_eq!(DetachKeys::parse(text).unwrap(), DetachKeys(want.to_vec()));
    }

    #[rstest::rstest]
    #[case::empty("")]
    #[case::unknown_ctrl("ctrl-1")]
    #[case::long_word("escape")]
    #[case::trailing_comma("a,")]
    #[case::not_ascii("é")]
    fn given_bad_sequence_when_parsed_then_it_fails(#[case] text: &str) {
        let err = DetachKeys::parse(text).unwrap_err();

        assert!(err.to_string().contains("invalid detach keys"), "{err}");
    }

    fn default_filter() -> DetachFilter {
        DetachKeys::parse(DEFAULT_SEQUENCE).unwrap().filter()
    }

    #[test]
    fn given_ordinary_input_when_filtered_then_it_passes_unchanged() {
        let mut filter = default_filter();

        assert_eq!(filter.feed(b"ls\n"), Filtered::Pass(b"ls\n".to_vec()));
    }

    #[test]
    fn given_the_whole_sequence_when_filtered_then_it_detaches() {
        let mut filter = default_filter();

        assert_eq!(filter.feed(&[b'a', CTRL_P, CTRL_Q]), Filtered::Detach);
    }

    #[test]
    fn given_the_sequence_split_across_reads_when_filtered_then_it_detaches() {
        let mut filter = default_filter();

        assert_eq!(filter.feed(&[CTRL_P]), Filtered::Pass(Vec::new()));
        assert_eq!(filter.feed(&[CTRL_Q]), Filtered::Detach);
    }

    #[test]
    fn given_a_partial_match_that_fails_when_filtered_then_the_held_bytes_are_sent() {
        let mut filter = default_filter();

        assert_eq!(filter.feed(&[CTRL_P]), Filtered::Pass(Vec::new()));
        assert_eq!(filter.feed(b"x"), Filtered::Pass(vec![CTRL_P, b'x']));
        assert_eq!(filter.feed(&[CTRL_Q]), Filtered::Pass(vec![CTRL_Q]));
    }

    #[test]
    fn given_a_custom_single_key_when_typed_then_it_detaches() {
        let mut filter = DetachKeys::parse("ctrl-x").unwrap().filter();

        assert_eq!(filter.feed(&[0x18]), Filtered::Detach);
    }
}
