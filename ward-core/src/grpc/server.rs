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
    async fn create_sandbox(
        &self,
        request: Request<CreateSandboxRequest>,
    ) -> Result<Response<SandboxInfo>, Status> {
        let req = request.into_inner();
        let info = self.sandbox.create(req).await.map_err(api_err_to_status)?;
        Ok(Response::new(info))
    }

    async fn get_sandbox(
        &self,
        request: Request<GetSandboxRequest>,
    ) -> Result<Response<SandboxInfo>, Status> {
        let req = request.into_inner();
        let info = self.sandbox.get(&req.id).await.map_err(api_err_to_status)?;
        Ok(Response::new(info))
    }

    async fn list_sandboxes(
        &self,
        _request: Request<()>,
    ) -> Result<Response<ListSandboxesResponse>, Status> {
        let sandboxes = self.sandbox.list().await.map_err(api_err_to_status)?;
        Ok(Response::new(ListSandboxesResponse { sandboxes }))
    }

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

    async fn exec(&self, request: Request<ExecRequest>) -> Result<Response<ProcessInfo>, Status> {
        let req = request.into_inner();
        let info = self.sandbox.exec(req).await.map_err(api_err_to_status)?;
        Ok(Response::new(info))
    }

    async fn run(&self, request: Request<RunRequest>) -> Result<Response<ProcessInfo>, Status> {
        let req = request.into_inner();
        let info = self.sandbox.run(req).await.map_err(api_err_to_status)?;
        Ok(Response::new(info))
    }

    type StreamOutputStream = tokio_stream::wrappers::ReceiverStream<Result<StreamEvent, Status>>;

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

    async fn create_volume(
        &self,
        request: Request<CreateVolumeRequest>,
    ) -> Result<Response<VolumeInfo>, Status> {
        let req = request.into_inner();
        let info = self.volume.create(req).await.map_err(api_err_to_status)?;
        Ok(Response::new(info))
    }

    async fn get_volume(
        &self,
        request: Request<GetVolumeRequest>,
    ) -> Result<Response<VolumeInfo>, Status> {
        let req = request.into_inner();
        let info = self.volume.get(&req.id).await.map_err(api_err_to_status)?;
        Ok(Response::new(info))
    }

    async fn list_volumes(
        &self,
        _request: Request<()>,
    ) -> Result<Response<ListVolumesResponse>, Status> {
        let volumes = self.volume.list().await.map_err(api_err_to_status)?;
        Ok(Response::new(ListVolumesResponse { volumes }))
    }

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
