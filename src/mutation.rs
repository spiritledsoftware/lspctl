#![allow(clippy::result_large_err)]

mod application;
mod planner;
mod state;

use std::{
    collections::BTreeMap,
    env, fs,
    path::PathBuf,
    time::{Duration, Instant},
};

use serde_json::{Map, Value, json};
use similar::TextDiff;
use url::Url;

use crate::{
    canonical_value::digest_canonical_value,
    cli::ParsedInvocation,
    configuration::{
        AuthorizedServer, LoadedConfiguration, MutationSettings, PreviewSettings, ReceiptSettings,
        authorize_server, load_configuration, select_named_server,
    },
    contract::ContractFailure,
    query::PreviewProposal,
    workspace::PositionEncoding,
};

use application::{
    ApplicationContext, apply_preview, lock_workspace, preview_manifest_mismatches,
    reconcile_recovery_status, recover_accept_current, recover_rollback,
};
use planner::{
    CanonicalOperation, DocumentVersionPrecondition, PlannedWorkspaceEdit, WorkspaceEditPlanner,
};
use state::{MutationStateStore, PreviewRecord, ReceiptRecord, StoredPreview, TransactionState};

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct PreauthorizedUsage {
    pub(crate) entries: u64,
    pub(crate) rollback_bytes: u64,
    pub(crate) staged_text_bytes: u64,
}

pub(crate) fn dispatch_mutation_command(
    invocation: &ParsedInvocation,
) -> Result<Value, ContractFailure> {
    match invocation.command_path() {
        [group, command] if group == "preview" && command == "create" => preview_create(invocation),
        [group, command] if group == "preview" && command == "show" => preview_show(invocation),
        [group, command] if group == "preview" && command == "list" => preview_list(invocation),
        [group, command] if group == "preview" && command == "discard" => {
            preview_discard(invocation)
        }
        [command] if command == "apply" => apply(invocation),
        [group, command] if group == "recovery" && command == "status" => {
            recovery_status(invocation)
        }
        [group, command] if group == "recovery" && command == "rollback" => {
            recovery_action(invocation, false)
        }
        [group, command] if group == "recovery" && command == "accept-current" => {
            recovery_action(invocation, true)
        }
        [group, command] if group == "receipt" && command == "show" => receipt_show(invocation),
        [group, command] if group == "receipt" && command == "list" => receipt_list(invocation),
        [group, command] if group == "state" && command == "prune" => state_prune(invocation),
        _ => unreachable!("the CLI catalog limits Mutation command paths"),
    }
}

/// Holds the same Workspace serialization lock used by Mutation Application.
pub(crate) fn acquire_workspace_application_lock(
    workspace_uri: &str,
    timeout: &str,
) -> Result<std::fs::File, ContractFailure> {
    let store = MutationStateStore::open()?;
    let lock = store.open_application_lock(workspace_uri)?;
    lock_workspace(&lock, workspace_uri, timeout)?;
    Ok(lock)
}

fn preview_create(invocation: &ParsedInvocation) -> Result<Value, ContractFailure> {
    let workspace = invocation.option_path("--workspace").unwrap();
    let server_name = invocation.option_string("--server").unwrap();
    let supplied_session_identity = invocation.option_string("--session-identity").unwrap();
    let position_encoding_text = invocation.option_string("--position-encoding").unwrap();
    let position_encoding = parse_position_encoding(&position_encoding_text);
    let (source, edit, command) = mutation_proposal(invocation)?;

    let configuration =
        load_configuration(&workspace, invocation.has_option("--ignore-project-config"))?;
    let selected = select_named_server(&configuration, &server_name, invocation)?;
    let authorized = authorize_server(&configuration, selected)?;
    if authorized.session_identity != supplied_session_identity {
        return Err(ContractFailure {
            exit_code: 6,
            category: "mutation",
            code: "proposal_stale",
            message:
                "The supplied Session Identity does not match the effective authorized server."
                    .to_owned(),
            stage: "validate_mutation",
            delivery: "not_applicable",
            retry: "after_change",
            data: json!({
                "proposal": edit,
                "reasons": [{
                    "code": "server_declaration",
                    "message": "The effective Session Identity changed."
                }],
                "preconditions": []
            }),
        });
    }

    let preview = persist_preview(
        &configuration,
        &authorized,
        position_encoding,
        source,
        edit,
        command,
        &BTreeMap::new(),
    )?;
    if preview.is_null() {
        return Ok(json!({
            "schemaVersion": 1,
            "ok": true,
            "command": ["preview", "create"],
            "outcome": "unchanged",
            "result": null
        }));
    }
    Ok(json!({
        "schemaVersion": 1,
        "ok": true,
        "command": ["preview", "create"],
        "outcome": "previewed",
        "result": preview
    }))
}

/// Validates and persists the Workspace Edit produced by a live Query.
pub(crate) fn create_query_preview(
    configuration: &LoadedConfiguration,
    authorized: &AuthorizedServer,
    position_encoding: PositionEncoding,
    proposal: PreviewProposal,
) -> Result<Value, ContractFailure> {
    let document_version_preconditions =
        synchronized_document_version_preconditions(&proposal.context);
    let source = json!({
        "kind": "lsp_request",
        "command": proposal.command.name(),
        "method": proposal.method,
        "context": proposal.context,
        "resolvedAction": proposal.resolved_action,
        "trace": proposal.trace,
        "applyEditLedger": proposal.apply_edit_ledger,
    });
    persist_preview(
        configuration,
        authorized,
        position_encoding,
        source,
        proposal.edit,
        proposal.command_payload,
        &document_version_preconditions,
    )
}

