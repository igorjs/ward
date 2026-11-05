// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

//! Port publishing through the manager, with a forwarder that serves a
//! fixed reply from a real loopback listener instead of a guest.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Mutex as StdMutex;

use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpStream};

use pretty_assertions::assert_eq;

use super::*;
use crate::backend::ports::OpenPort;

const GUEST_REPLY: &str = "hello from the guest";
const HTTP_PORT: u32 = 80;
const HTTPS_PORT: u32 = 443;
const ANY_PORT: u32 = 0;
const LOOPBACK: &str = "127.0.0.1";
const ALL_INTERFACES: &str = "0.0.0.0";
/// A documentation address no host owns, used to find the outbound interface.
const PROBE_PEER: &str = "192.0.2.1:9";
/// Longest the LAN reachability probe waits for a connect.
const LAN_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Every test binds port 0 and some re-bind a freed port. The OS hands a
/// freed port to the next bind, so parallel tests would steal each other's.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    SERIAL.lock().await
}

struct FakeForwarder {
    /// Bind address and guest port of every `open`.
    opened: StdMutex<Vec<(SocketAddr, u16)>>,
    /// Protocol of every `open`, in the same order.
    protocols: StdMutex<Vec<crate::protocol::PortProtocol>>,
}

impl FakeForwarder {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            opened: StdMutex::new(Vec::new()),
            protocols: StdMutex::new(Vec::new()),
        })
    }

    fn opened(&self) -> Vec<(SocketAddr, u16)> {
        self.opened.lock().unwrap().clone()
    }
}

struct FakePort {
    addr: SocketAddr,
    serving: tokio::task::JoinHandle<()>,
}

#[async_trait::async_trait]
impl OpenPort for FakePort {
    fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    async fn close(mut self: Box<Self>) {
        self.serving.abort();
        let _ = (&mut self.serving).await;
    }
}

/// A dropped listener frees its port only after this lag; `close` does not wait.
const RELEASE_LAG: std::time::Duration = std::time::Duration::from_millis(200);

impl Drop for FakePort {
    fn drop(&mut self) {
        let abort = self.serving.abort_handle();
        tokio::spawn(async move {
            tokio::time::sleep(RELEASE_LAG).await;
            abort.abort();
        });
    }
}

impl FakeForwarder {
    /// A UDP socket that holds the port and serves nothing.
    async fn open_udp(bind: SocketAddr) -> crate::backend::Result<PortListener> {
        let socket = tokio::net::UdpSocket::bind(bind).await.map_err(|e| {
            if e.kind() == std::io::ErrorKind::AddrInUse {
                BackendError::AddressInUse(bind.to_string())
            } else {
                BackendError::Internal(e.to_string())
            }
        })?;
        let addr = socket.local_addr().unwrap();
        let serving = tokio::spawn(async move {
            let _held = socket;
            std::future::pending::<()>().await;
        });
        Ok(Box::new(FakePort { addr, serving }))
    }
}

#[async_trait::async_trait]
impl PortForwarder for FakeForwarder {
    async fn open(
        &self,
        _id: &str,
        bind: SocketAddr,
        guest_port: u16,
        protocol: crate::protocol::PortProtocol,
    ) -> crate::backend::Result<PortListener> {
        use tokio::io::AsyncWriteExt;
        self.protocols.lock().unwrap().push(protocol);
        if protocol == crate::protocol::PortProtocol::Udp {
            return Self::open_udp(bind).await;
        }
        self.opened.lock().unwrap().push((bind, guest_port));
        let listener = TcpListener::bind(bind).await.map_err(|e| {
            if e.kind() == std::io::ErrorKind::AddrInUse {
                BackendError::AddressInUse(bind.to_string())
            } else {
                BackendError::Internal(e.to_string())
            }
        })?;
        let addr = listener.local_addr().unwrap();
        let serving = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let _ = stream.write_all(GUEST_REPLY.as_bytes()).await;
            }
        });
        Ok(Box::new(FakePort { addr, serving }))
    }
}

fn with_forwarder(mut mgr: Arc<SandboxManager>, fwd: &Arc<FakeForwarder>) -> Arc<SandboxManager> {
    let forwarder: Arc<dyn PortForwarder> = Arc::clone(fwd) as Arc<dyn PortForwarder>;
    Arc::get_mut(&mut mgr)
        .expect("manager is not shared yet")
        .forwarder = forwarder;
    mgr
}

fn port_manager() -> (Arc<SandboxManager>, Arc<FakeForwarder>) {
    let fwd = FakeForwarder::new();
    (with_forwarder(host_manager(MAIN_OUTPUT_CAP), &fwd), fwd)
}

