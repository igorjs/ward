// Copyright 2026 Ward Contributors. SPDX-License-Identifier: AGPL-3.0-only

use std::sync::Arc;
use std::time::Instant;

use tokio_stream::StreamExt;
use tonic::{Request, Response, Status};
use tracing::error;

use super::attach;
use crate::pb::attach_message::Kind;
use crate::pb::copy_to_message;
use crate::pb::ward_server::Ward;
use crate::pb::{
    AttachMessage, CommunicationLogEntry, CommunicationLogResponse, CopyFromChunk, CopyFromRequest,
    CopyToMessage, CreateSandboxRequest, CreateSnapshotRequest, CreateVolumeRequest, DaemonInfo,
    EgressLogEntry, EgressLogResponse, EventsRequest, ExecRequest, GetCommunicationLogRequest,
    GetEgressLogRequest, GetSandboxRequest, GetVolumeRequest, HealthStatus, KillProcessRequest,
    ListSandboxesResponse, ListSnapshotsRequest, ListSnapshotsResponse, ListVolumesResponse,
    Message, ProcessInfo, PublishRequest, RemoveSandboxRequest, RemoveVolumeRequest,
    RestartRequest, RestoreSnapshotRequest, RunRequest, SandboxEvent as PbSandboxEvent,
    SandboxInfo, SnapshotInfo, StartRequest, StatsRequest, StatsResponse, StopRequest, StreamEvent,
    StreamOutputRequest, SubscribeRequest, TopProcess, TopRequest, TopResponse, VolumeInfo,
    WaitRequest, WaitResponse, WriteStdinRequest,
};
use crate::protocol::ApiError;
use crate::sandbox::{MAIN_PROCESS_PID, SandboxManager};
use crate::volume::VolumeManager;

// ---------------------------------------------------------------------------
// Error mapping: internal details logged server-side, generic message to client
// ---------------------------------------------------------------------------

/// Convert an ApiError to a gRPC Status, logging the real error server-side
/// and returning a safe generic message to the client. This prevents internal
/// implementation details (paths, errno codes, backend types) from leaking
/// through the API.
pub(super) fn api_err_to_status(err: ApiError) -> Status {
    match &err {
        ApiError::SandboxNotFound(reference) => {
            Status::not_found(format!("No such container: {reference}"))
        }
        ApiError::NameInUse { name, id } => Status::already_exists(name_conflict_message(name, id)),
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
        ApiError::PermissionDenied(msg) => Status::permission_denied(msg.clone()),
        ApiError::AmbiguousReference(msg) => Status::failed_precondition(msg.clone()),
    }
}

/// Docker's wording, so scripts and users that know it recognise the error.
fn name_conflict_message(name: &str, id: &Option<String>) -> String {
    let holder = id
        .as_ref()
        .map(|id| format!(" by container \"{id}\""))
        .unwrap_or_default();
    format!(
        "Conflict. The container name \"/{name}\" is already in use{holder}. \
         You have to remove (or rename) that container to be able to reuse that name."
    )
}

/// Signal that ends an exec process; the backend cannot deliver others.
const KILL_SIGNAL: i32 = 9;

/// An exec process can only be killed, so any other signal is refused
/// instead of being silently turned into a kill.
fn check_signal_supported(pid: &str, signal: i32) -> Result<(), ApiError> {
    if pid != MAIN_PROCESS_PID && signal != 0 && signal != KILL_SIGNAL {
        return Err(ApiError::InvalidRequest(format!(
            "only signal {KILL_SIGNAL} can be sent to an exec process, got {signal}"
        )));
    }
    Ok(())
}

/// Turn a name, id or id prefix into a full sandbox id. Handlers call this
/// before authorizing, so the token is checked against the resolved id.
async fn resolve_sandbox(sandbox: &SandboxManager, reference: &str) -> Result<String, Status> {
    sandbox.resolve(reference).await.map_err(api_err_to_status)
}

/// Resolve `reference` to a sandbox id and check `token` against that id.
async fn authorized_id(
    sandbox: &SandboxManager,
    reference: &str,
    token: &str,
) -> Result<String, Status> {
    let id = resolve_sandbox(sandbox, reference).await?;
    sandbox
        .authorize(&id, token)
        .await
        .map_err(api_err_to_status)?;
    Ok(id)
}

