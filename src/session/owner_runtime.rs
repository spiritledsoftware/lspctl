use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs::{self, OpenOptions},
    io,
    num::NonZeroUsize,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use ::time::{OffsetDateTime, format_description::well_known::Rfc3339};
use async_lsp::{AnyRequest, ErrorCode, ResponseError, router::Router};
use atomic_write_file::AtomicWriteFile;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite},
    net::{TcpListener, TcpStream},
    sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot, watch},
    task::JoinHandle,
    time::{self as tokio_time, Instant as TokioInstant},
};
use tower_service::Service;
use url::Url;

use super::{
    capabilities::{
        CAPABILITY_PROFILE_VERSION, NegotiatedCapabilities, fixed_initialize_capabilities,
        normalize_initialize_result,
    },
    json_rpc_transport::{
        JsonRpcFrame, JsonRpcFrameReader, JsonRpcFrameWriter, JsonRpcTransportError,
    },
    owner_protocol::{
        AuthenticatedOwnerRequest, OWNER_PROTOCOL_VERSION, OWNER_QUEUE_LIMIT, OwnerDocumentInput,
        OwnerEndpoint, OwnerLaunchSettings, OwnerRequest, OwnerResponse,
        constant_time_token_matches, read_owner_message, write_owner_message,
    },
    process_supervision::SupervisedServerProcess,
    session_log::SessionLog,
};
use crate::{
    contract::ContractFailure,
    workspace::{DiagnosticCache, DocumentStore, SynchronizationEvent},
};

const TRACE_CAPACITY_BYTES: usize = 64 * 1024 * 1024;
const SERVER_STDERR_TAIL_BYTES: usize = 64 * 1024;
const OWNER_RESPONSE_FLUSH_TIMEOUT: Duration = Duration::from_secs(1);
const SERVER_REQUEST_LIMIT: usize = 64;
const OWNER_HANDSHAKE_LIMIT: usize = 4;
const OWNER_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) struct OwnerBootstrap {
    pub(crate) session_identity: String,
    pub(crate) owner_generation: String,
    pub(crate) token: String,
    pub(crate) workspace_uri: String,
    pub(crate) workspace_path: PathBuf,
    pub(crate) server: String,
    pub(crate) executable: PathBuf,
    pub(crate) endpoint_path: PathBuf,
    pub(crate) owner_lock_path: PathBuf,
    pub(crate) launch_path: PathBuf,
}

struct PendingOwnerRequest {
    request: OwnerRequest,
    response: oneshot::Sender<OwnerResponse>,
    dispatched: oneshot::Sender<()>,
    delivered: oneshot::Receiver<()>,
    cancelled: watch::Receiver<bool>,
}

struct PendingOwnerStop {
    response: oneshot::Sender<OwnerResponse>,
    delivered: oneshot::Receiver<()>,
}

struct ActiveQuery {
    method: String,
    params: Option<Value>,
    started_at: String,
    response: Option<oneshot::Sender<OwnerResponse>>,
    delivered: Option<oneshot::Receiver<()>>,
    cancelled: watch::Receiver<bool>,
    deadline: TokioInstant,
    timeout: Duration,
    cancellation: Option<QueryCancellation>,
    partial_token: Option<Value>,
    partial_chunks: Vec<Value>,
    partial_bytes: usize,
    trace: Option<ProtocolTrace>,
    validated_documents: Vec<OwnerDocumentInput>,
    raw_request: bool,
    apply_edits: bool,
    apply_edit_ledger: Vec<Value>,
    preauthorized_usage: crate::mutation::PreauthorizedUsage,
    synchronization: Value,
}

struct QueryCancellation {
    deadline: TokioInstant,
    failure: Value,
}

struct OwnerStartupFailure {
    source: io::Error,
    contract: Value,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum ServerRequestId {
    Integer(i64),
    String(String),
}

impl ServerRequestId {
    fn from_value(value: &Value) -> Option<Self> {
        match value {
            Value::Number(value) => value.as_i64().map(Self::Integer),
            Value::String(value) => Some(Self::String(value.clone())),
            _ => None,
        }
    }
}

struct PendingServerRequest {
    message: Value,
}

struct ReaderQuiescenceRequest {
    quiescent: oneshot::Sender<()>,
    resume: oneshot::Receiver<()>,
}

struct LspRuntime {
    process: SupervisedServerProcess,
    reader: Option<JsonRpcFrameReader<tokio::process::ChildStdout>>,
    frames: Option<mpsc::Receiver<Result<Option<JsonRpcFrame>, String>>>,
    reader_quiescence: Option<mpsc::Sender<ReaderQuiescenceRequest>>,
    reader_task: Option<JoinHandle<()>>,
    writer: JsonRpcFrameWriter<tokio::process::ChildStdin>,
    fatal_transport_failure: Option<Value>,
    stderr: mpsc::Receiver<String>,
    stderr_task: Option<JoinHandle<()>>,
    stderr_closed: bool,
    stderr_tail: Arc<Mutex<String>>,
    next_request_id: i64,
    negotiated: NegotiatedCapabilities,
    diagnostics: DiagnosticCache,
    documents: DocumentStore,
    progress: BTreeMap<String, Value>,
    server_requests: BTreeMap<ServerRequestId, PendingServerRequest>,
    processing_server_requests: BTreeSet<ServerRequestId>,
    server_request_order: VecDeque<ServerRequestId>,
    settings: Value,
    workspace_uri: String,
    workspace_path: PathBuf,
    server: String,
    session_identity: String,
    owner_generation: String,
    declaration_digest: Option<String>,
    workspace_folder: Value,
    cancellation_grace: Duration,
    shutdown_timeout: Duration,
    max_partial_result_bytes: usize,
    log: SessionLog,
    startup_trace: Option<ProtocolTrace>,
    preview_settings: crate::configuration::PreviewSettings,
    receipt_settings: crate::configuration::ReceiptSettings,
    mutation_settings: crate::configuration::MutationSettings,
}

pub(crate) async fn run_owner(bootstrap: OwnerBootstrap) -> io::Result<()> {
    let settings = read_launch_settings(&bootstrap.launch_path)?;
    let _ = fs::remove_file(&bootstrap.launch_path);
    let owner_lock = acquire_owner_lock(&bootstrap.owner_lock_path)?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let started_at = now_rfc3339();
    let mut endpoint = OwnerEndpoint {
        format_version: 1,
        owner_protocol_version: OWNER_PROTOCOL_VERSION,
        session_identity: bootstrap.session_identity.clone(),
        owner_generation: bootstrap.owner_generation.clone(),
        token: bootstrap.token.clone(),
        address: listener.local_addr()?.to_string(),
        workspace_uri: bootstrap.workspace_uri.clone(),
        server: bootstrap.server.clone(),
        owner_pid: std::process::id(),
        started_at,
        state: "starting".to_owned(),
        failure: None,
    };
    write_endpoint(&bootstrap.endpoint_path, &endpoint)?;

    endpoint.state = "initializing".to_owned();
    write_endpoint(&bootstrap.endpoint_path, &endpoint)?;

    let started = Instant::now();
    let mut last_connection_closed = Instant::now();
    let mut idle_deadline =
        last_connection_closed + Duration::from_millis(settings.idle_timeout_ms);
    let (requests_tx, mut requests_rx) = mpsc::channel(OWNER_QUEUE_LIMIT);
    let (controls_tx, mut controls_rx) = mpsc::channel(OWNER_QUEUE_LIMIT);
    let (status_tx, status_rx) = watch::channel(initializing_status(&bootstrap));
    let (connection_closed_tx, mut connection_closed_rx) = watch::channel(Instant::now());
    let draining = Arc::new(AtomicBool::new(false));
    let listener_task = spawn_owner_listener(
        listener,
        endpoint.clone(),
        requests_tx,
        controls_tx,
        Arc::clone(&draining),
        status_rx,
        connection_closed_tx,
    );
    let mut lsp = match LspRuntime::start(&bootstrap, &settings).await {
        Ok(runtime) => runtime,
        Err(error) => {
            endpoint.state = "failed".to_owned();
            endpoint.failure = Some(error.contract.clone());
            let _ = write_endpoint(&bootstrap.endpoint_path, &endpoint);
            status_tx.send_modify(|status| status["state"] = json!("failed"));
            fail_queued_requests(
                &bootstrap.owner_generation,
                &mut requests_rx,
                error.contract.clone(),
            )
            .await;
            fail_queued_requests(
                &bootstrap.owner_generation,
                &mut controls_rx,
                error.contract,
            )
            .await;
            listener_task.abort();
            drop(owner_lock);
            return Err(error.source);
        }
    };
    endpoint.state = "ready".to_owned();
    write_endpoint(&bootstrap.endpoint_path, &endpoint)?;
    let mut should_stop = false;
    let mut active_queries = BTreeMap::new();
    let mut pending_stop: Option<PendingOwnerStop> = None;
    status_tx.send_replace(status_result(
        &bootstrap,
        &lsp,
        &endpoint.state,
        started,
        requests_rx.len(),
        &active_queries,
        idle_deadline,
    ));

    let maintenance_period = Duration::from_millis(25);
    let mut maintenance =
        tokio_time::interval_at(TokioInstant::now() + maintenance_period, maintenance_period);
    maintenance.set_missed_tick_behavior(tokio_time::MissedTickBehavior::Skip);

    while !should_stop {
        status_tx.send_replace(status_result(
            &bootstrap,
            &lsp,
            &endpoint.state,
            started,
            requests_rx.len(),
            &active_queries,
            idle_deadline,
        ));
        if active_queries.is_empty()
            && let Some(pending) = pending_stop.take()
        {
            lsp.graceful_shutdown().await;
            let result = stop_result(&bootstrap.owner_generation, false);
            let _ = pending
                .response
                .send(OwnerResponse::success(&bootstrap.owner_generation, result));
            let _ = tokio_time::timeout(Duration::from_secs(1), pending.delivered).await;
            break;
        }
        let idle_sleep = tokio_time::sleep_until(TokioInstant::from_std(idle_deadline));
        tokio::pin!(idle_sleep);
        tokio::select! {
            biased;
            _ = maintenance.tick() => {
                if lsp.maintain_active_queries(
                    &bootstrap.owner_generation,
                    &mut active_queries,
                ).await {
                    should_stop = true;
                }
                if lsp.fatal_transport_failure.is_none()
                    && let Some(status) = lsp.process.try_wait()? {
                    lsp.log.push("lifecycle", "error", format!("Language server exited: {status}"));
                    lsp.finish_server_stderr().await;
                    let failure = server_exited_failure(Some(status), &lsp.server_stderr_tail());
                    fail_active_queries(
                        &bootstrap.owner_generation,
                        &mut active_queries,
                        failure.clone(),
                    ).await;
                    fail_queued_requests(&bootstrap.owner_generation, &mut requests_rx, failure).await;
                    should_stop = true;
                }
            }
            pending = controls_rx.recv() => {
                let Some(pending) = pending else { break };
                if *pending.cancelled.borrow() {
                    continue;
                }
                let _ = pending.dispatched.send(());
                match pending.request {
                    OwnerRequest::Stop { force } => {
                        draining.store(true, Ordering::Release);
                        endpoint.state = "draining".to_owned();
                        let _ = write_endpoint(&bootstrap.endpoint_path, &endpoint);
                        status_tx.send_replace(status_result(
                            &bootstrap,
                            &lsp,
                            &endpoint.state,
                            started,
                            requests_rx.len(),
                            &active_queries,
                            idle_deadline,
                        ));
                        fail_queued_requests(
                            &bootstrap.owner_generation,
                            &mut requests_rx,
                            owner_draining_failure(&bootstrap.session_identity),
                        ).await;
                        if force {
                            fail_active_queries(
                                &bootstrap.owner_generation,
                                &mut active_queries,
                                request_cancelled("force_stop"),
                            ).await;
                            lsp.process.terminate_process_tree(Duration::ZERO).await;
                            let result = stop_result(&bootstrap.owner_generation, true);
                            if let Some(graceful) = pending_stop.take() {
                                let _ = graceful.response.send(OwnerResponse::success(
                                    &bootstrap.owner_generation,
                                    result.clone(),
                                ));
                            }
                            let _ = pending.response.send(OwnerResponse::success(&bootstrap.owner_generation, result));
                            let _ = tokio_time::timeout(Duration::from_secs(1), pending.delivered).await;
                            should_stop = true;
                        } else if pending_stop.is_none() {
                            pending_stop = Some(PendingOwnerStop {
                                response: pending.response,
                                delivered: pending.delivered,
                            });
                        } else {
                            let _ = pending.response.send(OwnerResponse::failure(
                                &bootstrap.owner_generation,
                                owner_draining_failure(&bootstrap.session_identity),
                            ));
                        }
                    }
                    _ => unreachable!("only stop uses the Owner control queue"),
                }
            }
            pending = requests_rx.recv(), if active_queries.is_empty() => {
                let Some(pending) = pending else { break };
                if *pending.cancelled.borrow() {
                    continue;
                }
                let _ = pending.dispatched.send(());
                match pending.request {
                    OwnerRequest::Status => unreachable!("status is served by the Owner listener"),
                    OwnerRequest::Capabilities => {
                        let result = json!({
                            "protocolBaseline": "3.17",
                            "clientProfileVersion": CAPABILITY_PROFILE_VERSION,
                            "providers": lsp.negotiated.providers_json(),
                            "initializeResult": lsp.negotiated.initialize_result,
                            "positionEncoding": lsp.negotiated.position_encoding.name(),
                            "textSynchronization": match lsp.negotiated.text_synchronization {
                                crate::workspace::TextSynchronization::None => "none",
                                crate::workspace::TextSynchronization::OpenClose => "open_close",
                            }
                        });
                        let _ = pending.response.send(OwnerResponse::success(&bootstrap.owner_generation, result));
                    }
                    OwnerRequest::Logs { tail } => {
                        let result = lsp.log.render(&bootstrap.owner_generation, tail);
                        let _ = pending.response.send(OwnerResponse::success(&bootstrap.owner_generation, result));
                    }
                    OwnerRequest::Diagnostics { documents } => {
                        let deadline = TokioInstant::now() + lsp.shutdown_timeout;
                        let response = match lsp.synchronize_documents(&documents, deadline).await {
                            Ok(()) => {
                                let versions = documents.iter().filter_map(|document| {
                                    let uri = Url::from_file_path(&document.path).ok()?.to_string();
                                    let version = lsp.documents.get(&uri)?.version;
                                    Some((uri, json!(version)))
                                }).collect::<serde_json::Map<_, _>>();
                                OwnerResponse::success(&bootstrap.owner_generation, json!({
                                    "state": lsp.diagnostics.export_state(),
                                    "documentVersions": versions
                                }))
                            }
                            Err(failure) => OwnerResponse::failure(
                                &bootstrap.owner_generation,
                                contract_failure_value(failure),
                            ),
                        };
                        let _ = pending.response.send(response);
                        if lsp.fatal_transport_failure.is_some() || !lsp.writer.is_reusable() {
                            wait_for_owner_response_flush(pending.delivered).await;
                        }
                    }
                    OwnerRequest::RefreshDocuments { file_operations } => {
                        let deadline = TokioInstant::now() + lsp.shutdown_timeout;
                        let file_operations_delivered = lsp
                            .send_file_operation_notifications(&file_operations, deadline)
                            .await
                            .is_ok();
                        let (outcomes, failures) = lsp.documents.refresh_open_documents(
                            lsp.negotiated.text_synchronization,
                        );
                        let mut changed = Vec::new();
                        let mut delivery_failed = false;
                        for outcome in outcomes {
                            if !outcome.events.is_empty() {
                                changed.push(outcome.snapshot.uri.clone());
                            }
                            if lsp.send_synchronization_events(outcome.events, deadline).await.is_err() {
                                delivery_failed = true;
                            }
                        }
                        let pending_events = lsp.documents.drain_pending_events();
                        if lsp
                            .send_synchronization_events(pending_events, deadline)
                            .await
                            .is_err()
                        {
                            delivery_failed = true;
                        }
                        let result = json!({
                            "changedUris": changed,
                            "failures": failures.iter().map(contract_failure_ref_value).collect::<Vec<_>>(),
                            "delivered": file_operations_delivered && !delivery_failed && failures.is_empty()
                        });
                        let _ = pending.response.send(OwnerResponse::success(&bootstrap.owner_generation, result));
                        if lsp.fatal_transport_failure.is_some() || !lsp.writer.is_reusable() {
                            wait_for_owner_response_flush(pending.delivered).await;
                        }
                    }
                    OwnerRequest::Stop { force } => {
                        unreachable!("stop requests use the Owner control queue: force={force}");
                    }
                    OwnerRequest::Dispatch { method, params, documents, refresh_open_documents, raw_request, request_timeout_ms, trace_protocol, apply_edits } => {
                        lsp.start_dispatch(
                            &bootstrap.owner_generation,
                            method,
                            params,
                            documents,
                            refresh_open_documents,
                            raw_request,
                            Duration::from_millis(request_timeout_ms),
                            trace_protocol,
                            apply_edits,
                            pending.response,
                            pending.delivered,
                            pending.cancelled,
                            &mut active_queries,
                        ).await;
                    }
                }
            }
            stderr = lsp.stderr.recv(), if !lsp.stderr_closed => {
                if let Some(stderr) = stderr {
                    lsp.log.push("server_stderr", "error", stderr);
                } else {
                    lsp.stderr_closed = true;
                }
            }
            frame = lsp.frames.as_mut().expect("ready Owners have an LSP reader pump").recv() => {
                match frame {
                    Some(Ok(Some(frame))) => {
                        let deadline = active_write_deadline(&active_queries, TokioInstant::now() + lsp.shutdown_timeout);
                        if let Err(error) = lsp.handle_concurrent_frame(
                            &bootstrap.owner_generation,
                            frame,
                            &mut active_queries,
                            deadline,
                        ).await {
                            lsp.log.push("protocol_violation", "error", &error);
                            fail_active_queries(
                                &bootstrap.owner_generation,
                                &mut active_queries,
                                lsp.fatal_transport_failure.clone().unwrap_or_else(|| protocol_failure(error)),
                            ).await;
                            lsp.process.terminate_process_tree(Duration::ZERO).await;
                            should_stop = true;
                        }
                    }
                    Some(Ok(None)) | None => {
                        let status = tokio_time::timeout(
                            Duration::from_secs(1),
                            lsp.process.wait(),
                        )
                        .await
                        .ok()
                        .and_then(Result::ok);
                        lsp.finish_server_stderr().await;
                        let failure = server_exited_failure(
                            status,
                            &lsp.server_stderr_tail(),
                        );
                        fail_active_queries(&bootstrap.owner_generation, &mut active_queries, failure.clone()).await;
                        fail_queued_requests(&bootstrap.owner_generation, &mut requests_rx, failure).await;
                        should_stop = true;
                    }
                    Some(Err(error)) => {
                        let failure = protocol_failure(error);
                        fail_active_queries(
                            &bootstrap.owner_generation,
                            &mut active_queries,
                            failure.clone(),
                        ).await;
                        fail_queued_requests(&bootstrap.owner_generation, &mut requests_rx, failure).await;
                        lsp.process.terminate_process_tree(Duration::ZERO).await;
                        should_stop = true;
                    }
                }
            }
            _ = std::future::ready(()), if lsp.has_pending_server_requests() => {
                let deadline = active_write_deadline(&active_queries, TokioInstant::now() + lsp.shutdown_timeout);
                let result = lsp
                    .process_next_server_request(&mut active_queries, true, deadline)
                    .await;
                if let Err(error) = result {
                    lsp.log.push("protocol_violation", "error", &error);
                    fail_active_queries(
                        &bootstrap.owner_generation,
                        &mut active_queries,
                        lsp.fatal_transport_failure.clone().unwrap_or_else(|| protocol_failure(error)),
                    ).await;
                    lsp.process.terminate_process_tree(Duration::ZERO).await;
                    should_stop = true;
                }
            }
            _ = &mut idle_sleep, if active_queries.is_empty() => {
                lsp.log.push("lifecycle", "info", "Owner idle timeout reached");
                lsp.graceful_shutdown().await;
                should_stop = true;
            }
            changed = connection_closed_rx.changed() => {
                if changed.is_ok() {
                    last_connection_closed = *connection_closed_rx.borrow_and_update();
                    idle_deadline = last_connection_closed
                        + Duration::from_millis(settings.idle_timeout_ms);
                }
            }
        }
        // The failure survives best-effort callers and is checked before any further dispatch.
        if lsp.fatal_transport_failure.is_some() || !lsp.writer.is_reusable() {
            draining.store(true, Ordering::Release);
            listener_task.abort();
            let failure = lsp.fatal_transport_failure.clone().unwrap_or_else(|| {
                transport_failure(&io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "An LSP frame write was abandoned",
                ))
            });
            lsp.process.terminate_process_tree(Duration::ZERO).await;
            fail_active_queries(
                &bootstrap.owner_generation,
                &mut active_queries,
                failure.clone(),
            )
            .await;
            fail_queued_requests(
                &bootstrap.owner_generation,
                &mut requests_rx,
                failure.clone(),
            )
            .await;
            fail_queued_requests(
                &bootstrap.owner_generation,
                &mut controls_rx,
                failure.clone(),
            )
            .await;
            if let Some(pending) = pending_stop.take() {
                let _ = pending
                    .response
                    .send(OwnerResponse::failure(&bootstrap.owner_generation, failure));
            }
            should_stop = true;
        }
    }