/// Highest port a test range starts from, so the range end stays valid.
const LOW_PORT_CEILING: u16 = 65_000;
/// Ports a test range spans beyond its first.
const RANGE_SPAN: u32 = 8;

/// A bound loopback port with room above it for a range.
fn bind_low_port() -> std::net::TcpListener {
    loop {
        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        if listener.local_addr().unwrap().port() <= LOW_PORT_CEILING {
            return listener;
        }
    }
}

fn mapping(host_ip: &str, host_port: u32, container_port: u32) -> crate::pb::PortMapping {
    crate::pb::PortMapping {
        host_ip: host_ip.to_string(),
        host_port,
        container_port,
        ..Default::default()
    }
}

fn req_with(ports: Vec<crate::pb::PortMapping>, command: &[&str]) -> CreateSandboxRequest {
    CreateSandboxRequest {
        published_ports: ports,
        command: command.iter().map(|part| part.to_string()).collect(),
        ..create_req("alpine")
    }
}

fn host_ports(info: &PbSandboxInfo) -> Vec<u16> {
    info.published_ports
        .iter()
        .map(|m| u16::try_from(m.host_port).unwrap())
        .collect()
}

async fn dial(port: u16) -> std::io::Result<String> {
    let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).await?;
    let mut reply = String::new();
    stream.read_to_string(&mut reply).await?;
    Ok(reply)
}

async fn create_published(mgr: &SandboxManager, command: &[&str]) -> (String, u16) {
    let req = req_with(vec![mapping("", ANY_PORT, HTTP_PORT)], command);
    let info = mgr.create(req, "").await.expect("create");
    let port = host_ports(&info)[0];
    (info.id, port)
}

#[tokio::test]
async fn given_any_host_port_and_a_guest_server_when_host_port_is_dialed_then_the_reply_arrives() {
    let _serial = serial().await;
    // Arrange
    let (mgr, fwd) = port_manager();

    // Act
    let (_, port) = create_published(&mgr, &[]).await;
    let reply = dial(port).await;

    // Assert
    assert_eq!(reply.unwrap(), GUEST_REPLY);
    assert_eq!(fwd.opened()[0].1, 80);
}

#[tokio::test]
async fn given_two_published_ports_when_the_sandbox_is_read_then_both_mappings_are_listed() {
    let _serial = serial().await;
    // Arrange
    let (mgr, _) = port_manager();
    let ports = vec![
        mapping("", ANY_PORT, HTTP_PORT),
        mapping("", ANY_PORT, HTTPS_PORT),
    ];

    // Act
    let created = mgr.create(req_with(ports, &[]), "").await.unwrap();
    let info = mgr
        .get_authorized(&created.id, &created.token)
        .await
        .unwrap();

    // Assert
    let container: Vec<u32> = info
        .published_ports
        .iter()
        .map(|m| m.container_port)
        .collect();
    assert_eq!(container, vec![HTTP_PORT, HTTPS_PORT]);
    let hosts = host_ports(&info);
    assert!(hosts.iter().all(|port| *port != 0));
    assert_ne!(hosts[0], hosts[1]);
}

#[tokio::test]
async fn given_no_host_address_when_published_then_it_binds_loopback_and_refuses_other_addresses() {
    let _serial = serial().await;
    // Arrange
    let (mgr, fwd) = port_manager();

    // Act
    let (_, port) = create_published(&mgr, &[]).await;

    // Assert
    assert_eq!(fwd.opened()[0].0.ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
    let outbound = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).unwrap();
    if outbound.connect(PROBE_PEER).is_ok() {
        let local = outbound.local_addr().unwrap().ip();
        if !local.is_loopback() {
            // A silent drop counts as unreachable, so bound the wait.
            let attempt =
                tokio::time::timeout(LAN_PROBE_TIMEOUT, TcpStream::connect((local, port))).await;
            assert!(!matches!(attempt, Ok(Ok(_))), "reachable on {local}");
        }
    }
}

#[tokio::test]
async fn given_an_explicit_host_address_when_published_then_that_address_is_bound() {
    let _serial = serial().await;
    // Arrange
    let (mgr, fwd) = port_manager();
    let ports = vec![mapping(ALL_INTERFACES, ANY_PORT, HTTP_PORT)];

    // Act
    mgr.create(req_with(ports, &[]), "").await.unwrap();

    // Assert
    assert_eq!(fwd.opened()[0].0.ip(), IpAddr::V4(Ipv4Addr::UNSPECIFIED));
}