/// Largest data frame a `CopyTo` client may send, as the proto documents.
const MAX_COPY_DATA_BYTES: usize = crate::backend::agent::ARCHIVE_CHUNK_BYTES;

/// Events queued for one `Events` client before its bridge task waits for it.
const EVENT_STREAM_BUFFER: usize = 16;
/// Attribute names Docker uses on container events.
const EVENT_IMAGE_ATTRIBUTE: &str = "image";
const EVENT_NAME_ATTRIBUTE: &str = "name";
const EVENT_EXIT_CODE_ATTRIBUTE: &str = "exitCode";

const TOKEN_METADATA_KEY: &str = "x-ward-sandbox-token";

/// Pass the data frames of a `CopyTo` stream on to the copy. A frame past
/// the documented size, or any other kind, ends the stream with an error.
async fn forward_copy_data<S>(
    mut inbound: S,
    feed: tokio::sync::mpsc::Sender<bytes::Bytes>,
) -> Result<(), Status>
where
    S: tokio_stream::Stream<Item = Result<CopyToMessage, Status>> + Unpin,
{
    while let Some(message) = inbound.next().await {
        match message?.kind {
            Some(copy_to_message::Kind::Data(data)) if data.len() <= MAX_COPY_DATA_BYTES => {
                if feed.send(bytes::Bytes::from(data)).await.is_err() {
                    break;
                }
            }
            Some(copy_to_message::Kind::Data(_)) => {
                return Err(Status::invalid_argument(format!(
                    "copy data frames hold at most {MAX_COPY_DATA_BYTES} bytes"
                )));
            }
            _ => return Err(Status::invalid_argument("expected a data message")),
        }
    }
    Ok(())
}

/// Read the caller-presented capability token off request metadata. A
/// missing key and a value that fails ASCII/UTF-8 decoding are both
/// treated as "no token" (empty string): `SandboxManager::authorize`
/// rejects an empty token the same way it rejects a wrong one, so callers
/// don't need a separate error path for a malformed header.
fn token_from_metadata<T>(request: &Request<T>) -> &str {
    optional_token_from_metadata(request).unwrap_or("")
}