fn synchronized_document_version_preconditions(
    context: &Value,
) -> BTreeMap<String, DocumentVersionPrecondition> {
    context
        .pointer("/synchronization/before")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|document| {
            Some((
                document.get("uri")?.as_str()?.to_owned(),
                DocumentVersionPrecondition {
                    version: document.get("version")?.as_i64()?,
                    digest: document.get("digest")?.as_str()?.to_owned(),
                },
            ))
        })
        .collect()
}

/// Plans and immediately applies one Workspace Edit preauthorized by an
/// `execute-command --apply-edits` invocation.
#[allow(clippy::too_many_arguments)]
pub(crate) fn apply_preauthorized_workspace_edit<F>(
    workspace_path: &std::path::Path,
    workspace_uri: &str,
    server: &str,
    session_identity: &str,
    position_encoding: PositionEncoding,
    label: Option<&str>,
    edit: Value,
    previews: &PreviewSettings,
    receipts: &ReceiptSettings,
    mutation: &MutationSettings,
    post_commit: &mut F,
) -> Result<(Value, PreauthorizedUsage), ContractFailure>
where
    F: FnMut(&[Value]) -> bool,
{
    let planner = WorkspaceEditPlanner::open(workspace_path, position_encoding, previews, mutation)
        .map_err(|problem| unsupported_filesystem(workspace_uri, &[problem]))?;
    let planned = planner
        .plan_workspace_edit(&edit)
        .map_err(|problems| invalid_workspace_edit(&edit, problems))?;
    let usage = preauthorized_usage(&planned);
    if planned.plan.operations.is_empty() {
        return Ok((
            json!({
                "schemaVersion": 1,
                "ok": true,
                "command": ["apply"],
                "outcome": "applied",
                "result": {
                    "state": "terminal",
                    "outcome": "applied",
                    "filesystemState": "unchanged",
                    "sessionSynchronized": true,
                    "cleanupPending": false,
                    "manifest": []
                }
            }),
            usage,
        ));
    }
    let current = planner
        .inspect_manifest(&planned.plan.before_manifest)
        .map_err(|problems| unsupported_filesystem(workspace_uri, &problems))?;
    let stale_reasons = preview_manifest_mismatches(&planned.plan, &current);
    if !stale_reasons.is_empty() {
        return Err(ContractFailure {
            exit_code: 6,
            category: "mutation",
            code: "proposal_stale",
            message: "The Workspace changed while the callback was being planned.".to_owned(),
            stage: "validate_mutation",
            delivery: "not_applicable",
            retry: "after_change",
            data: json!({
                "proposal": edit,
                "reasons": stale_reasons,
                "preconditions": planned.plan.before_manifest
            }),
        });
    }
    let store = MutationStateStore::open()?;
    let preview_id = store.new_preview_id()?;
    let recovery_manifest_digest = current_recovery_manifest(&store, workspace_uri)?;
    let record = create_preview_record(
        PreviewRecordContext {
            preview_id: &preview_id,
            workspace_uri,
            server: Some(server.to_owned()),
            session_identity,
            position_encoding: position_encoding.name(),
            source: json!({"kind": "workspace_apply_edit", "label": label}),
            edit,
            command: None,
        },
        planned,
    );
    store.create_preview(
        record,
        workspace_path.to_path_buf(),
        authorization_digest(session_identity, None),
        recovery_manifest_digest,
        previews,
    )?;
    let mut synchronize = |_: &state::ReceiptRecord, operations: &[Value]| post_commit(operations);
    let mut context = ApplicationContext {
        store: &store,
        preview_limits: previews,
        receipt_limits: receipts,
        mutation_limits: mutation,
        reauthorize: None,
        post_commit: Some(&mut synchronize),
        preauthorized: true,
        caller_deadline: None,
    };
    apply_preview(&mut context, &preview_id).map(|application| (application, usage))
}

fn preauthorized_usage(planned: &PlannedWorkspaceEdit) -> PreauthorizedUsage {
    let entries = planned
        .plan
        .before_manifest
        .iter()
        .chain(planned.plan.intended_manifest.iter())
        .map(|entry| &entry.path)
        .collect::<std::collections::BTreeSet<_>>()
        .len() as u64;
    let rollback_bytes = planned
        .plan
        .before_manifest
        .iter()
        .filter(|entry| entry.exists)
        .map(|entry| {
            fs::metadata(&entry.path)
                .ok()
                .filter(|metadata| metadata.is_file())
                .map(|metadata| metadata.len())
                .unwrap_or(0)
        })
        .sum();
    let staged_text_bytes = planned
        .plan
        .operations
        .iter()
        .filter_map(|operation| match operation {
            CanonicalOperation::Text { edits, .. } => Some(
                edits
                    .iter()
                    .map(|edit| edit.new_text.len() as u64)
                    .sum::<u64>(),
            ),
            _ => None,
        })
        .sum();
    PreauthorizedUsage {
        entries,
        rollback_bytes,
        staged_text_bytes,
    }
}