    listener_task.abort();
    fail_active_queries(
        &bootstrap.owner_generation,
        &mut active_queries,
        request_cancelled("owner_stopped"),
    )
    .await;
    fail_queued_requests(
        &bootstrap.owner_generation,
        &mut requests_rx,
        request_cancelled("owner_stopped"),
    )
    .await;
    let _ = fs::remove_file(&bootstrap.endpoint_path);
    drop(owner_lock);
    Ok(())
}

impl LspRuntime {
    async fn start(
        bootstrap: &OwnerBootstrap,
        settings: &OwnerLaunchSettings,
    ) -> Result<Self, OwnerStartupFailure> {
        let (process, stdin, stdout, mut stderr) =
            SupervisedServerProcess::spawn(&bootstrap.executable, &settings.server_args).map_err(
                |source| OwnerStartupFailure {
                    contract: server_start_failure(bootstrap, &source),
                    source,
                },
            )?;
        let (stderr_tx, stderr_rx) = mpsc::channel(64);
        let stderr_tail = Arc::new(Mutex::new(String::new()));
        let stderr_tail_writer = Arc::clone(&stderr_tail);
        let stderr_task = tokio::spawn(async move {
            let mut bytes = vec![0; 8192];
            loop {
                match stderr.read(&mut bytes).await {
                    Ok(0) | Err(_) => break,
                    Ok(length) => {
                        let message = String::from_utf8_lossy(&bytes[..length]).into_owned();
                        if let Ok(mut tail) = stderr_tail_writer.lock() {
                            append_bounded_stderr_tail(&mut tail, &message);
                        }
                        let _ = stderr_tx.try_send(message);
                    }
                }
            }
        });
        let body_limit = NonZeroUsize::new(settings.max_message_bytes)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "message limit is zero"))
            .map_err(|source| OwnerStartupFailure {
                contract: initialization_failure(bootstrap, &source, ""),
                source,
            })?;
        let root_name = bootstrap
            .workspace_path
            .file_name()
            .map(|value| value.to_string_lossy().into_owned())
            .unwrap_or_else(|| bootstrap.workspace_uri.clone());
        let workspace_folder = json!({"uri": bootstrap.workspace_uri, "name": root_name});
        let placeholder = NegotiatedCapabilities {
            initialize_result: Value::Null,
            providers: BTreeMap::new(),
            position_encoding: crate::workspace::PositionEncoding::Utf16,
            text_synchronization: crate::workspace::TextSynchronization::None,
        };
        let mut runtime = Self {
            process,
            reader: Some(JsonRpcFrameReader::with_body_limit(stdout, body_limit)),
            frames: None,
            reader_quiescence: None,
            reader_task: None,
            writer: JsonRpcFrameWriter::with_body_limit(stdin, body_limit),
            fatal_transport_failure: None,
            stderr: stderr_rx,
            stderr_task: Some(stderr_task),
            stderr_closed: false,
            stderr_tail,
            next_request_id: 1,
            negotiated: placeholder,
            diagnostics: DiagnosticCache::new(
                settings.max_diagnostic_snapshots,
                settings.max_diagnostic_bytes,
            ),
            documents: DocumentStore::new(
                settings.max_open_documents,
                settings.max_document_bytes,
                settings.max_total_text_bytes,
            ),
            progress: BTreeMap::new(),
            server_requests: BTreeMap::new(),
            processing_server_requests: BTreeSet::new(),
            server_request_order: VecDeque::new(),
            settings: settings.settings.clone(),
            workspace_uri: bootstrap.workspace_uri.clone(),
            workspace_path: bootstrap.workspace_path.clone(),
            server: bootstrap.server.clone(),
            session_identity: bootstrap.session_identity.clone(),
            owner_generation: bootstrap.owner_generation.clone(),
            declaration_digest: settings.declaration_digest.clone(),
            workspace_folder,
            cancellation_grace: Duration::from_millis(settings.cancellation_grace_ms),
            shutdown_timeout: Duration::from_millis(settings.shutdown_timeout_ms),
            max_partial_result_bytes: settings.max_partial_result_bytes,
            log: SessionLog::default(),
            startup_trace: settings.trace_initialization.then(ProtocolTrace::new),
            preview_settings: settings.previews.clone(),
            receipt_settings: settings.receipts.clone(),
            mutation_settings: settings.mutation.clone(),
        };
        runtime.start_reader_pump();
        runtime
            .log
            .push("lifecycle", "info", "Language server process started");
        if let Err(source) = runtime
            .initialize(
                settings.initialization_options.clone(),
                Duration::from_millis(settings.initialization_timeout_ms),
            )
            .await
        {
            runtime.process.terminate_process_tree(Duration::ZERO).await;
            runtime.finish_server_stderr().await;
            return Err(OwnerStartupFailure {
                contract: initialization_failure(bootstrap, &source, &runtime.server_stderr_tail()),
                source,
            });
        }
        runtime
            .log
            .push("lifecycle", "info", "Language server initialized");
        Ok(runtime)
    }

    fn start_reader_pump(&mut self) {
        let mut reader = self
            .reader
            .take()
            .expect("the LSP reader pump starts exactly once");
        let (sender, receiver) = mpsc::channel(OWNER_QUEUE_LIMIT);
        let (quiescence_sender, mut quiescence_receiver) = mpsc::channel(1);
        self.frames = Some(receiver);
        self.reader_quiescence = Some(quiescence_sender);
        self.reader_task = Some(tokio::spawn(async move {
            loop {
                let read = reader.read_json_rpc_frame_with_bytes();
                tokio::pin!(read);
                loop {
                    tokio::select! {
                        biased;
                        frame = &mut read => {
                            let frame = frame.map_err(|error| error.to_string());
                            let terminal = !matches!(frame, Ok(Some(_)));
                            if sender.send(frame).await.is_err() || terminal {
                                return;
                            }
                            break;
                        }
                        quiescence = quiescence_receiver.recv() => {
                            let Some(quiescence) = quiescence else {
                                return;
                            };
                            let _ = quiescence.quiescent.send(());
                            if quiescence.resume.await.is_err() {
                                return;
                            }
                        }
                    }
                }
            }
        }));
    }

    fn server_stderr_tail(&self) -> String {
        self.stderr_tail
            .lock()
            .map(|tail| tail.clone())
            .unwrap_or_default()
    }

    async fn finish_server_stderr(&mut self) {
        if let Some(mut task) = self.stderr_task.take()
            && tokio_time::timeout(Duration::from_secs(1), &mut task)
                .await
                .is_err()
        {
            task.abort();
            let _ = task.await;
        }
        while let Ok(stderr) = self.stderr.try_recv() {
            self.log.push("server_stderr", "error", stderr);
        }
        self.stderr_closed = true;
    }

    async fn initialize(
        &mut self,
        initialization_options: Option<Value>,
        timeout: Duration,
    ) -> io::Result<()> {
        let deadline = TokioInstant::now() + timeout;
        let request = json!({
            "jsonrpc": "2.0",
            "id": 0,
            "method": "initialize",
            "params": {
                "processId": std::process::id(),
                "clientInfo": {"name": "lspctl", "version": env!("CARGO_PKG_VERSION")},
                "rootUri": self.workspace_uri,
                "capabilities": fixed_initialize_capabilities(),
                "initializationOptions": initialization_options,
                "workspaceFolders": [self.workspace_folder.clone()],
                "workDoneToken": "lspctl-initialize-1"
            }
        });
        self.write_lsp_message(&request, true, deadline).await?;
        let initialize_result = loop {
            tokio::select! {
                _ = tokio_time::sleep_until(deadline) => {
                    return Err(io::Error::new(io::ErrorKind::TimedOut, "LSP initialization timed out"));
                }
                stderr = self.stderr.recv(), if !self.stderr_closed => {
                    if let Some(stderr) = stderr {
                        self.log.push("server_stderr", "error", stderr);
                    } else {
                        self.stderr_closed = true;
                    }
                }
                frame = self.frames.as_mut().expect("initializing Owners have an LSP reader pump").recv() => {
                    let frame = frame
                        .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "LSP reader stopped during initialization"))?
                        .map_err(io::Error::other)?
                        .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "Server exited during initialization"))?;
                    if let Some(trace) = &mut self.startup_trace {
                        trace.push("server_to_client", &frame.header, &frame.body, &frame.message);
                    }
                    if is_response_for(&frame.message, &json!(0)) {
                        if let Some(error) = frame.message.get("error") {
                            return Err(io::Error::other(format!("Initialize failed: {error}")));
                        }
                        break frame.message.get("result").cloned().ok_or_else(|| {
                            io::Error::new(io::ErrorKind::InvalidData, "Initialize response omitted result")
                        })?;
                    }
                    self.handle_initialization_message(frame.message, deadline).await?;
                }
            }
        };
        self.negotiated = normalize_initialize_result(initialize_result)
            .map_err(|reason| io::Error::new(io::ErrorKind::InvalidData, reason))?;
        self.write_lsp_message(
            &json!({"jsonrpc": "2.0", "method": "initialized", "params": {}}),
            true,
            deadline,
        )
        .await
    }

    async fn handle_initialization_message(
        &mut self,
        message: Value,
        deadline: TokioInstant,
    ) -> io::Result<()> {
        if let (Some(id), Some(method)) = (message.get("id").cloned(), message["method"].as_str()) {
            let response = if method == "window/showMessageRequest" {
                json!({"jsonrpc": "2.0", "id": id, "result": null})
            } else {
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {
                        "code": -32600,
                        "message": "Initialization in progress",
                        "data": {"reason": "initialization_in_progress"}
                    }
                })
            };
            return self.write_lsp_message(&response, true, deadline).await;
        }
        self.handle_notification(&message, None);
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn start_dispatch(
        &mut self,
        owner_generation: &str,
        method: String,
        params: Option<Value>,
        documents: Vec<OwnerDocumentInput>,
        refresh_open_documents: bool,
        raw_request: bool,
        timeout: Duration,
        trace_protocol: bool,
        apply_edits: bool,
        response: oneshot::Sender<OwnerResponse>,
        delivered: oneshot::Receiver<()>,
        cancelled: watch::Receiver<bool>,
        active: &mut BTreeMap<i64, ActiveQuery>,
    ) {
        let deadline = TokioInstant::now() + timeout;
        if active.len() >= OWNER_QUEUE_LIMIT {
            let _ = response.send(OwnerResponse::failure(
                owner_generation,
                json!({
                    "category": "unavailable",
                    "code": "owner_queue_full",
                    "message": "The Owner has reached its active request limit.",
                    "stage": "queue",
                    "delivery": "not_sent",
                    "retry": "safe",
                    "data": {"limit": OWNER_QUEUE_LIMIT, "depth": active.len()}
                }),
            ));
            return;
        }
        let mut refresh_failures = Vec::new();
        if refresh_open_documents {
            refresh_failures = self.refresh_open_documents_best_effort(deadline).await;
        }
        if let Some(failure) = &self.fatal_transport_failure {
            let _ = response.send(OwnerResponse::failure(owner_generation, failure.clone()));
            wait_for_owner_response_flush(delivered).await;
            return;
        }
        if let Err(failure) = self.synchronize_documents(&documents, deadline).await {
            let _ = response.send(OwnerResponse::failure(
                owner_generation,
                contract_failure_value(failure),
            ));
            if self.fatal_transport_failure.is_some() || !self.writer.is_reusable() {
                wait_for_owner_response_flush(delivered).await;
            }
            return;
        }
        let validated_documents = if refresh_open_documents {
            self.open_document_inputs()
        } else {
            documents.clone()
        };
        let synchronization = self.synchronization_result(
            &method,
            &documents,
            refresh_open_documents,
            &refresh_failures,
        );
        let id = self.next_request_id;
        self.next_request_id = self.next_request_id.saturating_add(1);
        let request_params = params.clone();
        let mut request = json!({"jsonrpc": "2.0", "id": id, "method": method});
        if let Some(params) = params {
            request["params"] = params;
        }
        let partial_token = request.pointer("/params/partialResultToken").cloned();
        let mut trace = trace_protocol.then(|| self.startup_trace.take().unwrap_or_default());
        if let Err(error) = self
            .write_lsp_message_traced(&request, trace.as_mut(), deadline)
            .await
        {
            let mut failure = transport_failure(&error);
            attach_trace(&mut failure, trace);
            let _ = response.send(OwnerResponse::failure(owner_generation, failure));
            if self.fatal_transport_failure.is_some() || !self.writer.is_reusable() {
                wait_for_owner_response_flush(delivered).await;
            }
            return;
        }
        active.insert(
            id,
            ActiveQuery {
                method,
                params: request_params,
                started_at: now_rfc3339(),
                response: Some(response),
                delivered: Some(delivered),
                cancelled,
                deadline,
                timeout,
                cancellation: None,
                partial_token,
                partial_chunks: Vec::new(),
                partial_bytes: 0,
                trace,
                validated_documents,
                raw_request,
                apply_edits,
                apply_edit_ledger: Vec::new(),
                preauthorized_usage: crate::mutation::PreauthorizedUsage::default(),
                synchronization,
            },
        );
    }

    async fn handle_concurrent_frame(
        &mut self,
        owner_generation: &str,
        frame: JsonRpcFrame,
        active: &mut BTreeMap<i64, ActiveQuery>,
        deadline: TokioInstant,
    ) -> Result<(), String> {
        for query in active.values_mut() {
            if let Some(trace) = &mut query.trace {
                trace.push(
                    "server_to_client",
                    &frame.header,
                    &frame.body,
                    &frame.message,
                );
            }
        }
        let message = frame.message;
        if message.get("method").is_none() && message.get("id").is_some() {
            let id = message.get("id").and_then(Value::as_i64).ok_or_else(|| {
                "The server returned a response with a non-integer identifier".to_owned()
            })?;
            let Some(mut query) = active.remove(&id) else {
                return Err(
                    "The server returned an unknown or duplicate response identifier".to_owned(),
                );
            };
            let response = if let Some(cancellation) = query.cancellation.take() {
                let mut failure = cancellation.failure;
                attach_dispatch_evidence(&mut failure, &mut query);
                OwnerResponse::failure(owner_generation, failure)
            } else if let Some(error) = message.get("error") {
                let mut failure = server_error_failure(error);
                attach_dispatch_evidence(&mut failure, &mut query);
                OwnerResponse::failure(owner_generation, failure)
            } else if message.get("result").is_some() {
                let result = message.get("result").cloned().unwrap_or(Value::Null);
                match self
                    .validate_documents_after_query(
                        &query.validated_documents,
                        &result,
                        query.raw_request,
                        deadline,
                    )
                    .await
                {
                    Ok(changed) => {
                        self.record_pull_diagnostics(
                            &query.method,
                            query.params.as_ref(),
                            &result,
                            &query.partial_chunks,
                        );
                        query.synchronization["postResponseChanged"] = Value::Array(changed);
                        let mut output = json!({
                            "result": result,
                            "partialResults": query.partial_chunks,
                            "applyEditLedger": query.apply_edit_ledger,
                            "serverProgress": self.progress.values().collect::<Vec<_>>(),
                            "synchronization": query.synchronization,
                            "positionEncoding": self.negotiated.position_encoding.name(),
                            "textSynchronization": match self.negotiated.text_synchronization {
                                crate::workspace::TextSynchronization::None => "none",
                                crate::workspace::TextSynchronization::OpenClose => "open_close",
                            }
                        });
                        if let Some(trace) = query.trace {
                            output["trace"] = trace.render();
                        }
                        OwnerResponse::success(owner_generation, output)
                    }
                    Err(failure) => {
                        let mut failure = contract_failure_value(failure);
                        attach_dispatch_evidence(&mut failure, &mut query);
                        OwnerResponse::failure(owner_generation, failure)
                    }
                }
            } else {
                return Err("A JSON-RPC response omitted both result and error".to_owned());
            };
            if let Some(sender) = query.response.take() {
                let _ = sender.send(response);
            }
            if let Some(delivered) = query.delivered.take() {
                wait_for_owner_response_flush(delivered).await;
            }
            return Ok(());
        }

        if message.get("id").is_some() && message.get("method").is_some() {
            return self.queue_server_request(message, active, deadline).await;
        }

        if message.get("method").is_some() && message.get("id").is_none() {
            let method = message["method"].as_str().unwrap_or_default();
            let token = message.pointer("/params/token");
            let mut matched_partial = false;
            if method == "$/progress" {
                for (id, query) in active.iter_mut() {
                    if token != query.partial_token.as_ref() {
                        continue;
                    }
                    matched_partial = true;
                    let value = message
                        .pointer("/params/value")
                        .cloned()
                        .unwrap_or(Value::Null);
                    query.partial_bytes = query.partial_bytes.saturating_add(
                        serde_json::to_vec(&value)
                            .map(|bytes| bytes.len())
                            .unwrap_or(usize::MAX),
                    );
                    query.partial_chunks.push(value);
                    if query.partial_bytes > self.max_partial_result_bytes
                        && query.cancellation.is_none()
                    {
                        let partial_items = partial_result_items(&query.partial_chunks);
                        let failure = json!({
                            "category": "query",
                            "code": "partial_result_too_large",
                            "message": "Partial result data exceeded the configured byte limit.",
                            "stage": "await_response",
                            "delivery": "uncertain",
                            "retry": "unsafe",
                            "data": {
                                "limit": self.max_partial_result_bytes,
                                "collectedBytes": query.partial_bytes,
                                "partialItemCount": partial_items.len()
                            },
                            "partialResult": {"items": partial_items, "complete": false}
                        });
                        query.cancellation = Some(QueryCancellation {
                            deadline: TokioInstant::now() + self.cancellation_grace,
                            failure,
                        });
                        let cancel = json!({
                            "jsonrpc": "2.0",
                            "method": "$/cancelRequest",
                            "params": {"id": id}
                        });
                        self.write_lsp_message(
                            &cancel,
                            false,
                            query.cancellation.as_ref().unwrap().deadline,
                        )
                        .await
                        .map_err(|error| error.to_string())?;
                    }
                }
            }
            if method == "$/cancelRequest" {
                self.cancel_server_request(&message, active, deadline)
                    .await?;
            } else if !matched_partial {
                self.handle_notification(&message, None);
            }
            return Ok(());
        }
        Err("Malformed JSON-RPC routing".to_owned())
    }

    async fn queue_server_request(
        &mut self,
        message: Value,
        active: &mut BTreeMap<i64, ActiveQuery>,
        deadline: TokioInstant,
    ) -> Result<(), String> {
        let id = message.get("id").cloned().unwrap_or(Value::Null);
        let Some(key) = ServerRequestId::from_value(&id) else {
            let response = json_rpc_error(
                Value::Null,
                -32600,
                "Invalid server request identifier",
                None,
            );
            return self
                .write_server_response(&response, active, deadline)
                .await;
        };
        if self.server_requests.contains_key(&key) || self.processing_server_requests.contains(&key)
        {
            return Err("The server reused an active request identifier".to_owned());
        }
        if self.server_requests.len() + self.processing_server_requests.len()
            >= SERVER_REQUEST_LIMIT
        {
            let response = json_rpc_error(
                id,
                -32803,
                "Client busy",
                Some(json!({"reason": "client_busy", "limit": SERVER_REQUEST_LIMIT})),
            );
            return self
                .write_server_response(&response, active, deadline)
                .await;
        }
        self.server_request_order.push_back(key.clone());
        self.server_requests
            .insert(key, PendingServerRequest { message });
        Ok(())
    }

    fn has_pending_server_requests(&self) -> bool {
        !self.server_request_order.is_empty()
    }

    async fn process_next_server_request(
        &mut self,
        active: &mut BTreeMap<i64, ActiveQuery>,
        drain_frames: bool,
        deadline: TokioInstant,
    ) -> Result<(), String> {
        let Some(key) = self.server_request_order.pop_front() else {
            return Ok(());
        };
        let Some(pending) = self.server_requests.remove(&key) else {
            // A cancelled request leaves an order tombstone so cancellation
            // never needs an O(n) queue scan.
            return Ok(());
        };
        self.processing_server_requests.insert(key.clone());
        let response = if pending.message["method"] == "workspace/applyEdit" {
            self.handle_apply_edit_callback(&pending.message, active, deadline)
                .await
        } else {
            self.route_server_request(pending.message).await
        };
        let reader_resume = if drain_frames {
            Some(self.drain_reader_frames(active, deadline).await?)
        } else {
            None
        };
        if let Some(reader_resume) = reader_resume {
            reader_resume
                .send(())
                .map_err(|_| "The LSP reader pump stopped before resuming".to_owned())?;
        }
        self.write_server_response(&response, active, deadline)
            .await?;
        let reader_resume = if drain_frames {
            Some(self.drain_reader_frames(active, deadline).await?)
        } else {
            None
        };
        self.processing_server_requests.remove(&key);
        if let Some(reader_resume) = reader_resume {
            reader_resume
                .send(())
                .map_err(|_| "The LSP reader pump stopped before resuming".to_owned())?;
        }
        Ok(())
    }

    async fn drain_reader_frames(
        &mut self,
        active: &mut BTreeMap<i64, ActiveQuery>,
        deadline: TokioInstant,
    ) -> Result<oneshot::Sender<()>, String> {
        let quiescence_sender = self
            .reader_quiescence
            .as_ref()
            .expect("ready Owners have an LSP reader pump")
            .clone();
        let (quiescent, mut quiescence) = oneshot::channel();
        let (resume, resumed) = oneshot::channel();
        quiescence_sender
            .send(ReaderQuiescenceRequest {
                quiescent,
                resume: resumed,
            })
            .await
            .map_err(|_| "The LSP reader pump stopped before draining frames".to_owned())?;

        let mut drained = 0_usize;
        loop {
            tokio::select! {
                biased;
                result = &mut quiescence => {
                    result.map_err(|_| "The LSP reader pump stopped before acknowledging drained frames".to_owned())?;
                    while let Ok(frame) = self.frames.as_mut().expect("ready Owners have an LSP reader pump").try_recv() {
                        drained = drained.saturating_add(1);
                        if drained > SERVER_REQUEST_LIMIT {
                            return Err("The server emitted too many frames while a request was being processed".to_owned());
                        }
                        self.handle_drained_reader_frame(frame, active, deadline).await?;
                    }
                    return Ok(resume);
                }
                frame = self.frames.as_mut().expect("ready Owners have an LSP reader pump").recv() => {
                    let frame = frame.ok_or_else(|| "The LSP reader pump stopped while draining frames".to_owned())?;
                    drained = drained.saturating_add(1);
                    if drained > SERVER_REQUEST_LIMIT {
                        return Err("The server emitted too many frames while a request was being processed".to_owned());
                    }
                    self.handle_drained_reader_frame(frame, active, deadline).await?;
                }
            }
        }
    }

    async fn handle_drained_reader_frame(
        &mut self,
        frame: Result<Option<JsonRpcFrame>, String>,
        active: &mut BTreeMap<i64, ActiveQuery>,
        deadline: TokioInstant,
    ) -> Result<(), String> {
        let frame = frame?.ok_or_else(|| {
            "The language server exited while its frames were being drained".to_owned()
        })?;
        let owner_generation = self.owner_generation.clone();
        self.handle_concurrent_frame(&owner_generation, frame, active, deadline)
            .await
    }

    async fn cancel_server_request(
        &mut self,
        message: &Value,
        active: &mut BTreeMap<i64, ActiveQuery>,
        deadline: TokioInstant,
    ) -> Result<(), String> {
        let Some(id) = message.pointer("/params/id") else {
            return Ok(());
        };
        let Some(key) = ServerRequestId::from_value(id) else {
            return Ok(());
        };
        let Some(pending) = self.server_requests.remove(&key) else {
            self.log.push(
                "server_request_cancellation",
                "debug",
                "Ignored late or unknown server request cancellation",
            );
            return Ok(());
        };
        let response = json_rpc_error(
            pending.message.get("id").cloned().unwrap_or(Value::Null),
            -32800,
            "Request cancelled",
            None,
        );
        self.write_server_response(&response, active, deadline)
            .await
    }

    async fn write_server_response(
        &mut self,
        response: &Value,
        active: &mut BTreeMap<i64, ActiveQuery>,
        deadline: TokioInstant,
    ) -> Result<(), String> {
        // Drained progress can start cancellation while a callback is being processed.
        let deadline = active_write_deadline(active, deadline);
        let (header, body) = self
            .write_lsp_frame(response, deadline)
            .await
            .map_err(|error| error.to_string())?;
        for query in active.values_mut() {
            if let Some(trace) = &mut query.trace {
                trace.push("client_to_server", &header, &body, response);
            }
        }
        Ok(())
    }

    async fn handle_apply_edit_callback(
        &mut self,
        message: &Value,
        active: &mut BTreeMap<i64, ActiveQuery>,
        deadline: TokioInstant,
    ) -> Value {
        let callback_id = message.get("id").cloned().unwrap_or(Value::Null);
        let candidate = (active.len() == 1)
            .then(|| active.keys().next().copied())
            .flatten();
        let ordinal = candidate
            .and_then(|id| active.get(&id))
            .map_or(0, |query| query.apply_edit_ledger.len() as u64);
        let label = message.pointer("/params/label").and_then(Value::as_str);
        let Some(edit) = message.pointer("/params/edit").cloned() else {
            if let Some(query) = candidate.and_then(|id| active.get_mut(&id)) {
                query.apply_edit_ledger.push(json!({
                    "ordinal": ordinal,
                    "label": label,
                    "applied": false,
                    "outcome": "rejected",
                    "failureReason": "invalid_workspace_edit"
                }));
            }
            return json_rpc_error(
                callback_id,
                -32602,
                "workspace/applyEdit requires params.edit",
                None,
            );
        };
        let preauthorized = candidate
            .and_then(|id| active.get(&id))
            .is_some_and(|query| query.method == "workspace/executeCommand" && query.apply_edits);
        if !preauthorized {
            let preview = crate::mutation::create_callback_preview(
                &self.workspace_path,
                &self.workspace_uri,
                &self.server,
                &self.session_identity,
                self.declaration_digest.as_deref(),
                self.negotiated.position_encoding,
                label,
                edit,
                &self.preview_settings,
                &self.mutation_settings,
            );
            return match preview {
                Ok(preview) => {
                    let preview_id = preview
                        .get("previewId")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                    if let Some(query) = candidate.and_then(|id| active.get_mut(&id)) {
                        let mut ledger = json!({
                            "ordinal": ordinal,
                            "label": label,
                            "applied": false,
                            "outcome": if preview_id.is_some() { "previewed" } else { "unchanged" },
                            "failureReason": "preview_required",
                            "previewId": preview_id
                        });
                        compact_json_object(&mut ledger);
                        query.apply_edit_ledger.push(ledger);
                    }
                    let mut result = json!({
                        "applied": false,
                        "failureReason": "preview_required",
                        "previewId": preview_id
                    });
                    compact_json_object(&mut result);
                    json!({"jsonrpc": "2.0", "id": callback_id, "result": result})
                }
                Err(failure) => {
                    if let Some(query) = candidate.and_then(|id| active.get_mut(&id)) {
                        query.apply_edit_ledger.push(json!({
                            "ordinal": ordinal,
                            "label": label,
                            "applied": false,
                            "outcome": "rejected",
                            "failureReason": failure.code
                        }));
                    }
                    json!({
                        "jsonrpc": "2.0",
                        "id": callback_id,
                        "result": {"applied": false, "failureReason": failure.code}
                    })
                }
            };
        }

        let candidate = candidate.unwrap();
        if active[&candidate].apply_edit_ledger.len() as u64
            >= self.mutation_settings.max_preauthorized_callbacks
        {
            active
                .get_mut(&candidate)
                .unwrap()
                .apply_edit_ledger
                .push(json!({
                    "ordinal": ordinal,
                    "label": label,
                    "applied": false,
                    "outcome": "rejected",
                    "failureReason": "resource_limit_exceeded"
                }));
            return json!({
                "jsonrpc": "2.0",
                "id": callback_id,
                "result": {"applied": false, "failureReason": "resource_limit_exceeded"}
            });
        }

        // Application calls this hook after the filesystem commit and before
        // releasing the Workspace lock. Clone the immutable launch settings
        // so the hook can exclusively borrow the live runtime.
        let workspace_path = self.workspace_path.clone();
        let workspace_uri = self.workspace_uri.clone();
        let server = self.server.clone();
        let session_identity = self.session_identity.clone();
        let position_encoding = self.negotiated.position_encoding;
        let previews = self.preview_settings.clone();
        let receipts = self.receipt_settings.clone();
        let mut mutation = self.mutation_settings.clone();
        let used = active[&candidate].preauthorized_usage;
        mutation.max_entries = mutation.max_entries.saturating_sub(used.entries);
        mutation.max_rollback_bytes = mutation
            .max_rollback_bytes
            .saturating_sub(used.rollback_bytes);
        mutation.max_staged_text_bytes = mutation
            .max_staged_text_bytes
            .saturating_sub(used.staged_text_bytes);
        let mut synchronize = |operations: &[Value]| {
            tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current()
                    .block_on(self.synchronize_documents_after_commit(operations, deadline))
            })
        };
        match crate::mutation::apply_preauthorized_workspace_edit(
            &workspace_path,
            &workspace_uri,
            &server,
            &session_identity,
            position_encoding,
            label,
            edit,
            &previews,
            &receipts,
            &mutation,
            &mut synchronize,
        ) {
            Ok((application, usage)) => {
                let result = &application["result"];
                let synchronized = result["sessionSynchronized"].as_bool() == Some(true);
                {
                    let query = active.get_mut(&candidate).unwrap();
                    query.preauthorized_usage.entries = query
                        .preauthorized_usage
                        .entries
                        .saturating_add(usage.entries);
                    query.preauthorized_usage.rollback_bytes = query
                        .preauthorized_usage
                        .rollback_bytes
                        .saturating_add(usage.rollback_bytes);
                    query.preauthorized_usage.staged_text_bytes = query
                        .preauthorized_usage
                        .staged_text_bytes
                        .saturating_add(usage.staged_text_bytes);
                }
                if synchronized {
                    let documents = self.open_document_inputs();
                    active.get_mut(&candidate).unwrap().validated_documents = documents;
                }
                let mut ledger = json!({
                    "ordinal": ordinal,
                    "label": label,
                    "applied": true,
                    "outcome": "applied",
                    "previewId": result.get("previewId"),
                    "receiptId": result.get("receiptId"),
                    "filesystemState": result.get("filesystemState"),
                    "failureReason": (!synchronized).then_some("session_synchronization_failed")
                });
                compact_json_object(&mut ledger);
                active
                    .get_mut(&candidate)
                    .unwrap()
                    .apply_edit_ledger
                    .push(ledger);
                json!({"jsonrpc": "2.0", "id": callback_id, "result": {"applied": true}})
            }
            Err(failure) => {
                let outcome = match failure.code {
                    "proposal_stale" | "preview_stale" => "stale",
                    "workspace_lock_timeout" => "busy",
                    "rolled_back" => "rolled_back",
                    "recovery_required" | "recovery_evidence_invalid" => "recovery_required",
                    _ => "rejected",
                };
                let mut ledger = json!({
                    "ordinal": ordinal,
                    "label": label,
                    "applied": false,
                    "outcome": outcome,
                    "failureReason": failure.code,
                    "failedChange": failure.data.get("failedChange")
                });
                compact_json_object(&mut ledger);
                active
                    .get_mut(&candidate)
                    .unwrap()
                    .apply_edit_ledger
                    .push(ledger);
                json!({
                    "jsonrpc": "2.0",
                    "id": callback_id,
                    "result": {"applied": false, "failureReason": failure.code}
                })
            }
        }
    }

    async fn synchronize_documents_after_commit(
        &mut self,
        file_operations: &[Value],
        deadline: TokioInstant,
    ) -> bool {
        let file_operations_delivered = self
            .send_file_operation_notifications(file_operations, deadline)
            .await
            .is_ok();
        let (outcomes, failures) = self
            .documents
            .refresh_open_documents(self.negotiated.text_synchronization);
        let mut synchronized = failures.is_empty();
        for outcome in outcomes {
            synchronized &= self
                .send_synchronization_events(outcome.events, deadline)
                .await
                .is_ok();
        }
        let pending_events = self.documents.drain_pending_events();
        let pending_delivered = self
            .send_synchronization_events(pending_events, deadline)
            .await
            .is_ok();
        file_operations_delivered && synchronized && pending_delivered
    }

    async fn maintain_active_queries(
        &mut self,
        owner_generation: &str,
        active: &mut BTreeMap<i64, ActiveQuery>,
    ) -> bool {
        let now = TokioInstant::now();
        let mut cancel = Vec::new();
        let mut cancellation_grace_expired = false;
        for (id, query) in active.iter_mut() {
            if let Some(cancellation) = &query.cancellation {
                cancellation_grace_expired |= now >= cancellation.deadline;
                continue;
            }
            let caller_cancelled = *query.cancelled.borrow();
            if caller_cancelled || now >= query.deadline {
                let failure = if caller_cancelled {
                    request_cancelled("caller_disconnected")
                } else {
                    let mut failure = json!({
                        "category": "query",
                        "code": "request_timeout",
                        "message": "The language-server request timed out.",
                        "stage": "await_response",
                        "delivery": "uncertain",
                        "retry": "unsafe",
                        "data": {"timeout": format_duration(query.timeout)}
                    });
                    attach_trace(&mut failure, query.trace.take());
                    failure
                };
                query.cancellation = Some(QueryCancellation {
                    deadline: now + self.cancellation_grace,
                    failure,
                });
                cancel.push(*id);
            }
        }
        for id in cancel {
            if self
                .write_lsp_message(
                    &json!({
                        "jsonrpc": "2.0",
                        "method": "$/cancelRequest",
                        "params": {"id": id}
                    }),
                    false,
                    active[&id].cancellation.as_ref().unwrap().deadline,
                )
                .await
                .is_err()
            {
                cancellation_grace_expired = true;
            }
        }
        if self.fatal_transport_failure.is_some() {
            return true;
        }
        if cancellation_grace_expired {
            self.process.terminate_process_tree(Duration::ZERO).await;
            fail_active_queries(
                owner_generation,
                active,
                protocol_failure(
                    "The server did not settle every cancelled request within the cancellation grace period"
                        .to_owned(),
                ),
            ).await;
        }
        cancellation_grace_expired
    }

    async fn synchronize_documents(
        &mut self,
        documents: &[OwnerDocumentInput],
        deadline: TokioInstant,
    ) -> Result<(), ContractFailure> {
        for document in documents {
            let outcome = match self.documents.refresh(
                &document.path,
                &document.language_id,
                self.negotiated.text_synchronization,
            ) {
                Ok(outcome) => outcome,
                Err(failure) => {
                    let pending_events = self.documents.drain_pending_events();
                    self.send_synchronization_events(pending_events, deadline)
                        .await?;
                    return Err(failure);
                }
            };
            self.send_synchronization_events(outcome.events, deadline)
                .await?;
            if outcome.snapshot.digest != document.expected_digest {
                return Err(ContractFailure {
                    exit_code: 5,
                    category: "query",
                    code: "document_changed_while_reading",
                    message: "A synchronized Document changed before dispatch.".to_owned(),
                    stage: "synchronize",
                    delivery: "not_sent",
                    retry: "safe",
                    data: json!({
                        "uri": outcome.snapshot.uri,
                        "before": {"digest": document.expected_digest},
                        "after": {"digest": outcome.snapshot.digest}
                    }),
                });
            }
        }
        Ok(())
    }

    async fn refresh_open_documents_best_effort(
        &mut self,
        deadline: TokioInstant,
    ) -> Vec<ContractFailure> {
        let (outcomes, failures) = self
            .documents
            .refresh_open_documents(self.negotiated.text_synchronization);
        for outcome in outcomes {
            if let Err(failure) = self
                .send_synchronization_events(outcome.events, deadline)
                .await
            {
                return vec![failure];
            }
        }
        let pending_events = self.documents.drain_pending_events();
        if let Err(failure) = self
            .send_synchronization_events(pending_events, deadline)
            .await
        {
            return vec![failure];
        }
        failures
    }

    fn open_document_inputs(&self) -> Vec<OwnerDocumentInput> {
        self.documents
            .snapshots()
            .into_iter()
            .map(|snapshot| OwnerDocumentInput {
                path: snapshot.path.clone(),
                language_id: snapshot.language_id.clone(),
                expected_digest: snapshot.digest.clone(),
            })
            .collect()
    }

    fn synchronization_result(
        &self,
        method: &str,
        documents: &[OwnerDocumentInput],
        refresh_open_documents: bool,
        failures: &[ContractFailure],
    ) -> Value {
        let before = if refresh_open_documents {
            self.documents
                .snapshots()
                .into_iter()
                .map(|snapshot| {
                    json!({
                        "uri": snapshot.uri,
                        "digest": snapshot.digest,
                        "version": snapshot.version,
                        "languageId": snapshot.language_id
                    })
                })
                .collect::<Vec<_>>()
        } else {
            documents
                .iter()
                .filter_map(|document| {
                    let uri = Url::from_file_path(&document.path).ok()?.to_string();
                    let snapshot = self.documents.get(&uri)?;
                    Some(json!({
                        "uri": snapshot.uri,
                        "digest": snapshot.digest,
                        "version": snapshot.version,
                        "languageId": snapshot.language_id
                    }))
                })
                .collect::<Vec<_>>()
        };
        let mode = if refresh_open_documents || method == "workspace/diagnostic" {
            "workspace"
        } else if documents.is_empty() {
            "none"
        } else if documents.len() == 1 {
            "document"
        } else {
            "explicit"
        };
        json!({
            "mode": mode,
            "bestEffort": refresh_open_documents,
            "before": before,
            "failures": failures.iter().map(contract_failure_ref_value).collect::<Vec<_>>(),
            "postResponseChanged": []
        })
    }

    async fn validate_documents_after_query(
        &mut self,
        documents: &[OwnerDocumentInput],
        server_result: &Value,
        raw_request: bool,
        deadline: TokioInstant,
    ) -> Result<Vec<Value>, ContractFailure> {
        let mut changed = Vec::new();
        for document in documents {
            let outcome = match self.documents.refresh(
                &document.path,
                &document.language_id,
                self.negotiated.text_synchronization,
            ) {
                Ok(outcome) => outcome,
                Err(failure) => {
                    let pending_events = self.documents.drain_pending_events();
                    self.send_synchronization_events(pending_events, deadline)
                        .await?;
                    return Err(failure);
                }
            };
            self.send_synchronization_events(outcome.events, deadline)
                .await?;
            if outcome.snapshot.digest != document.expected_digest {
                if raw_request {
                    changed.push(json!({
                        "uri": outcome.snapshot.uri,
                        "beforeDigest": document.expected_digest,
                        "afterDigest": outcome.snapshot.digest
                    }));
                    continue;
                }
                return Err(ContractFailure {
                    exit_code: 5,
                    category: "query",
                    code: "document_changed_during_query",
                    message:
                        "A synchronized Document changed while the server request was running."
                            .to_owned(),
                    stage: "validate_result",
                    delivery: "sent",
                    retry: "after_change",
                    data: json!({
                        "uri": outcome.snapshot.uri,
                        "beforeDigest": document.expected_digest,
                        "afterDigest": outcome.snapshot.digest,
                        "serverResult": server_result
                    }),
                });
            }
        }
        Ok(changed)
    }

    async fn send_synchronization_events(
        &mut self,
        events: Vec<SynchronizationEvent>,
        deadline: TokioInstant,
    ) -> Result<(), ContractFailure> {
        for event in events {
            let notification = match event {
                SynchronizationEvent::DidOpen(snapshot) => json!({
                    "jsonrpc": "2.0",
                    "method": "textDocument/didOpen",
                    "params": {"textDocument": {
                        "uri": snapshot.uri,
                        "languageId": snapshot.language_id,
                        "version": snapshot.version,
                        "text": snapshot.text
                    }}
                }),
                SynchronizationEvent::DidClose { uri } => {
                    self.diagnostics.mark_closed(&uri);
                    json!({
                        "jsonrpc": "2.0",
                        "method": "textDocument/didClose",
                        "params": {"textDocument": {"uri": uri}}
                    })
                }
            };
            if let Err(error) = self.write_lsp_message(&notification, false, deadline).await {
                // Even pre-write rejection leaves committed Documents divergent, though the
                // byte stream itself is reusable. Retire through the existing fatal path.
                self.fatal_transport_failure
                    .get_or_insert_with(|| transport_failure(&error));
                self.process.terminate_process_tree(Duration::ZERO).await;
                return Err(ContractFailure {
                    exit_code: 4,
                    category: "unavailable",
                    code: "owner_unavailable",
                    message: "Document synchronization could not be delivered.".to_owned(),
                    stage: "synchronize",
                    delivery: "uncertain",
                    retry: "unsafe",
                    data: json!({"reason": error.to_string()}),
                });
            }
        }
        Ok(())
    }

    async fn send_file_operation_notifications(
        &mut self,
        operations: &[Value],
        deadline: TokioInstant,
    ) -> Result<(), ContractFailure> {
        for operation in operations {
            let Some(kind) = operation.get("kind").and_then(Value::as_str) else {
                continue;
            };
            let (capability, method, files) = match kind {
                "create" => (
                    "didCreate",
                    "workspace/didCreateFiles",
                    json!([{"uri": operation["uri"]}]),
                ),
                "rename" => (
                    "didRename",
                    "workspace/didRenameFiles",
                    json!([{
                        "oldUri": operation["oldUri"],
                        "newUri": operation["newUri"]
                    }]),
                ),
                "delete" => (
                    "didDelete",
                    "workspace/didDeleteFiles",
                    json!([{"uri": operation["uri"]}]),
                ),
                _ => continue,
            };
            if !file_operation_registered(&self.negotiated.initialize_result, capability, operation)
            {
                continue;
            }
            self.write_lsp_message(
                &json!({
                    "jsonrpc": "2.0",
                    "method": method,
                    "params": {"files": files}
                }),
                false,
                deadline,
            )
            .await
            .map_err(|error| ContractFailure {
                exit_code: 4,
                category: "unavailable",
                code: "owner_unavailable",
                message: "A post-commit file-operation notification could not be delivered."
                    .to_owned(),
                stage: "post_commit",
                delivery: "uncertain",
                retry: "unsafe",
                data: json!({"reason": error.to_string(), "method": method}),
            })?;
        }
        Ok(())
    }

    fn record_pull_diagnostics(
        &mut self,
        method: &str,
        params: Option<&Value>,
        result: &Value,
        partial_chunks: &[Value],
    ) {
        use crate::query::{QueryCommand, merge_partial_results, normalize_named_result};
        let command = match method {
            "textDocument/diagnostic" => QueryCommand::DocumentDiagnostics,
            "workspace/diagnostic" => QueryCommand::WorkspaceDiagnostics,
            _ => return,
        };
        // Cache validated effective content separately from exact raw Query evidence.
        let Ok(report) = merge_partial_results(command, result.clone(), partial_chunks.to_vec())
            .and_then(|report| normalize_named_result(command, report))
        else {
            return;
        };
        if command == QueryCommand::DocumentDiagnostics {
            if let Some(uri) = params
                .and_then(|params| params.pointer("/textDocument/uri"))
                .and_then(Value::as_str)
            {
                self.diagnostics.apply_document_pull_report(uri, report);
            }
        } else {
            self.diagnostics.apply_workspace_pull_report(report);
        }
    }

    async fn route_server_request(&mut self, message: Value) -> Value {
        let id = message.get("id").cloned().unwrap_or(Value::Null);
        let method = message["method"].as_str().unwrap_or_default();
        if method == "window/workDoneProgress/create" {
            let token = message.pointer("/params/token").cloned();
            let key = token.as_ref().map(token_key);
            let invalid = key
                .as_ref()
                .is_none_or(|key| self.progress.contains_key(key))
                || self.progress.len() >= 256;
            return if invalid {
                json_rpc_error(id, -32602, "Invalid work-done progress token", None)
            } else {
                self.progress.insert(
                    key.unwrap(),
                    json!({"token": token.unwrap(), "kind": "work_done"}),
                );
                json!({"jsonrpc": "2.0", "id": id, "result": null})
            };
        }
        if method == "workspace/diagnostic/refresh" {
            self.diagnostics.invalidate_pull_result_ids();
        }
        let context = CallbackContext {
            settings: self.settings.clone(),
            workspace_uri: self.workspace_uri.clone(),
            workspace_path: self.workspace_path.clone(),
            workspace_folder: self.workspace_folder.clone(),
        };
        let mut router = Router::new(context);
        router.unhandled_request(|state, request| {
            let result = route_callback(state, &request.method, request.params);
            async move { result }
        });
        let request = match serde_json::from_value::<AnyRequest>(message) {
            Ok(request) => request,
            Err(error) => {
                return json_rpc_error(
                    id,
                    -32600,
                    "Invalid server request",
                    Some(json!({"reason": error.to_string()})),
                );
            }
        };
        match router.call(request).await {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err(error) => json!({"jsonrpc": "2.0", "id": id, "error": error}),
        }
    }

    fn handle_notification(&mut self, message: &Value, partial_token: Option<&Value>) {
        let method = message["method"].as_str().unwrap_or_default();
        match method {
            "textDocument/publishDiagnostics" => {
                let uri = message.pointer("/params/uri").and_then(Value::as_str);
                let diagnostics = message.pointer("/params/diagnostics").cloned();
                let version = message.pointer("/params/version").and_then(Value::as_i64);
                if let (Some(uri), Some(diagnostics)) = (uri, diagnostics) {
                    let current_version = self.documents.get(uri).map(|document| document.version);
                    self.diagnostics.publish(
                        uri,
                        version,
                        diagnostics,
                        current_version,
                        current_version.is_some(),
                    );
                }
            }
            "window/logMessage" => {
                let text = message
                    .pointer("/params/message")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                self.log.push(
                    "window_log_message",
                    lsp_log_level(message.pointer("/params/type")),
                    text,
                );
            }
            "window/showMessage" => {
                let text = message
                    .pointer("/params/message")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                self.log.push(
                    "window_show_message",
                    lsp_log_level(message.pointer("/params/type")),
                    text,
                );
            }
            "$/progress" if message.pointer("/params/token") != partial_token => {
                self.update_work_done_progress(message);
            }
            "telemetry/event" | "$/cancelRequest" => {}
            _ => {}
        }
    }

    fn update_work_done_progress(&mut self, message: &Value) {
        let Some(token) = message.pointer("/params/token").cloned() else {
            return;
        };
        let key = token_key(&token);
        let kind = message
            .pointer("/params/value/kind")
            .and_then(Value::as_str);
        if kind == Some("end") {
            self.progress.remove(&key);
            return;
        }
        if !self.progress.contains_key(&key) {
            return;
        }
        let value = message
            .pointer("/params/value")
            .cloned()
            .unwrap_or(Value::Null);
        let mut record = json!({"token": token, "kind": "work_done"});
        for field in ["title", "message", "percentage", "cancellable"] {
            if let Some(value) = value.get(field) {
                record[field] = value.clone();
            }
        }
        self.progress.insert(key, record);
    }

    async fn graceful_shutdown(&mut self) {
        let deadline = TokioInstant::now() + self.shutdown_timeout;
        if self.fatal_transport_failure.is_some() || !self.writer.is_reusable() {
            self.process.terminate_process_tree(Duration::ZERO).await;
            return;
        }
        if self.process.try_wait().ok().flatten().is_some() {
            return;
        }

        // LSP requires clients to close every open text document before the
        // shutdown request. These notifications are best effort: a broken
        // transport must not prevent process-tree cleanup below.
        let mut close_events = self.documents.drain_pending_events();
        close_events.extend(
            self.documents
                .close_all(self.negotiated.text_synchronization),
        );
        if let Err(failure) = self
            .send_synchronization_events(close_events, deadline)
            .await
        {
            self.log.push(
                "lifecycle",
                "warning",
                format!(
                    "Could not close every Document before shutdown: {}",
                    failure.message
                ),
            );
        }

        if self.fatal_transport_failure.is_some() || !self.writer.is_reusable() {
            return;
        }
        let id = self.next_request_id;
        self.next_request_id = self.next_request_id.saturating_add(1);
        if self
            .write_lsp_message(
                &json!({"jsonrpc": "2.0", "id": id, "method": "shutdown"}),
                false,
                deadline,
            )
            .await
            .is_ok()
        {
            let mut no_active_queries = BTreeMap::new();
            loop {
                tokio::select! {
                    biased;
                    _ = tokio_time::sleep_until(deadline) => break,
                    frame = self.frames.as_mut().expect("ready Owners have an LSP reader pump").recv() => {
                        match frame {
                            Some(Ok(Some(frame))) if is_response_for(&frame.message, &json!(id)) => {
                                if frame.message.get("result") != Some(&Value::Null) {
                                    self.log.push("protocol_violation", "warning", "Shutdown returned a non-null result");
                                }
                                break;
                            }
                            Some(Ok(Some(frame))) => {
                                let message = frame.message;
                                if message.get("id").is_some() && message.get("method").is_some() {
                                    if self.queue_server_request(message, &mut no_active_queries, deadline).await.is_err() {
                                        break;
                                    }
                                } else if message.get("method").is_some() && message.get("id").is_none() {
                                    if message["method"] == "$/cancelRequest" {
                                        if self.cancel_server_request(&message, &mut no_active_queries, deadline).await.is_err() {
                                            break;
                                        }
                                    } else {
                                        self.handle_notification(&message, None);
                                    }
                                } else {
                                    break;
                                }
                            }
                            _ => break,
                        }
                    }
                    _ = std::future::ready(()), if self.has_pending_server_requests() => {
                        if self
                            .process_next_server_request(&mut no_active_queries, false, deadline)
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }
            if self.fatal_transport_failure.is_none() && self.writer.is_reusable() {
                let _ = self
                    .write_lsp_message(
                        &json!({"jsonrpc": "2.0", "method": "exit"}),
                        false,
                        deadline,
                    )
                    .await;
            }
        }
        if !matches!(
            tokio_time::timeout_at(deadline, self.process.wait()).await,
            Ok(Ok(_))
        ) {
            self.process.terminate_process_tree(Duration::ZERO).await;
        }
    }

    async fn write_lsp_frame(
        &mut self,
        message: &Value,
        deadline: TokioInstant,
    ) -> io::Result<(Vec<u8>, Vec<u8>)> {
        if self.fatal_transport_failure.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "The language-server session is no longer synchronized or writable",
            ));
        }
        let result = self
            .writer
            .write_json_rpc_frame_with_bytes(message, deadline)
            .await
            .map_err(|error| match error {
                JsonRpcTransportError::WriteFrame(source) => source,
                error => io::Error::other(error),
            });
        if !self.writer.is_reusable() {
            if let Err(error) = &result {
                self.fatal_transport_failure
                    .get_or_insert_with(|| transport_failure(error));
            }
            self.process.terminate_process_tree(Duration::ZERO).await;
        }
        result
    }

    async fn write_lsp_message(
        &mut self,
        message: &Value,
        startup: bool,
        deadline: TokioInstant,
    ) -> io::Result<()> {
        let bytes = self.write_lsp_frame(message, deadline).await?;
        if startup && let Some(trace) = &mut self.startup_trace {
            trace.push("client_to_server", &bytes.0, &bytes.1, message);
        }
        Ok(())
    }

    async fn write_lsp_message_traced(
        &mut self,
        message: &Value,
        trace: Option<&mut ProtocolTrace>,
        deadline: TokioInstant,
    ) -> io::Result<()> {
        let (header, body) = self.write_lsp_frame(message, deadline).await?;
        if let Some(trace) = trace {
            trace.push("client_to_server", &header, &body, message);
        }
        Ok(())
    }
}

