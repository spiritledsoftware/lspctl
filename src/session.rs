#![allow(clippy::result_large_err)]

pub(crate) mod json_rpc_transport;

mod capabilities;
mod owner_protocol;
mod owner_runtime;
mod process_supervision;
mod session_log;

use std::{
    ffi::OsString,
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    process::{Command, ExitCode, Stdio},
    thread,
    time::{Duration, Instant},
};

use atomic_write_file::AtomicWriteFile;
use serde_json::{Value, json};
use tokio::net::TcpStream;
use url::Url;

use crate::{
    cli::ParsedInvocation,
    configuration::{
        AuthorizedServer, LoadedConfiguration, authorize_server, load_configuration,
        resolved_user_state_directory, select_configured_server, select_named_server,
        select_server,
    },
    contract::ContractFailure,
    query::{
        Capabilities, DispatchFailure, DispatchRequest, DispatchResponse, PreviewCreator,
        PreviewProposal, QueryContext, QueryExecutionFailure, SessionDispatcher, compose, execute,
    },
    workspace::{
        DiagnosticCache, DocumentStore, PositionEncoding, TextSynchronization, select_workspace,
        validate_document_scope, workspace_from_path,
    },
};
use owner_protocol::{
    AuthenticatedOwnerRequest, OWNER_PROTOCOL_VERSION, OwnerEndpoint, OwnerLaunchSettings,
    OwnerRequest, OwnerResponse, parse_duration, read_owner_message, write_owner_message,
};
use owner_runtime::{OwnerBootstrap, read_launch_settings, run_owner};

/// Dispatches session lifecycle commands without starting an Owner during discovery.
pub(crate) fn dispatch_session_command(
    invocation: &ParsedInvocation,
) -> Result<Value, ContractFailure> {
    run_async(async {
        match invocation.command_path().get(1).map(String::as_str) {
            Some("list") => list_sessions(invocation).await,
            Some("status") => {
                let endpoint = select_live_endpoint(invocation).await?;
                let status = send_owner_request(&endpoint, OwnerRequest::Status).await?;
                Ok(success_envelope(invocation.command_path(), status))
            }
            Some("logs") => {
                let endpoint = select_live_endpoint(invocation).await?;
                let tail = invocation
                    .option_string("--tail")
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(100);
                let logs = send_owner_request(&endpoint, OwnerRequest::Logs { tail }).await?;
                Ok(success_envelope(invocation.command_path(), logs))
            }
            Some("stop") => {
                let endpoint = select_live_endpoint(invocation).await?;
                let stopped = send_owner_request(
                    &endpoint,
                    OwnerRequest::Stop {
                        force: invocation.has_option("--force"),
                    },
                )
                .await?;
                Ok(success_envelope(invocation.command_path(), stopped))
            }
            Some("restart") => restart_session(invocation).await,
            _ => unreachable!("the CLI catalog limits Session command paths"),
        }
    })
}

/// Runs every Query through the persistent Owner transport.
pub(crate) fn dispatch_owner_query_command(
    invocation: &ParsedInvocation,
) -> Result<Value, QueryExecutionFailure> {
    run_async(dispatch_owner_query(invocation))
}

