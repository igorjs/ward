// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

use std::sync::Arc;
use std::time::Instant;

use tonic::{Request, Response, Status};
use tracing::error;

use crate::pb::ward_server::Ward;
use crate::pb::{
    CommunicationLogEntry, CommunicationLogResponse, CreateSandboxRequest, CreateSnapshotRequest,
    CreateVolumeRequest, DaemonInfo, EgressLogEntry, EgressLogResponse, ExecRequest,
    GetCommunicationLogRequest, GetEgressLogRequest, GetSandboxRequest, GetVolumeRequest,
    HealthStatus, KillProcessRequest, ListSandboxesResponse, ListSnapshotsRequest,
    ListSnapshotsResponse, ListVolumesResponse, Message, ProcessInfo, PublishRequest,
    RemoveSandboxRequest, RemoveVolumeRequest, RestoreSnapshotRequest, RunRequest, SandboxInfo,
    SnapshotInfo, StreamEvent, StreamOutputRequest, SubscribeRequest, VolumeInfo,
    WriteStdinRequest,
};
use crate::protocol::ApiError;
use crate::sandbox::SandboxManager;
use crate::volume::VolumeManager;

// ---------------------------------------------------------------------------
// Error mapping: internal details logged server-side, generic message to client
// ---------------------------------------------------------------------------

/// Convert an ApiError to a gRPC Status, logging the real error server-side
/// and returning a safe generic message to the client. This prevents internal
/// implementation details (paths, errno codes, backend types) from leaking
/// through the API.
fn api_err_to_status(err: ApiError) -> Status {
    match &err {
        ApiError::SandboxNotFound(id) => Status::not_found(format!("sandbox not found: {id}")),
        ApiError::VolumeNotFound(id) => Status::not_found(format!("volume not found: {id}")),
        ApiError::SnapshotNotFound(id) => Status::not_found(format!("snapshot not found: {id}")),
        ApiError::ProcessNotFound(id) => Status::not_found(format!("process not found: {id}")),
        ApiError::ImageNotFound(id) => Status::not_found(format!("image not found: {id}")),
        ApiError::InvalidRequest(msg) => Status::invalid_argument(msg.clone()),
        ApiError::Backend(detail) => {
            error!(error = %detail, "backend error");
            Status::internal("internal error")
        }
        ApiError::Internal(detail) => {
            error!(error = %detail, "internal error");
            Status::internal("internal error")
        }
    }
}

// ---------------------------------------------------------------------------
// gRPC server
// ---------------------------------------------------------------------------

/// The main gRPC service implementation that delegates to domain managers.
pub struct WardGrpcServer {
    pub sandbox: Arc<SandboxManager>,
    pub volume: Arc<VolumeManager>,
    pub started_at: Instant,
}

impl WardGrpcServer {
    pub fn new(sandbox: Arc<SandboxManager>, volume: Arc<VolumeManager>) -> Self {
        Self {
            sandbox,
            volume,
            started_at: Instant::now(),
        }
    }
}

