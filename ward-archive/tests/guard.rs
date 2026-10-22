// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Hostile tar streams built byte by byte, because the tar builder refuses to
//! write the malformed fields these cases need.

use std::io::Cursor;
use std::time::{Duration, Instant};

use rstest::rstest;
use ward_archive::{ArchiveError, Limits, MAX_EXTENDED_HEADER_BYTES, Policy, unpack};

const BLOCK: usize = 512;
const NAME_OFFSET: usize = 0;
const MODE_OFFSET: usize = 100;
const SIZE_OFFSET: usize = 124;
const SIZE_LEN: usize = 12;
const CKSUM_OFFSET: usize = 148;
const CKSUM_END: usize = 156;
const TYPEFLAG_OFFSET: usize = 156;
const MAGIC_OFFSET: usize = 257;
/// GNU magic and version, so the tar crate treats extension headers as real.
const GNU_MAGIC: &[u8] = b"ustar  \0";
const EIGHT_GIB: u64 = 8 * 1024 * 1024 * 1024;
/// Largest octal size that fits the 11 digits of a size field.
const MAX_OCTAL_SIZE: &[u8] = b"77777777777";
const TIME_BUDGET: Duration = Duration::from_secs(5);

fn block(kind: u8, size_field: &[u8]) -> [u8; BLOCK] {
    let mut b = [0u8; BLOCK];
    b[NAME_OFFSET..NAME_OFFSET + 1].copy_from_slice(b"a");
    b[MODE_OFFSET..MODE_OFFSET + 7].copy_from_slice(b"0000644");
    b[SIZE_OFFSET..SIZE_OFFSET + size_field.len()].copy_from_slice(size_field);
    b[TYPEFLAG_OFFSET] = kind;
    b[MAGIC_OFFSET..MAGIC_OFFSET + GNU_MAGIC.len()].copy_from_slice(GNU_MAGIC);
    seal(&mut b);
    b
}

fn seal(b: &mut [u8; BLOCK]) {
    b[CKSUM_OFFSET..CKSUM_END].fill(b' ');
    let sum: u32 = b.iter().map(|&x| u32::from(x)).sum();
    let text = format!("{sum:06o}\0 ");
    b[CKSUM_OFFSET..CKSUM_END].copy_from_slice(text.as_bytes());
}

fn octal(size: u64) -> Vec<u8> {
    format!("{size:011o}").into_bytes()
}

fn padded(body: &[u8]) -> Vec<u8> {
    let mut v = body.to_vec();
    v.resize(v.len().next_multiple_of(BLOCK), 0);
    v
}

/// An extension header of `kind` followed by its `body`.
fn extension(kind: u8, body: &[u8]) -> Vec<u8> {
    let mut v = block(kind, &octal(body.len() as u64)).to_vec();
    v.extend(padded(body));
    v
}

/// An extension header declaring `size` bytes with only a stub body.
fn oversized(kind: u8) -> Vec<u8> {
    let mut v = block(kind, &octal(EIGHT_GIB)).to_vec();
    v.extend_from_slice(b"tiny");
    v
}

fn run(tar: &[u8], policy: Policy, limits: &Limits) -> Result<(), ArchiveError> {
    let dest = tempfile::tempdir().unwrap();
    let started = Instant::now();
    let result = unpack(Cursor::new(tar), dest.path(), policy, limits);
    assert!(started.elapsed() < TIME_BUDGET, "unpack was too slow");
    result
}

fn is_extended_limit(result: &Result<(), ArchiveError>) -> bool {
    matches!(
        result,
        Err(ArchiveError::LimitExceeded {
            what: "extended header size",
            ..
        })
    )
}

#[test]
fn given_a_base_256_size_that_tar_reads_as_zero_when_unpacked_then_the_next_header_is_still_checked()
 {
    // Arrange
    let mut size = [0u8; SIZE_LEN];
    size[0] = 0x81;
    let mut tar = block(b'0', &size).to_vec();
    tar.extend(oversized(b'L'));

    // Act
    let result = run(&tar, Policy::Snapshot, &Limits::snapshot());

    // Assert
    assert!(is_extended_limit(&result), "{result:?}");
}