async fn dispatch_owner_query(
    invocation: &ParsedInvocation,
) -> Result<Value, QueryExecutionFailure> {
    let queue_deadline = invocation
        .option_string("--deadline")
        .map(|value| QueueDeadline::new(parse_duration(&value)));
    let current_directory = std::env::current_dir().map_err(|error| {
        owner_unavailable(
            "sid_0000000000000000000000000000000000000000000000000000000000000000",
            &error.to_string(),
        )
    })?;
    let mut targets = invocation.option_paths("--sync-file");
    if let Some(file) = invocation.option_path("--file") {
        targets.push(file);
    }
    let workspace = select_workspace(
        invocation.option_path("--workspace").as_deref(),
        &targets,
        &current_directory,
    )?;
    let configuration = load_configuration(
        &workspace.root,
        invocation.has_option("--ignore-project-config"),
    )?;
    let server = select_server(&configuration, invocation)?;
    let request_timeout = parse_duration(&server.request_timeout);
    let authorized = authorize_server(&configuration, server)?;
    let trace_protocol = invocation.has_option("--trace-protocol");
    let endpoint = connect_or_start_owner(&configuration, &authorized, trace_protocol).await?;
    let owner_capabilities = send_owner_request_with_queue_deadline(
        &endpoint,
        OwnerRequest::Capabilities,
        queue_deadline_remaining(queue_deadline)?,
    )
    .await?;
    let capabilities = Capabilities::from_owner_result(&owner_capabilities);
    let position_encoding = match owner_capabilities["positionEncoding"].as_str() {
        Some("utf-8") => PositionEncoding::Utf8,
        _ => PositionEncoding::Utf16,
    };
    let text_synchronization = match owner_capabilities["textSynchronization"].as_str() {
        Some("open_close") => TextSynchronization::OpenClose,
        _ => TextSynchronization::None,
    };

    let mut document = if let Some(path) = invocation.option_path("--file") {
        let path =
            validate_document_scope(&workspace, &path, invocation.has_option("--server"), false)?;
        let language_id = crate::configuration::document_language_id(
            &configuration,
            &authorized.server.name,
            &path,
            invocation.option_string("--language-id").as_deref(),
        )?;
        let mut documents = DocumentStore::new(
            configuration.synchronization.max_open_documents,
            configuration.synchronization.max_document_bytes,
            configuration.synchronization.max_total_text_bytes,
        );
        Some(
            documents
                .refresh(&path, &language_id, text_synchronization)?
                .snapshot,
        )
    } else {
        None
    };
    let mut diagnostics = DiagnosticCache::new(
        configuration.synchronization.max_diagnostic_snapshots,
        configuration.synchronization.max_diagnostic_bytes,
    );
    if invocation.command_path().first().is_some_and(|command| {
        matches!(
            command.as_str(),
            "document-diagnostics" | "workspace-diagnostics" | "published-diagnostics"
        )
    }) {
        let synchronized = document
            .as_ref()
            .map(|snapshot| owner_protocol::OwnerDocumentInput {
                path: snapshot.path.clone(),
                language_id: snapshot.language_id.clone(),
                expected_digest: snapshot.digest.clone(),
            })
            .into_iter()
            .collect();
        let first_state = send_owner_request_with_queue_deadline(
            &endpoint,
            OwnerRequest::Diagnostics {
                documents: synchronized,
            },
            queue_deadline_remaining(queue_deadline)?,
        )
        .await?;
        let document_versions = first_state["documentVersions"].clone();
        diagnostics.import_state(&first_state["state"]);
        if let Some(document) = &mut document
            && let Some(version) = document_versions[&document.uri].as_i64()
        {
            document.version = version;
        }
        if invocation
            .command_path()
            .first()
            .is_some_and(|command| command == "published-diagnostics")
            && invocation.has_option("--file")
        {
            let document = document.as_ref().unwrap();
            let deadline =
                Instant::now() + parse_duration(&authorized.server.published_diagnostics_wait);
            while !diagnostics
                .published(&document.uri, Some(document.version))
                .complete
                && Instant::now() < deadline
            {
                tokio::time::sleep(
                    deadline
                        .saturating_duration_since(Instant::now())
                        .min(Duration::from_millis(25)),
                )
                .await;
                let state = send_owner_request_with_queue_deadline(
                    &endpoint,
                    OwnerRequest::Diagnostics {
                        documents: Vec::new(),
                    },
                    queue_deadline_remaining(queue_deadline)?,
                )
                .await?;
                diagnostics.import_state(&state["state"]);
            }
        }
    }
    let composed = compose(
        invocation,
        document.as_ref(),
        position_encoding,
        &capabilities,
        Some(&diagnostics),
    )?;
    let context = QueryContext {
        workspace_uri: configuration.workspace_uri.clone(),
        server: authorized.server.name.clone(),
        session_identity: authorized.session_identity.clone(),
        owner_generation: endpoint.owner_generation.clone(),
        result_position_encoding: position_encoding,
        server_progress: Vec::new(),
        synchronization: json!({
            "mode": if invocation.command_path().first().is_some_and(|command| command == "workspace-diagnostics") {
                "workspace"
            } else if document.is_some() {
                "document"
            } else {
                "none"
            },
            "bestEffort": false,
            "before": document.as_ref().map(|snapshot| json!({
                "uri": snapshot.uri,
                "digest": snapshot.digest,
                "version": snapshot.version,
                "languageId": snapshot.language_id
            })).into_iter().collect::<Vec<_>>(),
            "failures": [],
            "postResponseChanged": []
        }),
        recovery: json!({"required": false}),
    };
    let mut dispatcher = OwnerQueryDispatcher {
        endpoint: &endpoint,
        request_timeout,
        queue_deadline,
        configuration: &configuration,
        workspace: &workspace,
        server: &authorized.server.name,
        explicit_language_id: invocation.option_string("--language-id"),
        server_explicitly_selected: invocation.has_option("--server"),
        documents: DocumentStore::new(
            configuration.synchronization.max_open_documents,
            configuration.synchronization.max_document_bytes,
            configuration.synchronization.max_total_text_bytes,
        ),
        text_synchronization,
    };
    let mut previews = MutationPreviewCreator {
        configuration: &configuration,
        authorized: &authorized,
        position_encoding,
    };
    execute(
        &mut dispatcher,
        composed,
        context,
        &mut diagnostics,
        &mut previews,
    )
}

#[derive(Clone, Copy)]
struct QueueDeadline {
    started: Instant,
    duration: Duration,
}

impl QueueDeadline {
    fn new(duration: Duration) -> Self {
        Self {
            started: Instant::now(),
            duration,
        }
    }
}

fn queue_deadline_remaining(
    deadline: Option<QueueDeadline>,
) -> Result<Option<Duration>, ContractFailure> {
    let Some(deadline) = deadline else {
        return Ok(None);
    };
    let elapsed = deadline.started.elapsed();
    let Some(remaining) = deadline.duration.checked_sub(elapsed) else {
        return Err(ContractFailure {
            exit_code: 4,
            category: "unavailable",
            code: "queue_deadline_exceeded",
            message: "The invocation deadline expired before the operation was dispatched."
                .to_owned(),
            stage: "queue",
            delivery: "not_sent",
            retry: "safe",
            data: json!({
                "deadline": format_duration(deadline.duration),
                "waitedMs": elapsed.as_millis() as u64
            }),
        });
    };
    Ok(Some(remaining))
}