fn active_write_deadline(
    active: &BTreeMap<i64, ActiveQuery>,
    fallback: TokioInstant,
) -> TokioInstant {
    active
        .values()
        .map(|query| {
            query
                .cancellation
                .as_ref()
                .map_or(query.deadline, |cancellation| cancellation.deadline)
        })
        .min()
        .unwrap_or(fallback)
}

fn file_operation_registered(
    initialize_result: &Value,
    capability: &str,
    operation: &Value,
) -> bool {
    let Some(filters) = initialize_result
        .pointer(&format!(
            "/capabilities/workspace/fileOperations/{capability}/filters"
        ))
        .and_then(Value::as_array)
    else {
        return false;
    };
    let uris = match operation.get("kind").and_then(Value::as_str) {
        Some("rename") => vec![operation.get("oldUri"), operation.get("newUri")],
        _ => vec![operation.get("uri")],
    };
    filters.iter().any(|filter| {
        uris.iter().flatten().any(|uri| {
            file_operation_filter_matches(
                filter,
                uri.as_str().unwrap_or_default(),
                operation["isDirectory"].as_bool().unwrap_or(false),
            )
        })
    })
}

fn file_operation_filter_matches(filter: &Value, uri: &str, is_directory: bool) -> bool {
    let Some(filter) = filter.as_object() else {
        return false;
    };
    let Some(uri) = Url::parse(uri).ok() else {
        return false;
    };
    if filter
        .get("scheme")
        .and_then(Value::as_str)
        .is_some_and(|scheme| scheme != uri.scheme())
    {
        return false;
    }
    let Some(pattern) = filter.get("pattern").and_then(Value::as_object) else {
        return false;
    };
    if pattern
        .get("matches")
        .and_then(Value::as_str)
        .is_some_and(|matches| matches != if is_directory { "folder" } else { "file" })
    {
        return false;
    }
    let Some(glob) = pattern.get("glob").and_then(Value::as_str) else {
        return false;
    };
    let ignore_case = pattern
        .get("options")
        .and_then(Value::as_object)
        .and_then(|options| options.get("ignoreCase"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    crate::query::protocol_glob_matches(glob, uri.path(), ignore_case)
}

#[derive(Clone)]
struct CallbackContext {
    settings: Value,
    workspace_uri: String,
    workspace_path: PathBuf,
    workspace_folder: Value,
}

fn route_callback(
    context: &mut CallbackContext,
    method: &str,
    params: Value,
) -> Result<Value, ResponseError> {
    match method {
        "workspace/configuration" => workspace_configuration_result(context, &params),
        "workspace/workspaceFolders" => Ok(json!([context.workspace_folder])),
        "workspace/diagnostic/refresh" => Ok(Value::Null),
        "workspace/applyEdit" => Ok(json!({
            "applied": false,
            "failureReason": "preview_required"
        })),
        "window/showMessageRequest" => Ok(Value::Null),
        "window/showDocument" => Ok(json!({"success": false})),
        _ => Err(ResponseError::new(
            ErrorCode::METHOD_NOT_FOUND,
            format!("Unsupported server request: {method}"),
        )),
    }
}

fn workspace_configuration_result(
    context: &CallbackContext,
    params: &Value,
) -> Result<Value, ResponseError> {
    let Some(items) = params.get("items").and_then(Value::as_array) else {
        return Err(ResponseError::new(
            ErrorCode::INVALID_PARAMS,
            "workspace/configuration requires an items array".to_owned(),
        ));
    };
    Ok(Value::Array(
        items
            .iter()
            .map(|item| {
                let Some(item) = item.as_object() else {
                    return Value::Null;
                };
                if !configuration_scope_allowed(context, item.get("scopeUri")) {
                    return Value::Null;
                }
                let section = match item.get("section") {
                    None | Some(Value::Null) => "",
                    Some(Value::String(section)) => section,
                    Some(_) => return Value::Null,
                };
                if section.is_empty() {
                    return context.settings.clone();
                }
                let mut value = &context.settings;
                for component in section.split('.') {
                    if component.is_empty() {
                        return Value::Null;
                    }
                    let Some(next) = value.as_object().and_then(|object| object.get(component))
                    else {
                        return Value::Null;
                    };
                    value = next;
                }
                value.clone()
            })
            .collect(),
    ))
}

fn configuration_scope_allowed(context: &CallbackContext, scope_uri: Option<&Value>) -> bool {
    let scope_uri = match scope_uri {
        None | Some(Value::Null) => return true,
        Some(Value::String(scope_uri)) => scope_uri,
        Some(_) => return false,
    };
    if scope_uri.as_str() == context.workspace_uri {
        return true;
    }
    Url::parse(scope_uri)
        .ok()
        .filter(|uri| uri.scheme() == "file")
        .and_then(|uri| uri.to_file_path().ok())
        .and_then(|path| dunce::canonicalize(path).ok())
        .is_some_and(|path| path.starts_with(&context.workspace_path))
}

fn spawn_owner_listener(
    listener: TcpListener,
    endpoint: OwnerEndpoint,
    requests: mpsc::Sender<PendingOwnerRequest>,
    controls: mpsc::Sender<PendingOwnerRequest>,
    draining: Arc<AtomicBool>,
    status: watch::Receiver<Value>,
    connection_closed: watch::Sender<Instant>,
) -> JoinHandle<()> {
    spawn_owner_connections(
        listener,
        Arc::new(Semaphore::new(OWNER_HANDSHAKE_LIMIT)),
        OWNER_HANDSHAKE_TIMEOUT,
        connection_closed,
        move |mut stream, permit, deadline| {
            let endpoint = endpoint.clone();
            let requests = requests.clone();
            let controls = controls.clone();
            let draining = Arc::clone(&draining);
            let status = status.clone();
            async move {
                let authenticated =
                    authenticate_owner_connection(&mut stream, &endpoint, permit, deadline).await;
                if let Ok(Some(request)) = authenticated {
                    let _ = handle_owner_connection(
                        stream, request, endpoint, requests, controls, draining, status,
                    )
                    .await;
                }
            }
        },
    )
}

fn spawn_owner_connections<F>(
    listener: TcpListener,
    admission: Arc<Semaphore>,
    handshake_timeout: Duration,
    connection_closed: watch::Sender<Instant>,
    handler: impl Fn(TcpStream, OwnedSemaphorePermit, TokioInstant) -> F + Send + 'static,
) -> JoinHandle<()>
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            // ponytail: four 64 MiB frames plus task/JSON overhead; allocate incrementally if too high.
            // This bounds only unauthenticated work, not fairness under continuous hostile arrivals.
            let Ok(permit) = Arc::clone(&admission).try_acquire_owned() else {
                drop(stream);
                connection_closed.send_replace(Instant::now());
                continue;
            };
            let deadline = TokioInstant::now() + handshake_timeout;
            let connection_closed = connection_closed.clone();
            let handling = handler(stream, permit, deadline);
            tokio::spawn(async move {
                handling.await;
                connection_closed.send_replace(Instant::now());
            });
        }
    })
}