/// Like [`token_from_metadata`], but tells a missing header (`None`) apart
/// from a present one. An undecodable value counts as present and empty,
/// so it is rejected rather than treated as an anonymous caller.
fn optional_token_from_metadata<T>(request: &Request<T>) -> Option<&str> {
    request
        .metadata()
        .get(TOKEN_METADATA_KEY)
        .map(|value| value.to_str().unwrap_or(""))
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
        let token = token_from_metadata(&request).to_string();
        let req = request.into_inner();
        let info = self
            .sandbox
            .create(req, &token)
            .await
            .map_err(api_err_to_status)?;
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
        // A missing header is the public view; a present one must be valid.
        let token = optional_token_from_metadata(&request).map(str::to_string);
        let req = request.into_inner();
        let info = match token {
            Some(token) => self.sandbox.get_authorized(&req.id, &token).await,
            None => self.sandbox.get(&req.id).await,
        }
        .map_err(api_err_to_status)?;
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
        let token = token_from_metadata(&request).to_string();
        let req = request.into_inner();
        let id = resolve_sandbox(&self.sandbox, &req.id).await?;
        self.sandbox
            .authorize(&id, &token)
            .await
            .map_err(api_err_to_status)?;
        self.sandbox.remove(&id).await.map_err(api_err_to_status)?;
        Ok(Response::new(()))
    }

    #[tracing::instrument(
        name = "rpc.stop",
        skip_all,
        fields(request_id = %uuid::Uuid::new_v4(), sandbox_id = %request.get_ref().id)
    )]
    async fn stop(&self, request: Request<StopRequest>) -> Result<Response<SandboxInfo>, Status> {
        let token = token_from_metadata(&request).to_string();
        let req = request.into_inner();
        let id = authorized_id(&self.sandbox, &req.id, &token).await?;
        let info = self
            .sandbox
            .stop_within(&id, req.grace_seconds)
            .await
            .map_err(api_err_to_status)?;
        Ok(Response::new(info))
    }

    #[tracing::instrument(
        name = "rpc.start",
        skip_all,
        fields(request_id = %uuid::Uuid::new_v4(), sandbox_id = %request.get_ref().id)
    )]
    async fn start(&self, request: Request<StartRequest>) -> Result<Response<SandboxInfo>, Status> {
        let token = token_from_metadata(&request).to_string();
        let req = request.into_inner();
        let id = authorized_id(&self.sandbox, &req.id, &token).await?;
        let info = self.sandbox.start(&id).await.map_err(api_err_to_status)?;
        Ok(Response::new(info))
    }

    #[tracing::instrument(
        name = "rpc.restart",
        skip_all,
        fields(request_id = %uuid::Uuid::new_v4(), sandbox_id = %request.get_ref().id)
    )]
    async fn restart(
        &self,
        request: Request<RestartRequest>,
    ) -> Result<Response<SandboxInfo>, Status> {
        let token = token_from_metadata(&request).to_string();
        let req = request.into_inner();
        let id = authorized_id(&self.sandbox, &req.id, &token).await?;
        let info = self
            .sandbox
            .restart_within(&id, req.grace_seconds)
            .await
            .map_err(api_err_to_status)?;
        Ok(Response::new(info))
    }

    #[tracing::instrument(
        name = "rpc.exec",
        skip_all,
        fields(request_id = %uuid::Uuid::new_v4(), sandbox_id = %request.get_ref().sandbox_id)
    )]
    async fn exec(&self, request: Request<ExecRequest>) -> Result<Response<ProcessInfo>, Status> {
        let token = token_from_metadata(&request).to_string();
        let mut req = request.into_inner();
        let sandbox_id = resolve_sandbox(&self.sandbox, &req.sandbox_id).await?;
        self.sandbox
            .authorize(&sandbox_id, &token)
            .await
            .map_err(api_err_to_status)?;
        // Run against the id the token was checked for, never a second lookup.
        req.sandbox_id = sandbox_id;
        let info = self.sandbox.exec(req).await.map_err(api_err_to_status)?;
        Ok(Response::new(info))
    }

    #[tracing::instrument(
        name = "rpc.run",
        skip_all,
        fields(request_id = %uuid::Uuid::new_v4(), sandbox_id = %request.get_ref().sandbox_id)
    )]
    async fn run(&self, request: Request<RunRequest>) -> Result<Response<ProcessInfo>, Status> {
        let token = token_from_metadata(&request).to_string();
        let mut req = request.into_inner();
        let sandbox_id = resolve_sandbox(&self.sandbox, &req.sandbox_id).await?;
        self.sandbox
            .authorize(&sandbox_id, &token)
            .await
            .map_err(api_err_to_status)?;
        // Run against the id the token was checked for, never a second lookup.
        req.sandbox_id = sandbox_id;
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
        let token = token_from_metadata(&request).to_string();
        let req = request.into_inner();
        crate::validate::entity_id(&req.pid, "process").map_err(api_err_to_status)?;
        let sandbox_id = resolve_sandbox(&self.sandbox, &req.sandbox_id).await?;
        self.sandbox
            .authorize(&sandbox_id, &token)
            .await
            .map_err(api_err_to_status)?;

        // Manager re-resolves the sandbox, re-validates the pid and
        // performs the cross-sandbox ownership check before handing over
        // the receiver.
        let mut inner_rx = self
            .sandbox
            .stream_output(&sandbox_id, &req.pid)
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
        let token = token_from_metadata(&request).to_string();
        let req = request.into_inner();
        crate::validate::entity_id(&req.pid, "process").map_err(api_err_to_status)?;
        let sandbox_id = resolve_sandbox(&self.sandbox, &req.sandbox_id).await?;
        self.sandbox
            .authorize(&sandbox_id, &token)
            .await
            .map_err(api_err_to_status)?;
        self.sandbox
            .write_stdin(&sandbox_id, &req.pid, bytes::Bytes::from(req.data))
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
        let token = token_from_metadata(&request).to_string();
        let req = request.into_inner();
        crate::validate::entity_id(&req.pid, "process").map_err(api_err_to_status)?;
        check_signal_supported(&req.pid, req.signal).map_err(api_err_to_status)?;
        let sandbox_id = resolve_sandbox(&self.sandbox, &req.sandbox_id).await?;
        self.sandbox
            .authorize(&sandbox_id, &token)
            .await
            .map_err(api_err_to_status)?;
        if req.pid == MAIN_PROCESS_PID {
            self.sandbox
                .signal_main_process(&sandbox_id, req.signal)
                .await
                .map_err(api_err_to_status)?;
        } else {
            self.sandbox
                .kill_process(&sandbox_id, &req.pid)
                .await
                .map_err(api_err_to_status)?;
        }
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
        let token = token_from_metadata(&request).to_string();
        let req = request.into_inner();
        let sandbox_id = resolve_sandbox(&self.sandbox, &req.sandbox_id).await?;
        self.sandbox
            .authorize(&sandbox_id, &token)
            .await
            .map_err(api_err_to_status)?;
        let info = self
            .sandbox
            .create_snapshot(&sandbox_id, &req.label)
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
        let token = token_from_metadata(&request).to_string();
        let req = request.into_inner();
        crate::validate::entity_id(&req.snapshot_id, "snapshot").map_err(api_err_to_status)?;
        let sandbox_id = resolve_sandbox(&self.sandbox, &req.sandbox_id).await?;
        self.sandbox
            .authorize(&sandbox_id, &token)
            .await
            .map_err(api_err_to_status)?;
        self.sandbox
            .restore_snapshot(&sandbox_id, &req.snapshot_id)
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
        let token = token_from_metadata(&request).to_string();
        let req = request.into_inner();
        // list_snapshots is lenient toward an unknown sandbox (empty result,
        // not an error) even before token enforcement: callers use it as a
        // cheap existence check. Any other failure, including PermissionDenied for
        // a real sandbox with a missing or wrong token, still fails here.
        let sandbox_id = match self.sandbox.resolve(&req.sandbox_id).await {
            Ok(id) => id,
            Err(ApiError::SandboxNotFound(_)) => {
                return Ok(Response::new(ListSnapshotsResponse {
                    snapshots: Vec::new(),
                }));
            }
            Err(err) => return Err(api_err_to_status(err)),
        };
        self.sandbox
            .authorize(&sandbox_id, &token)
            .await
            .map_err(api_err_to_status)?;

        let snaps = self
            .sandbox
            .list_snapshots(&sandbox_id)
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
        let token = token_from_metadata(&request).to_string();
        let req = request.into_inner();
        let sandbox_id = resolve_sandbox(&self.sandbox, &req.sandbox_id).await?;
        self.sandbox
            .authorize(&sandbox_id, &token)
            .await
            .map_err(api_err_to_status)?;
        let log = self
            .sandbox
            .egress_log(&sandbox_id)
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
        let token = token_from_metadata(&request).to_string();
        let req = request.into_inner();
        crate::validate::topic_name(&req.topic).map_err(api_err_to_status)?;
        crate::validate::publish_payload(&req.payload).map_err(api_err_to_status)?;
        let sandbox_id = resolve_sandbox(&self.sandbox, &req.sandbox_id).await?;
        self.sandbox
            .authorize(&sandbox_id, &token)
            .await
            .map_err(api_err_to_status)?;

        // Delivery count is discarded by the proto (response is Empty) —
        // callers learn about fan-out via GetCommunicationLog. Errors
        // here cover Deny policy (InvalidArgument) and unregistered
        // sandbox (NotFound).
        self.sandbox
            .broker()
            .publish(&sandbox_id, &req.topic, bytes::Bytes::from(req.payload))
            .await
            .map_err(api_err_to_status)?;
        Ok(Response::new(()))
    }

    type AttachStream = tokio_stream::wrappers::ReceiverStream<Result<AttachMessage, Status>>;

    #[tracing::instrument(name = "rpc.attach", skip_all, fields(request_id = %uuid::Uuid::new_v4()))]
    async fn attach(
        &self,
        request: Request<tonic::Streaming<AttachMessage>>,
    ) -> Result<Response<Self::AttachStream>, Status> {
        let token = token_from_metadata(&request).to_string();
        let mut inbound = request.into_inner();
        let first = inbound.next().await.ok_or_else(|| {
            Status::invalid_argument("attach stream ended before a start message")
        })??;
        let Some(Kind::Start(start)) = first.kind else {
            return Err(Status::invalid_argument(
                "first attach message must be start",
            ));
        };
        let id = authorized_id(&self.sandbox, &start.sandbox_id, &token).await?;
        if !start.user.is_empty() {
            return Err(Status::unimplemented(
                "attach with a user is not supported yet",
            ));
        }
        let rx = attach::serve(Arc::clone(&self.sandbox), id, start, inbound).await?;
        Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(
            rx,
        )))
    }

    #[tracing::instrument(name = "rpc.copy_to", skip_all, fields(request_id = %uuid::Uuid::new_v4()))]
    async fn copy_to(
        &self,
        request: Request<tonic::Streaming<CopyToMessage>>,
    ) -> Result<Response<()>, Status> {
        let token = token_from_metadata(&request).to_string();
        let mut inbound = request.into_inner();
        let first = inbound.next().await.ok_or_else(|| {
            Status::invalid_argument("copy stream ended before a start message")
        })??;
        let Some(copy_to_message::Kind::Start(start)) = first.kind else {
            return Err(Status::invalid_argument("first copy message must be start"));
        };
        let id = authorized_id(&self.sandbox, &start.sandbox_id, &token).await?;
        let (feed, chunks) = tokio::sync::mpsc::channel(crate::backend::COPY_CHANNEL_CAPACITY);
        let pump = tokio::spawn(forward_copy_data(inbound, feed));
        let copied = self.sandbox.copy_to(&id, &start.path, chunks).await;
        // A copy that ended early must not leave the pump waiting on a client
        // that keeps its stream open.
        if copied.is_err() {
            pump.abort();
        }
        // A client that broke the stream must not look like a finished copy.
        match pump.await {
            Ok(Ok(())) => {}
            Ok(Err(status)) => return Err(status),
            Err(e) if e.is_cancelled() => {}
            Err(_) => return Err(Status::internal("internal error")),
        }
        copied.map_err(api_err_to_status)?;
        Ok(Response::new(()))
    }

    type CopyFromStream = std::pin::Pin<
        Box<dyn tokio_stream::Stream<Item = Result<CopyFromChunk, Status>> + Send + 'static>,
    >;

    #[tracing::instrument(
        name = "rpc.copy_from",
        skip_all,
        fields(request_id = %uuid::Uuid::new_v4(), sandbox_id = %request.get_ref().sandbox_id)
    )]
    async fn copy_from(
        &self,
        request: Request<CopyFromRequest>,
    ) -> Result<Response<Self::CopyFromStream>, Status> {
        let token = token_from_metadata(&request).to_string();
        let req = request.into_inner();
        let id = authorized_id(&self.sandbox, &req.sandbox_id, &token).await?;
        let tar = self
            .sandbox
            .copy_from(&id, &req.path)
            .await
            .map_err(api_err_to_status)?;
        let chunks = tokio_stream::wrappers::ReceiverStream::new(tar).map(|item| {
            item.map(|data| CopyFromChunk {
                data: data.to_vec(),
            })
            .map_err(api_err_to_status)
        });
        Ok(Response::new(Box::pin(chunks)))
    }

    #[tracing::instrument(
        name = "rpc.top",
        skip_all,
        fields(request_id = %uuid::Uuid::new_v4(), sandbox_id = %request.get_ref().sandbox_id)
    )]
    async fn top(&self, request: Request<TopRequest>) -> Result<Response<TopResponse>, Status> {
        let token = token_from_metadata(&request).to_string();
        let req = request.into_inner();
        let id = authorized_id(&self.sandbox, &req.sandbox_id, &token).await?;
        let table = self
            .sandbox
            .top(&id, &req.ps_args)
            .await
            .map_err(api_err_to_status)?;
        Ok(Response::new(TopResponse {
            titles: table.titles,
            processes: table
                .rows
                .into_iter()
                .map(|fields| TopProcess { fields })
                .collect(),
        }))
    }

    #[tracing::instrument(
        name = "rpc.stats",
        skip_all,
        fields(request_id = %uuid::Uuid::new_v4(), sandbox_id = %request.get_ref().sandbox_id)
    )]
    async fn stats(
        &self,
        request: Request<StatsRequest>,
    ) -> Result<Response<StatsResponse>, Status> {
        let token = token_from_metadata(&request).to_string();
        let req = request.into_inner();
        let id = authorized_id(&self.sandbox, &req.sandbox_id, &token).await?;
        let stats = self.sandbox.stats(&id).await.map_err(api_err_to_status)?;
        Ok(Response::new(StatsResponse {
            cpu_nanos: stats.cpu_nanos,
            online_cpus: stats.online_cpus,
            mem_used_bytes: stats.mem_used_bytes,
            mem_limit_bytes: stats.mem_limit_bytes,
            pids: stats.pids,
            read_at: Some(prost_types::Timestamp::from(stats.read_at)),
        }))
    }

    #[tracing::instrument(
        name = "rpc.wait",
        skip_all,
        fields(request_id = %uuid::Uuid::new_v4(), sandbox_id = %request.get_ref().sandbox_id)
    )]
    async fn wait(&self, request: Request<WaitRequest>) -> Result<Response<WaitResponse>, Status> {
        let token = token_from_metadata(&request).to_string();
        let req = request.into_inner();
        let id = authorized_id(&self.sandbox, &req.sandbox_id, &token).await?;
        let exit_code = self.sandbox.wait(&id).await.map_err(api_err_to_status)?;
        Ok(Response::new(WaitResponse { exit_code }))
    }

    type EventsStream = tokio_stream::wrappers::ReceiverStream<Result<PbSandboxEvent, Status>>;

    #[tracing::instrument(name = "rpc.events", skip_all, fields(request_id = %uuid::Uuid::new_v4()))]
    async fn events(
        &self,
        request: Request<EventsRequest>,
    ) -> Result<Response<Self::EventsStream>, Status> {
        let req = request.into_inner();
        let since = req.since.map(event_time).transpose()?;
        let until = req.until.map(event_time).transpose()?;
        let reader = self.sandbox.events().subscribe(since);
        let (out_tx, out_rx) = tokio::sync::mpsc::channel(EVENT_STREAM_BUFFER);
        // A client that reads slowly stalls only this task. The manager
        // never waits, and the reader reports what it missed.
        tokio::spawn(forward_events(reader, until, out_tx));
        Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(
            out_rx,
        )))
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
        let token = token_from_metadata(&request).to_string();
        let req = request.into_inner();
        crate::validate::topic_name(&req.topic).map_err(api_err_to_status)?;
        let sandbox_id = resolve_sandbox(&self.sandbox, &req.sandbox_id).await?;
        self.sandbox
            .authorize(&sandbox_id, &token)
            .await
            .map_err(api_err_to_status)?;

        let mut inner_rx = self
            .sandbox
            .broker()
            .subscribe(&sandbox_id, &req.topic)
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
        let token = token_from_metadata(&request).to_string();
        let req = request.into_inner();
        // get_communication_log is lenient toward an unknown sandbox (empty
        // result, not an error) even before token enforcement: callers use
        // it as a cheap existence check. Any other failure, including PermissionDenied for
        // a real sandbox with a missing or wrong token, still fails here.
        let sandbox_id = match self.sandbox.resolve(&req.sandbox_id).await {
            Ok(id) => id,
            Err(ApiError::SandboxNotFound(_)) => {
                return Ok(Response::new(CommunicationLogResponse {
                    entries: Vec::new(),
                }));
            }
            Err(err) => return Err(api_err_to_status(err)),
        };
        self.sandbox
            .authorize(&sandbox_id, &token)
            .await
            .map_err(api_err_to_status)?;

        let entries = self.sandbox.broker().log(&sandbox_id).await;
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
    use crate::protocol::{RawStream, StreamEventKind};

    // The pb event carries text lines only, so raw bytes are shown as lossy
    // UTF-8 here. Attach reads the raw variant directly.
    let line = match &evt.kind {
        StreamEventKind::Data { bytes, .. } => String::from_utf8_lossy(bytes).into_owned(),
        _ => evt.line,
    };

    let r#type = match evt.kind {
        StreamEventKind::Stdout
        | StreamEventKind::Data {
            stream: RawStream::Stdout,
            ..
        } => StreamEventType::Stdout,
        StreamEventKind::Stderr
        | StreamEventKind::Data {
            stream: RawStream::Stderr,
            ..
        } => StreamEventType::Stderr,
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
        line,
        exit_code: evt.exit_code.unwrap_or(0),
        timestamp,
        duration_ms: evt.duration_ms,
    }
}