#[tonic::async_trait]
impl Ward for WardGrpcServer {
    #[tracing::instrument(
        name = "rpc.create_sandbox",
        skip_all,
        fields(request_id = %uuid::Uuid::new_v4(), image = %request.get_ref().image)
    )]
    async fn create_sandbox(
        &self,
        request: Request<CreateSandboxRequest>,
    ) -> Result<Response<SandboxInfo>, Status> {
        let req = request.into_inner();
        let info = self.sandbox.create(req).await.map_err(api_err_to_status)?;
        Ok(Response::new(info))
    }

    #[tracing::instrument(
        name = "rpc.get_sandbox",
        skip_all,
        fields(request_id = %uuid::Uuid::new_v4(), sandbox_id = %request.get_ref().id)
    )]
    async fn get_sandbox(
        &self,
        request: Request<GetSandboxRequest>,
    ) -> Result<Response<SandboxInfo>, Status> {
        let req = request.into_inner();
        let info = self.sandbox.get(&req.id).await.map_err(api_err_to_status)?;
        Ok(Response::new(info))
    }

    #[tracing::instrument(
        name = "rpc.list_sandboxes",
        skip_all,
        fields(request_id = %uuid::Uuid::new_v4())
    )]
    async fn list_sandboxes(
        &self,
        _request: Request<()>,
    ) -> Result<Response<ListSandboxesResponse>, Status> {
        let sandboxes = self.sandbox.list().await.map_err(api_err_to_status)?;
        Ok(Response::new(ListSandboxesResponse { sandboxes }))
    }

    #[tracing::instrument(
        name = "rpc.remove_sandbox",
        skip_all,
        fields(request_id = %uuid::Uuid::new_v4(), sandbox_id = %request.get_ref().id)
    )]
    async fn remove_sandbox(
        &self,
        request: Request<RemoveSandboxRequest>,
    ) -> Result<Response<()>, Status> {
        let req = request.into_inner();
        self.sandbox
            .remove(&req.id)
            .await
            .map_err(api_err_to_status)?;
        Ok(Response::new(()))
    }

    #[tracing::instrument(
        name = "rpc.exec",
        skip_all,
        fields(request_id = %uuid::Uuid::new_v4(), sandbox_id = %request.get_ref().sandbox_id)
    )]
    async fn exec(&self, request: Request<ExecRequest>) -> Result<Response<ProcessInfo>, Status> {
        let req = request.into_inner();
        let info = self.sandbox.exec(req).await.map_err(api_err_to_status)?;
        Ok(Response::new(info))
    }

    #[tracing::instrument(
        name = "rpc.run",
        skip_all,
        fields(request_id = %uuid::Uuid::new_v4(), sandbox_id = %request.get_ref().sandbox_id)
    )]
    async fn run(&self, request: Request<RunRequest>) -> Result<Response<ProcessInfo>, Status> {
        let req = request.into_inner();
        let info = self.sandbox.run(req).await.map_err(api_err_to_status)?;
        Ok(Response::new(info))
    }

    type StreamOutputStream = tokio_stream::wrappers::ReceiverStream<Result<StreamEvent, Status>>;

    #[tracing::instrument(
        name = "rpc.stream_output",
        skip_all,
        fields(
            request_id = %uuid::Uuid::new_v4(),
            sandbox_id = %request.get_ref().sandbox_id,
            pid = %request.get_ref().pid,
        )
    )]
    async fn stream_output(
        &self,
        request: Request<StreamOutputRequest>,
    ) -> Result<Response<Self::StreamOutputStream>, Status> {
        let req = request.into_inner();

        // Manager runs the entity_id validators on both fields and the
        // cross-sandbox ownership check before handing over the receiver.
        let mut inner_rx = self
            .sandbox
            .stream_output(&req.sandbox_id, &req.pid)
            .await
            .map_err(api_err_to_status)?;

        // Bridge: drain protocol::StreamEvent on the manager side, convert
        // to the pb shape, push into the tonic-typed channel. A dedicated
        // task keeps the bridge cancellation-safe — if the client hangs up,
        // the bound channel's send fails and the task exits cleanly,
        // dropping the inner receiver and shutting the pipeline down.
        let (out_tx, out_rx) = tokio::sync::mpsc::channel::<Result<StreamEvent, Status>>(16);
        tokio::spawn(async move {
            while let Some(evt) = inner_rx.recv().await {
                let pb_evt = stream_event_to_pb(evt);
                if out_tx.send(Ok(pb_evt)).await.is_err() {
                    break;
                }
            }
        });

        Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(
            out_rx,
        )))
    }

    #[tracing::instrument(
        name = "rpc.write_stdin",
        skip_all,
        fields(
            request_id = %uuid::Uuid::new_v4(),
            sandbox_id = %request.get_ref().sandbox_id,
            pid = %request.get_ref().pid,
        )
    )]
    async fn write_stdin(
        &self,
        request: Request<WriteStdinRequest>,
    ) -> Result<Response<()>, Status> {
        let req = request.into_inner();
        self.sandbox
            .write_stdin(&req.sandbox_id, &req.pid, bytes::Bytes::from(req.data))
            .await
            .map_err(api_err_to_status)?;
        Ok(Response::new(()))
    }

    #[tracing::instrument(
        name = "rpc.kill_process",
        skip_all,
        fields(
            request_id = %uuid::Uuid::new_v4(),
            sandbox_id = %request.get_ref().sandbox_id,
            pid = %request.get_ref().pid,
        )
    )]
    async fn kill_process(
        &self,
        request: Request<KillProcessRequest>,
    ) -> Result<Response<()>, Status> {
        let req = request.into_inner();
        self.sandbox
            .kill_process(&req.sandbox_id, &req.pid)
            .await
            .map_err(api_err_to_status)?;
        Ok(Response::new(()))
    }

    #[tracing::instrument(
        name = "rpc.create_snapshot",
        skip_all,
        fields(request_id = %uuid::Uuid::new_v4(), sandbox_id = %request.get_ref().sandbox_id)
    )]
    async fn create_snapshot(
        &self,
        request: Request<CreateSnapshotRequest>,
    ) -> Result<Response<SnapshotInfo>, Status> {
        let req = request.into_inner();
        let info = self
            .sandbox
            .create_snapshot(&req.sandbox_id, &req.label)
            .await
            .map_err(api_err_to_status)?;
        Ok(Response::new(snapshot_info_to_pb(info)))
    }

    #[tracing::instrument(
        name = "rpc.restore_snapshot",
        skip_all,
        fields(
            request_id = %uuid::Uuid::new_v4(),
            sandbox_id = %request.get_ref().sandbox_id,
            snapshot_id = %request.get_ref().snapshot_id,
        )
    )]
    async fn restore_snapshot(
        &self,
        request: Request<RestoreSnapshotRequest>,
    ) -> Result<Response<()>, Status> {
        let req = request.into_inner();
        self.sandbox
            .restore_snapshot(&req.sandbox_id, &req.snapshot_id)
            .await
            .map_err(api_err_to_status)?;
        Ok(Response::new(()))
    }

    #[tracing::instrument(
        name = "rpc.list_snapshots",
        skip_all,
        fields(request_id = %uuid::Uuid::new_v4(), sandbox_id = %request.get_ref().sandbox_id)
    )]
    async fn list_snapshots(
        &self,
        request: Request<ListSnapshotsRequest>,
    ) -> Result<Response<ListSnapshotsResponse>, Status> {
        let req = request.into_inner();
        let snaps = self
            .sandbox
            .list_snapshots(&req.sandbox_id)
            .await
            .map_err(api_err_to_status)?;
        let snapshots = snaps.into_iter().map(snapshot_info_to_pb).collect();
        Ok(Response::new(ListSnapshotsResponse { snapshots }))
    }

    #[tracing::instrument(
        name = "rpc.create_volume",
        skip_all,
        fields(request_id = %uuid::Uuid::new_v4(), name = %request.get_ref().name)
    )]
    async fn create_volume(
        &self,
        request: Request<CreateVolumeRequest>,
    ) -> Result<Response<VolumeInfo>, Status> {
        let req = request.into_inner();
        let info = self.volume.create(req).await.map_err(api_err_to_status)?;
        Ok(Response::new(info))
    }

    #[tracing::instrument(
        name = "rpc.get_volume",
        skip_all,
        fields(request_id = %uuid::Uuid::new_v4(), volume_id = %request.get_ref().id)
    )]
    async fn get_volume(
        &self,
        request: Request<GetVolumeRequest>,
    ) -> Result<Response<VolumeInfo>, Status> {
        let req = request.into_inner();
        let info = self.volume.get(&req.id).await.map_err(api_err_to_status)?;
        Ok(Response::new(info))
    }

    #[tracing::instrument(
        name = "rpc.list_volumes",
        skip_all,
        fields(request_id = %uuid::Uuid::new_v4())
    )]
    async fn list_volumes(
        &self,
        _request: Request<()>,
    ) -> Result<Response<ListVolumesResponse>, Status> {
        let volumes = self.volume.list().await.map_err(api_err_to_status)?;
        Ok(Response::new(ListVolumesResponse { volumes }))
    }

    #[tracing::instrument(
        name = "rpc.remove_volume",
        skip_all,
        fields(request_id = %uuid::Uuid::new_v4(), volume_id = %request.get_ref().id)
    )]
    async fn remove_volume(
        &self,
        request: Request<RemoveVolumeRequest>,
    ) -> Result<Response<()>, Status> {
        let req = request.into_inner();
        self.volume
            .remove(&req.id)
            .await
            .map_err(api_err_to_status)?;
        Ok(Response::new(()))
    }

    #[tracing::instrument(
        name = "rpc.get_egress_log",
        skip_all,
        fields(request_id = %uuid::Uuid::new_v4(), sandbox_id = %request.get_ref().sandbox_id)
    )]
    async fn get_egress_log(
        &self,
        request: Request<GetEgressLogRequest>,
    ) -> Result<Response<EgressLogResponse>, Status> {
        let req = request.into_inner();
        let log = self
            .sandbox
            .egress_log(&req.sandbox_id)
            .await
            .map_err(api_err_to_status)?;

        let entries = log
            .into_iter()
            .map(|e| {
                let d = e
                    .timestamp
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default();
                EgressLogEntry {
                    sandbox_id: e.sandbox_id,
                    domain: e.domain,
                    // The proto carries the port as a string.
                    port: e.port.to_string(),
                    allowed: e.allowed,
                    timestamp: Some(prost_types::Timestamp {
                        seconds: d.as_secs() as i64,
                        nanos: d.subsec_nanos() as i32,
                    }),
                }
            })
            .collect();

        Ok(Response::new(EgressLogResponse { entries }))
    }

    #[tracing::instrument(
        name = "rpc.publish",
        skip_all,
        fields(
            request_id = %uuid::Uuid::new_v4(),
            sandbox_id = %request.get_ref().sandbox_id,
            topic = %request.get_ref().topic,
        )
    )]
    async fn publish(&self, request: Request<PublishRequest>) -> Result<Response<()>, Status> {
        let req = request.into_inner();
        crate::validate::entity_id(&req.sandbox_id, "sandbox").map_err(api_err_to_status)?;
        crate::validate::topic_name(&req.topic).map_err(api_err_to_status)?;
        crate::validate::publish_payload(&req.payload).map_err(api_err_to_status)?;

        // Delivery count is discarded by the proto (response is Empty) —
        // callers learn about fan-out via GetCommunicationLog. Errors
        // here cover Deny policy (InvalidArgument) and unregistered
        // sandbox (NotFound).
        self.sandbox
            .broker()
            .publish(&req.sandbox_id, &req.topic, bytes::Bytes::from(req.payload))
            .await
            .map_err(api_err_to_status)?;
        Ok(Response::new(()))
    }

    type SubscribeStream = tokio_stream::wrappers::ReceiverStream<Result<Message, Status>>;

    #[tracing::instrument(
        name = "rpc.subscribe",
        skip_all,
        fields(
            request_id = %uuid::Uuid::new_v4(),
            sandbox_id = %request.get_ref().sandbox_id,
            topic = %request.get_ref().topic,
        )
    )]
    async fn subscribe(
        &self,
        request: Request<SubscribeRequest>,
    ) -> Result<Response<Self::SubscribeStream>, Status> {
        let req = request.into_inner();
        crate::validate::entity_id(&req.sandbox_id, "sandbox").map_err(api_err_to_status)?;
        crate::validate::topic_name(&req.topic).map_err(api_err_to_status)?;

        let mut inner_rx = self
            .sandbox
            .broker()
            .subscribe(&req.sandbox_id, &req.topic)
            .await
            .map_err(api_err_to_status)?;

        // Bridge: drain DeliveredMessage on the broker side, convert to
        // the pb shape, push into the tonic-typed channel. Same cancellation
        // model as StreamOutput — client hangup closes out_tx, the bridge
        // task exits, inner_rx drops, broker reaps the subscription on its
        // next publish.
        let (out_tx, out_rx) = tokio::sync::mpsc::channel::<Result<Message, Status>>(16);
        tokio::spawn(async move {
            while let Some(msg) = inner_rx.recv().await {
                let pb_msg = delivered_message_to_pb(msg);
                if out_tx.send(Ok(pb_msg)).await.is_err() {
                    break;
                }
            }
        });

        Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(
            out_rx,
        )))
    }

    #[tracing::instrument(
        name = "rpc.get_communication_log",
        skip_all,
        fields(request_id = %uuid::Uuid::new_v4(), sandbox_id = %request.get_ref().sandbox_id)
    )]
    async fn get_communication_log(
        &self,
        request: Request<GetCommunicationLogRequest>,
    ) -> Result<Response<CommunicationLogResponse>, Status> {
        let req = request.into_inner();
        crate::validate::entity_id(&req.sandbox_id, "sandbox").map_err(api_err_to_status)?;

        let entries = self.sandbox.broker().log(&req.sandbox_id).await;
        let pb_entries = entries.into_iter().map(log_entry_to_pb).collect();
        Ok(Response::new(CommunicationLogResponse {
            entries: pb_entries,
        }))
    }

    #[tracing::instrument(
        name = "rpc.get_health",
        skip_all,
        fields(request_id = %uuid::Uuid::new_v4())
    )]
    async fn get_health(&self, _request: Request<()>) -> Result<Response<HealthStatus>, Status> {
        let uptime_seconds = self.started_at.elapsed().as_secs();
        let sandbox_count = self.sandbox.count().await.map_err(api_err_to_status)? as u32;

        Ok(Response::new(HealthStatus {
            status: "ok".to_string(),
            uptime_seconds,
            sandbox_count,
            checked_at: Some(prost_types::Timestamp {
                seconds: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs() as i64,
                nanos: 0,
            }),
        }))
    }

    #[tracing::instrument(
        name = "rpc.get_info",
        skip_all,
        fields(request_id = %uuid::Uuid::new_v4())
    )]
    async fn get_info(&self, _request: Request<()>) -> Result<Response<DaemonInfo>, Status> {
        Ok(Response::new(DaemonInfo {
            version: super::VERSION.to_string(),
            platform: std::env::consts::OS.to_string(),
            backend: "krunvm".to_string(),
            arch: std::env::consts::ARCH.to_string(),
        }))
    }
}