fn format_duration(duration: Duration) -> String {
    if duration.subsec_millis() == 0 {
        format!("{}s", duration.as_secs())
    } else {
        format!("{}ms", duration.as_millis())
    }
}

struct OwnerQueryDispatcher<'a> {
    endpoint: &'a OwnerEndpoint,
    request_timeout: Duration,
    queue_deadline: Option<QueueDeadline>,
    configuration: &'a LoadedConfiguration,
    workspace: &'a crate::workspace::Workspace,
    server: &'a str,
    explicit_language_id: Option<String>,
    server_explicitly_selected: bool,
    documents: DocumentStore,
    text_synchronization: TextSynchronization,
}

impl SessionDispatcher for OwnerQueryDispatcher<'_> {
    fn dispatch(
        &mut self,
        mut request: DispatchRequest,
    ) -> Result<DispatchResponse, DispatchFailure> {
        let mut synchronized = Vec::new();
        for path in &request.synchronized_files {
            let path = validate_document_scope(
                self.workspace,
                path,
                self.server_explicitly_selected,
                false,
            )
            .map_err(DispatchFailure::from)?;
            let language_id = crate::configuration::document_language_id(
                self.configuration,
                self.server,
                &path,
                self.explicit_language_id.as_deref(),
            )
            .map_err(DispatchFailure::from)?;
            let snapshot = self
                .documents
                .refresh(&path, &language_id, self.text_synchronization)
                .map_err(DispatchFailure::from)?
                .snapshot;
            synchronized.push(owner_protocol::OwnerDocumentInput {
                path: snapshot.path,
                language_id: snapshot.language_id,
                expected_digest: snapshot.digest,
            });
        }
        if let Some(params) = request.params.as_mut() {
            if request.partial_results && params.is_object() {
                params["partialResultToken"] = json!(format!(
                    "lspctl-partial-{}",
                    random_hex(16).map_err(DispatchFailure::from)?
                ));
            }
            if request.work_done_progress && params.is_object() {
                params["workDoneToken"] = json!(format!(
                    "lspctl-work-{}",
                    random_hex(16).map_err(DispatchFailure::from)?
                ));
            }
        }
        let queue_deadline =
            queue_deadline_remaining(self.queue_deadline).map_err(DispatchFailure::from)?;
        let response = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(exchange_owner_request_with_queue_deadline(
                self.endpoint,
                OwnerRequest::Dispatch {
                    method: request.method,
                    params: request.params,
                    documents: synchronized,
                    refresh_open_documents: request.refresh_open_documents,
                    raw_request: request.raw_request,
                    request_timeout_ms: self.request_timeout.as_millis() as u64,
                    trace_protocol: request.trace_protocol,
                    apply_edits: request.apply_edits,
                },
                queue_deadline,
            ))
        })
        .map_err(DispatchFailure::from)?;
        let mut response = if response.ok {
            response.result.unwrap_or(Value::Null)
        } else {
            return Err(dispatch_failure_from_owner(
                response.error.unwrap_or_else(|| json!({})),
            ));
        };
        let result = response
            .as_object_mut()
            .and_then(|object| object.remove("result"))
            .unwrap_or(Value::Null);
        let trace = response
            .as_object_mut()
            .and_then(|object| object.remove("trace"));
        let partial_results = response
            .as_object_mut()
            .and_then(|object| object.remove("partialResults"))
            .and_then(|value| value.as_array().cloned())
            .unwrap_or_default();
        let apply_edit_ledger = response
            .as_object_mut()
            .and_then(|object| object.remove("applyEditLedger"))
            .and_then(|value| value.as_array().cloned())
            .unwrap_or_default();
        let server_progress = response
            .as_object_mut()
            .and_then(|object| object.remove("serverProgress"))
            .and_then(|value| value.as_array().cloned())
            .unwrap_or_default();
        let synchronization = response
            .as_object_mut()
            .and_then(|object| object.remove("synchronization"));
        Ok(DispatchResponse {
            result,
            partial_results,
            trace,
            apply_edit_ledger,
            server_progress,
            synchronization,
        })
    }
}

struct MutationPreviewCreator<'a> {
    configuration: &'a LoadedConfiguration,
    authorized: &'a AuthorizedServer,
    position_encoding: PositionEncoding,
}

impl PreviewCreator for MutationPreviewCreator<'_> {
    fn create_preview(&mut self, proposal: PreviewProposal) -> Result<Value, ContractFailure> {
        crate::mutation::create_query_preview(
            self.configuration,
            self.authorized,
            self.position_encoding,
            proposal,
        )
    }
}

