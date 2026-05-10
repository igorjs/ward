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
        _request: Request<StreamOutputRequest>,
    ) -> Result<Response<Self::StreamOutputStream>, Status> {
        Err(Status::unimplemented("stream_output"))
    }

    async fn write_stdin(
        &self,
        _request: Request<WriteStdinRequest>,
    ) -> Result<Response<()>, Status> {
        Err(Status::unimplemented("write_stdin"))
    }

    async fn kill_process(
        &self,
        _request: Request<KillProcessRequest>,
    ) -> Result<Response<()>, Status> {
        Err(Status::unimplemented("kill_process"))
    }

    async fn create_snapshot(
        &self,
        _request: Request<CreateSnapshotRequest>,
    ) -> Result<Response<SnapshotInfo>, Status> {
        Err(Status::unimplemented("create_snapshot"))
    }

    async fn restore_snapshot(
        &self,
        _request: Request<RestoreSnapshotRequest>,
    ) -> Result<Response<()>, Status> {
        Err(Status::unimplemented("restore_snapshot"))
    }

    async fn list_snapshots(
        &self,
        _request: Request<ListSnapshotsRequest>,
    ) -> Result<Response<ListSnapshotsResponse>, Status> {
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

    async fn publish(&self, _request: Request<PublishRequest>) -> Result<Response<()>, Status> {
        Err(Status::unimplemented("publish"))
    }

    type SubscribeStream = tokio_stream::wrappers::ReceiverStream<Result<Message, Status>>;

    async fn subscribe(
        &self,
        _request: Request<SubscribeRequest>,
    ) -> Result<Response<Self::SubscribeStream>, Status> {
        Err(Status::unimplemented("subscribe"))
    }

    async fn get_communication_log(
        &self,
        _request: Request<GetCommunicationLogRequest>,
    ) -> Result<Response<CommunicationLogResponse>, Status> {
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