// ---------------------------------------------------------------------------
// Conversions
// ---------------------------------------------------------------------------

/// Translate an internal `protocol::StreamEvent` into the on-wire pb shape.
/// `exit_code` is widened from `Option<i32>` to `i32` because protobuf has
/// no native optional integer; callers distinguish "exit event" by checking
/// the `r#type` field, not by sniffing the integer.
fn stream_event_to_pb(evt: crate::protocol::StreamEvent) -> StreamEvent {
    use crate::pb::StreamEventType;
    use crate::protocol::StreamEventKind;

    let r#type = match evt.kind {
        StreamEventKind::Stdout => StreamEventType::Stdout,
        StreamEventKind::Stderr => StreamEventType::Stderr,
        StreamEventKind::Exit => StreamEventType::Exit,
    } as i32;

    let timestamp = evt
        .timestamp
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| prost_types::Timestamp {
            seconds: d.as_secs() as i64,
            nanos: d.subsec_nanos() as i32,
        });

    StreamEvent {
        r#type,
        line: evt.line,
        exit_code: evt.exit_code.unwrap_or(0),
        timestamp,
        duration_ms: evt.duration_ms,
    }
}

/// Convert a SystemTime into a protobuf Timestamp. Returns None for
/// pre-epoch values (which shouldn't happen in practice but we handle
/// gracefully rather than panic).
fn system_time_to_pb(ts: std::time::SystemTime) -> Option<prost_types::Timestamp> {
    ts.duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| prost_types::Timestamp {
            seconds: d.as_secs() as i64,
            nanos: d.subsec_nanos() as i32,
        })
}