#[tokio::test]
async fn given_a_host_port_in_use_when_creating_then_it_fails_naming_the_port_and_leaves_nothing() {
    let _serial = serial().await;
    // Arrange
    let tmp = tempfile::tempdir().unwrap();
    let (mgr, _) = manager_with_backend(tmp.path(), 2);
    let fwd = FakeForwarder::new();
    let mgr = with_forwarder(mgr, &fwd);
    let occupied = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let busy = occupied.local_addr().unwrap().port();
    let ports = vec![mapping(LOOPBACK, u32::from(busy), HTTP_PORT)];

    // Act
    let err = mgr.create(req_with(ports, &[]), "").await.unwrap_err();

    // Assert
    assert!(matches!(err, ApiError::AddressInUse(_)), "got {err:?}");
    assert!(err.to_string().contains(&busy.to_string()), "{err}");
    assert!(mgr.list().await.unwrap().is_empty());
    let leftovers: Vec<_> = std::fs::read_dir(tmp.path().join("sandboxes"))
        .map(|dir| dir.map(|entry| entry.unwrap().file_name()).collect())
        .unwrap_or_default();
    assert!(leftovers.is_empty(), "left behind: {leftovers:?}");
}

#[tokio::test]
async fn given_an_exited_sandbox_when_restarted_then_the_same_host_port_serves_again() {
    let _serial = serial().await;
    // Arrange
    let (mgr, _) = port_manager();
    let (id, port) = create_published(&mgr, &["sh", "-c", "exit 0"]).await;
    assert!(wait_until(|| async { status_of(&mgr, &id).await == SandboxStatus::Exited }).await);

    // Act
    mgr.restart(&id).await.expect("restart");

    // Assert
    assert_eq!(dial(port).await.unwrap(), GUEST_REPLY);
}

#[tokio::test]
async fn given_a_published_port_when_stopped_and_started_then_it_reopens_on_the_same_host_port() {
    let _serial = serial().await;
    // Arrange
    let (mgr, fwd) = port_manager();
    let (id, port) = create_published(&mgr, &[]).await;

    // Act
    mgr.stop(&id).await.unwrap();
    mgr.start(&id).await.unwrap();

    // Assert
    let reopened = fwd.opened()[1].0;
    assert_eq!(reopened.port(), port);
    assert!(dial(port).await.is_ok());
}

#[tokio::test]
async fn given_a_published_port_when_the_sandbox_stops_then_connections_are_refused() {
    let _serial = serial().await;
    // Arrange
    let (mgr, _) = port_manager();
    let (id, port) = create_published(&mgr, &[]).await;
    assert!(dial(port).await.is_ok());

    // Act
    mgr.stop(&id).await.unwrap();

    // Assert
    let err = dial(port).await.unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::ConnectionRefused);
}

#[tokio::test]
async fn given_a_removed_sandbox_when_the_same_host_port_is_published_at_once_then_it_succeeds() {
    let _serial = serial().await;
    // Arrange: the fake frees a port only when its task ends, so a
    // listener that is merely dropped still holds it for a while.
    let (mgr, _) = port_manager();
    let (id, port) = create_published(&mgr, &[]).await;
    mgr.remove(&id).await.unwrap();

    // Act: no waiting between the remove and the next create.
    let req = req_with(vec![mapping(LOOPBACK, u32::from(port), HTTP_PORT)], &[]);
    let created = mgr.create(req, "").await;

    // Assert
    assert_eq!(host_ports(&created.expect("create")), vec![port]);
}

#[tokio::test]
async fn given_a_busy_host_port_when_starting_then_it_fails_naming_the_port_and_a_later_start_works()
 {
    let _serial = serial().await;
    // Arrange
    let (mgr, _) = port_manager();
    let (id, port) = create_published(&mgr, &[]).await;
    mgr.stop(&id).await.unwrap();
    let occupied = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, port)).unwrap();

    // Act
    let err = mgr.start(&id).await.unwrap_err();

    // Assert
    assert!(matches!(err, ApiError::AddressInUse(_)), "got {err:?}");
    assert!(err.to_string().contains(&port.to_string()), "{err}");
    assert_eq!(status_of(&mgr, &id).await, SandboxStatus::Failed);
    drop(occupied);
    mgr.start(&id).await.expect("start after the port frees");
    assert_eq!(dial(port).await.unwrap(), GUEST_REPLY);
}