/// Connects to an authenticated Owner or starts the sole lock-winning generation.
pub(crate) async fn connect_or_start_owner(
    configuration: &LoadedConfiguration,
    authorized: &AuthorizedServer,
    trace_initialization: bool,
) -> Result<OwnerEndpoint, ContractFailure> {
    let paths = OwnerStatePaths::new()?;
    if let Some(endpoint) = read_endpoint(&paths.endpoint_path(&authorized.session_identity))
        && probe_endpoint(&endpoint).await.is_ok()
    {
        return Ok(endpoint);
    }
    reauthorize_project_launch(configuration, authorized)?;
    let startup_timeout = parse_duration(&configuration.session.owner_startup_timeout);
    let startup_lock_path = paths.startup_lock_path(&authorized.session_identity);
    let startup_lock = acquire_startup_lock(&startup_lock_path, startup_timeout)?;
    if let Some(endpoint) = read_endpoint(&paths.endpoint_path(&authorized.session_identity))
        && probe_endpoint(&endpoint).await.is_ok()
    {
        drop(startup_lock);
        return Ok(endpoint);
    }
    let generation = format!("gen_{}", random_hex(16)?);
    let token = random_hex(32)?;
    let endpoint_path = paths.endpoint_path(&authorized.session_identity);
    let owner_lock_path = paths.owner_lock_path(&authorized.session_identity);
    let launch_path = paths.launch_path(&generation);
    let launch = OwnerLaunchSettings {
        session_identity: authorized.session_identity.clone(),
        owner_generation: generation,
        token,
        workspace_uri: configuration.workspace_uri.clone(),
        server: authorized.server.name.clone(),
        declaration_digest: authorized.declaration_digest.clone(),
        server_args: authorized.server.args.value.clone(),
        initialization_options: authorized
            .server
            .initialization_options
            .as_ref()
            .map(|value| value.value.clone()),
        settings: authorized.server.settings.value.clone(),
        initialization_timeout_ms: duration_millis(&configuration.session.initialization_timeout),
        cancellation_grace_ms: duration_millis(&configuration.session.cancellation_grace),
        shutdown_timeout_ms: duration_millis(&configuration.session.shutdown_timeout),
        idle_timeout_ms: duration_millis(&configuration.session.idle_timeout),
        max_message_bytes: configuration.protocol.max_message_bytes as usize,
        max_partial_result_bytes: configuration.protocol.max_partial_result_bytes as usize,
        max_diagnostic_snapshots: configuration.synchronization.max_diagnostic_snapshots,
        max_diagnostic_bytes: configuration.synchronization.max_diagnostic_bytes,
        max_open_documents: configuration.synchronization.max_open_documents,
        max_document_bytes: configuration.synchronization.max_document_bytes,
        max_total_text_bytes: configuration.synchronization.max_total_text_bytes,
        previews: configuration.previews.clone(),
        receipts: configuration.receipts.clone(),
        mutation: configuration.mutation.clone(),
        trace_initialization,
    };
    write_private_json(&launch_path, &launch).map_err(|error| {
        owner_unavailable(
            &authorized.session_identity,
            &format!("launch_state_failed: {error}"),
        )
    })?;
    spawn_detached_owner(
        authorized,
        &configuration.workspace,
        &endpoint_path,
        &owner_lock_path,
        &launch_path,
    )?;
    let deadline = Instant::now() + startup_timeout;
    while Instant::now() < deadline {
        if let Some(endpoint) = read_endpoint(&endpoint_path) {
            if endpoint.state == "failed" {
                let failure = endpoint.failure.unwrap_or_else(|| {
                    json!({
                        "category": "query",
                        "code": "initialization_failed",
                        "message": "The language server failed to initialize.",
                        "stage": "initialize",
                        "delivery": "not_sent",
                        "retry": "after_change",
                        "data": {"server": authorized.server.name, "reason": "owner_initialization_failed"}
                    })
                });
                let _ = fs::remove_file(&endpoint_path);
                drop(startup_lock);
                return Err(contract_failure_from_owner(failure));
            }
            if endpoint.session_identity == authorized.session_identity
                && probe_endpoint(&endpoint).await.is_ok()
            {
                drop(startup_lock);
                return Ok(endpoint);
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    drop(startup_lock);
    Err(owner_unavailable(
        &authorized.session_identity,
        "startup_deadline_exceeded",
    ))
}

fn reauthorize_project_launch(
    configuration: &LoadedConfiguration,
    authorized: &AuthorizedServer,
) -> Result<(), ContractFailure> {
    if authorized.declaration_digest.is_none() {
        return Ok(());
    }
    let fresh_configuration = load_configuration(&configuration.workspace, false)?;
    let fresh_server = select_configured_server(&fresh_configuration, &authorized.server.name)?;
    let fresh_authorized = authorize_server(&fresh_configuration, fresh_server)?;
    if fresh_authorized.declaration_digest != authorized.declaration_digest {
        return Err(ContractFailure {
            exit_code: 3,
            category: "blocked",
            code: "project_trust_changed",
            message:
                "Project-controlled language-server configuration changed before Owner startup."
                    .to_owned(),
            stage: "authorize",
            delivery: "not_sent",
            retry: "after_change",
            data: json!({
                "workspaceUri": configuration.workspace_uri,
                "server": authorized.server.name,
                "recordedDigest": authorized.declaration_digest,
                "currentDigest": fresh_authorized.declaration_digest,
                "changedFields": [],
                "requiredCommand": ["trust", "status", "--workspace", configuration.workspace, "--server", authorized.server.name]
            }),
        });
    }
    Ok(())
}

pub(crate) fn run_hidden_owner(arguments: &[OsString]) -> ExitCode {
    if arguments.len() != 7 {
        return ExitCode::from(1);
    }
    let workspace_path = PathBuf::from(&arguments[1]);
    let executable = PathBuf::from(&arguments[2]);
    let endpoint_path = PathBuf::from(&arguments[3]);
    let owner_lock_path = PathBuf::from(&arguments[4]);
    let launch_path = PathBuf::from(&arguments[5]);
    let settings = match read_launch_settings(&launch_path) {
        Ok(settings) => settings,
        Err(_) => return ExitCode::from(1),
    };
    let bootstrap = OwnerBootstrap {
        session_identity: settings.session_identity,
        owner_generation: settings.owner_generation,
        token: settings.token,
        workspace_uri: settings.workspace_uri,
        workspace_path,
        server: settings.server,
        executable,
        endpoint_path,
        owner_lock_path,
        launch_path,
    };
    match run_async(run_owner(bootstrap)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::from(1),
    }
}

async fn list_sessions(invocation: &ParsedInvocation) -> Result<Value, ContractFailure> {
    let workspace_uri = invocation
        .option_path("--workspace")
        .map(|path| workspace_from_path(&path, true).map(|workspace| workspace.uri))
        .transpose()?;
    let server = invocation.option_string("--server");
    let mut sessions = Vec::new();
    for endpoint in live_endpoints().await? {
        if workspace_uri
            .as_ref()
            .is_some_and(|workspace| workspace != &endpoint.workspace_uri)
            || server
                .as_ref()
                .is_some_and(|server| server != &endpoint.server)
        {
            continue;
        }
        if let Ok(status) = send_owner_request(&endpoint, OwnerRequest::Status).await {
            sessions.push(status);
        }
    }
    sessions.sort_by(|left, right| {
        left["ownerGeneration"]
            .as_str()
            .cmp(&right["ownerGeneration"].as_str())
    });
    Ok(success_envelope(
        invocation.command_path(),
        Value::Array(sessions),
    ))
}

async fn restart_session(invocation: &ParsedInvocation) -> Result<Value, ContractFailure> {
    let endpoint = select_live_endpoint(invocation).await?;
    let previous_generation = endpoint.owner_generation.clone();
    let _ = send_owner_request(&endpoint, OwnerRequest::Stop { force: false }).await?;
    let workspace_path = Url::parse(&endpoint.workspace_uri)
        .ok()
        .and_then(|uri| uri.to_file_path().ok())
        .ok_or_else(|| session_selection_failure("The Owner Workspace URI is invalid.", vec![]))?;
    let configuration = load_configuration(&workspace_path, false)?;
    let server = select_named_server(&configuration, &endpoint.server, invocation)?;
    let authorized = authorize_server(&configuration, server)?;
    wait_for_owner_exit(
        &endpoint,
        parse_duration(&configuration.session.owner_startup_timeout),
    )
    .await?;
    let replacement = connect_or_start_owner(&configuration, &authorized, false).await?;
    Ok(success_envelope(
        invocation.command_path(),
        json!({
            "previousOwnerGeneration": previous_generation,
            "ownerGeneration": replacement.owner_generation
        }),
    ))
}

async fn wait_for_owner_exit(
    endpoint: &OwnerEndpoint,
    timeout: Duration,
) -> Result<(), ContractFailure> {
    let paths = OwnerStatePaths::new()?;
    let endpoint_path = paths.endpoint_path(&endpoint.session_identity);
    let owner_lock_path = paths.owner_lock_path(&endpoint.session_identity);
    let deadline = Instant::now() + timeout;
    loop {
        match read_endpoint(&endpoint_path) {
            Some(current) if current.owner_generation != endpoint.owner_generation => {
                if probe_endpoint(&current).await.is_ok() {
                    return Ok(());
                }
            }
            None if owner_lock_is_free(&owner_lock_path) => return Ok(()),
            _ => {}
        }
        if Instant::now() >= deadline {
            return Err(owner_unavailable(
                &endpoint.session_identity,
                "restart_shutdown_deadline_exceeded",
            ));
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn select_live_endpoint(
    invocation: &ParsedInvocation,
) -> Result<OwnerEndpoint, ContractFailure> {
    let endpoints = live_endpoints().await?;
    let selected = if let Some(generation) = invocation.positional_string(0) {
        endpoints
            .into_iter()
            .filter(|endpoint| endpoint.owner_generation == generation)
            .collect::<Vec<_>>()
    } else {
        let workspace_uri = invocation
            .option_path("--workspace")
            .map(|path| workspace_from_path(&path, true).map(|workspace| workspace.uri))
            .transpose()?;
        let server = invocation.option_string("--server");
        endpoints
            .into_iter()
            .filter(|endpoint| {
                workspace_uri
                    .as_ref()
                    .is_none_or(|workspace| workspace == &endpoint.workspace_uri)
                    && server
                        .as_ref()
                        .is_none_or(|server| server == &endpoint.server)
            })
            .collect::<Vec<_>>()
    };
    if selected.len() != 1 {
        return Err(session_selection_failure(
            if selected.is_empty() {
                "No live Owner matches the selector."
            } else {
                "The selector matches more than one live Owner."
            },
            selected
                .iter()
                .map(|endpoint| endpoint.owner_generation.clone())
                .collect(),
        ));
    }
    Ok(selected.into_iter().next().unwrap())
}

async fn live_endpoints() -> Result<Vec<OwnerEndpoint>, ContractFailure> {
    let paths = OwnerStatePaths::new()?;
    let entries = fs::read_dir(&paths.endpoints).map_err(|error| {
        owner_unavailable(
            "sid_0000000000000000000000000000000000000000000000000000000000000000",
            &error.to_string(),
        )
    })?;
    let mut endpoints = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(identity) = endpoint_identity_from_path(&path) else {
            continue;
        };
        let Some(endpoint) = read_endpoint(&path) else {
            if owner_lock_is_free(&paths.owner_lock_path(identity)) {
                let _ = fs::remove_file(path);
            }
            continue;
        };
        if endpoint.session_identity != identity {
            continue;
        }
        if probe_endpoint(&endpoint).await.is_ok() {
            endpoints.push(endpoint);
        } else if owner_lock_is_free(&paths.owner_lock_path(&endpoint.session_identity)) {
            let _ = fs::remove_file(path);
        }
    }
    Ok(endpoints)
}

/// Drains live Owners whose authorization may have changed.
pub(crate) fn signal_workspace_owners(
    workspace_uri: &str,
    servers: &[String],
) -> (Vec<String>, Vec<String>) {
    run_async(async {
        let Ok(endpoints) = live_endpoints().await else {
            return (Vec::new(), Vec::new());
        };
        let mut signalled = Vec::new();
        let mut failures = Vec::new();
        for endpoint in endpoints.into_iter().filter(|endpoint| {
            endpoint.workspace_uri == workspace_uri
                && servers.iter().any(|server| server == &endpoint.server)
        }) {
            let generation = endpoint.owner_generation.clone();
            match send_owner_request(&endpoint, OwnerRequest::Stop { force: false }).await {
                Ok(_) => signalled.push(generation),
                Err(_) => failures.push(generation),
            }
        }
        signalled.sort();
        failures.sort();
        (signalled, failures)
    })
}

/// Refreshes open Documents after a committed external Workspace mutation.
pub(crate) fn refresh_workspace_owners(
    workspace_uri: &str,
    server: Option<&str>,
    session_identity: Option<&str>,
    file_operations: &[Value],
) -> (Vec<String>, Vec<String>) {
    run_async(async {
        let Ok(endpoints) = live_endpoints().await else {
            return (Vec::new(), Vec::new());
        };
        let mut refreshed = Vec::new();
        let mut failures = Vec::new();
        for endpoint in endpoints.into_iter().filter(|endpoint| {
            endpoint.workspace_uri == workspace_uri
                && server.is_none_or(|server| server == endpoint.server)
                && session_identity
                    .is_none_or(|session_identity| session_identity == endpoint.session_identity)
        }) {
            let generation = endpoint.owner_generation.clone();
            match send_owner_request(
                &endpoint,
                OwnerRequest::RefreshDocuments {
                    file_operations: file_operations.to_vec(),
                },
            )
            .await
            {
                Ok(result) if result["delivered"].as_bool() == Some(true) => {
                    refreshed.push(generation)
                }
                _ => failures.push(generation),
            }
        }
        refreshed.sort();
        failures.sort();
        (refreshed, failures)
    })
}

async fn probe_endpoint(endpoint: &OwnerEndpoint) -> Result<(), ContractFailure> {
    let status = tokio::time::timeout(
        Duration::from_millis(500),
        send_owner_request(endpoint, OwnerRequest::Status),
    )
    .await
    .map_err(|_| owner_unavailable(&endpoint.session_identity, "probe_timed_out"))??;
    if status["ownerGeneration"] == endpoint.owner_generation {
        Ok(())
    } else {
        Err(owner_unavailable(
            &endpoint.session_identity,
            "generation_mismatch",
        ))
    }
}

async fn send_owner_request(
    endpoint: &OwnerEndpoint,
    request: OwnerRequest,
) -> Result<Value, ContractFailure> {
    send_owner_request_with_queue_deadline(endpoint, request, None).await
}

async fn send_owner_request_with_queue_deadline(
    endpoint: &OwnerEndpoint,
    request: OwnerRequest,
    queue_deadline: Option<Duration>,
) -> Result<Value, ContractFailure> {
    let response =
        exchange_owner_request_with_queue_deadline(endpoint, request, queue_deadline).await?;
    if response.ok {
        Ok(response.result.unwrap_or(Value::Null))
    } else {
        Err(contract_failure_from_owner(
            response.error.unwrap_or_else(|| json!({})),
        ))
    }
}

async fn exchange_owner_request_with_queue_deadline(
    endpoint: &OwnerEndpoint,
    request: OwnerRequest,
    queue_deadline: Option<Duration>,
) -> Result<OwnerResponse, ContractFailure> {
    if endpoint.owner_protocol_version != OWNER_PROTOCOL_VERSION {
        return Err(ContractFailure {
            exit_code: 4,
            category: "unavailable",
            code: "owner_protocol_incompatible",
            message: "The Owner protocol version is incompatible with this client.".to_owned(),
            stage: "discover_owner",
            delivery: "not_sent",
            retry: "after_change",
            data: json!({
                "clientVersion": OWNER_PROTOCOL_VERSION,
                "ownerVersion": endpoint.owner_protocol_version,
                "ownerGeneration": endpoint.owner_generation
            }),
        });
    }
    let mut stream = TcpStream::connect(&endpoint.address)
        .await
        .map_err(|error| owner_transport_failure(endpoint, &error))?;
    let authenticated = AuthenticatedOwnerRequest {
        owner_protocol_version: OWNER_PROTOCOL_VERSION,
        session_identity: endpoint.session_identity.clone(),
        owner_generation: endpoint.owner_generation.clone(),
        token: endpoint.token.clone(),
        queue_deadline_ms: queue_deadline.map(|deadline| deadline.as_millis() as u64),
        request,
    };
    write_owner_message(&mut stream, &authenticated)
        .await
        .map_err(|error| owner_transport_failure(endpoint, &error))?;
    let response: OwnerResponse = read_owner_message(&mut stream)
        .await
        .map_err(|error| owner_transport_failure(endpoint, &error))?;
    if response.owner_protocol_version != OWNER_PROTOCOL_VERSION
        || response.owner_generation != endpoint.owner_generation
    {
        return Err(owner_unavailable(
            &endpoint.session_identity,
            "response_identity_mismatch",
        ));
    }
    Ok(response)
}

fn spawn_detached_owner(
    authorized: &AuthorizedServer,
    workspace: &Path,
    endpoint_path: &Path,
    owner_lock_path: &Path,
    launch_path: &Path,
) -> Result<(), ContractFailure> {
    let executable = std::env::current_exe().map_err(|error| {
        owner_unavailable(
            &authorized.session_identity,
            &format!("current_executable: {error}"),
        )
    })?;
    let mut command = Command::new(executable);
    command
        .arg("__owner")
        .arg(workspace)
        .arg(&authorized.executable)
        .arg(endpoint_path)
        .arg(owner_lock_path)
        .arg(launch_path)
        .arg("1")
        .current_dir(&authorized.cwd)
        .env_clear()
        .envs(authorized.child_environment.iter().cloned())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    detach_command(&mut command);
    #[cfg(windows)]
    disable_standard_stream_inheritance().map_err(|error| {
        owner_unavailable(
            &authorized.session_identity,
            &format!("standard_stream_detachment_failed: {error}"),
        )
    })?;
    command
        .spawn()
        .map(|_| ())
        .map_err(|error| ContractFailure {
            exit_code: 5,
            category: "query",
            code: "server_start_failed",
            message: "The background Owner process could not start.".to_owned(),
            stage: "start_server",
            delivery: "not_sent",
            retry: "after_change",
            data: json!({
                "server": authorized.server.name,
                "executable": authorized.executable,
                "osCode": error.raw_os_error()
            }),
        })
}

#[cfg(unix)]
fn detach_command(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
}

#[cfg(windows)]
fn detach_command(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS | CREATE_NO_WINDOW);
}

#[cfg(windows)]
fn disable_standard_stream_inheritance() -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{
        HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, SetHandleInformation,
    };

    for handle in [
        std::io::stdin().as_raw_handle(),
        std::io::stdout().as_raw_handle(),
        std::io::stderr().as_raw_handle(),
    ] {
        let handle = handle.cast::<std::ffi::c_void>() as HANDLE;
        if !handle.is_null()
            && handle != INVALID_HANDLE_VALUE
            && unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) } == 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn detach_command(_command: &mut Command) {}

struct OwnerStatePaths {
    endpoints: PathBuf,
    locks: PathBuf,
    launches: PathBuf,
}

impl OwnerStatePaths {
    fn new() -> Result<Self, ContractFailure> {
        let root = resolved_user_state_directory()
            .ok_or_else(|| {
                owner_unavailable(
                    "sid_0000000000000000000000000000000000000000000000000000000000000000",
                    "user_state_directory_unavailable",
                )
            })?
            .join("owners");
        let paths = Self {
            endpoints: root.join("endpoints"),
            locks: root.join("locks"),
            launches: root.join("launches"),
        };
        for path in [&paths.endpoints, &paths.locks, &paths.launches] {
            fs::create_dir_all(path).map_err(|error| {
                owner_unavailable(
                    "sid_0000000000000000000000000000000000000000000000000000000000000000",
                    &error.to_string(),
                )
            })?;
            crate::state_permissions::restrict_directory(path).map_err(|error| {
                owner_unavailable(
                    "sid_0000000000000000000000000000000000000000000000000000000000000000",
                    &error.to_string(),
                )
            })?;
        }
        Ok(paths)
    }

    fn endpoint_path(&self, identity: &str) -> PathBuf {
        self.endpoints.join(format!("{identity}.json"))
    }
    fn owner_lock_path(&self, identity: &str) -> PathBuf {
        self.locks.join(format!("{identity}.lock"))
    }
    fn startup_lock_path(&self, identity: &str) -> PathBuf {
        self.locks.join(format!("{identity}.startup.lock"))
    }
    fn launch_path(&self, generation: &str) -> PathBuf {
        self.launches.join(format!("{generation}.json"))
    }
}

fn acquire_startup_lock(path: &Path, timeout: Duration) -> Result<std::fs::File, ContractFailure> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
        .map_err(|error| {
            owner_unavailable(
                "sid_0000000000000000000000000000000000000000000000000000000000000000",
                &error.to_string(),
            )
        })?;
    crate::state_permissions::restrict_file(path).map_err(|error| {
        owner_unavailable(
            "sid_0000000000000000000000000000000000000000000000000000000000000000",
            &format!("startup_lock_permissions_failed: {error}"),
        )
    })?;
    let deadline = Instant::now() + timeout;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(10))
            }
            Err(error) => {
                return Err(owner_unavailable(
                    "sid_0000000000000000000000000000000000000000000000000000000000000000",
                    &error.to_string(),
                ));
            }
        }
    }
}