async fn authenticate_owner_connection<S>(
    stream: &mut S,
    endpoint: &OwnerEndpoint,
    _permit: OwnedSemaphorePermit,
    deadline: TokioInstant,
) -> io::Result<Option<AuthenticatedOwnerRequest>>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let timeout_error = || io::Error::new(io::ErrorKind::TimedOut, "Owner handshake timed out");
    tokio_time::timeout_at(deadline, async {
        // timeout_at polls ready work first; do not admit an already-expired request.
        if TokioInstant::now() >= deadline {
            return Err(timeout_error());
        }
        let request: AuthenticatedOwnerRequest = read_owner_message(stream).await?;
        let authenticated = request.owner_protocol_version == OWNER_PROTOCOL_VERSION
            && request.session_identity == endpoint.session_identity
            && request.owner_generation == endpoint.owner_generation
            && constant_time_token_matches(&request.token, &endpoint.token);
        // Framing/JSON decoding and identity validation can finish without yielding.
        if TokioInstant::now() >= deadline {
            return Err(timeout_error());
        }
        if !authenticated {
            let failure = OwnerResponse::failure(
                &endpoint.owner_generation,
                json!({
                    "category": "unavailable",
                    "code": "owner_unavailable",
                    "message": "Owner authentication or identity verification failed.",
                    "stage": "discover_owner",
                    "delivery": "not_sent",
                    "retry": "safe",
                    "data": {"sessionIdentity": endpoint.session_identity, "reason": "authentication_failed"}
                }),
            );
            write_owner_message(stream, &failure).await?;
            return Ok(None);
        }
        Ok(Some(request))
    })
    .await
    .map_err(|_| timeout_error())?
}

