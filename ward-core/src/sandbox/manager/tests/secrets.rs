// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Secret files through the manager: they reach the backend, and their
//! content shows up nowhere the daemon reports or stores.

use std::io::Write;
use std::path::Path;
use std::sync::Mutex as StdMutex;

use pretty_assertions::assert_eq;
use prost::Message;

use super::*;

const SECRET_VALUE: &[u8] = b"hunter2-correct-horse-battery-staple";
const SECRET_NAME: &str = "db-password";
const OWNER: &str = "1000:1000";
const OTHER_OWNER: &str = "2000";
const LONG_RUNNING: [&str; 2] = ["sleep", "30"];
const EVENTS_AFTER_CREATE: usize = 2;

fn secret_file() -> crate::pb::SecretFile {
    crate::pb::SecretFile {
        name: SECRET_NAME.into(),
        content: SECRET_VALUE.to_vec(),
    }
}

fn holds_secret(bytes: &[u8]) -> bool {
    bytes.windows(SECRET_VALUE.len()).any(|w| w == SECRET_VALUE)
}

fn text_holds_secret(text: &str) -> bool {
    holds_secret(text.as_bytes())
}

/// Collects everything the daemon logs during a test.
#[derive(Clone, Default)]
struct LogCapture(Arc<StdMutex<Vec<u8>>>);

impl Write for LogCapture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogCapture {
    type Writer = LogCapture;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Installs the process-wide log capture once. A global subscriber, because
/// a per-thread one can miss log calls other tests registered first.
fn capture_logs() -> LogCapture {
    static CAPTURE: std::sync::OnceLock<LogCapture> = std::sync::OnceLock::new();
    CAPTURE
        .get_or_init(|| {
            let capture = LogCapture::default();
            let subscriber = tracing_subscriber::fmt()
                .with_writer(capture.clone())
                .with_max_level(tracing::Level::TRACE)
                .finish();
            tracing::subscriber::set_global_default(subscriber).expect("one global subscriber");
            capture
        })
        .clone()
}

fn files_under(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir).expect("read_dir").flatten() {
        let path = entry.path();
        if path.is_dir() {
            found.extend(files_under(&path));
        } else {
            found.push(path);
        }
    }
    found
}

fn run_request(secrets: Vec<crate::pb::SecretFile>) -> CreateSandboxRequest {
    CreateSandboxRequest {
        image: "alpine".into(),
        command: LONG_RUNNING.map(String::from).to_vec(),
        user: OWNER.into(),
        secrets,
        ..Default::default()
    }
}

#[tokio::test]
async fn given_a_run_with_secrets_when_inspected_listed_logged_or_stored_then_no_value_appears() {
    // Arrange
    let state = tempfile::tempdir().expect("tempdir");
    let (mgr, _, delivered) = host_manager_in(MAIN_OUTPUT_CAP, false, Some(state.path().into()));
    let logs = capture_logs();
    let request = run_request(vec![secret_file()]);
    let request_debug = format!("{request:?}");

    // Act
    let created = mgr.create(request, "").await.expect("create");
    let inspected = mgr
        .get_authorized(&created.id, &created.token)
        .await
        .expect("get");
    let listed = mgr.list().await.expect("list");
    let mut reader = mgr
        .events()
        .subscribe(Some(std::time::SystemTime::UNIX_EPOCH));
    let mut events = String::new();
    for _ in 0..EVENTS_AFTER_CREATE {
        let next = tokio::time::timeout(TEST_POLL_DEADLINE, reader.next())
            .await
            .expect("event in time")
            .expect("event");
        events.push_str(&format!("{:?}", next.event));
    }

    // Assert
    let delivered = delivered.lock().unwrap().clone();
    assert_eq!(delivered.len(), 1, "the backend received the secret once");
    assert_eq!(delivered[0].0, OWNER);
    assert_eq!(delivered[0].1[0].content, SECRET_VALUE);

    assert!(!text_holds_secret(&request_debug), "request Debug");
    assert!(!holds_secret(&created.encode_to_vec()), "create response");
    assert!(!holds_secret(&inspected.encode_to_vec()), "inspect");
    assert!(
        !listed.iter().any(|s| holds_secret(&s.encode_to_vec())),
        "ps"
    );
    assert!(!text_holds_secret(&events), "events");
    let logged = logs.0.lock().unwrap().clone();
    assert!(!logged.is_empty(), "the capture saw the daemon logs");
    assert!(!holds_secret(&logged), "daemon logs");
    for file in files_under(state.path()) {
        let bytes = std::fs::read(&file).expect("read stored file");
        assert!(!holds_secret(&bytes), "stored file {}", file.display());
    }
    assert!(
        files_under(state.path())
            .iter()
            .any(|p| p.ends_with("meta.json")),
        "the sandbox record was written"
    );
}

