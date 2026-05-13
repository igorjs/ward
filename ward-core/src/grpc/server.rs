// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

use std::sync::Arc;
use std::time::Instant;

use tonic::{Request, Response, Status};
use tracing::error;

use crate::pb::ward_server::Ward;
use crate::pb::{
    CommunicationLogResponse, CreateSandboxRequest, CreateSnapshotRequest, CreateVolumeRequest,
    DaemonInfo, EgressLogResponse, ExecRequest, GetCommunicationLogRequest, GetEgressLogRequest,
    GetSandboxRequest, GetVolumeRequest, HealthStatus, KillProcessRequest, ListSandboxesResponse,
    ListSnapshotsRequest, ListSnapshotsResponse, ListVolumesResponse, Message, ProcessInfo,
    PublishRequest, RemoveSandboxRequest, RemoveVolumeRequest, RestoreSnapshotRequest, RunRequest,
    SandboxInfo, SnapshotInfo, StreamEvent, StreamOutputRequest, SubscribeRequest, VolumeInfo,
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
        _request: Request<KillProcessRequest>,
    ) -> Result<Response<()>, Status> {
        Err(Status::unimplemented("kill_process"))
    }

    async fn create_snapshot(
        &self,
        request: Request<CreateSnapshotRequest>,
    ) -> Result<Response<SnapshotInfo>, Status> {
        // Validate identity inputs at the API boundary so that the eventual
        // backend implementation can rely on well-formed IDs. The contract
        // we're locking in here outlives `Unimplemented`: when snapshots
        // ship, these validators continue to gate the happy path.
        let req = request.into_inner();
        crate::validate::entity_id(&req.sandbox_id, "sandbox").map_err(api_err_to_status)?;
        Err(Status::unimplemented("create_snapshot"))
    }

    async fn restore_snapshot(
        &self,
        request: Request<RestoreSnapshotRequest>,
    ) -> Result<Response<()>, Status> {
        let req = request.into_inner();
        crate::validate::entity_id(&req.sandbox_id, "sandbox").map_err(api_err_to_status)?;
        crate::validate::entity_id(&req.snapshot_id, "snapshot").map_err(api_err_to_status)?;
        Err(Status::unimplemented("restore_snapshot"))
    }

    async fn list_snapshots(
        &self,
        request: Request<ListSnapshotsRequest>,
    ) -> Result<Response<ListSnapshotsResponse>, Status> {
        let req = request.into_inner();
        crate::validate::entity_id(&req.sandbox_id, "sandbox").map_err(api_err_to_status)?;
        Err(Status::unimplemented("list_snapshots"))
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
        _request: Request<GetEgressLogRequest>,
    ) -> Result<Response<EgressLogResponse>, Status> {
        Err(Status::unimplemented("get_egress_log"))
    }

    async fn publish(&self, request: Request<PublishRequest>) -> Result<Response<()>, Status> {
        let req = request.into_inner();
        // Validate at the boundary so malformed requests fail fast with a
        // distinct status code, even before the broker is implemented.
        crate::validate::entity_id(&req.sandbox_id, "sandbox").map_err(api_err_to_status)?;
        crate::validate::topic_name(&req.topic).map_err(api_err_to_status)?;
        crate::validate::publish_payload(&req.payload).map_err(api_err_to_status)?;
        Err(Status::unimplemented("publish"))
    }

    type SubscribeStream = tokio_stream::wrappers::ReceiverStream<Result<Message, Status>>;

    async fn subscribe(
        &self,
        request: Request<SubscribeRequest>,
    ) -> Result<Response<Self::SubscribeStream>, Status> {
        let req = request.into_inner();
        crate::validate::entity_id(&req.sandbox_id, "sandbox").map_err(api_err_to_status)?;
        crate::validate::topic_name(&req.topic).map_err(api_err_to_status)?;
        Err(Status::unimplemented("subscribe"))
    }

    async fn get_communication_log(
        &self,
        request: Request<GetCommunicationLogRequest>,
    ) -> Result<Response<CommunicationLogResponse>, Status> {
        let req = request.into_inner();
        crate::validate::entity_id(&req.sandbox_id, "sandbox").map_err(api_err_to_status)?;
        Err(Status::unimplemented("get_communication_log"))
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