#[tokio::test]
async fn given_the_stub_backend_when_publishing_then_create_fails_and_nothing_is_listed() {
    let _serial = serial().await;
    // Arrange
    let mgr = build_manager(2);
    let ports = vec![mapping("", ANY_PORT, HTTP_PORT)];

    // Act
    let err = mgr.create(req_with(ports, &[]), "").await.unwrap_err();

    // Assert
    assert!(err.to_string().contains("needs a microVM"), "{err}");
    assert!(mgr.list().await.unwrap().is_empty());
}

#[tokio::test]
async fn given_no_published_ports_when_created_then_the_forwarder_is_not_used() {
    let _serial = serial().await;
    // Arrange
    let (mgr, fwd) = port_manager();

    // Act
    let info = mgr.create(create_req("alpine"), "").await.unwrap();

    // Assert
    assert!(info.published_ports.is_empty());
    assert!(fwd.opened().is_empty());
}

#[tokio::test]
async fn given_a_udp_mapping_when_published_then_the_forwarder_opens_it_as_udp() {
    let _serial = serial().await;
    // Arrange
    let (mgr, fwd) = port_manager();
    let ports = vec![crate::pb::PortMapping {
        protocol: "udp".into(),
        ..mapping(LOOPBACK, ANY_PORT, HTTP_PORT)
    }];

    // Act
    let info = mgr.create(req_with(ports, &[]), "").await.unwrap();

    // Assert
    assert_eq!(
        *fwd.protocols.lock().unwrap(),
        vec![crate::protocol::PortProtocol::Udp]
    );
    assert_eq!(info.published_ports[0].protocol, "udp");
}

#[tokio::test]
async fn given_a_host_range_with_its_first_port_busy_when_published_then_a_later_port_is_bound() {
    let _serial = serial().await;
    // Arrange
    let (mgr, _) = port_manager();
    let occupied = bind_low_port();
    let busy = u32::from(occupied.local_addr().unwrap().port());
    let range_end = busy + RANGE_SPAN;
    let ports = vec![crate::pb::PortMapping {
        host_port_end: range_end,
        ..mapping(LOOPBACK, busy, HTTP_PORT)
    }];

    // Act
    let info = mgr.create(req_with(ports, &[]), "").await.unwrap();

    // Assert
    let chosen = info.published_ports[0].host_port;
    assert!((busy + 1..=range_end).contains(&chosen), "chose {chosen}");
}

#[tokio::test]
async fn given_a_host_range_that_is_all_busy_when_published_then_it_fails_with_address_in_use() {
    let _serial = serial().await;
    // Arrange
    let (mgr, _) = port_manager();
    let occupied = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let busy = u32::from(occupied.local_addr().unwrap().port());
    let ports = vec![crate::pb::PortMapping {
        host_port_end: busy,
        ..mapping(LOOPBACK, busy, HTTP_PORT)
    }];

    // Act
    let err = mgr.create(req_with(ports, &[]), "").await.unwrap_err();

    // Assert
    assert!(matches!(err, ApiError::AddressInUse(_)), "got {err:?}");
}

#[tokio::test]
async fn given_a_busy_udp_host_port_when_published_then_it_fails_with_address_in_use() {
    let _serial = serial().await;
    // Arrange
    let (mgr, _) = port_manager();
    let occupied = std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let busy = u32::from(occupied.local_addr().unwrap().port());
    let ports = vec![crate::pb::PortMapping {
        protocol: "udp".into(),
        ..mapping(LOOPBACK, busy, HTTP_PORT)
    }];

    // Act
    let err = mgr.create(req_with(ports, &[]), "").await.unwrap_err();

    // Assert
    assert!(matches!(err, ApiError::AddressInUse(_)), "got {err:?}");
}

#[tokio::test]
async fn given_a_range_listed_before_a_fixed_port_inside_it_when_published_then_both_open() {
    let _serial = serial().await;
    // Arrange
    let (mgr, _) = port_manager();
    let probe = bind_low_port();
    let wanted = u32::from(probe.local_addr().unwrap().port());
    drop(probe);
    let ports = vec![
        crate::pb::PortMapping {
            host_port_end: wanted + RANGE_SPAN,
            ..mapping(LOOPBACK, wanted, HTTP_PORT)
        },
        mapping(LOOPBACK, wanted, HTTPS_PORT),
    ];

    // Act
    let info = mgr.create(req_with(ports, &[]), "").await.unwrap();

    // Assert
    let by_container = |port: u32| {
        info.published_ports
            .iter()
            .find(|m| m.container_port == port)
            .unwrap()
            .host_port
    };
    assert_eq!(by_container(HTTPS_PORT), wanted);
    assert_ne!(by_container(HTTP_PORT), wanted);
}