/// Validates and persists a server-initiated edit that was not preauthorized.
#[allow(clippy::too_many_arguments)]
pub(crate) fn create_callback_preview(
    workspace_path: &std::path::Path,
    workspace_uri: &str,
    server: &str,
    session_identity: &str,
    declaration_digest: Option<&str>,
    position_encoding: PositionEncoding,
    label: Option<&str>,
    edit: Value,
    previews: &PreviewSettings,
    mutation: &MutationSettings,
) -> Result<Value, ContractFailure> {
    let planner = WorkspaceEditPlanner::open(workspace_path, position_encoding, previews, mutation)
        .map_err(|problem| unsupported_filesystem(workspace_uri, &[problem]))?;
    let planned = planner
        .plan_workspace_edit(&edit)
        .map_err(|problems| invalid_workspace_edit(&edit, problems))?;
    if planned.plan.operations.is_empty() {
        return Ok(Value::Null);
    }
    let current = planner
        .inspect_manifest(&planned.plan.before_manifest)
        .map_err(|problems| unsupported_filesystem(workspace_uri, &problems))?;
    let stale_reasons = preview_manifest_mismatches(&planned.plan, &current);
    if !stale_reasons.is_empty() {
        return Err(ContractFailure {
            exit_code: 6,
            category: "mutation",
            code: "proposal_stale",
            message: "The Workspace changed while the callback was being planned.".to_owned(),
            stage: "validate_mutation",
            delivery: "not_applicable",
            retry: "after_change",
            data: json!({
                "proposal": edit,
                "reasons": stale_reasons,
                "preconditions": planned.plan.before_manifest
            }),
        });
    }
    let store = MutationStateStore::open()?;
    let preview_id = store.new_preview_id()?;
    let recovery_manifest_digest = current_recovery_manifest(&store, workspace_uri)?;
    let record = create_preview_record(
        PreviewRecordContext {
            preview_id: &preview_id,
            workspace_uri,
            server: Some(server.to_owned()),
            session_identity,
            position_encoding: position_encoding.name(),
            source: json!({"kind": "workspace_apply_edit", "label": label}),
            edit,
            command: None,
        },
        planned,
    );
    let mut stored = store.create_preview(
        record,
        workspace_path.to_path_buf(),
        authorization_digest(session_identity, declaration_digest),
        recovery_manifest_digest,
        previews,
    )?;
    refresh_preview_presentation(&mut stored, previews, mutation);
    serde_json::to_value(stored.preview).map_err(|error| ContractFailure {
        exit_code: 70,
        category: "internal",
        code: "internal_error",
        message: "The callback Preview cannot be serialized.".to_owned(),
        stage: "create_preview",
        delivery: "sent",
        retry: "after_change",
        data: json!({"reason": error.to_string()}),
    })
}

fn persist_preview(
    configuration: &LoadedConfiguration,
    authorized: &AuthorizedServer,
    position_encoding: PositionEncoding,
    source: Value,
    edit: Value,
    command: Option<Value>,
    document_version_preconditions: &BTreeMap<String, DocumentVersionPrecondition>,
) -> Result<Value, ContractFailure> {
    let planner = WorkspaceEditPlanner::open(
        &configuration.workspace,
        position_encoding,
        &configuration.previews,
        &configuration.mutation,
    )
    .map_err(|problem| unsupported_filesystem(&configuration.workspace_uri, &[problem]))?;
    let planned = planner
        .plan_workspace_edit_with_document_preconditions(&edit, document_version_preconditions)
        .map_err(|problems| invalid_workspace_edit(&edit, problems))?;
    if planned.plan.operations.is_empty() {
        return Ok(Value::Null);
    }
    let current = planner
        .inspect_manifest(&planned.plan.before_manifest)
        .map_err(|problems| unsupported_filesystem(&configuration.workspace_uri, &problems))?;
    let stale_reasons = preview_manifest_mismatches(&planned.plan, &current);
    if !stale_reasons.is_empty() {
        return Err(ContractFailure {
            exit_code: 6,
            category: "mutation",
            code: "proposal_stale",
            message: "The Workspace changed while the Preview was being planned.".to_owned(),
            stage: "validate_mutation",
            delivery: "not_applicable",
            retry: "after_change",
            data: json!({
                "proposal": edit,
                "reasons": stale_reasons,
                "preconditions": planned.plan.before_manifest
            }),
        });
    }

    let store = MutationStateStore::open()?;
    let preview_id = store.new_preview_id()?;
    let authorization_digest = authorization_digest(
        &authorized.session_identity,
        authorized.declaration_digest.as_deref(),
    );
    let recovery_manifest_digest = current_recovery_manifest(&store, &configuration.workspace_uri)?;
    let record = create_preview_record(
        PreviewRecordContext {
            preview_id: &preview_id,
            workspace_uri: &configuration.workspace_uri,
            server: Some(authorized.server.name.clone()),
            session_identity: &authorized.session_identity,
            position_encoding: position_encoding.name(),
            source,
            edit,
            command,
        },
        planned,
    );
    let mut stored = store.create_preview(
        record,
        configuration.workspace.clone(),
        authorization_digest,
        recovery_manifest_digest,
        &configuration.previews,
    )?;
    refresh_preview_presentation(
        &mut stored,
        &configuration.previews,
        &configuration.mutation,
    );
    serde_json::to_value(stored.preview).map_err(|error| ContractFailure {
        exit_code: 70,
        category: "internal",
        code: "internal_error",
        message: "The Preview result cannot be serialized.".to_owned(),
        stage: "create_preview",
        delivery: "sent",
        retry: "after_change",
        data: json!({"reason": error.to_string()}),
    })
}