async fn handle_owner_connection(
    mut stream: TcpStream,
    request: AuthenticatedOwnerRequest,
    endpoint: OwnerEndpoint,
    requests: mpsc::Sender<PendingOwnerRequest>,
    controls: mpsc::Sender<PendingOwnerRequest>,
    draining: Arc<AtomicBool>,
    status: watch::Receiver<Value>,
) -> io::Result<()> {
    if matches!(&request.request, OwnerRequest::Status) {
        let response = OwnerResponse::success(&endpoint.owner_generation, status.borrow().clone());
        write_owner_message(&mut stream, &response).await?;
        return Ok(());
    }
    let (response_tx, response_rx) = oneshot::channel();
    let (dispatched_tx, dispatched_rx) = oneshot::channel();
    let (delivered_tx, delivered_rx) = oneshot::channel();
    let (cancel_tx, cancel_rx) = watch::channel(false);
    let queue_deadline = request.queue_deadline_ms.map(Duration::from_millis);
    let control_request = matches!(request.request, OwnerRequest::Stop { .. });
    if draining.load(Ordering::Acquire) && !control_request {
        let failure = OwnerResponse::failure(
            &endpoint.owner_generation,
            owner_draining_failure(&endpoint.session_identity),
        );
        write_owner_message(&mut stream, &failure).await?;
        return Ok(());
    }
    let pending = PendingOwnerRequest {
        request: request.request,
        response: response_tx,
        dispatched: dispatched_tx,
        delivered: delivered_rx,
        cancelled: cancel_rx,
    };
    let queued = if control_request {
        controls.try_send(pending)
    } else {
        requests.try_send(pending)
    };
    if queued.is_err() {
        let failure = OwnerResponse::failure(
            &endpoint.owner_generation,
            json!({
                "category": "unavailable",
                "code": "owner_queue_full",
                "message": "The Owner operation queue is full.",
                "stage": "queue",
                "delivery": "not_sent",
                "retry": "safe",
                "data": {"limit": OWNER_QUEUE_LIMIT, "depth": OWNER_QUEUE_LIMIT}
            }),
        );
        write_owner_message(&mut stream, &failure).await?;
        return Ok(());
    }
    if let Some(queue_deadline) = queue_deadline {
        let mut response_rx = response_rx;
        let mut dispatched_rx = dispatched_rx;
        tokio::select! {
            response = &mut response_rx => {
                if let Ok(response) = response {
                    write_owner_message(&mut stream, &response).await?;
                    let _ = delivered_tx.send(());
                }
            }
            _ = &mut dispatched_rx => {
                await_owner_response(
                    &mut stream,
                    response_rx,
                    delivered_tx,
                    cancel_tx,
                ).await?;
            }
            _ = tokio_time::sleep(queue_deadline) => {
                let _ = cancel_tx.send(true);
                let failure = OwnerResponse::failure(
                    &endpoint.owner_generation,
                    queue_deadline_failure(queue_deadline),
                );
                write_owner_message(&mut stream, &failure).await?;
            }
            disconnected = stream.read_u8() => {
                let _ = disconnected;
                let _ = cancel_tx.send(true);
            }
        }
    } else {
        await_owner_response(&mut stream, response_rx, delivered_tx, cancel_tx).await?;
    }
    Ok(())
}