#[rstest]
#[case::tab(b"\t1000")]
#[case::plus(b"+1000")]
fn given_an_octal_size_that_tar_accepts_when_unpacked_then_the_guard_skips_the_same_body(
    #[case] size: &[u8],
) {
    // Arrange
    let mut tar = block(b'0', size).to_vec();
    tar.extend(block(b'0', MAX_OCTAL_SIZE));
    tar.extend(oversized(b'L'));

    // Act
    let result = run(&tar, Policy::Snapshot, &Limits::snapshot());

    // Assert
    assert!(is_extended_limit(&result), "{result:?}");
}

#[rstest]
#[case::negative(&[0xff; SIZE_LEN])]
#[case::largest(&[0x80, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff])]
fn given_a_base_256_size_that_is_negative_or_huge_when_unpacked_then_it_fails_cleanly(
    #[case] size: &[u8],
    #[values(Policy::Snapshot, Policy::Copy)] policy: Policy,
) {
    // Arrange
    let tar = block(b'0', size).to_vec();
    let limits = if policy == Policy::Copy {
        Limits::default()
    } else {
        Limits::snapshot()
    };

    // Act
    let result = run(&tar, policy, &limits);

    // Assert
    assert!(result.is_err());
}

#[test]
fn given_two_long_name_headers_in_a_row_when_the_second_is_oversized_then_it_is_refused() {
    // Arrange
    let mut tar = extension(b'L', b"first\0");
    tar.extend(oversized(b'L'));

    // Act
    let result = run(&tar, Policy::Copy, &Limits::default());

    // Assert
    assert!(is_extended_limit(&result), "{result:?}");
}

#[test]
fn given_two_small_long_name_headers_in_a_row_when_unpacked_then_it_fails() {
    // Arrange
    let mut tar = extension(b'L', b"first\0");
    tar.extend(extension(b'L', b"second\0"));
    tar.extend(block(b'0', &octal(0)));

    // Act
    let result = run(&tar, Policy::Copy, &Limits::default());

    // Assert
    assert!(result.is_err());
}

#[rstest]
#[case::at_cap(MAX_EXTENDED_HEADER_BYTES, false)]
#[case::above_cap(MAX_EXTENDED_HEADER_BYTES + 1, true)]
fn given_a_long_name_header_around_the_cap_when_unpacked_then_only_the_larger_is_refused(
    #[case] body_len: u64,
    #[case] refused: bool,
) {
    // Arrange
    let mut name = vec![b'n'; body_len as usize];
    *name.last_mut().unwrap() = 0;
    let mut tar = extension(b'L', &name);
    tar.extend(block(b'0', &octal(0)));
    let limits = Limits {
        max_path_len: body_len as usize * 2,
        ..Limits::snapshot()
    };

    // Act
    let result = run(&tar, Policy::Snapshot, &limits);

    // Assert
    assert_eq!(is_extended_limit(&result), refused, "{result:?}");
}

#[rstest]
#[case::no_trailing_newline(b"11 size=10".as_slice())]
#[case::with_newline(b"11 size=10\n".as_slice())]
#[case::second_record(b"12 path=abc\n11 size=10".as_slice())]
fn given_a_pax_size_record_when_unpacked_then_it_is_refused_with_or_without_a_newline(
    #[case] body: &[u8],
) {
    // Arrange
    let mut tar = extension(b'x', body);
    tar.extend(block(b'0', &octal(0)));

    // Act
    let result = run(&tar, Policy::Snapshot, &Limits::snapshot());

    // Assert
    assert!(
        matches!(result, Err(ArchiveError::ForbiddenHeader(_))),
        "{result:?}"
    );
}

#[test]
fn given_a_pax_body_the_tar_parser_cannot_read_when_unpacked_then_it_is_refused() {
    // Arrange
    let mut tar = extension(b'x', b"not a record\n");
    tar.extend(block(b'0', &octal(0)));

    // Act
    let result = run(&tar, Policy::Snapshot, &Limits::snapshot());

    // Assert
    assert!(
        matches!(result, Err(ArchiveError::ForbiddenHeader(_))),
        "{result:?}"
    );
}

#[test]
fn given_a_header_with_a_bad_checksum_when_unpacked_then_it_is_refused() {
    // Arrange
    let mut header = block(b'0', &octal(0));
    header[CKSUM_OFFSET] = b'7';
    let tar = header.to_vec();

    // Act
    let result = run(&tar, Policy::Snapshot, &Limits::snapshot());

    // Assert
    assert!(
        matches!(result, Err(ArchiveError::ForbiddenHeader(_))),
        "{result:?}"
    );
}