fn preview_show(invocation: &ParsedInvocation) -> Result<Value, ContractFailure> {
    let id = invocation.positional_string(0).unwrap();
    let store = MutationStateStore::open()?;
    let mut stored = store.read_preview(&id)?;
    let (previews, mutation) = inspection_settings();
    refresh_preview_presentation(&mut stored, &previews, &mutation);
    Ok(json!({
        "schemaVersion": 1,
        "ok": true,
        "command": ["preview", "show"],
        "result": stored.preview
    }))
}

fn preview_list(invocation: &ParsedInvocation) -> Result<Value, ContractFailure> {
    let store = MutationStateStore::open()?;
    let workspace = workspace_filter(invocation.option_path("--workspace"))?;
    let server = invocation.option_string("--server");
    let (previews, mutation) = inspection_settings();
    let mut entries = Vec::new();
    for mut stored in store.list_previews()? {
        if workspace
            .as_ref()
            .is_some_and(|workspace| workspace != &stored.preview.workspace_uri)
            || server
                .as_ref()
                .is_some_and(|server| stored.preview.server.as_ref() != Some(server))
        {
            continue;
        }
        refresh_preview_presentation(&mut stored, &previews, &mutation);
        entries.push(preview_list_entry(&stored));
    }
    let (entries, page) = paginate(entries, invocation);
    Ok(json!({
        "schemaVersion": 1,
        "ok": true,
        "command": ["preview", "list"],
        "result": entries,
        "page": page
    }))
}

fn preview_discard(invocation: &ParsedInvocation) -> Result<Value, ContractFailure> {
    let id = invocation.positional_string(0).unwrap();
    MutationStateStore::open()?.discard_preview(&id)?;
    Ok(json!({
        "schemaVersion": 1,
        "ok": true,
        "command": ["preview", "discard"],
        "result": {"previewId": id, "discarded": true}
    }))
}

fn apply(invocation: &ParsedInvocation) -> Result<Value, ContractFailure> {
    let id = invocation.positional_string(0).unwrap();
    let caller_deadline = invocation
        .option_string("--deadline")
        .map(|value| Instant::now() + parse_duration(&value));
    let store = MutationStateStore::open()?;
    let application = if let Some(receipt) = store.already_applied(&id)?
        && receipt.receipt.outcome == "applied"
    {
        let (previews, receipts, mutation) = default_mutation_settings();
        let mut context = ApplicationContext {
            store: &store,
            preview_limits: &previews,
            receipt_limits: &receipts,
            mutation_limits: &mutation,
            reauthorize: None,
            post_commit: None,
            preauthorized: false,
            caller_deadline,
        };
        apply_preview(&mut context, &id)?
    } else {
        let preview = store.read_preview(&id)?;
        let configuration = load_configuration(
            &preview.workspace_path,
            invocation.has_option("--ignore-project-config"),
        )?;
        let reauthorize = |stored: &StoredPreview| reauthorize_preview(invocation, &store, stored);
        let mut synchronize = |receipt: &state::ReceiptRecord, operations: &[Value]| {
            let (_, failures) = crate::session::refresh_workspace_owners(
                &receipt.workspace_uri,
                receipt.server.as_deref(),
                receipt.session_identity.as_deref(),
                operations,
            );
            failures.is_empty()
        };
        let mut context = ApplicationContext {
            store: &store,
            preview_limits: &configuration.previews,
            receipt_limits: &configuration.receipts,
            mutation_limits: &configuration.mutation,
            reauthorize: Some(&reauthorize),
            post_commit: Some(&mut synchronize),
            preauthorized: false,
            caller_deadline,
        };
        apply_preview(&mut context, &id)?
    };
    synchronize_application_receipt(&store, application)
}

fn synchronize_application_receipt(
    store: &MutationStateStore,
    mut application: Value,
) -> Result<Value, ContractFailure> {
    if application
        .pointer("/result/sessionSynchronized")
        .and_then(Value::as_bool)
        == Some(true)
    {
        return Ok(application);
    }
    let Some(receipt_id) = application
        .pointer("/result/receiptId")
        .and_then(Value::as_str)
    else {
        return Ok(application);
    };
    let receipt = store.read_receipt(receipt_id)?;
    let (_, failures) = crate::session::refresh_workspace_owners(
        &receipt.receipt.workspace_uri,
        receipt.receipt.server.as_deref(),
        receipt.receipt.session_identity.as_deref(),
        &[],
    );
    if failures.is_empty() {
        store.mark_receipt_session_synchronized(receipt_id)?;
        application["result"]["sessionSynchronized"] = Value::Bool(true);
    } else {
        application["warnings"] = json!([{
            "code": "session_synchronization_failed",
            "message": "One or more live Owners could not refresh after the commit.",
            "data": {"ownerGenerations": failures}
        }]);
    }
    Ok(application)
}

