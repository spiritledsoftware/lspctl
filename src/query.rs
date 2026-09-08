//! Pure named-Query composition and Owner-dispatch normalization.
//!
//! The Owner owns transport, synchronization, and lifecycle; this module owns
//! the stable translation between CLI inputs and LSP request/result JSON.

#![allow(clippy::result_large_err)]

use std::{
    collections::BTreeMap,
    fs,
    io::{self, Read},
    path::PathBuf,
};

use serde_json::{Value, json};
use url::Url;

use crate::{
    cli::ParsedInvocation,
    contract::{ContractFailure, failure_envelope},
    workspace::{
        DiagnosticCache, DocumentSnapshot, PositionEncoding, ProtocolPosition,
        resolve_source_position,
    },
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QueryCommand {
    Definition,
    References,
    Hover,
    DocumentSymbols,
    WorkspaceSymbols,
    DocumentDiagnostics,
    WorkspaceDiagnostics,
    PublishedDiagnostics,
    PrepareRename,
    Rename,
    Format,
    CodeActions,
    ResolveCodeAction,
    ExecuteCommand,
    Raw,
    Capabilities,
}

impl QueryCommand {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Definition => "definition",
            Self::References => "references",
            Self::Hover => "hover",
            Self::DocumentSymbols => "document-symbols",
            Self::WorkspaceSymbols => "workspace-symbols",
            Self::DocumentDiagnostics => "document-diagnostics",
            Self::WorkspaceDiagnostics => "workspace-diagnostics",
            Self::PublishedDiagnostics => "published-diagnostics",
            Self::PrepareRename => "prepare-rename",
            Self::Rename => "rename",
            Self::Format => "format",
            Self::CodeActions => "code-actions",
            Self::ResolveCodeAction => "resolve-code-action",
            Self::ExecuteCommand => "execute-command",
            Self::Raw => "raw",
            Self::Capabilities => "capabilities",
        }
    }

    pub(crate) fn from_path(path: &[String]) -> Option<Self> {
        Some(match path {
            [name] if name == "definition" => Self::Definition,
            [name] if name == "references" => Self::References,
            [name] if name == "hover" => Self::Hover,
            [name] if name == "document-symbols" => Self::DocumentSymbols,
            [name] if name == "workspace-symbols" => Self::WorkspaceSymbols,
            [name] if name == "document-diagnostics" => Self::DocumentDiagnostics,
            [name] if name == "workspace-diagnostics" => Self::WorkspaceDiagnostics,
            [name] if name == "published-diagnostics" => Self::PublishedDiagnostics,
            [name] if name == "prepare-rename" => Self::PrepareRename,
            [name] if name == "rename" => Self::Rename,
            [name] if name == "format" => Self::Format,
            [name] if name == "code-actions" => Self::CodeActions,
            [name] if name == "resolve-code-action" => Self::ResolveCodeAction,
            [name] if name == "execute-command" => Self::ExecuteCommand,
            [name] if name == "raw" => Self::Raw,
            [name] if name == "capabilities" => Self::Capabilities,
            _ => return None,
        })
    }

    pub(crate) fn method(self) -> Option<&'static str> {
        Some(match self {
            Self::Definition => "textDocument/definition",
            Self::References => "textDocument/references",
            Self::Hover => "textDocument/hover",
            Self::DocumentSymbols => "textDocument/documentSymbol",
            Self::WorkspaceSymbols => "workspace/symbol",
            Self::DocumentDiagnostics => "textDocument/diagnostic",
            Self::WorkspaceDiagnostics => "workspace/diagnostic",
            Self::PublishedDiagnostics | Self::Capabilities => return None,
            Self::PrepareRename => "textDocument/prepareRename",
            Self::Rename => "textDocument/rename",
            Self::Format => "textDocument/formatting",
            Self::CodeActions => "textDocument/codeAction",
            Self::ResolveCodeAction => "codeAction/resolve",
            Self::ExecuteCommand => "workspace/executeCommand",
            Self::Raw => return None,
        })
    }

    fn provider(self) -> Option<ProviderRequirement> {
        Some(match self {
            Self::Definition => ProviderRequirement::new("definition", "definitionProvider"),
            Self::References => ProviderRequirement::new("references", "referencesProvider"),
            Self::Hover => ProviderRequirement::new("hover", "hoverProvider"),
            Self::DocumentSymbols => {
                ProviderRequirement::new("documentSymbols", "documentSymbolProvider")
            }
            Self::WorkspaceSymbols => {
                ProviderRequirement::new("workspaceSymbols", "workspaceSymbolProvider")
            }
            Self::DocumentDiagnostics | Self::WorkspaceDiagnostics => {
                ProviderRequirement::new("diagnostics", "diagnosticProvider")
            }
            Self::PrepareRename | Self::Rename => {
                ProviderRequirement::new("rename", "renameProvider")
            }
            Self::Format => ProviderRequirement::new("formatting", "documentFormattingProvider"),
            Self::CodeActions | Self::ResolveCodeAction => {
                ProviderRequirement::new("codeActions", "codeActionProvider")
            }
            Self::ExecuteCommand => {
                ProviderRequirement::new("executeCommand", "executeCommandProvider")
            }
            Self::PublishedDiagnostics | Self::Raw | Self::Capabilities => return None,
        })
    }

    fn supports_partial_results(self) -> bool {
        matches!(
            self,
            Self::References
                | Self::WorkspaceSymbols
                | Self::DocumentDiagnostics
                | Self::WorkspaceDiagnostics
                | Self::CodeActions
        )
    }

    fn paged(self) -> bool {
        matches!(
            self,
            Self::References
                | Self::WorkspaceSymbols
                | Self::DocumentDiagnostics
                | Self::WorkspaceDiagnostics
                | Self::PublishedDiagnostics
                | Self::CodeActions
        )
    }
}

#[derive(Debug, Clone, Copy)]
struct ProviderRequirement {
    key: &'static str,
    capability: &'static str,
}