fn event_time(ts: prost_types::Timestamp) -> Result<std::time::SystemTime, Status> {
    std::time::SystemTime::try_from(ts)
        .map_err(|_| Status::invalid_argument("event time is out of range"))
}

/// Send events to `out` until `until` passes or the client hangs up.
async fn forward_events(
    mut reader: crate::sandbox::events::EventReader,
    until: Option<std::time::SystemTime>,
    out: tokio::sync::mpsc::Sender<Result<PbSandboxEvent, Status>>,
) {
    let deadline = until.map(|until| {
        let left = until
            .duration_since(std::time::SystemTime::now())
            .unwrap_or_default();
        tokio::time::Instant::now() + left
    });
    let expired = async {
        match deadline {
            Some(deadline) => tokio::time::sleep_until(deadline).await,
            None => std::future::pending().await,
        }
    };
    tokio::pin!(expired);
    loop {
        // `closed` notices a client that hung up while no event arrives.
        let next = tokio::select! {
            () = out.closed() => return,
            () = &mut expired => return,
            next = reader.next() => next,
        };
        let Some(delivered) = next else { return };
        if until.is_some_and(|until| delivered.event.at > until) {
            return;
        }
        if out.send(Ok(event_to_pb(delivered))).await.is_err() {
            return;
        }
    }
}