async fn await_owner_response(
    stream: &mut TcpStream,
    response_rx: oneshot::Receiver<OwnerResponse>,
    delivered_tx: oneshot::Sender<()>,
    cancel_tx: watch::Sender<bool>,
) -> io::Result<()> {
    tokio::select! {
        response = response_rx => {
            if let Ok(response) = response {
                write_owner_message(stream, &response).await?;
                let _ = delivered_tx.send(());
            }
        }
        disconnected = stream.read_u8() => {
            let _ = disconnected;
            let _ = cancel_tx.send(true);
        }
    }
    Ok(())
}

fn queue_deadline_failure(deadline: Duration) -> Value {
    json!({
        "category": "unavailable",
        "code": "queue_deadline_exceeded",
        "message": "The invocation deadline expired before the operation was dispatched.",
        "stage": "queue",
        "delivery": "not_sent",
        "retry": "safe",
        "data": {
            "deadline": format_duration(deadline),
            "waitedMs": deadline.as_millis() as u64
        }
    })
}

fn initializing_status(bootstrap: &OwnerBootstrap) -> Value {
    json!({
        "sessionIdentity": bootstrap.session_identity,
        "ownerGeneration": bootstrap.owner_generation,
        "workspaceUri": bootstrap.workspace_uri,
        "server": bootstrap.server,
        "state": "initializing",
        "uptimeMs": 0,
        "idleDeadline": Value::Null,
        "queueDepth": 0,
        "activeQuery": Value::Null,
        "capabilities": {},
        "progress": []
    })
}