fn owner_lock_is_free(path: &Path) -> bool {
    let Ok(file) = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
    else {
        return false;
    };
    if crate::state_permissions::restrict_file(path).is_err() {
        return false;
    }
    file.try_lock().is_ok()
}

fn write_private_json(path: &Path, value: &impl serde::Serialize) -> io::Result<()> {
    let mut file = AtomicWriteFile::options().open(path)?;
    serde_json::to_writer(&mut file, value).map_err(io::Error::other)?;
    file.flush()?;
    file.commit()?;
    crate::state_permissions::restrict_file(path)
}

fn read_endpoint(path: &Path) -> Option<OwnerEndpoint> {
    let bytes = fs::read(path).ok()?;
    let endpoint: OwnerEndpoint = serde_json::from_slice(&bytes).ok()?;
    (endpoint.format_version == 1).then_some(endpoint)
}

fn endpoint_identity_from_path(path: &Path) -> Option<&str> {
    let identity = path.file_stem()?.to_str()?;
    (identity.len() == 68
        && identity.starts_with("sid_")
        && identity[4..].bytes().all(|byte| byte.is_ascii_hexdigit()))
    .then_some(identity)
}

fn random_hex(bytes: usize) -> Result<String, ContractFailure> {
    let mut random = vec![0; bytes];
    getrandom::fill(&mut random).map_err(|error| {
        owner_unavailable(
            "sid_0000000000000000000000000000000000000000000000000000000000000000",
            &format!("secure_random_failed: {error}"),
        )
    })?;
    Ok(hex::encode(random))
}