/// Translate a broker DeliveredMessage into the pb Message wire shape.
fn delivered_message_to_pb(msg: crate::comms::DeliveredMessage) -> Message {
    Message {
        topic: msg.topic,
        from_sandbox: msg.from_sandbox,
        payload: msg.payload.to_vec(),
        timestamp: system_time_to_pb(msg.timestamp),
    }
}

/// Translate a broker LogEntry into the pb CommunicationLogEntry wire shape.
fn log_entry_to_pb(entry: crate::comms::LogEntry) -> CommunicationLogEntry {
    CommunicationLogEntry {
        from_sandbox: entry.from_sandbox,
        topic: entry.topic,
        allowed: entry.allowed,
        subscriber_count: entry.subscriber_count,
        timestamp: system_time_to_pb(entry.timestamp),
    }
}

/// Translate an internal SnapshotInfo into the pb wire shape.
fn snapshot_info_to_pb(info: crate::protocol::SnapshotInfo) -> SnapshotInfo {
    SnapshotInfo {
        snapshot_id: info.snapshot_id,
        sandbox_id: info.sandbox_id,
        label: info.label,
        created_at: system_time_to_pb(info.created_at),
        size_bytes: info.size_bytes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::Backend;
    use crate::backend::image::{ImagePuller, ImageStore};
    use crate::backend::krunvm::KrunvmBackend;
    use crate::comms::Broker;
    use crate::sandbox::SandboxManager;
    use crate::volume::VolumeManager;
    use std::path::Path;

    /// Offline image puller: materialises a minimal bin/ rootfs without
    /// touching the network. Mirrors the FakePuller pattern in image.rs
    /// and manager.rs; duplicated here because both are gated
    /// `#[cfg(test)]` and not visible across module boundaries.
    #[derive(Debug)]
    struct FakePuller;

    #[async_trait::async_trait]
    impl ImagePuller for FakePuller {
        async fn pull(
            &self,
            reference: &str,
            dest: &Path,
        ) -> Result<String, crate::backend::BackendError> {
            std::fs::create_dir_all(dest.join("bin")).map_err(crate::backend::BackendError::Io)?;
            let hash: u64 = reference
                .bytes()
                .fold(0u64, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u64));
            Ok(format!("sha256:{hash:016x}"))
        }
    }

    fn build_test_server() -> WardGrpcServer {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().to_path_buf();
        std::mem::forget(dir);
        let cache_dir = path.join("cache").join("images");
        let store = Arc::new(ImageStore::with_puller(cache_dir, 64, Arc::new(FakePuller)));
        let backend: Arc<dyn Backend> = Arc::new(KrunvmBackend::with_image_store_for_test(
            path.clone(),
            store,
        ));
        let broker = Arc::new(Broker::new());
        let sandbox = Arc::new(SandboxManager::new(backend, broker, 8, false));
        let volume = Arc::new(VolumeManager::new(path, 8));
        WardGrpcServer::new(sandbox, volume)
    }

    /// Regression guard: every RPC handler in this file must carry a
    /// `#[tracing::instrument(...)]` attribute. The instrument pattern is
    /// the project's contract for request_id propagation + uniform span
    /// names (`rpc.<method>`). A new handler added without the attribute
    /// would silently disappear from the request_id surface; this static
    /// grep over the source catches that.
    #[test]
    fn given_grpc_server_source_when_inspected_then_every_handler_is_instrumented() {
        let src = include_str!("server.rs");
        // Count `    async fn ` (handlers inside `impl Ward for ...`)
        // vs `#[tracing::instrument` attributes. The two MUST balance.
        // Helpers and free functions use 4-space-prefixed `async fn`
        // only inside the impl block; nothing else in this file matches.
        let handlers = src.matches("    async fn ").count();
        let instruments = src.matches("#[tracing::instrument(").count();
        assert_eq!(
            handlers, instruments,
            "every gRPC handler must have a #[tracing::instrument(...)] attribute; \
             found {handlers} handlers and {instruments} instrument attributes. \
             A new RPC without the attribute would silently lose request_id propagation."
        );
    }

    /// Sanity check that the instrumented handler compiles and runs end
    /// to end. `get_health` is the cheapest handler (no request fields
    /// to validate, no backend work) so it doubles as a smoke test for
    /// the attribute macro expansion.
    #[tokio::test]
    async fn given_instrumented_get_health_when_called_then_returns_ok() {
        let svc = build_test_server();
        let resp = svc.get_health(Request::new(())).await.expect("get_health");
        let inner = resp.into_inner();
        assert_eq!(inner.status, "ok");
    }

    /// All instrumented handler attributes follow the same shape:
    /// `name = "rpc.<method>"` so log lines can be filtered with a
    /// stable prefix. This pins the convention.
    #[test]
    fn given_grpc_server_source_when_inspected_then_every_span_uses_rpc_name_prefix() {
        let src = include_str!("server.rs");
        let mut bad: Vec<&str> = Vec::new();
        for line in src.lines() {
            let trimmed = line.trim();
            if let Some(after) = trimmed.strip_prefix("name = \"") {
                // Only inspect `name = "..."` lines inside an instrument
                // attribute block. Heuristic: those lines sit indented
                // inside `#[tracing::instrument(...)]` and end with `",`.
                if let Some(name) = after.split('"').next()
                    && !name.starts_with("rpc.")
                {
                    bad.push(line);
                }
            }
        }
        assert!(
            bad.is_empty(),
            "every instrumented span must use the `rpc.<method>` name prefix; offenders: {bad:?}"
        );
    }
}