fn event_to_pb(delivered: crate::sandbox::events::Delivered) -> PbSandboxEvent {
    let event = delivered.event;
    let mut attributes = std::collections::HashMap::from([
        (EVENT_IMAGE_ATTRIBUTE.to_string(), event.image),
        (EVENT_NAME_ATTRIBUTE.to_string(), event.name),
    ]);
    if let Some(code) = event.exit_code {
        attributes.insert(EVENT_EXIT_CODE_ATTRIBUTE.to_string(), code.to_string());
    }
    PbSandboxEvent {
        action: event.action.as_str().to_string(),
        sandbox_id: event.sandbox_id,
        at: system_time_to_pb(event.at),
        attributes,
        dropped: delivered.dropped,
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

    #[test]
    fn given_raw_stderr_event_when_mapped_to_pb_then_lossy_stderr_line() {
        // Arrange
        let evt = crate::protocol::StreamEvent {
            kind: crate::protocol::StreamEventKind::Data {
                stream: crate::protocol::RawStream::Stderr,
                bytes: b"a\xffb".to_vec(),
            },
            line: String::new(),
            exit_code: None,
            wait_status: None,
            timestamp: std::time::SystemTime::now(),
            duration_ms: 0,
        };

        // Act
        let pb = stream_event_to_pb(evt);

        // Assert
        assert_eq!(pb.r#type, crate::pb::StreamEventType::Stderr as i32);
        assert_eq!(pb.line, "a\u{fffd}b");
    }

    fn data_frame(len: usize) -> Result<CopyToMessage, Status> {
        Ok(CopyToMessage {
            kind: Some(copy_to_message::Kind::Data(vec![0; len])),
        })
    }

    #[rstest::rstest]
    #[case::at_the_cap(MAX_COPY_DATA_BYTES, None)]
    #[case::over_the_cap(MAX_COPY_DATA_BYTES + 1, Some(tonic::Code::InvalidArgument))]
    #[test]
    fn given_a_copy_data_frame_when_forwarded_then_only_one_over_the_cap_is_refused(
        #[case] len: usize,
        #[case] refused: Option<tonic::Code>,
    ) {
        // Arrange
        let (feed, mut chunks) = tokio::sync::mpsc::channel(crate::backend::COPY_CHANNEL_CAPACITY);
        let inbound = tokio_stream::iter(vec![data_frame(len)]);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();

        // Act
        let result = runtime.block_on(forward_copy_data(inbound, feed));

        // Assert
        assert_eq!(result.err().map(|s| s.code()), refused);
        assert_eq!(chunks.try_recv().is_ok(), refused.is_none());
    }

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
        ) -> Result<crate::backend::image::PulledImage, crate::backend::BackendError> {
            std::fs::create_dir_all(dest.join("bin")).map_err(crate::backend::BackendError::Io)?;
            let hash: u64 = reference
                .bytes()
                .fold(0u64, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u64));
            Ok(crate::backend::image::PulledImage::without_config(format!(
                "sha256:{hash:016x}"
            )))
        }
    }

    #[rstest::rstest]
    #[case::exec_default("1a2b", 0, true)]
    #[case::exec_kill("1a2b", 9, true)]
    #[case::exec_term("1a2b", 15, false)]
    #[case::main_term("0", 15, true)]
    fn given_pid_and_signal_when_checked_then_only_kill_is_allowed_for_exec(
        #[case] pid: &str,
        #[case] signal: i32,
        #[case] allowed: bool,
    ) {
        // Act
        let result = check_signal_supported(pid, signal);

        // Assert
        assert_eq!(result.is_ok(), allowed);
        if let Err(err) = result {
            assert!(matches!(err, ApiError::InvalidRequest(_)));
            assert_eq!(api_err_to_status(err).code(), tonic::Code::InvalidArgument);
        }
    }

    #[test]
    fn given_ambiguous_reference_when_mapped_then_failed_precondition_naming_ids() {
        // Arrange
        let err = ApiError::AmbiguousReference("matches aaaa-1 and aaaa-2".into());

        // Act
        let status = api_err_to_status(err);

        // Assert
        assert_eq!(status.code(), tonic::Code::FailedPrecondition);
        assert!(status.message().contains("aaaa-1") && status.message().contains("aaaa-2"));
    }

    #[test]
    fn given_missing_sandbox_when_mapped_then_not_found_in_docker_wording() {
        // Arrange
        let err = ApiError::SandboxNotFound("web".into());

        // Act
        let status = api_err_to_status(err);

        // Assert
        assert_eq!(status.code(), tonic::Code::NotFound);
        assert_eq!(status.message(), "No such container: web");
    }

    #[test]
    fn given_name_held_by_a_sandbox_when_mapped_then_already_exists_in_docker_wording() {
        // Arrange
        let err = ApiError::NameInUse {
            name: "web".into(),
            id: Some("abc123".into()),
        };

        // Act
        let status = api_err_to_status(err);

        // Assert
        assert_eq!(status.code(), tonic::Code::AlreadyExists);
        assert_eq!(
            status.message(),
            "Conflict. The container name \"/web\" is already in use by container \"abc123\". \
             You have to remove (or rename) that container to be able to reuse that name."
        );
    }

    #[test]
    fn given_name_held_by_a_pending_create_when_mapped_then_message_omits_the_container() {
        // Arrange
        let err = ApiError::NameInUse {
            name: "web".into(),
            id: None,
        };

        // Act
        let status = api_err_to_status(err);

        // Assert
        assert_eq!(status.code(), tonic::Code::AlreadyExists);
        assert!(
            status
                .message()
                .contains("\"/web\" is already in use. You have to remove")
        );
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
        let sandbox = Arc::new(SandboxManager::new(
            backend,
            broker,
            8,
            false,
            crate::config::NetworkBackendChoice::Smoltcp,
        ));
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

    #[test]
    fn given_an_idle_events_client_when_it_hangs_up_then_the_forwarder_ends() {
        // Arrange
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let log = crate::sandbox::events::EventLog::default();
            let (out, rx) = tokio::sync::mpsc::channel(1);
            let task = tokio::spawn(forward_events(log.subscribe(None), None, out));
            tokio::task::yield_now().await;

            // Act
            drop(rx);

            // Assert
            let ended = tokio::time::timeout(std::time::Duration::from_secs(5), task).await;
            assert!(ended.is_ok(), "forwarder outlived its client");
        });
    }
}