impl ProviderRequirement {
    const fn new(key: &'static str, capability: &'static str) -> Self {
        Self { key, capability }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Provider {
    pub(crate) state: ProviderState,
    pub(crate) capability_path: String,
    pub(crate) options: Option<Value>,
    pub(crate) selector: Option<Value>,
    pub(crate) problems: Vec<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProviderState {
    Supported,
    Unsupported,
    Invalid,
}

impl ProviderState {
    fn name(self) -> &'static str {
        match self {
            Self::Supported => "supported",
            Self::Unsupported => "unsupported",
            Self::Invalid => "invalid",
        }
    }
}

#[derive(Debug, Default, Clone)]
pub(crate) struct Capabilities {
    pub(crate) providers: BTreeMap<String, Provider>,
    pub(crate) initialize_result: Option<Value>,
}

impl Capabilities {
    /// Converts the Owner's normalized Capability response into Query gates.
    pub(crate) fn from_owner_result(owner_result: &Value) -> Self {
        let providers = owner_result
            .get("providers")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
            .map(|(name, value)| {
                let state = match value.get("state").and_then(Value::as_str) {
                    Some("supported") => ProviderState::Supported,
                    Some("unsupported") => ProviderState::Unsupported,
                    _ => ProviderState::Invalid,
                };
                let capability_path = value
                    .get("capabilityPath")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("capabilities.{name}"));
                let options = value.get("options").cloned();
                let selector = value
                    .get("selector")
                    .cloned()
                    .or_else(|| options.as_ref()?.get("documentSelector").cloned());
                let mut problems = value
                    .get("problems")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                if !matches!(
                    value.get("state").and_then(Value::as_str),
                    Some("supported" | "unsupported" | "invalid")
                ) {
                    problems.push(json!({
                        "code": "malformed_capability",
                        "message": "The Owner returned an invalid normalized provider state.",
                        "jsonPath": format!("providers.{name}.state")
                    }));
                }
                (
                    name.clone(),
                    Provider {
                        state,
                        capability_path,
                        options,
                        selector,
                        problems,
                    },
                )
            })
            .collect();
        Self {
            providers,
            initialize_result: owner_result.get("initializeResult").cloned(),
        }
    }

    pub(crate) fn require(
        &self,
        command: QueryCommand,
        document: Option<&DocumentSnapshot>,
        invocation: &ParsedInvocation,
    ) -> Result<(), ContractFailure> {
        let Some(requirement) = command.provider() else {
            return Ok(());
        };
        let provider = self
            .providers
            .get(requirement.key)
            .or_else(|| self.providers.get(requirement.capability));
        let (state, capability_path, problems) = provider.map_or_else(
            || {
                (
                    "unsupported",
                    format!("capabilities.{}", requirement.capability),
                    Vec::new(),
                )
            },
            |provider| {
                (
                    provider.state.name(),
                    provider.capability_path.clone(),
                    provider.problems.clone(),
                )
            },
        );
        let unavailable =
            |selector: Option<Value>, state: &str, problems: Vec<Value>| ContractFailure {
                exit_code: 3,
                category: "blocked",
                code: "capability_unavailable",
                message: "The selected server does not support this Query.".to_owned(),
                stage: "dispatch",
                delivery: "not_sent",
                retry: "after_change",
                data: compact_object(json!({
                    "provider": requirement.key,
                    "state": state,
                    "capabilityPath": &capability_path,
                    "problems": problems,
                    "selector": selector,
                })),
            };
        let Some(provider) = provider else {
            return Err(unavailable(None, state, problems));
        };
        if provider.state != ProviderState::Supported {
            return Err(unavailable(provider.selector.clone(), state, problems));
        }
        let options = provider.options.as_ref().and_then(Value::as_object);
        let required_option = match command {
            QueryCommand::PrepareRename => Some(("prepareProvider", true)),
            QueryCommand::ResolveCodeAction => Some(("resolveProvider", true)),
            QueryCommand::WorkspaceDiagnostics => Some(("workspaceDiagnostics", true)),
            _ => None,
        };
        if let Some((name, expected)) = required_option
            && options
                .and_then(|options| options.get(name))
                .and_then(Value::as_bool)
                != Some(expected)
        {
            return Err(unavailable(
                provider.selector.clone(),
                "unsupported",
                Vec::new(),
            ));
        }
        if command == QueryCommand::ExecuteCommand {
            let wanted = invocation.option_string("--command").unwrap_or_default();
            let advertised = options
                .and_then(|options| options.get("commands"))
                .and_then(Value::as_array)
                .is_some_and(|commands| commands.iter().any(|command| command == &wanted));
            if !advertised {
                return Err(unavailable(
                    provider.selector.clone(),
                    "unsupported",
                    Vec::new(),
                ));
            }
        }
        let selector = provider
            .selector
            .as_ref()
            .or_else(|| options.and_then(|options| options.get("documentSelector")));
        if let (Some(selector), Some(document)) = (selector, document)
            && !selector_matches(selector, document)
        {
            return Err(unavailable(
                Some(selector.clone()),
                "unsupported",
                Vec::new(),
            ));
        }
        Ok(())
    }

    fn work_done_progress(&self, command: QueryCommand) -> bool {
        command
            .provider()
            .and_then(|requirement| {
                self.providers
                    .get(requirement.key)
                    .or_else(|| self.providers.get(requirement.capability))
            })
            .and_then(|provider| provider.options.as_ref())
            .and_then(|options| options.get("workDoneProgress"))
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DispatchRequest {
    pub(crate) method: String,
    /// `None` means omit JSON-RPC `params`; `Some(Value::Null)` is explicit null.
    pub(crate) params: Option<Value>,
    /// Native paths that the Owner must explicitly synchronize before raw dispatch.
    pub(crate) synchronized_files: Vec<PathBuf>,
    /// Whether this named Workspace operation refreshes every Owner-open Document.
    pub(crate) refresh_open_documents: bool,
    /// Raw requests retain post-response staleness as metadata instead of failing.
    pub(crate) raw_request: bool,
    /// The Owner assigns a unique token and injects `partialResultToken` when true.
    pub(crate) partial_results: bool,
    /// The Owner assigns a unique token and injects `workDoneToken` when true.
    pub(crate) work_done_progress: bool,
    /// Whether the Owner should include exact protocol frames in its response.
    pub(crate) trace_protocol: bool,
    /// Whether execute-command callbacks may apply each proposed Workspace Edit.
    pub(crate) apply_edits: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct DispatchResponse {
    pub(crate) result: Value,
    pub(crate) partial_results: Vec<Value>,
    pub(crate) trace: Option<Value>,
    pub(crate) apply_edit_ledger: Vec<Value>,
    pub(crate) server_progress: Vec<Value>,
    pub(crate) synchronization: Option<Value>,
}

/// A failed Owner dispatch, including observations made before it failed.
#[derive(Debug)]
pub(crate) struct DispatchFailure {
    pub(crate) failure: ContractFailure,
    /// The exact LSP ResponseError for server-derived failures.
    pub(crate) server_error: Option<Value>,
    /// Exact `$/progress` values collected for the request's partial-result token.
    pub(crate) partial_results: Vec<Value>,
    pub(crate) trace: Option<Value>,
    pub(crate) apply_edit_ledger: Vec<Value>,
}

impl From<ContractFailure> for DispatchFailure {
    fn from(failure: ContractFailure) -> Self {
        Self {
            failure,
            server_error: None,
            partial_results: Vec::new(),
            trace: None,
            apply_edit_ledger: Vec::new(),
        }
    }
}

/// A Query failure plus the optional top-level evidence in its frozen envelope.
#[derive(Debug)]
pub(crate) struct QueryExecutionFailure {
    pub(crate) failure: ContractFailure,
    pub(crate) server_error: Option<Value>,
    pub(crate) method: Option<String>,
    pub(crate) context: Option<Value>,
    pub(crate) partial_result: Option<Value>,
    pub(crate) trace: Option<Value>,
    pub(crate) apply_edit_ledger: Vec<Value>,
}

impl QueryExecutionFailure {
    fn before_dispatch(failure: ContractFailure) -> Self {
        Self {
            failure,
            server_error: None,
            method: None,
            context: None,
            partial_result: None,
            trace: None,
            apply_edit_ledger: Vec::new(),
        }
    }

    fn after_dispatch(
        failure: ContractFailure,
        method: &str,
        context: &Value,
        trace: Option<Value>,
        apply_edit_ledger: Vec<Value>,
    ) -> Self {
        Self {
            failure,
            server_error: None,
            method: Some(method.to_owned()),
            context: Some(context.clone()),
            partial_result: None,
            trace,
            apply_edit_ledger,
        }
    }
}

impl From<ContractFailure> for QueryExecutionFailure {
    fn from(failure: ContractFailure) -> Self {
        Self::before_dispatch(failure)
    }
}

/// Renders the complete failure envelope without dropping partial results or protocol evidence.
pub(crate) fn query_failure_envelope(
    command: Vec<String>,
    failure: &QueryExecutionFailure,
) -> Value {
    let mut envelope = failure_envelope(command, &failure.failure);
    if let Some(server_error) = &failure.server_error {
        envelope["error"]["serverError"] = server_error.clone();
    }
    if let Some(method) = &failure.method {
        envelope["method"] = Value::String(method.clone());
    }
    if let Some(context) = &failure.context {
        envelope["context"] = context.clone();
    }
    if let Some(partial_result) = &failure.partial_result {
        envelope["partialResult"] = partial_result.clone();
    }
    if let Some(trace) = &failure.trace {
        envelope["trace"] = trace.clone();
    }
    if !failure.apply_edit_ledger.is_empty() {
        envelope["applyEditLedger"] = Value::Array(failure.apply_edit_ledger.clone());
    }
    envelope
}

#[derive(Debug, Clone)]
pub(crate) struct QueryContext {
    pub(crate) workspace_uri: String,
    pub(crate) server: String,
    pub(crate) session_identity: String,
    pub(crate) owner_generation: String,
    pub(crate) result_position_encoding: PositionEncoding,
    pub(crate) server_progress: Vec<Value>,
    pub(crate) synchronization: Value,
    pub(crate) recovery: Value,
}

impl QueryContext {
    fn json(&self, has_input_position: bool) -> Value {
        let mut context = json!({
            "workspaceUri": self.workspace_uri,
            "server": self.server,
            "sessionIdentity": self.session_identity,
            "ownerGeneration": self.owner_generation,
            "resultPositionEncoding": self.result_position_encoding.name(),
            "synchronization": self.synchronization,
            "recovery": self.recovery,
        });
        if has_input_position {
            context["inputPositionEncoding"] = Value::String("unicode-scalar".to_owned());
        }
        if !self.server_progress.is_empty() {
            context["serverProgress"] = Value::Array(self.server_progress.clone());
        }
        context
    }
}

/// The only seam required by the Query layer. The Owner implements this once.
pub(crate) trait SessionDispatcher {
    fn dispatch(&mut self, request: DispatchRequest) -> Result<DispatchResponse, DispatchFailure>;
}

#[derive(Debug, Clone)]
pub(crate) struct ComposedQuery {
    pub(crate) command: QueryCommand,
    pub(crate) request: Option<DispatchRequest>,
    pub(crate) page: Option<Page>,
    pub(crate) document_uri: Option<String>,
    pub(crate) document_version: Option<i64>,
    capabilities: Option<Capabilities>,
    capabilities_raw: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Page {
    pub(crate) offset: usize,
    pub(crate) limit: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct PreviewProposal {
    pub(crate) command: QueryCommand,
    pub(crate) method: String,
    pub(crate) context: Value,
    pub(crate) edit: Value,
    pub(crate) command_payload: Option<Value>,
    pub(crate) resolved_action: Option<Value>,
    pub(crate) trace: Option<Value>,
    pub(crate) apply_edit_ledger: Vec<Value>,
}

/// Mutation owns validation, persistence, and the complete Preview shape.
pub(crate) trait PreviewCreator {
    fn create_preview(&mut self, proposal: PreviewProposal) -> Result<Value, ContractFailure>;
}

/// Composes all named Query/raw request params from validated CLI options.
/// Documents have already been refreshed by the Owner and retain its negotiated encoding.
pub(crate) fn compose(
    invocation: &ParsedInvocation,
    document: Option<&DocumentSnapshot>,
    encoding: PositionEncoding,
    capabilities: &Capabilities,
    diagnostics: Option<&DiagnosticCache>,
) -> Result<ComposedQuery, ContractFailure> {
    let command = QueryCommand::from_path(invocation.command_path()).ok_or_else(unknown_query)?;
    let page = command.paged().then(|| page(invocation)).transpose()?;
    let document_uri = document.map(|document| document.uri.clone());
    if command != QueryCommand::Raw {
        capabilities.require(command, document, invocation)?;
    }
    let mut request = match command {
        QueryCommand::PublishedDiagnostics | QueryCommand::Capabilities => None,
        QueryCommand::Raw => Some(raw_request(invocation)?),
        QueryCommand::WorkspaceSymbols => Some(request(
            command.method().unwrap(),
            json!({"query": required_string(invocation, "--query")?}),
        )),
        QueryCommand::WorkspaceDiagnostics => Some(request(
            command.method().unwrap(),
            json!({"previousResultIds": diagnostics.map_or_else(Vec::new, DiagnosticCache::pull_result_ids)}),
        )),
        QueryCommand::DocumentSymbols => Some(document_request(
            command.method().unwrap(),
            required_document(document)?,
            None,
        )),
        QueryCommand::DocumentDiagnostics => {
            let document = required_document(document)?;
            let mut extra = json!({});
            if let Some(result_id) =
                diagnostics.and_then(|cache| cache.pull_result_id(&document.uri))
            {
                extra["previousResultId"] = Value::String(result_id.to_owned());
            }
            Some(document_request(
                command.method().unwrap(),
                document,
                Some(extra),
            ))
        }
        QueryCommand::Definition | QueryCommand::Hover | QueryCommand::PrepareRename => {
            let document = required_document(document)?;
            Some(document_request(
                command.method().unwrap(),
                document,
                Some(position_params(invocation, document, encoding)?),
            ))
        }
        QueryCommand::References => {
            let document = required_document(document)?;
            let mut params = position_params(invocation, document, encoding)?;
            params["context"] =
                json!({"includeDeclaration": required_bool(invocation, "--include-declaration")?});
            Some(document_request(
                command.method().unwrap(),
                document,
                Some(params),
            ))
        }
        QueryCommand::Rename => {
            let document = required_document(document)?;
            let mut params = position_params(invocation, document, encoding)?;
            params["newName"] = json!(required_string(invocation, "--new-name")?);
            Some(document_request(
                command.method().unwrap(),
                document,
                Some(params),
            ))
        }
        QueryCommand::Format => Some(document_request(
            command.method().unwrap(),
            required_document(document)?,
            Some(
                json!({"options": object_json_input(invocation, "--options-json", "--options-file", "FormattingOptions")?}),
            ),
        )),
        QueryCommand::CodeActions => {
            let document = required_document(document)?;
            let range = range_params(invocation, document, encoding)?;
            Some(document_request(
                command.method().unwrap(),
                document,
                Some(
                    json!({"range": range, "context": object_json_input(invocation, "--context-json", "--context-file", "CodeActionContext")?}),
                ),
            ))
        }
        QueryCommand::ResolveCodeAction => Some(request(
            command.method().unwrap(),
            object_json_input(invocation, "--action-json", "--action-file", "CodeAction")?,
        )),
        QueryCommand::ExecuteCommand => Some(request(
            command.method().unwrap(),
            json!({
                "command": required_string(invocation, "--command")?,
                "arguments": optional_array_json_input(invocation, "--arguments-json", "--arguments-file", "execute-command arguments")?.unwrap_or_default()
            }),
        )),
    };
    if let Some(request) = &mut request
        && command != QueryCommand::Raw
    {
        request.partial_results = command.supports_partial_results();
        request.work_done_progress = capabilities.work_done_progress(command);
    }
    if let Some(request) = &mut request {
        request.refresh_open_documents = matches!(
            command,
            QueryCommand::WorkspaceSymbols
                | QueryCommand::WorkspaceDiagnostics
                | QueryCommand::ExecuteCommand
        );
        request.raw_request = command == QueryCommand::Raw;
        request.trace_protocol = invocation.has_option("--trace-protocol");
        request.apply_edits =
            command == QueryCommand::ExecuteCommand && invocation.has_option("--apply-edits");
    }
    Ok(ComposedQuery {
        command,
        request,
        page,
        document_uri,
        document_version: document.map(|document| document.version),
        capabilities: (command == QueryCommand::Capabilities).then(|| capabilities.clone()),
        capabilities_raw: command == QueryCommand::Capabilities && invocation.has_option("--raw"),
    })
}

/// Dispatches one composed LSP Query and produces a contract-shaped envelope.
pub(crate) fn execute(
    dispatcher: &mut impl SessionDispatcher,
    composed: ComposedQuery,
    mut context: QueryContext,
    diagnostics: &mut DiagnosticCache,
    previews: &mut impl PreviewCreator,
) -> Result<Value, QueryExecutionFailure> {
    match composed.command {
        QueryCommand::Capabilities => {
            return Ok(capabilities_envelope(
                context.json(has_source_position(composed.command)),
                composed.capabilities.as_ref().unwrap(),
                composed.capabilities_raw,
            ));
        }
        QueryCommand::PublishedDiagnostics => {
            let has_position = has_source_position(composed.command);
            return published_envelope(composed, context.json(has_position), diagnostics)
                .map_err(QueryExecutionFailure::before_dispatch);
        }
        _ => {}
    }

    let method = composed.request.as_ref().unwrap().method.clone();
    let response = dispatcher
        .dispatch(composed.request.clone().unwrap())
        .map_err(|failure| {
            let context = context.json(has_source_position(composed.command));
            query_dispatch_failure(&composed, &context, failure)
        })?;
    if let Some(synchronization) = response.synchronization.clone() {
        context.synchronization = synchronization;
    }
    context.server_progress = response.server_progress.clone();
    let context = context.json(has_source_position(composed.command));
    if composed.command == QueryCommand::Raw {
        return query_envelope(
            &composed,
            context,
            response.result,
            response.trace,
            response.apply_edit_ledger,
        )
        .map_err(QueryExecutionFailure::before_dispatch);
    }
    let trace = response.trace;
    let apply_edit_ledger = response.apply_edit_ledger;
    let enrich_failure = |failure| {
        QueryExecutionFailure::after_dispatch(
            failure,
            &method,
            &context,
            trace.clone(),
            apply_edit_ledger.clone(),
        )
    };
    let result = merge_partial_results(composed.command, response.result, response.partial_results)
        .map_err(enrich_failure)?;
    let result = normalize_named_result(composed.command, result).map_err(enrich_failure)?;

    let output = match composed.command {
        QueryCommand::DocumentDiagnostics => document_diagnostic_envelope(
            &composed,
            context.clone(),
            result,
            trace.clone(),
            apply_edit_ledger.clone(),
            diagnostics,
        ),
        QueryCommand::WorkspaceDiagnostics => workspace_diagnostic_envelope(
            &composed,
            context.clone(),
            result,
            trace.clone(),
            apply_edit_ledger.clone(),
            diagnostics,
        ),
        QueryCommand::Rename | QueryCommand::Format | QueryCommand::ResolveCodeAction => {
            mutation_query_envelope(
                &composed,
                context.clone(),
                result,
                trace.clone(),
                apply_edit_ledger.clone(),
                previews,
            )
        }
        _ => query_envelope(
            &composed,
            context.clone(),
            result,
            trace.clone(),
            apply_edit_ledger.clone(),
        ),
    };
    output.map_err(enrich_failure)
}

fn query_dispatch_failure(
    composed: &ComposedQuery,
    context: &Value,
    failure: DispatchFailure,
) -> QueryExecutionFailure {
    let partial_items = failure
        .partial_results
        .into_iter()
        .flat_map(|partial| match partial {
            Value::Array(items) => items,
            partial => vec![partial],
        })
        .collect::<Vec<_>>();
    QueryExecutionFailure {
        failure: failure.failure,
        server_error: failure.server_error,
        method: composed
            .request
            .as_ref()
            .map(|request| request.method.clone()),
        context: Some(context.clone()),
        partial_result: (composed.command.supports_partial_results() && !partial_items.is_empty())
            .then(|| json!({"items": partial_items, "complete": false})),
        trace: failure.trace,
        apply_edit_ledger: failure.apply_edit_ledger,
    }
}

fn request(method: &str, params: Value) -> DispatchRequest {
    DispatchRequest {
        method: method.to_owned(),
        params: Some(params),
        synchronized_files: Vec::new(),
        refresh_open_documents: false,
        raw_request: false,
        partial_results: false,
        work_done_progress: false,
        trace_protocol: false,
        apply_edits: false,
    }
}

fn document_request(
    method: &str,
    document: &DocumentSnapshot,
    extra: Option<Value>,
) -> DispatchRequest {
    let mut params = extra.unwrap_or_else(|| json!({}));
    params["textDocument"] = json!({"uri": document.uri});
    let mut request = request(method, params);
    request.synchronized_files.push(document.path.clone());
    request
}

fn raw_request(invocation: &ParsedInvocation) -> Result<DispatchRequest, ContractFailure> {
    let method = required_string(invocation, "--method")?;
    if forbidden_raw_method(&method) {
        return Err(ContractFailure {
            exit_code: 3,
            category: "blocked",
            code: "raw_method_forbidden",
            message: "This raw method is controlled by lspctl.".to_owned(),
            stage: "dispatch",
            delivery: "not_sent",
            retry: "never",
            data: json!({"method": method}),
        });
    }
    Ok(DispatchRequest {
        method,
        params: optional_json_input(invocation, "--params-json", "--params-file")?,
        synchronized_files: invocation.option_paths("--sync-file"),
        refresh_open_documents: false,
        raw_request: true,
        partial_results: false,
        work_done_progress: false,
        trace_protocol: false,
        apply_edits: false,
    })
}

fn forbidden_raw_method(method: &str) -> bool {
    matches!(
        method,
        "initialize" | "initialized" | "shutdown" | "exit" | "$/cancelRequest"
    )
}

fn compact_object(mut value: Value) -> Value {
    if let Some(object) = value.as_object_mut() {
        object.retain(|_, value| {
            !value.is_null() && !matches!(value, Value::Array(items) if items.is_empty())
        });
    }
    value
}

fn selector_matches(selector: &Value, document: &DocumentSnapshot) -> bool {
    let Some(filters) = selector.as_array() else {
        return false;
    };
    filters
        .iter()
        .any(|filter| document_filter_matches(filter, document))
}

fn document_filter_matches(filter: &Value, document: &DocumentSnapshot) -> bool {
    let Some(filter) = filter.as_object() else {
        return false;
    };
    if filter
        .get("language")
        .and_then(Value::as_str)
        .is_some_and(|language| language != document.language_id)
    {
        return false;
    }
    if filter
        .get("scheme")
        .and_then(Value::as_str)
        .is_some_and(|scheme| scheme != "file")
    {
        return false;
    }
    let Some(pattern) = filter.get("pattern") else {
        return true;
    };
    document_pattern_matches(pattern, document)
}

// One memoization cell per pattern/path byte pair and at most 256 brace
// expansions bounds matching of server-controlled selector patterns.
pub(crate) fn protocol_glob_matches(pattern: &str, path: &str, ignore_case: bool) -> bool {
    if ignore_case {
        glob_matches(
            pattern.to_ascii_lowercase().as_bytes(),
            path.to_ascii_lowercase().as_bytes(),
        )
    } else {
        glob_matches(pattern.as_bytes(), path.as_bytes())
    }
}

fn glob_matches(pattern: &[u8], path: &[u8]) -> bool {
    let mut expanded = Vec::new();
    expand_braces(pattern, &mut expanded, 256);
    expanded.into_iter().any(|pattern| {
        let mut states = vec![vec![None; path.len() + 1]; pattern.len() + 1];
        glob_matches_at(&pattern, path, 0, 0, &mut states)
    })
}

fn document_pattern_matches(pattern: &Value, document: &DocumentSnapshot) -> bool {
    let uri_path = Url::parse(&document.uri)
        .ok()
        .map(|uri| uri.path().to_owned())
        .unwrap_or_else(|| document.path.to_string_lossy().replace('\\', "/"));
    if let Some(pattern) = pattern.as_str() {
        return glob_matches(pattern.as_bytes(), uri_path.as_bytes());
    }
    let Some(relative) = pattern.as_object() else {
        return false;
    };
    let Some(pattern) = relative.get("pattern").and_then(Value::as_str) else {
        return false;
    };
    let base_uri = relative.get("baseUri").and_then(|base| {
        base.as_str()
            .or_else(|| base.get("uri").and_then(Value::as_str))
    });
    let Some(base_path) = base_uri
        .and_then(|uri| Url::parse(uri).ok())
        .map(|uri| uri.path().trim_end_matches('/').to_owned())
    else {
        return false;
    };
    let Some(relative_path) = uri_path
        .strip_prefix(&base_path)
        .filter(|suffix| suffix.is_empty() || suffix.starts_with('/'))
    else {
        return false;
    };
    glob_matches(
        pattern.as_bytes(),
        relative_path.trim_start_matches('/').as_bytes(),
    )
}

fn expand_braces(pattern: &[u8], output: &mut Vec<Vec<u8>>, limit: usize) {
    if output.len() >= limit {
        return;
    }
    let mut open = None;
    let mut depth = 0_u32;
    for (index, byte) in pattern.iter().copied().enumerate() {
        match byte {
            b'{' if depth == 0 => {
                open = Some(index);
                depth = 1;
            }
            b'{' if depth > 0 => depth += 1,
            b'}' if depth > 0 => {
                depth -= 1;
                if depth == 0 {
                    let open = open.unwrap();
                    let alternatives = split_brace_alternatives(&pattern[open + 1..index]);
                    if alternatives.len() <= 1 {
                        break;
                    }
                    for alternative in alternatives {
                        if output.len() >= limit {
                            break;
                        }
                        let mut expanded = pattern[..open].to_vec();
                        expanded.extend(alternative);
                        expanded.extend(&pattern[index + 1..]);
                        expand_braces(&expanded, output, limit);
                    }
                    return;
                }
            }
            _ => {}
        }
    }
    output.push(pattern.to_vec());
}

fn split_brace_alternatives(value: &[u8]) -> Vec<&[u8]> {
    let mut alternatives = Vec::new();
    let mut start = 0;
    let mut depth = 0_u32;
    for (index, byte) in value.iter().copied().enumerate() {
        match byte {
            b'{' => depth += 1,
            b'}' => depth = depth.saturating_sub(1),
            b',' if depth == 0 => {
                alternatives.push(&value[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    alternatives.push(&value[start..]);
    alternatives
}

fn glob_matches_at(
    pattern: &[u8],
    path: &[u8],
    pattern_index: usize,
    path_index: usize,
    states: &mut [Vec<Option<bool>>],
) -> bool {
    if let Some(result) = states[pattern_index][path_index] {
        return result;
    }
    let result = if pattern_index == pattern.len() {
        path_index == path.len()
    } else if pattern[pattern_index] == b'*' {
        let recursive = pattern.get(pattern_index + 1) == Some(&b'*');
        let mut next_pattern = pattern_index + if recursive { 2 } else { 1 };
        let recursive_directory = recursive && pattern.get(next_pattern) == Some(&b'/');
        if recursive_directory {
            next_pattern += 1;
        }
        ((!recursive_directory || path_index == 0 || path[path_index - 1] == b'/')
            && glob_matches_at(pattern, path, next_pattern, path_index, states))
            || (path_index < path.len()
                && (recursive || path[path_index] != b'/')
                && glob_matches_at(pattern, path, pattern_index, path_index + 1, states))
    } else if pattern[pattern_index] == b'?' {
        path_index < path.len()
            && path[path_index] != b'/'
            && glob_matches_at(pattern, path, pattern_index + 1, path_index + 1, states)
    } else if pattern[pattern_index] == b'[' {
        character_class(pattern, pattern_index, path.get(path_index).copied()).is_some_and(
            |(matches, next_pattern)| {
                matches && glob_matches_at(pattern, path, next_pattern, path_index + 1, states)
            },
        )
    } else {
        path.get(path_index) == pattern.get(pattern_index)
            && glob_matches_at(pattern, path, pattern_index + 1, path_index + 1, states)
    };
    states[pattern_index][path_index] = Some(result);
    result
}

fn character_class(pattern: &[u8], start: usize, character: Option<u8>) -> Option<(bool, usize)> {
    let character = character.filter(|character| *character != b'/')?;
    let end = pattern[start + 1..].iter().position(|byte| *byte == b']')? + start + 1;
    let class = &pattern[start + 1..end];
    let (negated, class) = class
        .strip_prefix(b"!")
        .map_or((false, class), |class| (true, class));
    let mut matched = false;
    let mut index = 0;
    while index < class.len() {
        if index + 2 < class.len() && class[index + 1] == b'-' {
            matched |= (class[index]..=class[index + 2]).contains(&character);
            index += 3;
        } else {
            matched |= class[index] == character;
            index += 1;
        }
    }
    Some((matched != negated, end + 1))
}

fn position_params(
    invocation: &ParsedInvocation,
    document: &DocumentSnapshot,
    encoding: PositionEncoding,
) -> Result<Value, ContractFailure> {
    let position = source_position(invocation, document, encoding, "--line", "--column")?;
    Ok(json!({"position": position}))
}

fn range_params(
    invocation: &ParsedInvocation,
    document: &DocumentSnapshot,
    encoding: PositionEncoding,
) -> Result<Value, ContractFailure> {
    let start = source_position(
        invocation,
        document,
        encoding,
        "--start-line",
        "--start-column",
    )?;
    let end = source_position(invocation, document, encoding, "--end-line", "--end-column")?;
    if (end.line, end.character) < (start.line, start.character) {
        return Err(input_failure(
            "The code-action range ends before it starts.",
        ));
    }
    Ok(json!({"start": start, "end": end}))
}

fn source_position(
    invocation: &ParsedInvocation,
    document: &DocumentSnapshot,
    encoding: PositionEncoding,
    line: &str,
    column: &str,
) -> Result<ProtocolPosition, ContractFailure> {
    let line = u32::try_from(required_usize(invocation, line)?)
        .map_err(|_| input_failure("A source line is too large."))?;
    let column = u32::try_from(required_usize(invocation, column)?)
        .map_err(|_| input_failure("A source column is too large."))?;
    Ok(resolve_source_position(&document.path, &document.text, line, column, encoding)?.protocol)
}

fn page(invocation: &ParsedInvocation) -> Result<Page, ContractFailure> {
    let page = Page {
        offset: optional_usize(invocation, "--offset")?.unwrap_or(0),
        limit: optional_usize(invocation, "--limit")?.unwrap_or(100),
    };
    if !(1..=1000).contains(&page.limit) {
        return Err(input_failure("The page limit must be between 1 and 1000."));
    }
    Ok(page)
}

fn optional_usize(
    invocation: &ParsedInvocation,
    flag: &str,
) -> Result<Option<usize>, ContractFailure> {
    invocation
        .option_string(flag)
        .map(|value| {
            value
                .parse()
                .map_err(|_| input_failure("A numeric option is invalid."))
        })
        .transpose()
}

fn required_usize(invocation: &ParsedInvocation, flag: &str) -> Result<usize, ContractFailure> {
    optional_usize(invocation, flag)?.ok_or_else(|| input_failure("A required position is absent."))
}

fn required_string(invocation: &ParsedInvocation, flag: &str) -> Result<String, ContractFailure> {
    invocation
        .option_string(flag)
        .ok_or_else(|| input_failure("A required option is absent."))
}

fn required_bool(invocation: &ParsedInvocation, flag: &str) -> Result<bool, ContractFailure> {
    required_string(invocation, flag)?
        .parse()
        .map_err(|_| input_failure("A Boolean option is invalid."))
}

fn required_document(
    document: Option<&DocumentSnapshot>,
) -> Result<&DocumentSnapshot, ContractFailure> {
    document.ok_or_else(|| input_failure("A synchronized Document is required."))
}

fn json_input(
    invocation: &ParsedInvocation,
    json_flag: &str,
    file_flag: &str,
) -> Result<Value, ContractFailure> {
    optional_json_input(invocation, json_flag, file_flag)?
        .ok_or_else(|| input_failure("A JSON input source is required."))
}

fn object_json_input(
    invocation: &ParsedInvocation,
    json_flag: &str,
    file_flag: &str,
    expected: &str,
) -> Result<Value, ContractFailure> {
    let value = json_input(invocation, json_flag, file_flag)?;
    if value.is_object() {
        Ok(value)
    } else {
        Err(input_failure(&format!(
            "The JSON input must be a {expected} object."
        )))
    }
}

fn optional_array_json_input(
    invocation: &ParsedInvocation,
    json_flag: &str,
    file_flag: &str,
    expected: &str,
) -> Result<Option<Vec<Value>>, ContractFailure> {
    optional_json_input(invocation, json_flag, file_flag)?
        .map(|value| {
            value.as_array().cloned().ok_or_else(|| {
                input_failure(&format!("The JSON input must be an array of {expected}."))
            })
        })
        .transpose()
}

fn optional_json_input(
    invocation: &ParsedInvocation,
    json_flag: &str,
    file_flag: &str,
) -> Result<Option<Value>, ContractFailure> {
    if let Some(value) = invocation.option_string(json_flag) {
        return serde_json::from_str(&value)
            .map(Some)
            .map_err(|error| invalid_json_input(json_flag, None, &error));
    }
    let Some(path) = invocation.option_path(file_flag) else {
        return Ok(None);
    };
    let contents = if path.as_os_str() == "-" {
        let mut contents = String::new();
        io::stdin()
            .read_to_string(&mut contents)
            .map_err(|error| input_read_failure(file_flag, None, &error))?;
        contents
    } else {
        fs::read_to_string(&path)
            .map_err(|error| input_read_failure(file_flag, Some(&path), &error))?
    };
    serde_json::from_str(&contents)
        .map_err(|error| invalid_json_input(file_flag, Some(&path), &error))
        .map(Some)
}

fn input_read_failure(source: &str, path: Option<&PathBuf>, error: &io::Error) -> ContractFailure {
    ContractFailure {
        exit_code: 2,
        category: "input",
        code: "input_read_failed",
        message: "A JSON input source could not be read.".to_owned(),
        stage: "read_input",
        delivery: "not_applicable",
        retry: "after_change",
        data: compact_object(json!({
            "source": source,
            "path": path.map(|path| path.to_string_lossy().into_owned()),
            "osCode": error.raw_os_error(),
        })),
    }
}

fn invalid_json_input(
    source: &str,
    path: Option<&PathBuf>,
    error: &serde_json::Error,
) -> ContractFailure {
    ContractFailure {
        exit_code: 2,
        category: "input",
        code: "invalid_json_input",
        message: "A JSON input source is invalid.".to_owned(),
        stage: "read_input",
        delivery: "not_applicable",
        retry: "never",
        data: compact_object(json!({
            "source": source,
            "path": path.map(|path| path.to_string_lossy().into_owned()),
            "line": error.line(),
            "column": error.column(),
        })),
    }
}

pub(crate) fn merge_partial_results(
    command: QueryCommand,
    mut result: Value,
    partials: Vec<Value>,
) -> Result<Value, ContractFailure> {
    if partials.is_empty() {
        return Ok(result);
    }
    match command {
        QueryCommand::References | QueryCommand::WorkspaceSymbols | QueryCommand::CodeActions => {
            let final_items = take_array_or_null(
                &mut result,
                command.method().unwrap(),
                "an array or null",
                "$",
            )?;
            let mut items = Vec::new();
            for (index, partial) in partials.into_iter().enumerate() {
                let Value::Array(mut partial) = partial else {
                    return Err(invalid_result(
                        command.method().unwrap(),
                        "an array partial result",
                        &format!("$partial[{index}]"),
                    ));
                };
                items.append(&mut partial);
            }
            items.extend(final_items);
            Ok(Value::Array(items))
        }
        QueryCommand::WorkspaceDiagnostics => {
            let Value::Object(mut final_report) = result else {
                return Err(invalid_result(
                    command.method().unwrap(),
                    "a WorkspaceDiagnosticReport object",
                    "$",
                ));
            };
            let mut items = Vec::new();
            for (index, partial) in partials.into_iter().enumerate() {
                let Some(mut partial_items) =
                    partial.get("items").and_then(Value::as_array).cloned()
                else {
                    return Err(invalid_result(
                        command.method().unwrap(),
                        "a WorkspaceDiagnosticReportPartialResult object",
                        &format!("$partial[{index}]"),
                    ));
                };
                items.append(&mut partial_items);
            }
            let Some(final_items) = final_report.get("items").and_then(Value::as_array) else {
                return Err(invalid_result(
                    command.method().unwrap(),
                    "an items array",
                    "$.items",
                ));
            };
            items.extend(final_items.iter().cloned());
            final_report.insert("items".to_owned(), Value::Array(items));
            Ok(Value::Object(final_report))
        }
        QueryCommand::DocumentDiagnostics => {
            let Value::Object(mut final_report) = result else {
                return Err(invalid_result(
                    command.method().unwrap(),
                    "a DocumentDiagnosticReport object",
                    "$",
                ));
            };
            let mut related = serde_json::Map::new();
            for (index, partial) in partials.into_iter().enumerate() {
                let Some(partial_related) =
                    partial.get("relatedDocuments").and_then(Value::as_object)
                else {
                    return Err(invalid_result(
                        command.method().unwrap(),
                        "a DocumentDiagnosticReportPartialResult object",
                        &format!("$partial[{index}]"),
                    ));
                };
                related.extend(partial_related.clone());
            }
            if let Some(final_related) = final_report
                .get("relatedDocuments")
                .and_then(Value::as_object)
            {
                related.extend(final_related.clone());
            }
            if !related.is_empty() {
                final_report.insert("relatedDocuments".to_owned(), Value::Object(related));
            }
            Ok(Value::Object(final_report))
        }
        _ => Err(invalid_result(
            command.method().unwrap_or("unknown"),
            "no partial results for this method",
            "$partial",
        )),
    }
}

fn take_array_or_null(
    value: &mut Value,
    method: &str,
    expected: &str,
    json_path: &str,
) -> Result<Vec<Value>, ContractFailure> {
    match std::mem::take(value) {
        Value::Array(items) => Ok(items),
        Value::Null => Ok(Vec::new()),
        _ => Err(invalid_result(method, expected, json_path)),
    }
}

pub(crate) fn normalize_named_result(
    command: QueryCommand,
    mut result: Value,
) -> Result<Value, ContractFailure> {
    let method = command.method().unwrap();
    match command {
        QueryCommand::Definition => {
            result = match result {
                Value::Null => Value::Array(Vec::new()),
                Value::Object(_) => Value::Array(vec![result]),
                Value::Array(_) => result,
                _ => {
                    return Err(invalid_result(
                        method,
                        "a Location, Location array, or null",
                        "$",
                    ));
                }
            };
            canonicalize_result_uris(command, &mut result)?;
        }
        QueryCommand::Hover | QueryCommand::PrepareRename => {
            result = match result {
                Value::Null => Value::Array(Vec::new()),
                Value::Object(_) => Value::Array(vec![result]),
                _ => return Err(invalid_result(method, "an object or null", "$")),
            };
        }
        QueryCommand::References
        | QueryCommand::DocumentSymbols
        | QueryCommand::WorkspaceSymbols
        | QueryCommand::CodeActions
        | QueryCommand::Format => {
            result = match result {
                Value::Null => Value::Array(Vec::new()),
                Value::Array(_) => result,
                _ => return Err(invalid_result(method, "an array or null", "$")),
            };
            for (index, item) in result.as_array().unwrap().iter().enumerate() {
                if !item.is_object() {
                    return Err(invalid_result(
                        method,
                        "an array of LSP objects",
                        &format!("$[{index}]"),
                    ));
                }
            }
            canonicalize_result_uris(command, &mut result)?;
        }
        QueryCommand::DocumentDiagnostics => validate_document_diagnostic_report(
            &result,
            method,
            "$",
            DiagnosticReportContext::Document,
        )?,
        QueryCommand::WorkspaceDiagnostics => {
            let Some(items) = result.get("items").and_then(Value::as_array) else {
                return Err(invalid_result(
                    method,
                    "a WorkspaceDiagnosticReport items array",
                    "$.items",
                ));
            };
            for (index, item) in items.iter().enumerate() {
                validate_document_diagnostic_report(
                    item,
                    method,
                    &format!("$.items[{index}]"),
                    DiagnosticReportContext::Workspace,
                )?;
            }
        }
        QueryCommand::Rename => {
            if !result.is_null() && !result.is_object() {
                return Err(invalid_result(
                    method,
                    "a WorkspaceEdit object or null",
                    "$",
                ));
            }
        }
        QueryCommand::ResolveCodeAction => {
            if !result.is_object() {
                return Err(invalid_result(method, "a CodeAction object", "$"));
            }
        }
        QueryCommand::ExecuteCommand => {}
        QueryCommand::Raw | QueryCommand::PublishedDiagnostics | QueryCommand::Capabilities => {
            unreachable!("local and raw Queries bypass named result normalization")
        }
    }
    Ok(result)
}

#[derive(Clone, Copy)]
enum DiagnosticReportContext {
    Document,
    Workspace,
    Related,
}

fn validate_document_diagnostic_report(
    report: &Value,
    method: &str,
    json_path: &str,
    context: DiagnosticReportContext,
) -> Result<(), ContractFailure> {
    let Some(report) = report.as_object() else {
        return Err(invalid_result(
            method,
            "a diagnostic report object",
            json_path,
        ));
    };
    if matches!(context, DiagnosticReportContext::Workspace) {
        if report.get("uri").and_then(Value::as_str).is_none() {
            return Err(invalid_result(
                method,
                "a Workspace diagnostic report URI",
                &format!("{json_path}.uri"),
            ));
        }
        if !matches!(report.get("version"), Some(Value::Number(_) | Value::Null)) {
            return Err(invalid_result(
                method,
                "a Workspace diagnostic report version or null",
                &format!("{json_path}.version"),
            ));
        }
    }
    match report.get("kind").and_then(Value::as_str) {
        Some("full") if report.get("items").is_some_and(Value::is_array) => {}
        Some("full") => {
            return Err(invalid_result(
                method,
                "a full diagnostic report items array",
                &format!("{json_path}.items"),
            ));
        }
        Some("unchanged") if report.get("resultId").is_some_and(Value::is_string) => {}
        Some("unchanged") => {
            return Err(invalid_result(
                method,
                "an unchanged diagnostic report resultId",
                &format!("{json_path}.resultId"),
            ));
        }
        _ => {
            return Err(invalid_result(
                method,
                "a full or unchanged diagnostic report kind",
                &format!("{json_path}.kind"),
            ));
        }
    }
    if matches!(context, DiagnosticReportContext::Document)
        && let Some(related) = report.get("relatedDocuments")
    {
        if related.is_null() {
            return Ok(());
        }
        let Some(related) = related.as_object() else {
            return Err(invalid_result(
                method,
                "a relatedDocuments object or null",
                &format!("{json_path}.relatedDocuments"),
            ));
        };
        for (uri, report) in related {
            validate_document_diagnostic_report(
                report,
                method,
                &format!("{json_path}.relatedDocuments[{uri:?}]"),
                DiagnosticReportContext::Related,
            )?;
        }
    }
    Ok(())
}

fn canonicalize_result_uris(
    command: QueryCommand,
    result: &mut Value,
) -> Result<(), ContractFailure> {
    let Some(items) = result.as_array_mut() else {
        return Ok(());
    };
    for (index, item) in items.iter_mut().enumerate() {
        match command {
            QueryCommand::Definition => {
                if item.get("targetUri").is_some() {
                    canonicalize_uri_field(
                        item,
                        "targetUri",
                        command.method().unwrap(),
                        &format!("$[{index}].targetUri"),
                    )?;
                } else {
                    canonicalize_uri_field(
                        item,
                        "uri",
                        command.method().unwrap(),
                        &format!("$[{index}].uri"),
                    )?;
                }
            }
            QueryCommand::References => canonicalize_uri_field(
                item,
                "uri",
                command.method().unwrap(),
                &format!("$[{index}].uri"),
            )?,
            QueryCommand::DocumentSymbols | QueryCommand::WorkspaceSymbols => {
                if let Some(location) = item.get_mut("location") {
                    canonicalize_uri_field(
                        location,
                        "uri",
                        command.method().unwrap(),
                        &format!("$[{index}].location.uri"),
                    )?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn canonicalize_uri_field(
    object: &mut Value,
    field: &str,
    method: &str,
    json_path: &str,
) -> Result<(), ContractFailure> {
    let Some(uri) = object.get_mut(field) else {
        return Err(invalid_result(method, "an LSP URI field", json_path));
    };
    let Some(uri_text) = uri.as_str() else {
        return Err(invalid_result(method, "a URI string", json_path));
    };
    let parsed =
        Url::parse(uri_text).map_err(|_| invalid_result(method, "a valid URI", json_path))?;
    if parsed.scheme() != "file" {
        return Ok(());
    }
    let path = parsed
        .to_file_path()
        .map_err(|()| invalid_result(method, "a valid file URI", json_path))?;
    *uri = Value::String(
        Url::from_file_path(path)
            .map_err(|()| invalid_result(method, "a valid file URI", json_path))?
            .to_string(),
    );
    Ok(())
}

fn query_envelope(
    composed: &ComposedQuery,
    context: Value,
    result: Value,
    trace: Option<Value>,
    apply_edit_ledger: Vec<Value>,
) -> Result<Value, ContractFailure> {
    let command = composed.command.name();
    let method = composed
        .request
        .as_ref()
        .map(|request| request.method.as_str())
        .unwrap_or("textDocument/publishDiagnostics");
    let mut envelope = json!({
        "schemaVersion": 1,
        "ok": true,
        "command": [command],
        "method": method,
        "context": context,
        "result": result,
    });
    if let Some(page) = composed.page {
        add_page(&mut envelope, page, method)?;
    }
    if composed.command == QueryCommand::Raw {
        envelope["raw"] = Value::Bool(true);
    }
    if let Some(trace) = trace {
        envelope["trace"] = trace;
    }
    if !apply_edit_ledger.is_empty() || composed.command == QueryCommand::ExecuteCommand {
        envelope["applyEditLedger"] = Value::Array(apply_edit_ledger);
    }
    if composed.command == QueryCommand::ExecuteCommand {
        envelope["directServerSideEffects"] = Value::String("unknown".to_owned());
    }
    Ok(envelope)
}

struct DiagnosticMetadata {
    raw_report: Option<Value>,
    fresh: bool,
    complete: bool,
    workspace_complete: Option<bool>,
    source: &'static str,
}

fn diagnostic_envelope(
    composed: &ComposedQuery,
    context: Value,
    result: Value,
    metadata: DiagnosticMetadata,
    trace: Option<Value>,
    apply_edit_ledger: Vec<Value>,
) -> Result<Value, ContractFailure> {
    let mut envelope = query_envelope(composed, context, result, trace, apply_edit_ledger)?;
    envelope["diagnostics"] = compact_object(json!({
        "source": metadata.source,
        "fresh": metadata.fresh,
        "complete": metadata.complete,
        "workspaceComplete": metadata.workspace_complete,
        "rawReport": metadata.raw_report,
    }));
    if metadata.workspace_complete.is_none() {
        envelope["diagnostics"]["workspaceComplete"] = Value::Null;
    }
    Ok(envelope)
}

fn document_diagnostic_envelope(
    composed: &ComposedQuery,
    context: Value,
    result: Value,
    trace: Option<Value>,
    apply_edit_ledger: Vec<Value>,
    diagnostics: &mut DiagnosticCache,
) -> Result<Value, ContractFailure> {
    let uri = composed.document_uri.as_deref().unwrap();
    let raw_contains_unchanged = diagnostic_report_contains_unchanged(&result);
    require_cached_unchanged_report(uri, &result, diagnostics, "$")?;
    if let Some(related) = result.get("relatedDocuments").and_then(Value::as_object) {
        for (related_uri, related_report) in related {
            require_cached_unchanged_report(
                related_uri,
                related_report,
                diagnostics,
                &format!("$.relatedDocuments[{related_uri:?}]"),
            )?;
        }
    }
    let diagnostic = diagnostics.apply_document_pull_report(uri, result);
    if diagnostic.effective_report.is_null()
        || diagnostic_report_contains_unchanged(&diagnostic.effective_report)
    {
        return Err(invalid_result(
            composed.command.method().unwrap(),
            "a cached full report for an unchanged diagnostic result",
            "$",
        ));
    }
    let raw_report = raw_contains_unchanged.then_some(diagnostic.raw_report);
    diagnostic_envelope(
        composed,
        context,
        diagnostic.effective_report,
        DiagnosticMetadata {
            raw_report,
            fresh: diagnostic.fresh,
            complete: diagnostic.complete,
            workspace_complete: None,
            source: "pull-document",
        },
        trace,
        apply_edit_ledger,
    )
}

fn require_cached_unchanged_report(
    uri: &str,
    report: &Value,
    diagnostics: &DiagnosticCache,
    json_path: &str,
) -> Result<(), ContractFailure> {
    if report.get("kind").and_then(Value::as_str) == Some("unchanged")
        && diagnostics.pull_result_id(uri).is_none()
    {
        return Err(invalid_result(
            "textDocument/diagnostic",
            "a cached full report for an unchanged diagnostic result",
            json_path,
        ));
    }
    Ok(())
}

fn diagnostic_report_contains_unchanged(report: &Value) -> bool {
    report.get("kind").and_then(Value::as_str) == Some("unchanged")
        || report
            .get("relatedDocuments")
            .and_then(Value::as_object)
            .is_some_and(|reports| {
                reports
                    .values()
                    .any(|report| report.get("kind").and_then(Value::as_str) == Some("unchanged"))
            })
        || report
            .get("items")
            .and_then(Value::as_array)
            .is_some_and(|reports| {
                reports
                    .iter()
                    .any(|report| report.get("kind").and_then(Value::as_str) == Some("unchanged"))
            })
}

fn workspace_diagnostic_envelope(
    composed: &ComposedQuery,
    context: Value,
    result: Value,
    trace: Option<Value>,
    apply_edit_ledger: Vec<Value>,
    diagnostics: &mut DiagnosticCache,
) -> Result<Value, ContractFailure> {
    for (index, item) in result["items"].as_array().unwrap().iter().enumerate() {
        let uri = item["uri"].as_str().unwrap();
        if item.get("kind").and_then(Value::as_str) == Some("unchanged")
            && diagnostics.pull_result_id(uri).is_none()
        {
            return Err(invalid_result(
                composed.command.method().unwrap(),
                "a cached full report for an unchanged diagnostic result",
                &format!("$.items[{index}]"),
            ));
        }
    }
    let diagnostic = diagnostics.apply_workspace_pull_report(result);
    if diagnostic_report_contains_unchanged(&diagnostic.effective_report) {
        return Err(invalid_result(
            composed.command.method().unwrap(),
            "cached full reports for unchanged diagnostic results",
            "$.items",
        ));
    }
    diagnostic_envelope(
        composed,
        context,
        diagnostic.effective_report,
        DiagnosticMetadata {
            raw_report: (!diagnostic.raw_report.is_null()).then_some(diagnostic.raw_report),
            fresh: diagnostic.fresh,
            complete: diagnostic.complete,
            workspace_complete: Some(diagnostic.workspace_complete),
            source: "pull-workspace",
        },
        trace,
        apply_edit_ledger,
    )
}

fn mutation_query_envelope(
    composed: &ComposedQuery,
    context: Value,
    result: Value,
    trace: Option<Value>,
    apply_edit_ledger: Vec<Value>,
    previews: &mut impl PreviewCreator,
) -> Result<Value, ContractFailure> {
    if composed.command == QueryCommand::ResolveCodeAction
        && let Some(disabled) = result
            .get("disabled")
            .filter(|disabled| !disabled.is_null())
    {
        let Some(disabled) = disabled.as_object() else {
            return Err(invalid_result(
                composed.command.method().unwrap(),
                "a CodeAction disabled object",
                "$.disabled",
            ));
        };
        let reason = disabled
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("The selected code action is disabled.");
        return Err(ContractFailure {
            exit_code: 6,
            category: "mutation",
            code: "disabled_code_action",
            message: "The selected code action is disabled.".to_owned(),
            stage: "validate_mutation",
            delivery: "sent",
            retry: "never",
            data: json!({"reason": reason, "action": result}),
        });
    }

    let (edit, command_payload, resolved_action) = match composed.command {
        QueryCommand::Rename => (result, None, None),
        QueryCommand::Format => {
            let edits = result.as_array().unwrap();
            if edits.is_empty() {
                (Value::Null, None, None)
            } else {
                (
                    json!({
                        "documentChanges": [{
                            "textDocument": {
                                "uri": composed.document_uri,
                                "version": composed.document_version,
                            },
                            "edits": edits,
                        }]
                    }),
                    None,
                    None,
                )
            }
        }
        QueryCommand::ResolveCodeAction => (
            result.get("edit").cloned().unwrap_or(Value::Null),
            result.get("command").cloned(),
            Some(result),
        ),
        _ => unreachable!("only Mutation Queries create Previews"),
    };

    let preview = if workspace_edit_is_empty(&edit) {
        Value::Null
    } else {
        previews.create_preview(PreviewProposal {
            command: composed.command,
            method: composed.request.as_ref().unwrap().method.clone(),
            context: context.clone(),
            edit,
            command_payload,
            resolved_action: resolved_action.clone(),
            trace: trace.clone(),
            apply_edit_ledger: apply_edit_ledger.clone(),
        })?
    };
    let outcome = if preview.is_null() {
        "unchanged"
    } else {
        "previewed"
    };
    let mut envelope = query_envelope(composed, context, preview, trace, apply_edit_ledger)?;
    envelope["outcome"] = Value::String(outcome.to_owned());
    if let Some(resolved_action) = resolved_action {
        envelope["resolvedAction"] = resolved_action;
    }
    Ok(envelope)
}

fn workspace_edit_is_empty(edit: &Value) -> bool {
    if edit.is_null() {
        return true;
    }
    let Some(edit) = edit.as_object() else {
        return false;
    };
    let changes_are_empty = edit.get("changes").is_none_or(|changes| {
        changes.as_object().is_some_and(|changes| {
            changes
                .values()
                .all(|edits| edits.as_array().is_some_and(Vec::is_empty))
        })
    });
    let document_changes_are_empty = edit
        .get("documentChanges")
        .is_none_or(|changes| changes.as_array().is_some_and(Vec::is_empty));
    changes_are_empty && document_changes_are_empty
}

fn published_envelope(
    composed: ComposedQuery,
    context: Value,
    diagnostics: &mut DiagnosticCache,
) -> Result<Value, ContractFailure> {
    let (results, fresh, complete) = if let Some(uri) = &composed.document_uri {
        let current = diagnostics.published(uri, composed.document_version);
        (current.diagnostics, current.fresh, current.complete)
    } else {
        (
            json!(diagnostics.all_known().into_iter().map(|current| json!({"uri": current.uri, "version": current.version, "diagnostics": current.diagnostics, "fresh": current.fresh, "closed": current.closed})).collect::<Vec<_>>()),
            false,
            true,
        )
    };
    diagnostic_envelope(
        &composed,
        context,
        results,
        DiagnosticMetadata {
            raw_report: None,
            fresh,
            complete,
            workspace_complete: None,
            source: "published",
        },
        None,
        Vec::new(),
    )
}

fn capabilities_envelope(context: Value, capabilities: &Capabilities, include_raw: bool) -> Value {
    let providers = capabilities
        .providers
        .iter()
        .map(|(name, provider)| {
            (
                name.clone(),
                compact_object(json!({
                    "state": provider.state.name(),
                    "capabilityPath": provider.capability_path,
                    "options": provider.options,
                    "problems": provider.problems,
                })),
            )
        })
        .collect::<serde_json::Map<_, _>>();
    let mut result = json!({
        "protocolBaseline": "3.17",
        "clientProfileVersion": 1,
        "providers": providers,
    });
    if include_raw && let Some(initialize_result) = &capabilities.initialize_result {
        result["initializeResult"] = initialize_result.clone();
    }
    json!({"schemaVersion": 1, "ok": true, "command": ["capabilities"], "context": context, "result": result})
}

fn add_page(envelope: &mut Value, page: Page, method: &str) -> Result<(), ContractFailure> {
    let result = &mut envelope["result"];
    let items = if result.is_array() {
        result.as_array_mut()
    } else {
        result.get_mut("items").and_then(Value::as_array_mut)
    };
    let Some(items) = items else {
        return Err(invalid_result(
            method,
            "a pageable top-level item collection",
            "$",
        ));
    };
    let total = items.len();
    let returned_items = items
        .drain(..)
        .skip(page.offset)
        .take(page.limit)
        .collect::<Vec<_>>();
    let returned = returned_items.len();
    *items = returned_items;
    let complete = page.offset.saturating_add(returned) >= total;
    envelope["page"] = json!({"offset": page.offset, "limit": page.limit, "returned": returned, "complete": complete, "nextOffset": (!complete).then_some(page.offset + returned)});
    Ok(())
}

fn has_source_position(command: QueryCommand) -> bool {
    matches!(
        command,
        QueryCommand::Definition
            | QueryCommand::References
            | QueryCommand::Hover
            | QueryCommand::PrepareRename
            | QueryCommand::Rename
            | QueryCommand::CodeActions
    )
}

fn input_failure(message: &str) -> ContractFailure {
    ContractFailure {
        exit_code: 2,
        category: "input",
        code: "invalid_arguments",
        message: message.to_owned(),
        stage: "parse_cli",
        delivery: "not_applicable",
        retry: "never",
        data: json!({"problems": []}),
    }
}

fn unknown_query() -> ContractFailure {
    input_failure("The command is not a Query.")
}

fn invalid_result(method: &str, expected: &str, json_path: &str) -> ContractFailure {
    ContractFailure {
        exit_code: 5,
        category: "query",
        code: "invalid_server_result",
        message: format!("The {method} result did not match {expected}."),
        stage: "validate_result",
        delivery: "sent",
        retry: "after_change",
        data: json!({"method": method, "expected": expected, "jsonPath": json_path}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::{DocumentStore, TextSynchronization};
    use tempfile::TempDir;

    fn document() -> DocumentSnapshot {
        let root = TempDir::new().unwrap();
        let path = root.path().join("main.rs");
        std::fs::write(&path, "fn 😀() {}\n").unwrap();
        DocumentStore::new(2, 1024, 2048)
            .refresh(&path, "rust", TextSynchronization::OpenClose)
            .unwrap()
            .snapshot
    }

    fn supported(provider: &str) -> Capabilities {
        supported_with_options(provider, json!({}))
    }

    fn supported_with_options(provider: &str, options: Value) -> Capabilities {
        Capabilities {
            providers: BTreeMap::from([(
                provider.to_owned(),
                Provider {
                    state: ProviderState::Supported,
                    capability_path: provider.to_owned(),
                    options: Some(options),
                    selector: None,
                    problems: Vec::new(),
                },
            )]),
            initialize_result: None,
        }
    }

    fn context() -> QueryContext {
        QueryContext {
            workspace_uri: "file:///workspace/".to_owned(),
            server: "test".to_owned(),
            session_identity:
                "sid_0000000000000000000000000000000000000000000000000000000000000000".to_owned(),
            owner_generation: "gen_00000000000000000000000000000000".to_owned(),
            result_position_encoding: PositionEncoding::Utf16,
            server_progress: Vec::new(),
            synchronization: json!({"mode":"document", "bestEffort":false, "before":[], "failures":[], "postResponseChanged":[]}),
            recovery: json!({"required":false}),
        }
    }

    struct TestDispatcher(DispatchResponse);

    struct FailedDispatcher(Option<DispatchFailure>);

    struct NoPreview;

    impl PreviewCreator for NoPreview {
        fn create_preview(&mut self, _proposal: PreviewProposal) -> Result<Value, ContractFailure> {
            Ok(Value::Null)
        }
    }

    #[derive(Default)]
    struct RecordingPreview {
        proposals: Vec<PreviewProposal>,
    }

    impl PreviewCreator for RecordingPreview {
        fn create_preview(&mut self, proposal: PreviewProposal) -> Result<Value, ContractFailure> {
            self.proposals.push(proposal);
            Ok(json!({"previewId":"prv_00000000000000000000000000000000"}))
        }
    }

    impl SessionDispatcher for TestDispatcher {
        fn dispatch(
            &mut self,
            _request: DispatchRequest,
        ) -> Result<DispatchResponse, DispatchFailure> {
            Ok(self.0.clone())
        }
    }

    impl SessionDispatcher for FailedDispatcher {
        fn dispatch(
            &mut self,
            _request: DispatchRequest,
        ) -> Result<DispatchResponse, DispatchFailure> {
            Err(self.0.take().unwrap())
        }
    }

    #[test]
    fn named_position_requests_use_negotiated_encoding() {
        let document = document();
        let invocation = ParsedInvocation {
            command: vec!["definition".into()],
            options: BTreeMap::from([
                ("--line".into(), vec!["0".into()]),
                ("--column".into(), vec!["4".into()]),
            ]),
            positionals: Vec::new(),
        };
        let capabilities = supported("definitionProvider");
        let query = compose(
            &invocation,
            Some(&document),
            PositionEncoding::Utf16,
            &capabilities,
            None,
        )
        .unwrap();
        assert_eq!(
            query.request.unwrap().params.unwrap()["position"]["character"],
            5
        );
    }

    #[test]
    fn raw_preserves_explicit_null_and_sync_files() {
        let invocation = ParsedInvocation {
            command: vec!["raw".into()],
            options: BTreeMap::from([
                ("--method".into(), vec!["custom/method".into()]),
                ("--params-json".into(), vec!["null".into()]),
                ("--sync-file".into(), vec!["one.rs".into(), "two.rs".into()]),
            ]),
            positionals: Vec::new(),
        };
        let request = compose(
            &invocation,
            None,
            PositionEncoding::Utf16,
            &Capabilities::default(),
            None,
        )
        .unwrap()
        .request
        .unwrap();
        assert_eq!(request.params, Some(Value::Null));
        assert_eq!(
            request.synchronized_files,
            [PathBuf::from("one.rs"), PathBuf::from("two.rs")]
        );
    }

    #[test]
    fn unsupported_named_provider_fails_before_dispatch() {
        let invocation = ParsedInvocation {
            command: vec!["hover".into()],
            options: BTreeMap::new(),
            positionals: Vec::new(),
        };
        assert_eq!(
            compose(
                &invocation,
                None,
                PositionEncoding::Utf16,
                &Capabilities::default(),
                None,
            )
            .unwrap_err()
            .code,
            "capability_unavailable"
        );
    }

    #[test]
    fn recursive_glob_respects_directory_boundaries() {
        for (pattern, path, ignore_case, expected) in [
            ("**/main.rs", "main.rs", false, true),
            ("**/main.rs", "/main.rs", false, true),
            ("**/main.rs", "src/main.rs", false, true),
            ("**/main.rs", "/workspace/src/main.rs", false, true),
            ("**/main.rs", "domain.rs", false, false),
            ("**/main.rs", "/workspace/domain.rs", false, false),
            ("**/main.rs", "/workspace/main.rs.bak", false, false),
            ("src/**/main.rs", "src/main.rs", false, true),
            ("src/**/main.rs", "src/one/two/main.rs", false, true),
            ("src/**/main.rs", "src/domain.rs", false, false),
            ("*.rs", "main.rs", false, true),
            ("*.rs", "src/main.rs", false, false),
            ("**.rs", "src/main.rs", false, true),
            ("**main.rs", "src/domain.rs", false, true),
            ("**/main.{rs,py}", "src/main.py", false, true),
            ("**/main.{rs,py}", "src/domain.rs", false, false),
            ("**/main.[r-t][!x]", "src/main.rs", false, true),
            ("**/main.[!r]s", "src/main.rs", false, false),
            ("**/MAIN.RS", "src/main.rs", true, true),
            ("**/MAIN.RS", "src/main.rs", false, false),
            ("**/MAIN.RS", "src/DOMAIN.RS", true, false),
        ] {
            assert_eq!(
                protocol_glob_matches(pattern, path, ignore_case),
                expected,
                "pattern={pattern:?}, path={path:?}, ignore_case={ignore_case}"
            );
        }
    }

    #[test]
    fn recursive_glob_document_selectors_reject_filename_suffixes() {
        let invocation = ParsedInvocation {
            command: vec!["definition".into()],
            options: BTreeMap::from([
                ("--line".into(), vec!["0".into()]),
                ("--column".into(), vec!["0".into()]),
            ]),
            positionals: Vec::new(),
        };
        let mut document = document();
        for pattern in [
            json!("**/main.rs"),
            json!({"baseUri": "file:///workspace", "pattern": "**/main.rs"}),
        ] {
            let mut capabilities = supported_with_options("definition", json!({}));
            capabilities
                .providers
                .get_mut("definition")
                .unwrap()
                .selector =
                Some(json!([{"language": "rust", "scheme": "file", "pattern": pattern}]));
            for (name, expected) in [("main.rs", true), ("domain.rs", false)] {
                document.uri = format!("file:///workspace/{name}");
                let result = compose(
                    &invocation,
                    Some(&document),
                    PositionEncoding::Utf16,
                    &capabilities,
                    None,
                );
                if expected {
                    assert!(result.is_ok(), "{pattern:?}: {name}: {result:?}");
                } else {
                    assert_eq!(
                        result
                            .expect_err("filename suffix must not admit a Document")
                            .code,
                        "capability_unavailable",
                        "{pattern:?}: {name}"
                    );
                }
            }
        }
    }

    #[test]
    fn capability_gates_subfeatures_commands_and_document_selectors() {
        let document = document();
        let prepare = ParsedInvocation {
            command: vec!["prepare-rename".into()],
            options: BTreeMap::from([
                ("--line".into(), vec!["0".into()]),
                ("--column".into(), vec!["0".into()]),
            ]),
            positionals: Vec::new(),
        };
        assert_eq!(
            compose(
                &prepare,
                Some(&document),
                PositionEncoding::Utf16,
                &supported("renameProvider"),
                None,
            )
            .unwrap_err()
            .code,
            "capability_unavailable"
        );
        compose(
            &prepare,
            Some(&document),
            PositionEncoding::Utf16,
            &supported_with_options("rename", json!({"prepareProvider":true})),
            None,
        )
        .unwrap();

        let definition = ParsedInvocation {
            command: vec!["definition".into()],
            options: BTreeMap::from([
                ("--line".into(), vec!["0".into()]),
                ("--column".into(), vec!["0".into()]),
            ]),
            positionals: Vec::new(),
        };
        let mut capabilities = supported_with_options(
            "definition",
            json!({"documentSelector":[{"language":"rust","scheme":"file","pattern":"**/*.{rs,txt}"}]}),
        );
        capabilities
            .providers
            .get_mut("definition")
            .unwrap()
            .selector =
            Some(json!([{"language":"rust","scheme":"file","pattern":"**/*.{rs,txt}"}]));
        compose(
            &definition,
            Some(&document),
            PositionEncoding::Utf16,
            &capabilities,
            None,
        )
        .unwrap();
        capabilities
            .providers
            .get_mut("definition")
            .unwrap()
            .selector = Some(json!([{"language":"typescript"}]));
        assert_eq!(
            compose(
                &definition,
                Some(&document),
                PositionEncoding::Utf16,
                &capabilities,
                None,
            )
            .unwrap_err()
            .data["selector"][0]["language"],
            "typescript"
        );

        assert!(glob_matches(b"**/main.[r-t][!x]", b"/workspace/main.rs"));
        assert!(!glob_matches(b"**/main.[!r]s", b"/workspace/main.rs"));
    }

    #[test]
    fn compose_requests_owner_progress_trace_and_apply_policies() {
        let document = document();
        let references = ParsedInvocation {
            command: vec!["references".into()],
            options: BTreeMap::from([
                ("--line".into(), vec!["0".into()]),
                ("--column".into(), vec!["0".into()]),
                ("--include-declaration".into(), vec!["false".into()]),
                ("--trace-protocol".into(), Vec::new()),
            ]),
            positionals: Vec::new(),
        };
        let request = compose(
            &references,
            Some(&document),
            PositionEncoding::Utf16,
            &supported_with_options("references", json!({"workDoneProgress":true})),
            None,
        )
        .unwrap()
        .request
        .unwrap();
        assert!(request.partial_results);
        assert!(request.work_done_progress);
        assert!(request.trace_protocol);

        let execute_command = ParsedInvocation {
            command: vec!["execute-command".into()],
            options: BTreeMap::from([
                ("--command".into(), vec!["test.run".into()]),
                ("--apply-edits".into(), Vec::new()),
            ]),
            positionals: Vec::new(),
        };
        let request = compose(
            &execute_command,
            None,
            PositionEncoding::Utf16,
            &supported_with_options("executeCommand", json!({"commands":["test.run"]})),
            None,
        )
        .unwrap()
        .request
        .unwrap();
        assert!(request.apply_edits);
    }

    #[test]
    fn partials_and_paging_are_normalized() {
        assert_eq!(
            merge_partial_results(QueryCommand::References, json!([3]), vec![json!([1, 2])])
                .unwrap(),
            json!([1, 2, 3])
        );
        let mut envelope = json!({"result": [0, 1, 2]});
        add_page(
            &mut envelope,
            Page {
                offset: 1,
                limit: 1,
            },
            "test/method",
        )
        .unwrap();
        assert_eq!(envelope["result"], json!([1]));
        assert_eq!(envelope["page"]["nextOffset"], 2);

        assert_eq!(
            merge_partial_results(
                QueryCommand::WorkspaceDiagnostics,
                json!({"items":[{"uri":"file:///final","kind":"full","items":[]}]}),
                vec![json!({"items":[{"uri":"file:///partial","kind":"full","items":[]}]})],
            )
            .unwrap()["items"][0]["uri"],
            "file:///partial"
        );
        assert_eq!(
            merge_partial_results(
                QueryCommand::DocumentDiagnostics,
                json!({"kind":"full","items":[]}),
                vec![json!({"relatedDocuments":{"file:///related":{"kind":"full","items":[]}}})],
            )
            .unwrap()["relatedDocuments"]["file:///related"]["kind"],
            "full"
        );
    }

    #[test]
    fn failed_dispatch_keeps_partial_results_trace_and_apply_ledger() {
        let document = document();
        let invocation = ParsedInvocation {
            command: vec!["references".into()],
            options: BTreeMap::from([
                ("--line".into(), vec!["0".into()]),
                ("--column".into(), vec!["0".into()]),
                ("--include-declaration".into(), vec!["false".into()]),
            ]),
            positionals: Vec::new(),
        };
        let query = compose(
            &invocation,
            Some(&document),
            PositionEncoding::Utf16,
            &supported("references"),
            None,
        )
        .unwrap();
        let failure = execute(
            &mut FailedDispatcher(Some(DispatchFailure {
                failure: ContractFailure {
                    exit_code: 5,
                    category: "query",
                    code: "request_timeout",
                    message: "The language-server request timed out.".to_owned(),
                    stage: "await_response",
                    delivery: "uncertain",
                    retry: "unsafe",
                    data: json!({"timeout":"1s"}),
                },
                server_error: None,
                partial_results: vec![json!([1, 2]), json!([3])],
                trace: Some(json!({"frames":[]})),
                apply_edit_ledger: vec![json!({
                    "ordinal": 0,
                    "applied": false,
                    "outcome": "previewed"
                })],
            })),
            query,
            context(),
            &mut DiagnosticCache::new(4, 4096),
            &mut NoPreview,
        )
        .unwrap_err();
        let envelope = query_failure_envelope(invocation.command_path().to_vec(), &failure);
        assert_eq!(envelope["method"], "textDocument/references");
        assert_eq!(
            envelope["partialResult"],
            json!({"items":[1, 2, 3], "complete":false})
        );
        assert_eq!(envelope["trace"]["frames"], json!([]));
        assert_eq!(envelope["applyEditLedger"][0]["ordinal"], 0);
        assert_eq!(envelope["context"]["server"], "test");
    }

    #[test]
    fn code_action_and_execute_command_params_are_exact() {
        let document = document();
        let code_actions = ParsedInvocation {
            command: vec!["code-actions".into()],
            options: BTreeMap::from([
                ("--start-line".into(), vec!["0".into()]),
                ("--start-column".into(), vec!["0".into()]),
                ("--end-line".into(), vec!["0".into()]),
                ("--end-column".into(), vec!["2".into()]),
                (
                    "--context-json".into(),
                    vec![r#"{"diagnostics":[]}"#.into()],
                ),
            ]),
            positionals: Vec::new(),
        };
        let request = compose(
            &code_actions,
            Some(&document),
            PositionEncoding::Utf16,
            &supported("codeActionProvider"),
            None,
        )
        .unwrap()
        .request
        .unwrap();
        assert_eq!(request.params.unwrap()["context"]["diagnostics"], json!([]));

        let command = ParsedInvocation {
            command: vec!["execute-command".into()],
            options: BTreeMap::from([
                ("--command".into(), vec!["test.run".into()]),
                ("--arguments-json".into(), vec!["[1]".into()]),
            ]),
            positionals: Vec::new(),
        };
        assert_eq!(
            compose(
                &command,
                None,
                PositionEncoding::Utf16,
                &supported_with_options(
                    "executeCommandProvider",
                    json!({"commands": ["test.run"]}),
                ),
                None,
            )
            .unwrap()
            .request
            .unwrap()
            .params
            .unwrap(),
            json!({"command":"test.run", "arguments":[1]})
        );
    }

    #[test]
    fn dispatch_normalizes_partials_context_and_document_diagnostics() {
        let document = document();
        let invocation = ParsedInvocation {
            command: vec!["document-diagnostics".into()],
            options: BTreeMap::new(),
            positionals: Vec::new(),
        };
        let query = compose(
            &invocation,
            Some(&document),
            PositionEncoding::Utf16,
            &supported("diagnosticProvider"),
            None,
        )
        .unwrap();
        let mut dispatcher = TestDispatcher(DispatchResponse {
            result: json!({"kind":"full", "resultId":"one", "items":[{"message":"one"}]}),
            partial_results: Vec::new(),
            trace: None,
            apply_edit_ledger: Vec::new(),
            server_progress: Vec::new(),
            synchronization: None,
        });
        let mut diagnostics = DiagnosticCache::new(4, 4096);
        let output = execute(
            &mut dispatcher,
            query,
            context(),
            &mut diagnostics,
            &mut NoPreview,
        )
        .unwrap();
        assert_eq!(output["context"]["inputPositionEncoding"], Value::Null);
        assert_eq!(
            output["result"],
            json!({"kind":"full", "resultId":"one", "items":[{"message":"one"}]})
        );
        assert_eq!(output["diagnostics"]["source"], "pull-document");
        assert_eq!(output["diagnostics"].get("rawReport"), None);
    }

    #[test]
    fn mutation_queries_create_previews_without_applying_edits() {
        let document = document();
        let invocation = ParsedInvocation {
            command: vec!["format".into()],
            options: BTreeMap::from([("--options-json".into(), vec!["{}".into()])]),
            positionals: Vec::new(),
        };
        let query = compose(
            &invocation,
            Some(&document),
            PositionEncoding::Utf16,
            &supported("formatting"),
            None,
        )
        .unwrap();
        let mut dispatcher = TestDispatcher(DispatchResponse {
            result: json!([{"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":0}},"newText":"use x;\n"}]),
            partial_results: Vec::new(),
            trace: Some(json!({"frames":[]})),
            apply_edit_ledger: vec![json!({"ordinal":0,"applied":false,"outcome":"previewed"})],
            server_progress: Vec::new(),
            synchronization: None,
        });
        let mut previews = RecordingPreview::default();
        let output = execute(
            &mut dispatcher,
            query,
            context(),
            &mut DiagnosticCache::new(4, 4096),
            &mut previews,
        )
        .unwrap();
        assert_eq!(output["outcome"], "previewed");
        assert_eq!(output["method"], "textDocument/formatting");
        assert_eq!(output["applyEditLedger"][0]["ordinal"], 0);
        assert_eq!(previews.proposals.len(), 1);
        assert_eq!(
            previews.proposals[0].edit["documentChanges"][0]["textDocument"]["uri"],
            document.uri
        );
        assert_eq!(
            previews.proposals[0].edit["documentChanges"][0]["textDocument"]["version"],
            document.version
        );
    }

    #[test]
    fn unchanged_and_disabled_mutation_results_are_distinct() {
        let document = document();
        let rename = ParsedInvocation {
            command: vec!["rename".into()],
            options: BTreeMap::from([
                ("--line".into(), vec!["0".into()]),
                ("--column".into(), vec!["0".into()]),
                ("--new-name".into(), vec!["next".into()]),
            ]),
            positionals: Vec::new(),
        };
        let query = compose(
            &rename,
            Some(&document),
            PositionEncoding::Utf16,
            &supported("rename"),
            None,
        )
        .unwrap();
        let mut dispatcher = TestDispatcher(DispatchResponse {
            result: json!({}),
            partial_results: Vec::new(),
            trace: None,
            apply_edit_ledger: Vec::new(),
            server_progress: Vec::new(),
            synchronization: None,
        });
        let output = execute(
            &mut dispatcher,
            query,
            context(),
            &mut DiagnosticCache::new(4, 4096),
            &mut NoPreview,
        )
        .unwrap();
        assert_eq!(output["outcome"], "unchanged");
        assert_eq!(output["result"], Value::Null);
        assert!(workspace_edit_is_empty(
            &json!({"changes":{"file:///one":[]}})
        ));
        assert!(!workspace_edit_is_empty(
            &json!({"documentChanges":[{"kind":"create","uri":"file:///one"}]})
        ));

        let resolve = ParsedInvocation {
            command: vec!["resolve-code-action".into()],
            options: BTreeMap::from([(
                "--action-json".into(),
                vec![r#"{"title":"disabled"}"#.into()],
            )]),
            positionals: Vec::new(),
        };
        let query = compose(
            &resolve,
            None,
            PositionEncoding::Utf16,
            &supported_with_options("codeActions", json!({"resolveProvider":true})),
            None,
        )
        .unwrap();
        let mut dispatcher = TestDispatcher(DispatchResponse {
            result: json!({"title":"disabled","disabled":{"reason":"not applicable"}}),
            partial_results: Vec::new(),
            trace: Some(json!({"frames":[]})),
            apply_edit_ledger: Vec::new(),
            server_progress: Vec::new(),
            synchronization: None,
        });
        let failure = execute(
            &mut dispatcher,
            query,
            context(),
            &mut DiagnosticCache::new(4, 4096),
            &mut NoPreview,
        )
        .unwrap_err();
        assert_eq!(failure.failure.code, "disabled_code_action");
        assert_eq!(failure.failure.data["reason"], "not applicable");
        assert_eq!(failure.method.as_deref(), Some("codeAction/resolve"));
        assert_eq!(failure.trace.unwrap()["frames"], json!([]));
    }

    #[test]
    fn named_cardinality_and_invalid_results_are_normalized() {
        assert_eq!(
            normalize_named_result(QueryCommand::Hover, Value::Null).unwrap(),
            json!([])
        );
        assert_eq!(
            normalize_named_result(
                QueryCommand::Definition,
                json!({"uri":"https://example.test/a","range":{}}),
            )
            .unwrap(),
            json!([{"uri":"https://example.test/a","range":{}}])
        );
        let failure =
            normalize_named_result(QueryCommand::References, json!([{"range":{}}])).unwrap_err();
        assert_eq!(failure.code, "invalid_server_result");
        assert_eq!(failure.data["jsonPath"], "$[0].uri");

        let failure = normalize_named_result(
            QueryCommand::WorkspaceDiagnostics,
            json!({"items":[{"uri":"file:///one","kind":"full","items":[]}]}),
        )
        .unwrap_err();
        assert_eq!(failure.data["jsonPath"], "$.items[0].version");
    }

    #[test]
    fn published_and_workspace_diagnostics_report_contract_metadata() {
        let document = document();
        let mut diagnostics = DiagnosticCache::new(8, 8192);
        diagnostics.publish(
            &document.uri,
            Some(document.version),
            json!([{"message":"published"}]),
            Some(document.version),
            true,
        );
        let published = ParsedInvocation {
            command: vec!["published-diagnostics".into()],
            options: BTreeMap::new(),
            positionals: Vec::new(),
        };
        let query = compose(
            &published,
            Some(&document),
            PositionEncoding::Utf16,
            &Capabilities::default(),
            Some(&diagnostics),
        )
        .unwrap();
        let output = execute(
            &mut TestDispatcher(DispatchResponse {
                result: Value::Null,
                partial_results: Vec::new(),
                trace: None,
                apply_edit_ledger: Vec::new(),
                server_progress: Vec::new(),
                synchronization: None,
            }),
            query,
            context(),
            &mut diagnostics,
            &mut NoPreview,
        )
        .unwrap();
        assert_eq!(output["method"], "textDocument/publishDiagnostics");
        assert_eq!(output["result"][0]["message"], "published");
        assert_eq!(output["diagnostics"]["fresh"], true);
        assert_eq!(output["diagnostics"]["workspaceComplete"], Value::Null);

        let workspace = ParsedInvocation {
            command: vec!["workspace-diagnostics".into()],
            options: BTreeMap::new(),
            positionals: Vec::new(),
        };
        let query = compose(
            &workspace,
            None,
            PositionEncoding::Utf16,
            &supported_with_options("diagnostics", json!({"workspaceDiagnostics":true})),
            Some(&diagnostics),
        )
        .unwrap();
        let output = execute(
            &mut TestDispatcher(DispatchResponse {
                result: json!({"items":[{"uri":"file:///one","version":null,"kind":"full","items":[]}]}),
                partial_results: Vec::new(),
                trace: None,
                apply_edit_ledger: Vec::new(),
                server_progress: Vec::new(),
                synchronization: None,
            }),
            query,
            context(),
            &mut diagnostics,
            &mut NoPreview,
        )
        .unwrap();
        assert_eq!(output["diagnostics"]["source"], "pull-workspace");
        assert_eq!(output["diagnostics"]["workspaceComplete"], true);
    }

    #[test]
    fn capabilities_raw_output_and_json_input_errors_are_exact() {
        let capabilities = Capabilities::from_owner_result(&json!({
            "providers": {
                "definition": {
                    "state": "supported",
                    "capabilityPath": "capabilities.definitionProvider",
                    "options": {"documentSelector": [{"language":"rust"}]}
                }
            },
            "initializeResult": {"capabilities":{}}
        }));
        assert_eq!(
            capabilities.providers["definition"].selector,
            Some(json!([{"language":"rust"}]))
        );
        let invocation = ParsedInvocation {
            command: vec!["capabilities".into()],
            options: BTreeMap::from([("--raw".into(), Vec::new())]),
            positionals: Vec::new(),
        };
        let query = compose(
            &invocation,
            None,
            PositionEncoding::Utf16,
            &capabilities,
            None,
        )
        .unwrap();
        let output = execute(
            &mut TestDispatcher(DispatchResponse {
                result: Value::Null,
                partial_results: Vec::new(),
                trace: None,
                apply_edit_ledger: Vec::new(),
                server_progress: Vec::new(),
                synchronization: None,
            }),
            query,
            context(),
            &mut DiagnosticCache::new(1, 1024),
            &mut NoPreview,
        )
        .unwrap();
        assert_eq!(
            output["result"]["initializeResult"]["capabilities"],
            json!({})
        );

        let format = ParsedInvocation {
            command: vec!["format".into()],
            options: BTreeMap::from([("--options-json".into(), vec!["{".into()])]),
            positionals: Vec::new(),
        };
        let failure = compose(
            &format,
            Some(&document()),
            PositionEncoding::Utf16,
            &supported("formatting"),
            None,
        )
        .unwrap_err();
        assert_eq!(failure.code, "invalid_json_input");
        assert_eq!(failure.stage, "read_input");

        let execute = ParsedInvocation {
            command: vec!["execute-command".into()],
            options: BTreeMap::from([
                ("--command".into(), vec!["test.run".into()]),
                ("--arguments-json".into(), vec!["{}".into()]),
            ]),
            positionals: Vec::new(),
        };
        let failure = compose(
            &execute,
            None,
            PositionEncoding::Utf16,
            &supported_with_options("executeCommand", json!({"commands":["test.run"]})),
            None,
        )
        .unwrap_err();
        assert_eq!(failure.code, "invalid_arguments");
    }

    #[test]
    fn diagnostics_requests_reuse_cached_result_ids() {
        let document = document();
        let mut diagnostics = DiagnosticCache::new(4, 4096);
        diagnostics.apply_pull_report(
            &document.uri,
            json!({"kind":"full", "resultId":"old", "items":[]}),
        );
        let invocation = ParsedInvocation {
            command: vec!["document-diagnostics".into()],
            options: BTreeMap::new(),
            positionals: Vec::new(),
        };
        let request = compose(
            &invocation,
            Some(&document),
            PositionEncoding::Utf16,
            &supported("diagnosticProvider"),
            Some(&diagnostics),
        )
        .unwrap()
        .request
        .unwrap();
        assert_eq!(request.params.unwrap()["previousResultId"], "old");
    }

    #[test]
    fn raw_owner_methods_are_forbidden() {
        let invocation = ParsedInvocation {
            command: vec!["raw".into()],
            options: BTreeMap::from([("--method".into(), vec!["initialize".into()])]),
            positionals: Vec::new(),
        };
        assert_eq!(
            compose(
                &invocation,
                None,
                PositionEncoding::Utf16,
                &Capabilities::default(),
                None,
            )
            .unwrap_err()
            .code,
            "raw_method_forbidden"
        );

        let progress = ParsedInvocation {
            command: vec!["raw".into()],
            options: BTreeMap::from([("--method".into(), vec!["$/progress".into()])]),
            positionals: Vec::new(),
        };
        compose(
            &progress,
            None,
            PositionEncoding::Utf16,
            &Capabilities::default(),
            None,
        )
        .unwrap();
    }
}