fn status_result(
    bootstrap: &OwnerBootstrap,
    lsp: &LspRuntime,
    state: &str,
    started: Instant,
    queue_depth: usize,
    active_queries: &BTreeMap<i64, ActiveQuery>,
    idle_deadline: Instant,
) -> Value {
    let active_query = active_queries.values().next().map(|query| {
        json!({
            "command": ["raw"],
            "method": query.method,
            "startedAt": query.started_at
        })
    });
    let idle_deadline = active_query
        .is_none()
        .then(|| rfc3339_after(idle_deadline.saturating_duration_since(Instant::now())));
    json!({
        "sessionIdentity": bootstrap.session_identity,
        "ownerGeneration": bootstrap.owner_generation,
        "workspaceUri": bootstrap.workspace_uri,
        "server": bootstrap.server,
        "state": state,
        "serverPid": lsp.process.pid(),
        "uptimeMs": started.elapsed().as_millis() as u64,
        "idleDeadline": idle_deadline,
        "queueDepth": queue_depth.min(OWNER_QUEUE_LIMIT),
        "activeQuery": active_query,
        "capabilities": lsp.negotiated.providers_json(),
        "progress": lsp.progress.values().collect::<Vec<_>>()
    })
}

#[derive(Default)]
struct ProtocolTrace {
    frames: Vec<Value>,
    retained_bytes: usize,
    omitted_messages: u64,
    omitted_bytes: u64,
}

impl ProtocolTrace {
    fn new() -> Self {
        Self::default()
    }

    fn push(&mut self, direction: &str, header: &[u8], body: &[u8], message: &Value) {
        let size = header.len().saturating_add(body.len());
        if self.retained_bytes.saturating_add(size) > TRACE_CAPACITY_BYTES {
            self.omitted_messages = self.omitted_messages.saturating_add(1);
            self.omitted_bytes = self.omitted_bytes.saturating_add(size as u64);
            return;
        }
        let ordinal = self.frames.len();
        self.retained_bytes = self.retained_bytes.saturating_add(size);
        self.frames.push(json!({
            "ordinal": ordinal,
            "direction": direction,
            "headerHex": hex::encode(header),
            "bodyHex": hex::encode(body),
            "message": message
        }));
    }

    fn render(self) -> Value {
        json!({
            "retainedBytes": self.retained_bytes,
            "limitBytes": TRACE_CAPACITY_BYTES,
            "truncated": self.omitted_messages > 0,
            "omittedMessages": self.omitted_messages,
            "omittedBytes": self.omitted_bytes,
            "frames": self.frames
        })
    }
}

pub(crate) fn read_launch_settings(path: &Path) -> io::Result<OwnerLaunchSettings> {
    let bytes = fs::read(path)?;
    serde_json::from_slice(&bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn acquire_owner_lock(path: &Path) -> io::Result<std::fs::File> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?;
    crate::state_permissions::restrict_file(path)?;
    file.try_lock()?;
    Ok(file)
}

fn write_endpoint(path: &Path, endpoint: &OwnerEndpoint) -> io::Result<()> {
    let mut file = AtomicWriteFile::options().open(path)?;
    serde_json::to_writer(&mut file, endpoint).map_err(io::Error::other)?;
    file.commit()?;
    crate::state_permissions::restrict_file(path)
}

fn is_response_for(message: &Value, id: &Value) -> bool {
    message.get("id") == Some(id)
        && message.get("method").is_none()
        && (message.get("result").is_some() ^ message.get("error").is_some())
}

fn token_key(token: &Value) -> String {
    serde_json::to_string(token).unwrap_or_default()
}

fn json_rpc_error(id: Value, code: i64, message: &str, data: Option<Value>) -> Value {
    let mut error = json!({"code": code, "message": message});
    if let Some(data) = data {
        error["data"] = data;
    }
    json!({"jsonrpc": "2.0", "id": id, "error": error})
}

fn compact_json_object(value: &mut Value) {
    if let Some(object) = value.as_object_mut() {
        object.retain(|_, value| !value.is_null());
    }
}

fn lsp_log_level(value: Option<&Value>) -> &'static str {
    match value.and_then(Value::as_u64) {
        Some(1) => "error",
        Some(2) => "warning",
        Some(4) => "log",
        _ => "info",
    }
}

fn server_error_failure(error: &Value) -> Value {
    let code = error.get("code").and_then(Value::as_i64).unwrap_or(-32603);
    let classification = match code {
        -32800 => "request_cancelled",
        -32801 => "content_modified",
        -32802 => "server_cancelled",
        _ => "server_error",
    };
    let mut failure = json!({
        "category": "query",
        "code": classification,
        "message": error.get("message").and_then(Value::as_str).unwrap_or("The server returned an error."),
        "stage": "await_response",
        "delivery": "sent",
        "retry": "after_change",
        "data": {},
        "serverError": error
    });
    if classification == "server_cancelled" {
        failure["data"] = json!({
            "retriggerRequest": error.pointer("/data/retriggerRequest").and_then(Value::as_bool).unwrap_or(false)
        });
    } else if classification == "request_cancelled" {
        failure["data"] = json!({"source": "server"});
    }
    failure
}

fn contract_failure_value(failure: ContractFailure) -> Value {
    json!({
        "category": failure.category,
        "code": failure.code,
        "message": failure.message,
        "stage": failure.stage,
        "delivery": failure.delivery,
        "retry": failure.retry,
        "data": failure.data
    })
}

fn contract_failure_ref_value(failure: &ContractFailure) -> Value {
    json!({
        "category": failure.category,
        "code": failure.code,
        "message": failure.message,
        "stage": failure.stage,
        "delivery": failure.delivery,
        "retry": failure.retry,
        "data": failure.data
    })
}

fn attach_trace(failure: &mut Value, trace: Option<ProtocolTrace>) {
    if let Some(trace) = trace {
        failure["trace"] = trace.render();
    }
}

fn partial_result_items(chunks: &[Value]) -> Vec<Value> {
    chunks
        .iter()
        .flat_map(|chunk| match chunk {
            Value::Array(items) => items.as_slice(),
            chunk => std::slice::from_ref(chunk),
        })
        .cloned()
        .collect()
}

fn attach_dispatch_evidence(failure: &mut Value, query: &mut ActiveQuery) {
    let partial_items = partial_result_items(&query.partial_chunks);
    if !partial_items.is_empty() {
        failure["partialResult"] = json!({"items": partial_items, "complete": false});
    }
    if !query.apply_edit_ledger.is_empty() {
        failure["applyEditLedger"] = Value::Array(query.apply_edit_ledger.clone());
    }
    attach_trace(failure, query.trace.take());
}

async fn fail_active_queries(
    owner_generation: &str,
    active: &mut BTreeMap<i64, ActiveQuery>,
    failure: Value,
) {
    let mut deliveries = Vec::new();
    for (_, mut query) in std::mem::take(active) {
        let mut failure = failure.clone();
        attach_dispatch_evidence(&mut failure, &mut query);
        if let Some(response) = query.response.take() {
            let _ = response.send(OwnerResponse::failure(owner_generation, failure));
        }
        if let Some(delivered) = query.delivered.take() {
            deliveries.push(delivered);
        }
    }
    wait_for_owner_response_flushes(deliveries).await;
}

async fn fail_queued_requests(
    owner_generation: &str,
    requests: &mut mpsc::Receiver<PendingOwnerRequest>,
    failure: Value,
) {
    let mut deliveries = Vec::new();
    while let Ok(pending) = requests.try_recv() {
        if !*pending.cancelled.borrow() {
            let _ = pending
                .response
                .send(OwnerResponse::failure(owner_generation, failure.clone()));
            deliveries.push(pending.delivered);
        }
    }
    wait_for_owner_response_flushes(deliveries).await;
}

async fn wait_for_owner_response_flush(delivered: oneshot::Receiver<()>) {
    let _ = tokio_time::timeout(OWNER_RESPONSE_FLUSH_TIMEOUT, delivered).await;
}

async fn wait_for_owner_response_flushes(deliveries: Vec<oneshot::Receiver<()>>) {
    let _ = tokio_time::timeout(OWNER_RESPONSE_FLUSH_TIMEOUT, async {
        for delivered in deliveries {
            let _ = delivered.await;
        }
    })
    .await;
}

fn stop_result(owner_generation: &str, force: bool) -> Value {
    json!({
        "ownerGeneration": owner_generation,
        "outcome": if force { "force_stopped" } else { "stopped" },
        "recoveryRequired": false
    })
}

fn owner_draining_failure(session_identity: &str) -> Value {
    json!({
        "category": "unavailable",
        "code": "owner_unavailable",
        "message": "The Owner is draining and is not accepting new work.",
        "stage": "discover_owner",
        "delivery": "not_sent",
        "retry": "safe",
        "data": {"sessionIdentity": session_identity, "reason": "draining"}
    })
}

fn server_start_failure(bootstrap: &OwnerBootstrap, error: &io::Error) -> Value {
    json!({
        "category": "query",
        "code": "server_start_failed",
        "message": "The language-server process could not start.",
        "stage": "start_server",
        "delivery": "not_sent",
        "retry": "after_change",
        "data": {
            "server": bootstrap.server,
            "executable": bootstrap.executable,
            "osCode": error.raw_os_error()
        }
    })
}

fn initialization_failure(
    bootstrap: &OwnerBootstrap,
    error: &io::Error,
    stderr_tail: &str,
) -> Value {
    let mut data = json!({"server": bootstrap.server, "reason": error.to_string()});
    if !stderr_tail.is_empty() {
        data["stderrTail"] = json!(stderr_tail);
    }
    json!({
        "category": "query",
        "code": "initialization_failed",
        "message": "The language server failed to initialize.",
        "stage": "initialize",
        "delivery": "not_sent",
        "retry": "after_change",
        "data": data
    })
}

fn append_bounded_stderr_tail(tail: &mut String, chunk: &str) {
    tail.push_str(chunk);
    if tail.len() <= SERVER_STDERR_TAIL_BYTES {
        return;
    }
    let mut start = tail.len() - SERVER_STDERR_TAIL_BYTES;
    while !tail.is_char_boundary(start) {
        start += 1;
    }
    tail.drain(..start);
}

fn transport_failure(error: &io::Error) -> Value {
    json!({
        "category": "query", "code": "transport_failed", "message": "The language-server transport failed.",
        "stage": "await_response", "delivery": "uncertain", "retry": "unsafe",
        "data": {"reason": error.to_string(), "osCode": error.raw_os_error()}
    })
}

fn protocol_failure(reason: String) -> Value {
    json!({
        "category": "query", "code": "protocol_failed", "message": "The language-server protocol became unsafe.",
        "stage": "await_response", "delivery": "uncertain", "retry": "unsafe", "data": {"reason": reason}
    })
}

fn request_cancelled(source: &str) -> Value {
    json!({
        "category": "query", "code": "request_cancelled", "message": "The language-server request was cancelled.",
        "stage": "await_response", "delivery": "sent", "retry": "after_change", "data": {"source": source}
    })
}

fn server_exited_failure(status: Option<std::process::ExitStatus>, stderr_tail: &str) -> Value {
    json!({
        "category": "query", "code": "server_exited", "message": "The language server exited unexpectedly.",
        "stage": "await_response", "delivery": "uncertain", "retry": "unsafe",
        "data": {"status": process_status(status), "stderrTail": stderr_tail}
    })
}

fn process_status(status: Option<std::process::ExitStatus>) -> Value {
    #[cfg(unix)]
    use std::os::unix::process::ExitStatusExt;
    #[cfg(unix)]
    let signal = status.as_ref().and_then(ExitStatusExt::signal);
    #[cfg(not(unix))]
    let signal = Option::<i32>::None;
    json!({
        "success": status.as_ref().is_some_and(std::process::ExitStatus::success),
        "code": status.as_ref().and_then(std::process::ExitStatus::code),
        "signal": signal
    })
}

fn format_duration(duration: Duration) -> String {
    if duration.subsec_millis() == 0 {
        format!("{}s", duration.as_secs())
    } else {
        format!("{}ms", duration.as_millis())
    }
}

fn now_rfc3339() -> String {
    OffsetDateTime::now_utc().format(&Rfc3339).unwrap()
}