fn recovery_status(invocation: &ParsedInvocation) -> Result<Value, ContractFailure> {
    let workspace_uri = match workspace_filter(invocation.option_path("--workspace"))? {
        Some(uri) => uri,
        None => workspace_filter(Some(
            env::current_dir().map_err(|error| user_path_failure(&error.to_string()))?,
        ))?
        .unwrap(),
    };
    let store = MutationStateStore::open()?;
    let (previews, _, mutation) = default_mutation_settings();
    let mut entries = Vec::new();
    for entry in store.list_transactions()? {
        match entry {
            Ok(transaction) if transaction.workspace_uri == workspace_uri => {
                let Some(transaction) =
                    reconcile_recovery_status(&store, transaction, &previews, &mutation)?
                else {
                    continue;
                };
                entries.push(json!({
                    "kind": "valid",
                    "transactionId": transaction.transaction_id,
                    "workspaceUri": transaction.workspace_uri,
                    "manifestDigest": transaction.manifest_digest,
                    "receiptId": transaction.receipt_id,
                    "filesystemState": match transaction.state {
                        TransactionState::RecoveryRequired => "partial",
                        TransactionState::CleanupPending => "changed",
                        TransactionState::Staged | TransactionState::Committing => "in_transition",
                    },
                    "cleanupPending": transaction.cleanup_pending,
                    "protected": true,
                    "intendedManifest": transaction.intended_manifest,
                    "observedManifest": transaction.observed_manifest
                }));
            }
            Ok(_) => {}
            Err(corrupt) => entries.push(corrupt),
        }
    }
    Ok(json!({
        "schemaVersion": 1,
        "ok": true,
        "command": ["recovery", "status"],
        "result": {
            "workspaceUri": workspace_uri,
            "required": !entries.is_empty(),
            "transactions": entries
        }
    }))
}

fn recovery_action(
    invocation: &ParsedInvocation,
    accept_current: bool,
) -> Result<Value, ContractFailure> {
    let transaction_id = invocation.positional_string(0).unwrap();
    let digest = invocation.option_string("--manifest-digest").unwrap();
    let store = MutationStateStore::open()?;
    if let Some(corrupt) = store.list_transactions()?.into_iter().find_map(Result::err) {
        return Err(ContractFailure {
            exit_code: 7,
            category: "recovery",
            code: "recovery_evidence_invalid",
            message: "Corrupt Recovery evidence prevents an automatic Recovery action.".to_owned(),
            stage: "recover",
            delivery: "not_applicable",
            retry: "never",
            data: json!({"transactionId": transaction_id, "problems": corrupt["problems"]}),
        });
    }
    let (previews, receipts, mutation) = default_mutation_settings();
    if accept_current {
        recover_accept_current(
            &store,
            &transaction_id,
            &digest,
            &previews,
            &receipts,
            &mutation,
        )
    } else {
        recover_rollback(
            &store,
            &transaction_id,
            &digest,
            &previews,
            &receipts,
            &mutation,
        )
    }
}

fn receipt_show(invocation: &ParsedInvocation) -> Result<Value, ContractFailure> {
    let id = invocation.positional_string(0).unwrap();
    let receipt = MutationStateStore::open()?.read_receipt(&id)?;
    Ok(json!({
        "schemaVersion": 1,
        "ok": true,
        "command": ["receipt", "show"],
        "result": receipt.receipt
    }))
}

fn receipt_list(invocation: &ParsedInvocation) -> Result<Value, ContractFailure> {
    let workspace = workspace_filter(invocation.option_path("--workspace"))?;
    let outcome = invocation.option_string("--outcome");
    let receipts = MutationStateStore::open()?
        .list_receipts()?
        .into_iter()
        .filter(|stored| {
            workspace
                .as_ref()
                .is_none_or(|workspace| workspace == &stored.receipt.workspace_uri)
                && outcome
                    .as_ref()
                    .is_none_or(|outcome| outcome == &stored.receipt.outcome)
        })
        .map(|stored| receipt_list_entry(&stored.receipt))
        .collect::<Vec<_>>();
    let (receipts, page) = paginate(receipts, invocation);
    Ok(json!({
        "schemaVersion": 1,
        "ok": true,
        "command": ["receipt", "list"],
        "result": receipts,
        "page": page
    }))
}

fn state_prune(invocation: &ParsedInvocation) -> Result<Value, ContractFailure> {
    let requested_ids = invocation.positionals_strings();
    let removed_ids = MutationStateStore::open()?.prune_state(&requested_ids)?;
    Ok(json!({
        "schemaVersion": 1,
        "ok": true,
        "command": ["state", "prune"],
        "result": {
            "requestedIds": requested_ids,
            "removedIds": removed_ids,
            "expiredOnly": requested_ids.is_empty()
        }
    }))
}