#[rstest::rstest]
#[case::sandbox_user("", OWNER)]
#[case::requested_user(OTHER_OWNER, OTHER_OWNER)]
#[tokio::test]
async fn given_an_exec_with_secrets_when_delivered_then_they_belong_to_the_exec_user(
    #[case] requested: &str,
    #[case] expected_owner: &str,
) {
    // Arrange
    let (mgr, _, delivered) = host_manager_in(MAIN_OUTPUT_CAP, false, None);
    let created = mgr
        .create(run_request(Vec::new()), "")
        .await
        .expect("create");
    let request = crate::pb::ExecRequest {
        sandbox_id: created.id,
        command: vec!["true".into()],
        user: requested.into(),
        secrets: vec![secret_file()],
        ..Default::default()
    };

    // Act
    mgr.exec(request).await.expect("exec");

    // Assert
    let delivered = delivered.lock().unwrap().clone();
    assert_eq!(delivered.len(), 1);
    assert_eq!(delivered[0].0, expected_owner);
    assert_eq!(delivered[0].1[0].name, SECRET_NAME);
}

#[rstest::rstest]
#[case::bad_name("../escape", 1)]
#[case::too_big(SECRET_NAME, crate::validate::MAX_SECRET_BYTES + 1)]
#[tokio::test]
async fn given_an_invalid_secret_when_run_then_it_is_refused_without_echoing_content(
    #[case] name: &str,
    #[case] size: usize,
) {
    // Arrange
    let (mgr, _, delivered) = host_manager_in(MAIN_OUTPUT_CAP, false, None);
    let mut content = SECRET_VALUE.to_vec();
    content.resize(size.max(content.len()), b'x');
    let request = run_request(vec![crate::pb::SecretFile {
        name: name.into(),
        content,
    }]);

    // Act
    let error = mgr.create(request, "").await.expect_err("refused");

    // Assert
    assert!(matches!(error, ApiError::InvalidRequest(_)));
    assert!(!text_holds_secret(&error.to_string()));
    assert!(delivered.lock().unwrap().is_empty());
}

#[tokio::test]
async fn given_a_sandbox_created_with_secrets_when_stopped_then_start_and_restart_are_refused() {
    // Arrange
    let state = tempfile::tempdir().expect("tempdir");
    let (mgr, _, _) = host_manager_in(MAIN_OUTPUT_CAP, false, Some(state.path().into()));
    let created = mgr
        .create(run_request(vec![secret_file()]), "")
        .await
        .expect("create");
    mgr.stop(&created.id).await.expect("stop");

    // Act
    let started = mgr.start(&created.id).await.expect_err("start refused");
    let restarted = mgr.restart(&created.id).await.expect_err("restart refused");

    // Assert
    for error in [started, restarted] {
        assert!(matches!(error, ApiError::InvalidRequest(_)));
        assert!(error.to_string().contains("created with --secret"));
    }
}

#[tokio::test]
async fn given_a_sandbox_created_without_secrets_when_stopped_then_start_works() {
    // Arrange
    let (mgr, _, _) = host_manager_in(MAIN_OUTPUT_CAP, false, None);
    let created = mgr
        .create(run_request(Vec::new()), "")
        .await
        .expect("create");
    mgr.stop(&created.id).await.expect("stop");

    // Act
    let started = mgr.start(&created.id).await;

    // Assert
    assert!(started.is_ok());
}