fn rfc3339_after(duration: Duration) -> String {
    (OffsetDateTime::now_utc()
        + ::time::Duration::try_from(duration).unwrap_or(::time::Duration::ZERO))
    .format(&Rfc3339)
    .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    struct HandshakeFixture {
        endpoint: OwnerEndpoint,
        admission: Arc<Semaphore>,
        requests: mpsc::Receiver<PendingOwnerRequest>,
        entered: mpsc::UnboundedReceiver<()>,
        finished: mpsc::UnboundedReceiver<()>,
        listener: JoinHandle<()>,
    }

    impl HandshakeFixture {
        async fn new(handshake_timeout: Duration) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = OwnerEndpoint {
                format_version: 1,
                owner_protocol_version: OWNER_PROTOCOL_VERSION,
                session_identity: "test-session".to_owned(),
                owner_generation: "test-generation".to_owned(),
                token: "test-only-token".to_owned(),
                address: listener.local_addr().unwrap().to_string(),
                workspace_uri: "file:///test-workspace/".to_owned(),
                server: "test-server".to_owned(),
                owner_pid: 0,
                started_at: String::new(),
                state: "ready".to_owned(),
                failure: None,
            };
            let admission = Arc::new(Semaphore::new(OWNER_HANDSHAKE_LIMIT));
            let (requests_tx, requests) = mpsc::channel(OWNER_QUEUE_LIMIT);
            let (entered_tx, entered) = mpsc::unbounded_channel();
            let (finished_tx, finished) = mpsc::unbounded_channel();
            let endpoint_for_handler = endpoint.clone();
            let (connection_closed, _) = watch::channel(Instant::now());
            let listener = spawn_owner_connections(
                listener,
                Arc::clone(&admission),
                handshake_timeout,
                connection_closed,
                move |mut stream, permit, deadline| {
                    let endpoint = endpoint_for_handler.clone();
                    let requests = requests_tx.clone();
                    let entered = entered_tx.clone();
                    let finished = finished_tx.clone();
                    async move {
                        let _ = entered.send(());
                        let authenticated =
                            authenticate_owner_connection(&mut stream, &endpoint, permit, deadline)
                                .await;
                        if let Ok(Some(request)) = authenticated {
                            let (_, status) = watch::channel(json!({"state": "ready"}));
                            let _ = handle_owner_connection(
                                stream,
                                request,
                                endpoint,
                                requests.clone(),
                                requests,
                                Arc::new(AtomicBool::new(false)),
                                status,
                            )
                            .await;
                        } else {
                            drop(stream);
                        }
                        let _ = finished.send(());
                    }
                },
            );
            Self {
                endpoint,
                admission,
                requests,
                entered,
                finished,
                listener,
            }
        }

        fn request(&self, request: OwnerRequest) -> AuthenticatedOwnerRequest {
            AuthenticatedOwnerRequest {
                owner_protocol_version: OWNER_PROTOCOL_VERSION,
                session_identity: self.endpoint.session_identity.clone(),
                owner_generation: self.endpoint.owner_generation.clone(),
                token: self.endpoint.token.clone(),
                queue_deadline_ms: None,
                request,
            }
        }

        async fn connect(&mut self) -> TcpStream {
            let stream = TcpStream::connect(&self.endpoint.address).await.unwrap();
            self.entered.recv().await.unwrap();
            stream
        }

        async fn stop(&mut self) {
            self.listener.abort();
            assert!(
                (&mut self.listener)
                    .await
                    .as_ref()
                    .is_err_and(|error| error.is_cancelled())
            );
        }
    }

    impl Drop for HandshakeFixture {
        fn drop(&mut self) {
            self.listener.abort();
        }
    }

    async fn assert_handshake_closed(stream: &mut TcpStream) {
        let result = tokio_time::timeout(Duration::from_millis(500), stream.read_u8())
            .await
            .expect("unauthenticated socket exceeded its deadline");
        assert!(result.is_err_and(|error| matches!(
            error.kind(),
            io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset
        )));
    }

    #[tokio::test]
    async fn owner_handshake_admission_is_bounded() {
        tokio_time::timeout(Duration::from_secs(5), async {
            let mut fixture = HandshakeFixture::new(Duration::from_secs(1)).await;
            let mut stalled = Vec::new();
            for _ in 0..4 {
                stalled.push(fixture.connect().await);
            }
            assert_eq!(fixture.admission.available_permits(), 0);
            for _ in 0..3 {
                let mut excess = TcpStream::connect(&fixture.endpoint.address).await.unwrap();
                tokio::select! {
                    _ = fixture.entered.recv() => panic!("more than four authentication tasks admitted"),
                    _ = assert_handshake_closed(&mut excess) => {}
                }
            }
            assert!(fixture.entered.try_recv().is_err());
            drop(stalled);
            for _ in 0..4 {
                fixture.finished.recv().await.unwrap();
            }
            assert_eq!(fixture.admission.available_permits(), 4);
            fixture.stop().await;
        }).await.expect("admission test watchdog");
    }

    #[tokio::test]
    async fn owner_handshake_deadline_releases_admission() {
        tokio_time::timeout(Duration::from_secs(5), async {
            let mut fixture = HandshakeFixture::new(Duration::from_millis(100)).await;

            // A ready request must not bypass an already-expired admission deadline.
            let (mut peer, mut server) = tokio::io::duplex(1024);
            write_owner_message(&mut peer, &fixture.request(OwnerRequest::Status))
                .await
                .unwrap();
            let permit = Arc::clone(&fixture.admission).try_acquire_owned().unwrap();
            let result = authenticate_owner_connection(
                &mut server,
                &fixture.endpoint,
                permit,
                TokioInstant::now() - Duration::from_millis(1),
            )
            .await;
            assert!(matches!(result, Err(error) if error.kind() == io::ErrorKind::TimedOut));
            assert_eq!(fixture.admission.available_permits(), 4);

            let mut header = fixture.connect().await;
            header.write_all(&[0]).await.unwrap();
            let mut body = fixture.connect().await;
            body.write_u32(8).await.unwrap();
            body.write_all(b"{").await.unwrap();
            assert_eq!(fixture.admission.available_permits(), 2);
            assert_handshake_closed(&mut header).await;
            assert_handshake_closed(&mut body).await;
            for _ in 0..2 {
                fixture.finished.recv().await.unwrap();
            }
            assert_eq!(fixture.admission.available_permits(), 4);

            // Malformed JSON and over-limit frames also release admission without large allocations.
            for bytes in [&b"\x00\x00\x00\x01!"[..], &u32::MAX.to_be_bytes()[..]] {
                let mut stream = fixture.connect().await;
                stream.write_all(bytes).await.unwrap();
                assert_handshake_closed(&mut stream).await;
                fixture.finished.recv().await.unwrap();
                assert_eq!(fixture.admission.available_permits(), 4);
            }
            let mut stream = fixture.connect().await;
            write_owner_message(&mut stream, &fixture.request(OwnerRequest::Status))
                .await
                .unwrap();
            let response: OwnerResponse = read_owner_message(&mut stream).await.unwrap();
            assert!(response.ok);
            assert_eq!(response.result.unwrap()["state"], "ready");
            fixture.finished.recv().await.unwrap();
            assert_eq!(fixture.admission.available_permits(), 4);

            // Already-admitted handshakes still expire after the accept loop shuts down.
            let mut stalled = fixture.connect().await;
            fixture.stop().await;
            assert_handshake_closed(&mut stalled).await;
            fixture.finished.recv().await.unwrap();
            assert_eq!(fixture.admission.available_permits(), 4);
        })
        .await
        .expect("handshake deadline test watchdog");
    }

    #[tokio::test]
    async fn owner_handshake_failure_response_is_bounded() {
        tokio_time::timeout(Duration::from_secs(5), async {
            let mut fixture = HandshakeFixture::new(Duration::from_millis(100)).await;
            for invalid_field in 0..4 {
                let mut request = fixture.request(OwnerRequest::Status);
                match invalid_field {
                    0 => request.owner_protocol_version += 1,
                    1 => request.session_identity.push('!'),
                    2 => request.owner_generation.push('!'),
                    _ => request.token.push('!'),
                }
                // Preserve the existing failure JSON whenever the peer reads it.
                let mut stream = fixture.connect().await;
                write_owner_message(&mut stream, &request).await.unwrap();
                let response: OwnerResponse = read_owner_message(&mut stream).await.unwrap();
                assert!(!response.ok);
                assert_eq!(
                    response.error.as_ref().unwrap()["code"],
                    "owner_unavailable"
                );
                assert_eq!(
                    response.error.unwrap()["data"]["reason"],
                    "authentication_failed"
                );
                fixture.finished.recv().await.unwrap();
                assert_eq!(fixture.admission.available_permits(), 4);

                // Pause only in-memory I/O: scheduler delays must not consume the request budget.
                tokio_time::pause();
                // A one-byte duplex capacity deterministically blocks the failure response.
                let (mut peer, mut server) = tokio::io::duplex(1);
                let permit = Arc::clone(&fixture.admission).try_acquire_owned().unwrap();
                let deadline = TokioInstant::now() + Duration::from_millis(100);
                let (finished_tx, finished_rx) = oneshot::channel();
                let authentication = async {
                    let result = tokio_time::timeout_at(
                        deadline + Duration::from_millis(50),
                        authenticate_owner_connection(
                            &mut server,
                            &fixture.endpoint,
                            permit,
                            deadline,
                        ),
                    )
                    .await
                    .expect("failure write restarted or exceeded the handshake deadline");
                    drop(server);
                    assert!(
                        matches!(result, Err(error) if error.kind() == io::ErrorKind::TimedOut)
                    );
                    finished_tx.send(()).unwrap();
                };
                let caller = async {
                    // Spend most of the same absolute budget receiving the request.
                    tokio_time::sleep_until(deadline - Duration::from_millis(25)).await;
                    write_owner_message(&mut peer, &request).await.unwrap();
                    peer.read_u8().await.unwrap();
                    finished_rx.await.unwrap();
                };
                tokio::join!(authentication, caller);
                tokio_time::resume();
                assert_eq!(fixture.admission.available_permits(), 4);
            }
            fixture.stop().await;
        })
        .await
        .expect("failure response test watchdog");
    }

    #[tokio::test]
    async fn owner_handshake_does_not_deadline_authenticated_query() {
        tokio_time::timeout(Duration::from_secs(5), async {
            let mut fixture = HandshakeFixture::new(Duration::from_millis(100)).await;
            let mut stream = fixture.connect().await;
            let mut request = fixture.request(OwnerRequest::Dispatch {
                method: "test/query".to_owned(),
                params: None,
                documents: Vec::new(),
                refresh_open_documents: false,
                raw_request: true,
                request_timeout_ms: 2000,
                trace_protocol: false,
                apply_edits: false,
            });
            request.queue_deadline_ms = Some(2000);
            write_owner_message(&mut stream, &request).await.unwrap();
            let pending = fixture.requests.recv().await.unwrap();
            assert!(matches!(pending.request, OwnerRequest::Dispatch { .. }));
            assert_eq!(fixture.admission.available_permits(), 4);
            assert!(
                tokio_time::timeout(
                    Duration::from_millis(200),
                    read_owner_message::<_, OwnerResponse>(&mut stream)
                )
                .await
                .is_err(),
                "queued Query inherited the handshake deadline"
            );
            assert!(!*pending.cancelled.borrow());
            pending.dispatched.send(()).unwrap();
            assert!(
                tokio_time::timeout(
                    Duration::from_millis(200),
                    read_owner_message::<_, OwnerResponse>(&mut stream)
                )
                .await
                .is_err(),
                "active Query inherited the handshake deadline"
            );
            assert!(!*pending.cancelled.borrow());
            assert_eq!(fixture.admission.available_permits(), 4);
            pending
                .response
                .send(OwnerResponse::success(
                    &fixture.endpoint.owner_generation,
                    json!({"completed": true}),
                ))
                .unwrap();
            let response: OwnerResponse = read_owner_message(&mut stream).await.unwrap();
            assert!(response.ok);
            assert_eq!(response.result.unwrap()["completed"], true);
            pending.delivered.await.unwrap();
            fixture.finished.recv().await.unwrap();
            fixture.stop().await;
        })
        .await
        .expect("authenticated Query test watchdog");
    }

    #[test]
    fn recursive_glob_file_operation_filters_reject_filename_suffixes() {
        let filter = json!({
            "scheme": "file",
            "pattern": {"glob": "**/main.rs", "matches": "file", "options": {"ignoreCase": false}}
        });
        let mut initialized = json!({"capabilities": {"workspace": {"fileOperations": {
            "didRename": {"filters": [filter.clone()]},
            "didCreate": {"filters": [filter]}
        }}}});
        for (old, new, expected) in [
            ("domain.rs", "other.rs", false),
            ("other.rs", "domain.rs", false),
            ("main.rs", "other.rs", true),
            ("other.rs", "nested/main.rs", true),
        ] {
            let operation = json!({
                "kind": "rename", "oldUri": format!("file:///workspace/{old}"),
                "newUri": format!("file:///workspace/{new}"), "isDirectory": false
            });
            assert_eq!(
                file_operation_registered(&initialized, "didRename", &operation),
                expected,
                "{old} -> {new}"
            );
        }
        for (name, directory, expected) in [
            ("domain.rs", false, false),
            ("main.rs", false, true),
            ("main.rs", true, false),
            ("MAIN.RS", false, false),
        ] {
            assert_eq!(
                file_operation_registered(
                    &initialized,
                    "didCreate",
                    &json!({
                        "kind": "create", "uri": format!("file:///workspace/{name}"),
                        "isDirectory": directory
                    })
                ),
                expected,
                "create {name}, directory={directory}"
            );
        }
        initialized["capabilities"]["workspace"]["fileOperations"]["didCreate"]["filters"][0]["pattern"]
            ["options"]["ignoreCase"] = json!(true);
        for (name, expected) in [("MAIN.RS", true), ("DOMAIN.RS", false)] {
            assert_eq!(
                file_operation_registered(
                    &initialized,
                    "didCreate",
                    &json!({
                        "kind": "create", "uri": format!("file:///workspace/{name}"),
                        "isDirectory": false
                    })
                ),
                expected,
                "case-insensitive create {name}"
            );
        }
    }

    #[test]
    fn file_operation_filters_honor_kind_glob_and_case_options() {
        let initialized = json!({
            "capabilities": {"workspace": {"fileOperations": {"didRename": {
                "filters": [{
                    "scheme": "file",
                    "pattern": {
                        "glob": "**/*.{RS,py}",
                        "matches": "file",
                        "options": {"ignoreCase": true}
                    }
                }]
            }}}}
        });
        assert!(file_operation_registered(
            &initialized,
            "didRename",
            &json!({
                "kind": "rename",
                "oldUri": "file:///workspace/old.txt",
                "newUri": "file:///workspace/src/main.rs",
                "isDirectory": false
            })
        ));
        assert!(!file_operation_registered(
            &initialized,
            "didRename",
            &json!({
                "kind": "rename",
                "oldUri": "file:///workspace/old.txt",
                "newUri": "file:///workspace/src/main.rs",
                "isDirectory": true
            })
        ));
    }

    #[test]
    fn workspace_configuration_returns_null_for_malformed_items() {
        let context = CallbackContext {
            settings: json!({"rust": {"check": true}}),
            workspace_uri: "file:///workspace/".to_owned(),
            workspace_path: PathBuf::from("/workspace"),
            workspace_folder: json!({"uri": "file:///workspace/", "name": "workspace"}),
        };
        let result = workspace_configuration_result(
            &context,
            &json!({"items": [null, {"scopeUri": 7}, {"section": 7}, {"section": "rust.check"}]}),
        )
        .unwrap();
        assert_eq!(result, json!([null, null, null, true]));
    }
}