fn mutation_proposal(
    invocation: &ParsedInvocation,
) -> Result<(Value, Value, Option<Value>), ContractFailure> {
    if invocation.has_option("--code-action-json") || invocation.has_option("--code-action-file") {
        let action = read_json_option(
            invocation,
            "--code-action-json",
            "--code-action-file",
            "codeAction",
        )?;
        if let Some(disabled) = action.get("disabled").filter(|value| !value.is_null()) {
            let reason = disabled
                .get("reason")
                .or_else(|| disabled.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("The selected Code Action is disabled.");
            return Err(ContractFailure {
                exit_code: 6,
                category: "mutation",
                code: "disabled_code_action",
                message: "A disabled Code Action cannot be previewed.".to_owned(),
                stage: "validate_mutation",
                delivery: "not_applicable",
                retry: "never",
                data: json!({"reason": reason, "action": action}),
            });
        }
        let edit = action.get("edit").cloned().unwrap_or_else(|| json!({}));
        let command = action
            .get("command")
            .cloned()
            .filter(|value| !value.is_null());
        Ok((json!({"kind": "code_action"}), edit, command))
    } else {
        let edit = read_json_option(
            invocation,
            "--workspace-edit-json",
            "--workspace-edit-file",
            "workspaceEdit",
        )?;
        let command =
            if invocation.has_option("--command-json") || invocation.has_option("--command-file") {
                Some(read_json_option(
                    invocation,
                    "--command-json",
                    "--command-file",
                    "command",
                )?)
            } else {
                None
            };
        Ok((json!({"kind": "workspace_edit"}), edit, command))
    }
}

fn read_json_option(
    invocation: &ParsedInvocation,
    inline_option: &str,
    file_option: &str,
    source: &str,
) -> Result<Value, ContractFailure> {
    let (text, path) = if let Some(text) = invocation.option_string(inline_option) {
        (text, None)
    } else {
        let path = invocation.option_path(file_option).unwrap();
        let text = fs::read_to_string(&path).map_err(|error| ContractFailure {
            exit_code: 2,
            category: "input",
            code: "input_read_failed",
            message: "A JSON input file cannot be read.".to_owned(),
            stage: "read_input",
            delivery: "not_applicable",
            retry: "after_change",
            data: json!({"source": source, "path": path, "osCode": error.raw_os_error()}),
        })?;
        (text, Some(path))
    };
    serde_json::from_str(&text).map_err(|error| {
        let mut data = json!({"source": source, "line": error.line(), "column": error.column()});
        if let Some(path) = path {
            data["path"] = json!(path);
        }
        ContractFailure {
            exit_code: 2,
            category: "input",
            code: "invalid_json_input",
            message: "A JSON input is malformed.".to_owned(),
            stage: "read_input",
            delivery: "not_applicable",
            retry: "never",
            data,
        }
    })
}

fn reauthorize_preview(
    invocation: &ParsedInvocation,
    store: &MutationStateStore,
    stored: &StoredPreview,
) -> Result<Vec<Value>, ContractFailure> {
    let mut reasons = Vec::new();
    if let Some(workspace) = invocation.option_path("--workspace") {
        let supplied_uri = workspace_filter(Some(workspace))?.unwrap();
        if supplied_uri != stored.preview.workspace_uri {
            reasons
                .push(json!({"code": "workspace", "message": "The selected Workspace changed."}));
        }
    }
    if let Some(server) = invocation.option_string("--server")
        && stored.preview.server.as_ref() != Some(&server)
    {
        reasons
            .push(json!({"code": "server_declaration", "message": "The selected server changed."}));
    }
    let configuration = load_configuration(
        &stored.workspace_path,
        invocation.has_option("--ignore-project-config"),
    )?;
    if configuration.workspace_uri != stored.preview.workspace_uri {
        reasons.push(json!({"code": "workspace", "message": "The canonical Workspace changed."}));
    }
    let server_name = stored.preview.server.as_deref().ok_or_else(|| ContractFailure {
        exit_code: 6,
        category: "mutation",
        code: "preview_stale",
        message: "The Preview has no server authorization binding.".to_owned(),
        stage: "reserve",
        delivery: "not_applicable",
        retry: "after_change",
        data: json!({"previewId": stored.preview.preview_id, "reasons": [{"code": "authorization", "message": "The server binding is absent."}], "preconditions": stored.preview.preconditions}),
    })?;
    let selected = select_named_server(&configuration, server_name, invocation)?;
    let authorized = authorize_server(&configuration, selected)?;
    let digest = authorization_digest(
        &authorized.session_identity,
        authorized.declaration_digest.as_deref(),
    );
    if authorized.session_identity != stored.preview.session_identity {
        reasons.push(json!({"code": "server_declaration", "message": "The effective Session Identity changed."}));
    }
    if digest != stored.authorization_digest {
        reasons.push(json!({
            "code": "authorization",
            "message": "The effective authorization declaration changed.",
            "expectedDigest": stored.authorization_digest,
            "actualDigest": digest
        }));
    }
    let recovery_digest = current_recovery_manifest(store, &stored.preview.workspace_uri)?;
    if recovery_digest != stored.recovery_manifest_digest {
        let mut reason = Map::from_iter([
            ("code".to_owned(), json!("recovery_manifest")),
            (
                "message".to_owned(),
                json!("The Workspace Recovery manifest changed."),
            ),
        ]);
        if let Some(expected) = &stored.recovery_manifest_digest {
            reason.insert("expectedDigest".to_owned(), json!(expected));
        }
        if let Some(actual) = recovery_digest {
            reason.insert("actualDigest".to_owned(), json!(actual));
        }
        reasons.push(Value::Object(reason));
    }
    Ok(reasons)
}

fn authorization_digest(session_identity: &str, declaration_digest: Option<&str>) -> String {
    digest_canonical_value(
        "lspctl-mutation-authorization-v1",
        &json!({
            "sessionIdentity": session_identity,
            "declarationDigest": declaration_digest
        }),
    )
}

fn current_recovery_manifest(
    store: &MutationStateStore,
    workspace_uri: &str,
) -> Result<Option<String>, ContractFailure> {
    let mut digest = None;
    for entry in store.list_transactions()? {
        match entry {
            Ok(transaction) if transaction.workspace_uri == workspace_uri => {
                digest = Some(transaction.manifest_digest);
            }
            Ok(_) => {}
            Err(corrupt) => {
                return Err(ContractFailure {
                    exit_code: 7,
                    category: "recovery",
                    code: "recovery_evidence_invalid",
                    message: "Corrupt Recovery evidence blocks Mutation planning.".to_owned(),
                    stage: "recover",
                    delivery: "not_applicable",
                    retry: "never",
                    data: json!({"transactionId": "txn_00000000000000000000000000000000", "problems": corrupt["problems"]}),
                });
            }
        }
    }
    Ok(digest)
}

fn refresh_preview_presentation(
    stored: &mut StoredPreview,
    preview_settings: &PreviewSettings,
    mutation_settings: &MutationSettings,
) {
    let Ok(planner) = WorkspaceEditPlanner::open(
        &stored.workspace_path,
        parse_position_encoding(&stored.preview.position_encoding),
        preview_settings,
        mutation_settings,
    ) else {
        stored.preview.diff = None;
        return;
    };
    let Ok(current) = planner.inspect_manifest(&stored.preview.plan.before_manifest) else {
        stored.preview.diff = None;
        return;
    };
    stored.preview.stale_reasons = preview_manifest_mismatches(&stored.preview.plan, &current);
    stored.preview.diff = if stored.preview.stale_reasons.is_empty() {
        preview_diff(stored)
    } else {
        None
    };
}

fn preview_diff(stored: &StoredPreview) -> Option<String> {
    let mut output = String::new();
    for operation in &stored.preview.plan.operations {
        match operation {
            CanonicalOperation::Text { path, edits, .. } => {
                let bytes = fs::read(path).ok()?;
                let old = std::str::from_utf8(&bytes).ok()?;
                let mut new = String::new();
                let mut cursor = 0;
                for edit in edits {
                    let start = usize::try_from(edit.start_byte).ok()?;
                    let end = usize::try_from(edit.end_byte).ok()?;
                    new.push_str(old.get(cursor..start)?);
                    new.push_str(&edit.new_text);
                    cursor = end;
                }
                new.push_str(old.get(cursor..)?);
                output.push_str(&contextual_text_diff(path, old, &new));
            }
            CanonicalOperation::Create { path, .. } => {
                output.push_str(&format!("create {}\n", path.display()));
            }
            CanonicalOperation::Rename {
                old_path, new_path, ..
            } => {
                output.push_str(&format!(
                    "rename {} -> {}\n",
                    old_path.display(),
                    new_path.display()
                ));
            }
            CanonicalOperation::Delete { path, .. } => {
                output.push_str(&format!("delete {}\n", path.display()));
            }
        }
    }
    Some(output)
}

fn contextual_text_diff(path: &std::path::Path, old: &str, new: &str) -> String {
    let path = path.to_string_lossy();
    TextDiff::from_lines(old, new)
        .unified_diff()
        .context_radius(3)
        .header(&path, &path)
        .to_string()
}

fn preview_list_entry(stored: &StoredPreview) -> Value {
    let preview = &stored.preview;
    let mut entry = Map::from_iter([
        ("previewId".to_owned(), json!(preview.preview_id)),
        ("workspaceUri".to_owned(), json!(preview.workspace_uri)),
        ("expiresAt".to_owned(), json!(preview.expires_at)),
        ("summary".to_owned(), json!(preview.summary)),
        ("reserved".to_owned(), json!(preview.reserved)),
        ("stale".to_owned(), json!(!preview.stale_reasons.is_empty())),
    ]);
    if let Some(server) = &preview.server {
        entry.insert("server".to_owned(), json!(server));
    }
    Value::Object(entry)
}

fn receipt_list_entry(receipt: &ReceiptRecord) -> Value {
    let mut entry = Map::from_iter([
        ("receiptId".to_owned(), json!(receipt.receipt_id)),
        ("kind".to_owned(), json!(receipt.kind)),
        ("workspaceUri".to_owned(), json!(receipt.workspace_uri)),
        ("completedAt".to_owned(), json!(receipt.completed_at)),
        ("outcome".to_owned(), json!(receipt.outcome)),
        (
            "filesystemState".to_owned(),
            json!(receipt.filesystem_state),
        ),
        ("summary".to_owned(), json!(receipt.summary)),
        ("cleanupPending".to_owned(), json!(receipt.cleanup_pending)),
        ("transactionId".to_owned(), json!(receipt.transaction_id)),
    ]);
    for (name, value) in [
        ("server", receipt.server.as_ref()),
        ("previewId", receipt.preview_id.as_ref()),
        ("linkedReceiptId", receipt.linked_receipt_id.as_ref()),
    ] {
        if let Some(value) = value {
            entry.insert(name.to_owned(), json!(value));
        }
    }
    Value::Object(entry)
}

fn paginate<T>(mut records: Vec<T>, invocation: &ParsedInvocation) -> (Vec<T>, Value) {
    let offset = invocation
        .option_string("--offset")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    let limit = invocation
        .option_string("--limit")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(100);
    let total = records.len();
    let returned = records
        .drain(offset.min(total)..(offset.saturating_add(limit)).min(total))
        .collect::<Vec<_>>();
    let next = offset.saturating_add(returned.len());
    let complete = next >= total;
    let page = json!({
        "offset": offset,
        "limit": limit,
        "returned": returned.len(),
        "complete": complete,
        "nextOffset": if complete { None } else { Some(next) }
    });
    (returned, page)
}

fn workspace_filter(path: Option<PathBuf>) -> Result<Option<String>, ContractFailure> {
    let Some(path) = path else { return Ok(None) };
    let path = dunce::canonicalize(&path).map_err(|error| user_path_failure(&error.to_string()))?;
    Url::from_directory_path(path)
        .map(|uri| Some(uri.to_string()))
        .map_err(|()| user_path_failure("The Workspace path cannot be represented as a file URI."))
}

fn user_path_failure(reason: &str) -> ContractFailure {
    ContractFailure {
        exit_code: 4,
        category: "unavailable",
        code: "user_path_unavailable",
        message: "A requested local path is unavailable.".to_owned(),
        stage: "load_configuration",
        delivery: "not_applicable",
        retry: "after_change",
        data: json!({"kind": "workspace", "reason": reason}),
    }
}

fn invalid_workspace_edit(
    proposal: &Value,
    problems: Vec<planner::WorkspaceEditProblem>,
) -> ContractFailure {
    ContractFailure {
        exit_code: 6,
        category: "mutation",
        code: "invalid_workspace_edit",
        message: "The Workspace Edit cannot be represented as a safe canonical plan.".to_owned(),
        stage: "validate_mutation",
        delivery: "not_applicable",
        retry: "after_change",
        data: json!({"proposal": proposal, "problems": problems}),
    }
}

fn unsupported_filesystem(
    workspace_uri: &str,
    problems: &[planner::WorkspaceEditProblem],
) -> ContractFailure {
    ContractFailure {
        exit_code: 3,
        category: "blocked",
        code: "unsupported_filesystem",
        message: "The Workspace filesystem cannot provide required Mutation safety guarantees."
            .to_owned(),
        stage: "validate_mutation",
        delivery: "not_applicable",
        retry: "after_change",
        data: json!({
            "workspaceUri": workspace_uri,
            "missingCapabilities": problems.iter().map(|problem| &problem.code).collect::<Vec<_>>()
        }),
    }
}

fn parse_position_encoding(value: &str) -> PositionEncoding {
    if value == "utf-8" {
        PositionEncoding::Utf8
    } else {
        PositionEncoding::Utf16
    }
}

fn inspection_settings() -> (PreviewSettings, MutationSettings) {
    let (previews, _, mutation) = default_mutation_settings();
    (previews, mutation)
}

fn default_mutation_settings() -> (PreviewSettings, ReceiptSettings, MutationSettings) {
    (
        PreviewSettings {
            max_count: 64,
            max_total_bytes: 268_435_456,
            max_document_text_bytes: 16_777_216,
            max_text_bytes: 67_108_864,
        },
        ReceiptSettings { max_count: 1024 },
        MutationSettings {
            application_lock_timeout: "30s".to_owned(),
            max_entries: 10_000,
            max_recursion_depth: 64,
            max_rollback_bytes: 1_073_741_824,
            max_staged_text_bytes: 67_108_864,
            max_preauthorized_callbacks: 64,
        },
    )
}

fn parse_duration(value: &str) -> Duration {
    if let Some(value) = value.strip_suffix("ms") {
        Duration::from_millis(value.parse().unwrap())
    } else if let Some(value) = value.strip_suffix('s') {
        Duration::from_secs(value.parse().unwrap())
    } else {
        Duration::from_secs(value.strip_suffix('m').unwrap().parse::<u64>().unwrap() * 60)
    }
}

pub(crate) struct PreviewRecordContext<'a> {
    preview_id: &'a str,
    workspace_uri: &'a str,
    server: Option<String>,
    session_identity: &'a str,
    position_encoding: &'a str,
    source: Value,
    edit: Value,
    command: Option<Value>,
}

pub(crate) fn create_preview_record(
    context: PreviewRecordContext<'_>,
    planned: PlannedWorkspaceEdit,
) -> PreviewRecord {
    PreviewRecord {
        preview_id: context.preview_id.to_owned(),
        workspace_uri: context.workspace_uri.to_owned(),
        server: context.server,
        session_identity: context.session_identity.to_owned(),
        position_encoding: context.position_encoding.to_owned(),
        expires_at: String::new(),
        source: context.source,
        summary: planned.summary.clone(),
        edit: context.edit,
        command: context.command,
        annotations: planned.annotations,
        preconditions: planned.plan.before_manifest.clone(),
        plan: planned.plan,
        conflicts: Vec::new(),
        stale_reasons: Vec::new(),
        diff: None,
        reserved: false,
    }
}
