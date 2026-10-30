// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! `--secret [NAME=]FILE`: read secret files on the client, so the daemon
//! only ever holds their content in memory.

use std::fs::File;
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result, bail};
use ward_core::pb::SecretFile;
use ward_core::validate::{MAX_SECRET_BYTES, MAX_SECRETS, secret_name};

/// Why `create` takes no secrets: they are delivered when a process starts.
pub const CREATE_REFUSAL: &str = "--secret works with run and exec";

/// Fail when `create` was given secrets.
pub fn refuse_on_create(specs: &[String]) -> Result<()> {
    if specs.is_empty() {
        Ok(())
    } else {
        bail!(CREATE_REFUSAL)
    }
}

/// Read every `--secret` value into a secret file.
pub fn load_all(specs: &[String]) -> Result<Vec<SecretFile>> {
    if specs.len() > MAX_SECRETS {
        bail!("at most {MAX_SECRETS} --secret flags can be given");
    }
    specs.iter().map(|spec| load(spec)).collect()
}

fn load(spec: &str) -> Result<SecretFile> {
    let (name, path) = split(spec)?;
    secret_name(&name)?;
    let content = read(Path::new(path))?;
    Ok(SecretFile { name, content })
}

/// Split `NAME=FILE`. A value whose part before the first `=` holds a slash
/// is a path that contains `=`, so the name is the file's base name.
fn split(spec: &str) -> Result<(String, &str)> {
    if let Some((name, path)) = spec.split_once('=')
        && !name.is_empty()
        && !name.contains('/')
    {
        return Ok((name.to_string(), path));
    }
    let name = Path::new(spec)
        .file_name()
        .and_then(|n| n.to_str())
        .with_context(|| format!("--secret {spec:?} has no file name to use as the secret name"))?;
    Ok((name.to_string(), spec))
}

/// Read a regular file of at most `MAX_SECRET_BYTES`. The path is checked
/// before opening, so a FIFO or device cannot block the open, and again on
/// the open handle in case the file was swapped in between.
fn read(path: &Path) -> Result<Vec<u8>> {
    let shown = path.display();
    let before =
        std::fs::metadata(path).with_context(|| format!("cannot stat secret file {shown}"))?;
    if !before.is_file() {
        bail!("secret file {shown} is not a regular file");
    }
    let file = File::open(path).with_context(|| format!("cannot open secret file {shown}"))?;
    let meta = file
        .metadata()
        .with_context(|| format!("cannot stat secret file {shown}"))?;
    if !meta.is_file() {
        bail!("secret file {shown} is not a regular file");
    }
    let mut content = Vec::new();
    file.take(MAX_SECRET_BYTES as u64 + 1)
        .read_to_end(&mut content)
        .with_context(|| format!("cannot read secret file {shown}"))?;
    if content.len() > MAX_SECRET_BYTES {
        bail!("secret file {shown} is over {MAX_SECRET_BYTES} bytes");
    }
    Ok(content)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    fn write(dir: &Path, name: &str, content: &[u8]) -> String {
        let path = dir.join(name);
        std::fs::write(&path, content).expect("write");
        path.to_string_lossy().into_owned()
    }

    #[test]
    fn given_a_bare_path_when_loaded_then_the_name_is_the_base_name() {
        // Arrange
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write(dir.path(), "db-password", b"s3cret");

        // Act
        let files = load_all(&[path]).expect("load");

        // Assert
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].name, "db-password");
        assert_eq!(files[0].content, b"s3cret");
    }

    #[test]
    fn given_name_equals_path_when_loaded_then_the_given_name_is_used() {
        // Arrange
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write(dir.path(), "a.txt", b"x");

        // Act
        let files = load_all(&[format!("token={path}")]).expect("load");

        // Assert
        assert_eq!(files[0].name, "token");
    }

    #[test]
    fn given_a_path_with_an_equals_sign_when_loaded_then_it_is_not_split() {
        // Arrange
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write(dir.path(), "a=b", b"x");

        // Act
        let files = load_all(&[path]).expect("load");

        // Assert
        assert_eq!(files[0].name, "a=b");
    }

    #[rstest]
    #[case::missing("/definitely/not/here")]
    #[case::directory("DIR")]
    fn given_an_unusable_path_when_loaded_then_it_fails(#[case] spec: &str) {
        // Arrange
        let dir = tempfile::tempdir().expect("tempdir");
        let spec = spec.replace("DIR", &dir.path().to_string_lossy());

        // Act
        let result = load_all(&[spec]);

        // Assert
        assert!(result.is_err());
    }

    #[test]
    fn given_a_fifo_when_loaded_then_it_is_refused_without_blocking() {
        // Arrange
        let dir = tempfile::tempdir().expect("tempdir");
        let fifo = dir.path().join("pipe");
        let made = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("mkfifo");
        assert!(made.success());

        // Act
        let error = load_all(&[fifo.to_string_lossy().into_owned()]).expect_err("refused");

        // Assert
        assert!(error.to_string().contains("not a regular file"));
    }

    #[test]
    fn given_an_oversized_file_when_loaded_then_it_fails_without_its_content() {
        // Arrange
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write(dir.path(), "big", &vec![b'q'; MAX_SECRET_BYTES + 1]);

        // Act
        let error = load_all(&[path]).expect_err("too big").to_string();

        // Assert
        assert!(error.contains("over"));
        assert!(!error.contains("qqqq"));
    }

    #[test]
    fn given_too_many_flags_or_a_bad_name_when_loaded_then_it_fails() {
        // Arrange
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write(dir.path(), "a", b"x");
        let many = vec![path.clone(); MAX_SECRETS + 1];

        // Act
        let too_many = load_all(&many);
        let dots = load_all(&[format!("..={path}")]);

        // Assert
        assert!(too_many.is_err());
        assert!(dots.is_err());
    }

    #[test]
    fn given_secrets_on_create_when_checked_then_the_refusal_names_run_and_exec() {
        // Act
        let error = refuse_on_create(&["x".into()]).expect_err("refused");

        // Assert
        assert_eq!(error.to_string(), CREATE_REFUSAL);
        assert!(refuse_on_create(&[]).is_ok());
    }
}