fn duration_millis(value: &str) -> u64 {
    parse_duration(value).as_millis() as u64
}

fn success_envelope(command: &[String], result: Value) -> Value {
    json!({"schemaVersion": 1, "ok": true, "command": command, "result": result})
}

fn contract_failure_from_owner(error: Value) -> ContractFailure {
    let code = error
        .get("code")
        .and_then(Value::as_str)
        .unwrap_or("owner_unavailable");
    ContractFailure {
        exit_code: match error.get("category").and_then(Value::as_str) {
            Some("blocked") => 3,
            Some("unavailable") => 4,
            Some("query") => 5,
            _ => 70,
        },
        category: leak_contract_string(
            error
                .get("category")
                .and_then(Value::as_str)
                .unwrap_or("internal"),
        ),
        code: leak_contract_string(code),
        message: error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("The Owner request failed.")
            .to_owned(),
        stage: leak_contract_string(
            error
                .get("stage")
                .and_then(Value::as_str)
                .unwrap_or("discover_owner"),
        ),
        delivery: leak_contract_string(
            error
                .get("delivery")
                .and_then(Value::as_str)
                .unwrap_or("not_sent"),
        ),
        retry: leak_contract_string(error.get("retry").and_then(Value::as_str).unwrap_or("safe")),
        data: error.get("data").cloned().unwrap_or_else(|| json!({})),
    }
}

fn dispatch_failure_from_owner(mut error: Value) -> DispatchFailure {
    let server_error = error
        .as_object_mut()
        .and_then(|error| error.remove("serverError"));
    let trace = error
        .as_object_mut()
        .and_then(|error| error.remove("trace"));
    let apply_edit_ledger = error
        .as_object_mut()
        .and_then(|error| error.remove("applyEditLedger"))
        .and_then(|ledger| ledger.as_array().cloned())
        .unwrap_or_default();
    let partial_results = error
        .as_object_mut()
        .and_then(|error| error.remove("partialResult"))
        .and_then(|partial| partial.get("items").and_then(Value::as_array).cloned())
        .unwrap_or_default();
    DispatchFailure {
        failure: contract_failure_from_owner(error),
        server_error,
        partial_results,
        trace,
        apply_edit_ledger,
    }
}

fn leak_contract_string(value: &str) -> &'static str {
    Box::leak(value.to_owned().into_boxed_str())
}

fn owner_unavailable(identity: &str, reason: &str) -> ContractFailure {
    ContractFailure {
        exit_code: 4,
        category: "unavailable",
        code: "owner_unavailable",
        message: "The background Owner is unavailable.".to_owned(),
        stage: "discover_owner",
        delivery: "not_sent",
        retry: "safe",
        data: json!({"sessionIdentity": identity, "reason": reason}),
    }
}

fn owner_transport_failure(endpoint: &OwnerEndpoint, error: &io::Error) -> ContractFailure {
    OwnerStatePaths::new()
        .ok()
        .and_then(|paths| read_endpoint(&paths.endpoint_path(&endpoint.session_identity)))
        .filter(|current| {
            current.session_identity == endpoint.session_identity
                && current.owner_generation == endpoint.owner_generation
                && current.token == endpoint.token
                && current.state == "failed"
        })
        .and_then(|current| current.failure)
        .map(contract_failure_from_owner)
        .unwrap_or_else(|| owner_unavailable(&endpoint.session_identity, &error.to_string()))
}

fn session_selection_failure(reason: &str, candidates: Vec<String>) -> ContractFailure {
    ContractFailure {
        exit_code: 4,
        category: "unavailable",
        code: "session_selection_failed",
        message: "The Session selector did not resolve one live Owner.".to_owned(),
        stage: "discover_owner",
        delivery: "not_applicable",
        retry: "after_change",
        data: json!({"reason": reason, "candidates": candidates}),
    }
}

fn run_async<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("Tokio runtime construction is infallible on supported platforms")
        .block_on(future)
}
