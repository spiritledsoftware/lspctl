#![allow(clippy::result_large_err)]

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

use atomic_write_file::AtomicWriteFile;
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, OpenOptions as CapabilityOpenOptions};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    canonical_value::{digest_canonical_value, digest_raw_bytes},
    configuration::{MutationSettings, PreviewSettings, ReceiptSettings},
    contract::ContractFailure,
    state_permissions,
    workspace::PositionEncoding,
};

use super::{
    planner::{
        CanonicalOperation, CanonicalPlan, ManifestEntry, ResourceKind, WorkspaceEditPlanner,
        WorkspaceEditProblem, missing_manifest, open_file_identity,
    },
    state::{
        BackupEntry, MUTATION_STATE_VERSION, MutationStateStore, ReceiptRecord, StoredPreview,
        TransactionRecord, TransactionState, manifest_digest, now_rfc3339,
    },
};

#[cfg(windows)]
use super::planner::windows_security_descriptor;

type ReauthorizePreview<'a> = dyn Fn(&StoredPreview) -> Result<Vec<Value>, ContractFailure> + 'a;
type PostCommit<'a> = dyn FnMut(&ReceiptRecord, &[Value]) -> bool + 'a;
const ARTIFACT_OWNER_FILE: &str = ".lspctl-transaction-owner";

pub(crate) struct ApplicationContext<'a> {
    pub(crate) store: &'a MutationStateStore,
    pub(crate) preview_limits: &'a PreviewSettings,
    pub(crate) receipt_limits: &'a ReceiptSettings,
    pub(crate) mutation_limits: &'a MutationSettings,
    pub(crate) reauthorize: Option<&'a ReauthorizePreview<'a>>,
    pub(crate) post_commit: Option<&'a mut PostCommit<'a>>,
    pub(crate) preauthorized: bool,
    pub(crate) caller_deadline: Option<Instant>,
}

/// Applies one exact Preview under the Workspace lock and records at-most-once completion.
pub(crate) fn apply_preview(
    context: &mut ApplicationContext<'_>,
    preview_id: &str,
) -> Result<Value, ContractFailure> {
    if let Some(receipt) = context.store.already_applied(preview_id)?
        && receipt.receipt.outcome == "applied"
    {
        return Ok(application_success(&receipt.receipt, "already_applied"));
    };
    let preliminary = match context.store.read_preview(preview_id) {
        Ok(preview) => preview,
        Err(failure) if failure.code == "preview_unknown" => {
            if let Some(receipt) = context.store.already_applied(preview_id)?
                && receipt.receipt.outcome == "applied"
            {
                return Ok(application_success(&receipt.receipt, "already_applied"));
            }
            return Err(failure);
        }
        Err(failure) => return Err(failure),
    };
    let lock_file = context
        .store
        .open_application_lock(&preliminary.preview.workspace_uri)?;
    lock_workspace_for_application(
        &lock_file,
        &preliminary.preview.workspace_uri,
        &context.mutation_limits.application_lock_timeout,
        context.caller_deadline,
        preview_id,
    )?;
    if let Some(receipt) = context.store.already_applied(preview_id)?
        && receipt.receipt.outcome == "applied"
    {
        return Ok(application_success(&receipt.receipt, "already_applied"));
    }
    let mut stored = context.store.reserve_preview(preview_id)?;
    if deadline_expired(context.caller_deadline) {
        let _ = context.store.release_preview(&mut stored);
        return Err(application_cancelled(preview_id));
    }
    if let Some(reauthorize) = context.reauthorize {
        let stale_reasons = match reauthorize(&stored) {
            Ok(reasons) => reasons,
            Err(failure) => {
                let _ = context.store.release_preview(&mut stored);
                return Err(failure);
            }
        };
        if !stale_reasons.is_empty() {
            let _ = context.store.release_preview(&mut stored);
            return Err(preview_stale_failure(&stored, stale_reasons));
        }
    }
    for transaction in context.store.list_transactions()? {
        match transaction {
            Ok(transaction) if transaction.workspace_uri == stored.preview.workspace_uri => {
                let _ = context.store.release_preview(&mut stored);
                return Err(recovery_required_failure(&transaction));
            }
            Ok(_) => {}
            Err(evidence) => {
                let _ = context.store.release_preview(&mut stored);
                return Err(ContractFailure {
                    exit_code: 7,
                    category: "recovery",
                    code: "recovery_evidence_invalid",
                    message: "Corrupt Recovery evidence blocks Workspace Application.".to_owned(),
                    stage: "recover",
                    delivery: "not_applicable",
                    retry: "never",
                    data: json!({
                        "transactionId": transaction_id_from_evidence(&evidence),
                        "problems": evidence["problems"]
                    }),
                });
            }
        }
    }
    context
        .store
        .ensure_receipt_capacity(context.receipt_limits)?;
    let planner = WorkspaceEditPlanner::open(
        &stored.workspace_path,
        parse_position_encoding(&stored.preview.position_encoding),
        context.preview_limits,
        context.mutation_limits,
    )
    .map_err(|problem| unsupported_filesystem_failure(&stored.preview.workspace_uri, &[problem]))?;
    let current = planner
        .inspect_manifest(&stored.preview.plan.before_manifest)
        .map_err(|problems| {
            unsupported_filesystem_failure(&stored.preview.workspace_uri, &problems)
        })?;
    let stale_reasons = preview_manifest_mismatches(&stored.preview.plan, &current);
    if !stale_reasons.is_empty() {
        let _ = context.store.release_preview(&mut stored);
        return Err(ContractFailure {
            exit_code: 6,
            category: "mutation",
            code: "preview_stale",
            message: "The Preview no longer matches the Workspace filesystem.".to_owned(),
            stage: "reserve",
            delivery: "not_applicable",
            retry: "after_change",
            data: json!({
                "previewId": preview_id,
                "reasons": stale_reasons,
                "preconditions": stored.preview.preconditions
            }),
        });
    }

    let transaction_id = context.store.new_transaction_id()?;
    let artifact_directory = stored
        .workspace_path
        .join(format!(".lspctl-{transaction_id}"));
    let mut transaction = TransactionRecord {
        format_version: MUTATION_STATE_VERSION,
        transaction_id: transaction_id.clone(),
        preview_id: preview_id.to_owned(),
        receipt_id: preview_id.to_owned(),
        workspace_path: stored.workspace_path.clone(),
        workspace_uri: stored.preview.workspace_uri.clone(),
        state: TransactionState::Staged,
        started_at: now_rfc3339(),
        artifact_directory: artifact_directory.clone(),
        backups: planned_backups(&stored.preview.plan.before_manifest, &artifact_directory),
        operations: stored.preview.plan.operations.clone(),
        before_manifest: stored.preview.plan.before_manifest.clone(),
        intended_manifest: stored.preview.plan.intended_manifest.clone(),
        observed_manifest: current,
        manifest_digest: manifest_digest(&stored.preview.plan.before_manifest),
        cleanup_pending: false,
    };
    context.store.write_transaction(&transaction)?;
    if let Err(stage_failure) = stage_transaction(
        &transaction,
        &stored.preview.plan.operations,
        context.mutation_limits,
    ) {
        if !stage_failure.artifact_created {
            let _ = context.store.remove_transaction(&transaction_id);
            let _ = context.store.release_preview(&mut stored);
            return Err(stage_failure.failure);
        }
        match cleanup_transaction_artifacts(&transaction) {
            Ok(()) => {
                let _ = context.store.remove_transaction(&transaction_id);
                let _ = context.store.release_preview(&mut stored);
                return Err(stage_failure.failure);
            }
            Err(_) => {
                transaction.state = TransactionState::RecoveryRequired;
                transaction.cleanup_pending = true;
                context.store.write_transaction(&transaction)?;
                return Err(recovery_required_failure(&transaction));
            }
        }
    }
    if deadline_expired(context.caller_deadline) {
        match cleanup_transaction_artifacts(&transaction) {
            Ok(()) => {
                let _ = context.store.remove_transaction(&transaction_id);
                let _ = context.store.release_preview(&mut stored);
                return Err(application_cancelled(preview_id));
            }
            Err(_) => {
                transaction.state = TransactionState::RecoveryRequired;
                transaction.cleanup_pending = true;
                context.store.write_transaction(&transaction)?;
                return Err(recovery_required_failure(&transaction));
            }
        }
    }
    transaction.state = TransactionState::Committing;
    context.store.write_transaction(&transaction)?;
    let started_at = transaction.started_at.clone();
    let commit_result = commit_operations(&planner, &transaction);
    let observed = planner
        .inspect_manifest(&stored.preview.plan.intended_manifest)
        .unwrap_or_default();
    let manifest_ok =
        manifest_mismatches(&stored.preview.plan.intended_manifest, &observed).is_empty();
    if commit_result.is_ok() && manifest_ok {
        transaction.observed_manifest.clone_from(&observed);
        transaction.state = TransactionState::CleanupPending;
        transaction.cleanup_pending = true;
        transaction.manifest_digest = manifest_digest(&observed);
        context.store.write_transaction(&transaction)?;
        let mut receipt = ReceiptRecord {
            receipt_id: preview_id.to_owned(),
            kind: "receipt".to_owned(),
            transaction_id: transaction_id.clone(),
            workspace_uri: stored.preview.workspace_uri.clone(),
            server: stored.preview.server.clone(),
            session_identity: Some(stored.preview.session_identity.clone()),
            preview_id: Some(preview_id.to_owned()),
            linked_receipt_id: None,
            preauthorized: context.preauthorized,
            started_at,
            completed_at: now_rfc3339(),
            outcome: "applied".to_owned(),
            filesystem_state: "changed".to_owned(),
            summary: stored.preview.summary.clone(),
            before_manifest: stored.preview.plan.before_manifest.clone(),
            intended_manifest: stored.preview.plan.intended_manifest.clone(),
            observed_manifest: observed.clone(),
            session_synchronized: false,
            cleanup_pending: true,
            durability: durability_value(),
            manifest_digest: manifest_digest(&observed),
            failure_stage: None,
            failed_change: None,
        };
        receipt = context
            .store
            .convert_preview_to_receipt(preview_id, receipt.clone(), context.receipt_limits)?
            .receipt;
        receipt.cleanup_pending = finish_terminal_cleanup(context.store, &transaction, preview_id);
        let file_operations = post_commit_file_operations(&stored);
        if context
            .post_commit
            .as_deref_mut()
            .is_some_and(|post_commit| post_commit(&receipt, &file_operations))
        {
            context
                .store
                .mark_receipt_session_synchronized(&receipt.receipt_id)?;
            receipt.session_synchronized = true;
        }
        return Ok(application_success(&receipt, "applied"));
    }

    transaction.observed_manifest = observed;
    match rollback_transaction(&transaction, &planner) {
        Ok(restored) => {
            transaction.state = TransactionState::CleanupPending;
            transaction.observed_manifest.clone_from(&restored);
            transaction.manifest_digest = manifest_digest(&restored);
            transaction.cleanup_pending = true;
            context.store.write_transaction(&transaction)?;
            let receipt = ReceiptRecord {
                receipt_id: preview_id.to_owned(),
                kind: "receipt".to_owned(),
                transaction_id: transaction_id.clone(),
                workspace_uri: stored.preview.workspace_uri.clone(),
                server: stored.preview.server.clone(),
                session_identity: Some(stored.preview.session_identity.clone()),
                preview_id: Some(preview_id.to_owned()),
                linked_receipt_id: None,
                preauthorized: context.preauthorized,
                started_at,
                completed_at: now_rfc3339(),
                outcome: "rolled_back".to_owned(),
                filesystem_state: "unchanged".to_owned(),
                summary: stored.preview.summary.clone(),
                before_manifest: stored.preview.plan.before_manifest.clone(),
                intended_manifest: stored.preview.plan.intended_manifest.clone(),
                observed_manifest: restored.clone(),
                session_synchronized: false,
                cleanup_pending: true,
                durability: durability_value(),
                manifest_digest: manifest_digest(&restored),
                failure_stage: Some("commit".to_owned()),
                failed_change: commit_result.err().map(|failure| failure.operation_index),
            };
            context.store.convert_preview_to_receipt(
                preview_id,
                receipt,
                context.receipt_limits,
            )?;
            let _ = finish_terminal_cleanup(context.store, &transaction, preview_id);
            Err(ContractFailure {
                exit_code: 6,
                category: "mutation",
                code: "rolled_back",
                message:
                    "The Application failed and the pre-Application filesystem state was restored."
                        .to_owned(),
                stage: "rollback",
                delivery: "not_applicable",
                retry: "after_change",
                data: json!({
                    "previewId": preview_id,
                    "receiptId": preview_id,
                    "transactionId": transaction_id,
                    "failureStage": "commit",
                    "manifest": restored
                }),
            })
        }
        Err(_) => {
            transaction.state = TransactionState::RecoveryRequired;
            transaction.observed_manifest = inspect_transaction_manifest(&planner, &transaction)
                .unwrap_or_else(|_| inspect_paths_best_effort(&transaction.before_manifest));
            transaction.manifest_digest = manifest_digest(&transaction.observed_manifest);
            context.store.write_transaction(&transaction)?;
            let receipt = ReceiptRecord {
                receipt_id: preview_id.to_owned(),
                kind: "receipt".to_owned(),
                transaction_id: transaction_id.clone(),
                workspace_uri: stored.preview.workspace_uri.clone(),
                server: stored.preview.server.clone(),
                session_identity: Some(stored.preview.session_identity.clone()),
                preview_id: Some(preview_id.to_owned()),
                linked_receipt_id: None,
                preauthorized: context.preauthorized,
                started_at,
                completed_at: now_rfc3339(),
                outcome: "recovery_required".to_owned(),
                filesystem_state: "partial".to_owned(),
                summary: stored.preview.summary.clone(),
                before_manifest: stored.preview.plan.before_manifest.clone(),
                intended_manifest: stored.preview.plan.intended_manifest.clone(),
                observed_manifest: transaction.observed_manifest.clone(),
                session_synchronized: false,
                cleanup_pending: true,
                durability: durability_value(),
                manifest_digest: transaction.manifest_digest.clone(),
                failure_stage: Some("rollback".to_owned()),
                failed_change: commit_result.err().map(|failure| failure.operation_index),
            };
            context.store.convert_preview_to_receipt(
                preview_id,
                receipt,
                context.receipt_limits,
            )?;
            Err(recovery_required_failure(&transaction))
        }
    }
}

fn finish_terminal_cleanup(
    store: &MutationStateStore,
    transaction: &TransactionRecord,
    preview_id: &str,
) -> bool {
    if cleanup_transaction_artifacts(transaction).is_err()
        || store.retire_preview_after_recovery(preview_id).is_err()
        || store
            .remove_transaction(&transaction.transaction_id)
            .is_err()
        || store
            .mark_receipt_cleanup_complete(&transaction.receipt_id)
            .is_err()
    {
        return true;
    }
    false
}

fn post_commit_file_operations(stored: &StoredPreview) -> Vec<Value> {
    stored
        .preview
        .plan
        .operations
        .iter()
        .filter_map(|operation| match operation {
            CanonicalOperation::Text { .. } => None,
            CanonicalOperation::Create { uri, path, .. } => Some(json!({
                "kind": "create",
                "uri": uri,
                "isDirectory": manifest_is_directory(&stored.preview.plan.intended_manifest, path)
            })),
            CanonicalOperation::Rename {
                old_uri,
                new_uri,
                new_path,
                ..
            } => Some(json!({
                "kind": "rename",
                "oldUri": old_uri,
                "newUri": new_uri,
                "isDirectory": manifest_is_directory(&stored.preview.plan.intended_manifest, new_path)
            })),
            CanonicalOperation::Delete { uri, path, .. } => Some(json!({
                "kind": "delete",
                "uri": uri,
                "isDirectory": manifest_is_directory(&stored.preview.plan.before_manifest, path)
            })),
        })
        .collect()
}

fn manifest_is_directory(manifest: &[ManifestEntry], path: &Path) -> bool {
    manifest
        .iter()
        .find(|entry| entry.path == path)
        .is_some_and(|entry| entry.resource_kind == ResourceKind::Directory)
}

/// Seals an abandoned staged or committing journal into exact Recovery evidence.
pub(crate) fn reconcile_recovery_status(
    store: &MutationStateStore,
    initial: TransactionRecord,
    preview_limits: &PreviewSettings,
    mutation_limits: &MutationSettings,
) -> Result<Option<TransactionRecord>, ContractFailure> {
    let lock = store.open_application_lock(&initial.workspace_uri)?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => return Ok(Some(initial)),
        Err(std::fs::TryLockError::Error(error)) => {
            return Err(ContractFailure {
                exit_code: 4,
                category: "unavailable",
                code: "state_unavailable",
                message: "The Workspace Application lock failed.".to_owned(),
                stage: "persist",
                delivery: "not_applicable",
                retry: "after_change",
                data: json!({
                    "recordType": "transaction",
                    "path": "workspace-application-lock",
                    "osCode": error.raw_os_error()
                }),
            });
        }
    }
    let mut transaction = match store.read_transaction(&initial.transaction_id) {
        Ok(transaction) => transaction,
        Err(failure) if failure.code == "recovery_not_found" => return Ok(None),
        Err(failure) => return Err(failure),
    };
    if !matches!(
        transaction.state,
        TransactionState::Staged | TransactionState::Committing
    ) {
        return Ok(Some(transaction));
    }
    let planner = WorkspaceEditPlanner::open(
        &transaction.workspace_path,
        PositionEncoding::Utf8,
        preview_limits,
        mutation_limits,
    )
    .map_err(|problem| unsupported_filesystem_failure(&transaction.workspace_uri, &[problem]))?;
    let current = inspect_transaction_manifest(&planner, &transaction).map_err(|problems| {
        unsupported_filesystem_failure(&transaction.workspace_uri, &problems)
    })?;
    let abandoned_commit = transaction.state == TransactionState::Committing
        || !manifest_mismatches(&transaction.before_manifest, &current).is_empty();
    transaction.observed_manifest = current;
    transaction.manifest_digest = manifest_digest(&transaction.observed_manifest);
    if abandoned_commit {
        transaction.state = TransactionState::RecoveryRequired;
        transaction.cleanup_pending = true;
    }
    store.write_transaction(&transaction)?;
    Ok(Some(transaction))
}

/// Rolls a recovery transaction back only while its observed manifest still matches.
pub(crate) fn recover_rollback(
    store: &MutationStateStore,
    transaction_id: &str,
    supplied_manifest_digest: &str,
    preview_limits: &PreviewSettings,
    receipt_limits: &ReceiptSettings,
    mutation_limits: &MutationSettings,
) -> Result<Value, ContractFailure> {
    recover_transaction(
        store,
        transaction_id,
        supplied_manifest_digest,
        false,
        preview_limits,
        receipt_limits,
        mutation_limits,
    )
}

/// Accepts the exact current recovery manifest without replaying filesystem writes.
pub(crate) fn recover_accept_current(
    store: &MutationStateStore,
    transaction_id: &str,
    supplied_manifest_digest: &str,
    preview_limits: &PreviewSettings,
    receipt_limits: &ReceiptSettings,
    mutation_limits: &MutationSettings,
) -> Result<Value, ContractFailure> {
    recover_transaction(
        store,
        transaction_id,
        supplied_manifest_digest,
        true,
        preview_limits,
        receipt_limits,
        mutation_limits,
    )
}

fn recover_transaction(
    store: &MutationStateStore,
    transaction_id: &str,
    supplied_manifest_digest: &str,
    accept_current: bool,
    preview_limits: &PreviewSettings,
    receipt_limits: &ReceiptSettings,
    mutation_limits: &MutationSettings,
) -> Result<Value, ContractFailure> {
    let initial = store.read_transaction(transaction_id)?;
    let lock = store.open_application_lock(&initial.workspace_uri)?;
    lock_workspace(
        &lock,
        &initial.workspace_uri,
        &mutation_limits.application_lock_timeout,
    )?;
    let mut transaction = store.read_transaction(transaction_id)?;
    if transaction.state == TransactionState::CleanupPending && !accept_current {
        return Err(ContractFailure {
            exit_code: 7,
            category: "recovery",
            code: "recovery_not_found",
            message: "A cleanup-only transaction cannot be rolled back.".to_owned(),
            stage: "recover",
            delivery: "not_applicable",
            retry: "never",
            data: json!({"transactionId": transaction_id}),
        });
    }
    if supplied_manifest_digest != transaction.manifest_digest {
        return Err(ContractFailure {
            exit_code: 7,
            category: "recovery",
            code: "recovery_manifest_mismatch",
            message: "The supplied Recovery manifest digest does not match the journal.".to_owned(),
            stage: "recover",
            delivery: "not_applicable",
            retry: "after_change",
            data: json!({
                "transactionId": transaction_id,
                "expectedDigest": transaction.manifest_digest,
                "actualDigest": supplied_manifest_digest
            }),
        });
    }
    store.ensure_receipt_capacity(receipt_limits)?;
    let planner = WorkspaceEditPlanner::open(
        &transaction.workspace_path,
        PositionEncoding::Utf8,
        preview_limits,
        mutation_limits,
    )
    .map_err(|problem| unsupported_filesystem_failure(&transaction.workspace_uri, &[problem]))?;
    let current = inspect_transaction_manifest(&planner, &transaction).map_err(|problems| {
        unsupported_filesystem_failure(&transaction.workspace_uri, &problems)
    })?;
    let current_digest = manifest_digest(&current);
    if current_digest != transaction.manifest_digest {
        return Err(ContractFailure {
            exit_code: 7,
            category: "recovery",
            code: "recovery_conflict",
            message: "The filesystem changed after Recovery was recorded.".to_owned(),
            stage: "recover",
            delivery: "not_applicable",
            retry: "after_change",
            data: json!({
                "transactionId": transaction_id,
                "paths": current.iter().map(|entry| &entry.path).collect::<Vec<_>>(),
                "intended": transaction.observed_manifest,
                "observed": current
            }),
        });
    }
    let outcome = if accept_current {
        "accepted_current"
    } else if transaction.state == TransactionState::Staged
        && manifest_mismatches(&transaction.before_manifest, &current).is_empty()
    {
        "restored"
    } else {
        rollback_transaction(&transaction, &planner).map_err(|failure| ContractFailure {
            exit_code: 7,
            category: "recovery",
            code: "recovery_failed",
            message: "Recovery rollback could not restore the recorded filesystem state."
                .to_owned(),
            stage: "recover",
            delivery: "not_applicable",
            retry: "after_change",
            data: json!({
                "transactionId": transaction_id,
                "failureStage": failure,
                "intended": transaction.before_manifest,
                "observed": inspect_paths_best_effort(&transaction.before_manifest)
            }),
        })?;
        "restored"
    };
    let final_manifest = if accept_current {
        current
    } else {
        inspect_transaction_manifest(&planner, &transaction)
            .unwrap_or_else(|_| inspect_paths_best_effort(&transaction.before_manifest))
    };
    let recovery_receipt_id = store.new_receipt_id()?;
    let original = store.read_receipt(&transaction.receipt_id).ok();
    let pending_preview = store.read_preview(&transaction.preview_id).ok();
    let receipt = ReceiptRecord {
        receipt_id: recovery_receipt_id.clone(),
        kind: "recovery_receipt".to_owned(),
        transaction_id: transaction_id.to_owned(),
        workspace_uri: transaction.workspace_uri.clone(),
        server: original
            .as_ref()
            .and_then(|record| record.receipt.server.clone())
            .or_else(|| pending_preview.as_ref()?.preview.server.clone()),
        session_identity: original
            .as_ref()
            .and_then(|record| record.receipt.session_identity.clone())
            .or_else(|| Some(pending_preview.as_ref()?.preview.session_identity.clone())),
        preview_id: original
            .as_ref()
            .and_then(|record| record.receipt.preview_id.clone())
            .or_else(|| Some(transaction.preview_id.clone())),
        linked_receipt_id: original
            .as_ref()
            .map(|record| record.receipt.receipt_id.clone()),
        preauthorized: false,
        started_at: now_rfc3339(),
        completed_at: now_rfc3339(),
        outcome: outcome.to_owned(),
        filesystem_state: if accept_current {
            "changed"
        } else {
            "unchanged"
        }
        .to_owned(),
        summary: original
            .as_ref()
            .map(|record| record.receipt.summary.clone())
            .or_else(|| {
                pending_preview
                    .as_ref()
                    .map(|record| record.preview.summary.clone())
            })
            .unwrap_or_default(),
        before_manifest: transaction.observed_manifest.clone(),
        intended_manifest: final_manifest.clone(),
        observed_manifest: final_manifest.clone(),
        session_synchronized: false,
        cleanup_pending: false,
        durability: durability_value(),
        manifest_digest: manifest_digest(&final_manifest),
        failure_stage: None,
        failed_change: None,
    };
    store.write_receipt(receipt, receipt_limits)?;
    store.retire_preview_after_recovery(&transaction.preview_id)?;
    cleanup_transaction_artifacts(&transaction).map_err(|error| ContractFailure {
        exit_code: 7,
        category: "recovery",
        code: "recovery_failed",
        message: "Recovery completed but its protected artifacts could not be cleaned up."
            .to_owned(),
        stage: "recover",
        delivery: "not_applicable",
        retry: "after_change",
        data: json!({"transactionId": transaction_id, "failureStage": error}),
    })?;
    store.remove_transaction(transaction_id)?;
    transaction.cleanup_pending = false;
    Ok(json!({
        "schemaVersion": 1,
        "ok": true,
        "command": ["recovery", if accept_current { "accept-current" } else { "rollback" }],
        "outcome": outcome,
        "result": {
            "transactionId": transaction_id,
            "recoveryReceiptId": recovery_receipt_id,
            "filesystemState": if accept_current { "changed" } else { "unchanged" },
            "manifestDigest": manifest_digest(&final_manifest),
            "cleanupPending": false
        }
    }))
}

const PROGRESS_FILE: &str = "commit-progress.json";

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CommitProgress {
    version: u32,
    transaction_id: String,
    operations_digest: String,
    before_digest: String,
    effects: Vec<CommitEffect>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CommitEffect {
    index: u64,
    before: Vec<ManifestEntry>,
    // None is durable pending intent, never evidence that no effect occurred.
    after: Option<Vec<ManifestEntry>>,
}

impl CommitProgress {
    fn new(transaction: &TransactionRecord) -> Self {
        Self {
            version: 1,
            transaction_id: transaction.transaction_id.clone(),
            operations_digest: digest_canonical_value(
                "lspctl-commit-operations-v1",
                &json!(transaction.operations),
            ),
            before_digest: manifest_digest(&transaction.before_manifest),
            effects: Vec::new(),
        }
    }
}

fn operation_index(operation: &CanonicalOperation) -> u64 {
    match operation {
        CanonicalOperation::Text { index, .. }
        | CanonicalOperation::Create { index, .. }
        | CanonicalOperation::Rename { index, .. }
        | CanonicalOperation::Delete { index, .. } => *index,
    }
}

fn initial_progress_manifest(transaction: &TransactionRecord) -> Vec<ManifestEntry> {
    let mut entries = transaction
        .intended_manifest
        .iter()
        .map(|entry| (entry.path.clone(), missing_manifest(&entry.path)))
        .collect::<BTreeMap<_, _>>();
    entries.extend(
        transaction
            .before_manifest
            .iter()
            .map(|entry| (entry.path.clone(), entry.clone())),
    );
    let mut entries = entries.into_values().collect::<Vec<_>>();
    // Private commit evidence may certify the already enumerated legacy tree without changing v1 vocabulary.
    for entry in &mut entries {
        if entry.resource_kind == ResourceKind::Directory
            && transaction
                .operations
                .iter()
                .any(|operation| match operation {
                    CanonicalOperation::Rename {
                        old_path,
                        new_path,
                        overwrite,
                        ..
                    } => {
                        entry.path.starts_with(old_path)
                            || (*overwrite && entry.path.starts_with(new_path))
                    }
                    CanonicalOperation::Delete { path, .. } => entry.path.starts_with(path),
                    _ => false,
                })
        {
            entry.content_digest = Some(String::new());
        }
    }
    refresh_membership(&mut entries);
    entries
}

fn translated_path(path: &Path, from: &Path, to: &Path) -> PathBuf {
    let suffix = path.strip_prefix(from).unwrap();
    if suffix.as_os_str().is_empty() {
        to.to_path_buf()
    } else {
        to.join(suffix)
    }
}

fn refresh_membership(entries: &mut [ManifestEntry]) {
    for index in 0..entries.len() {
        if entries[index].resource_kind != ResourceKind::Directory
            || entries[index].content_digest.is_none()
        {
            continue;
        }
        let children = entries
            .iter()
            .filter(|entry| {
                entry.exists && entry.path.parent() == Some(entries[index].path.as_path())
            })
            .map(|entry| {
                (
                    entry.path.file_name().unwrap().to_str().unwrap().to_owned(),
                    entry.resource_kind,
                )
            })
            .collect::<BTreeMap<_, _>>();
        entries[index].content_digest = Some(digest_canonical_value(
            "lspctl-directory-membership-v1",
            &json!(children),
        ));
    }
}

fn add_undo_manifest(
    entries: &mut Vec<ManifestEntry>,
    transaction: &TransactionRecord,
    operation: &CanonicalOperation,
) {
    let path = match operation {
        CanonicalOperation::Rename { new_path, .. } => new_path,
        CanonicalOperation::Delete { path, .. } => path,
        _ => return,
    };
    let undo = undo_resource_path(&transaction.artifact_directory, operation_index(operation));
    let missing = entries
        .iter()
        .filter(|entry| entry.path.starts_with(path))
        .map(|entry| missing_manifest(&translated_path(&entry.path, path, &undo)))
        .collect::<Vec<_>>();
    entries.extend(missing);
    entries.sort_by(|a, b| a.path.cmp(&b.path));
}

fn require_manifest(
    planner: &WorkspaceEditPlanner<'_>,
    expected: &[ManifestEntry],
) -> Result<(), String> {
    let actual = planner
        .inspect_manifest(expected)
        .map_err(|_| "Progress resources cannot be safely inspected.".to_owned())?;
    if actual != expected {
        return Err("Resources differ from the proven Application state.".to_owned());
    }
    Ok(())
}

fn open_owned_artifacts(
    transaction: &TransactionRecord,
    planner: &WorkspaceEditPlanner<'_>,
) -> Result<Dir, String> {
    if transaction.artifact_directory
        != transaction
            .workspace_path
            .join(format!(".lspctl-{}", transaction.transaction_id))
    {
        return Err("Invalid transaction artifact location.".to_owned());
    }
    let dir = planner
        .capability_root()
        .open_dir_nofollow(planner.relative_path(&transaction.artifact_directory)?)
        .map_err(|e| e.to_string())?;
    let mut options = CapabilityOpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    let mut owner = dir
        .open_with(ARTIFACT_OWNER_FILE, &options)
        .map_err(|e| e.to_string())?;
    if !owner.metadata().map_err(|e| e.to_string())?.is_file() {
        return Err("Invalid artifact owner marker.".to_owned());
    }
    let mut marker = String::new();
    Read::by_ref(&mut owner)
        .take(transaction.transaction_id.len() as u64 + 1)
        .read_to_string(&mut marker)
        .map_err(|e| e.to_string())?;
    if marker != transaction.transaction_id {
        return Err("Transaction artifact ownership is unavailable.".to_owned());
    }
    Ok(dir)
}

fn progress_size_limit(transaction: &TransactionRecord) -> Result<u64, String> {
    // ponytail: full snapshots cost O(entries * operations²) with undo trees; use deltas if large plans need it.
    // Each operation can add one translated undo tree, bounded by the already limited plan.
    let bytes = serde_json::to_vec(transaction)
        .map_err(|e| e.to_string())?
        .len() as u64;
    let count = transaction.operations.len() as u64 + 1;
    bytes
        .checked_mul(count)
        .and_then(|n| n.checked_mul(count))
        .and_then(|n| n.checked_mul(4))
        .ok_or_else(|| "Commit evidence size bound overflowed.".to_owned())
}

fn write_progress(
    transaction: &TransactionRecord,
    planner: &WorkspaceEditPlanner<'_>,
    progress: &CommitProgress,
) -> Result<(), String> {
    let _dir = open_owned_artifacts(transaction, planner)?;
    let bytes = serde_json::to_vec(progress).map_err(|e| e.to_string())?;
    if bytes.len() as u64 > progress_size_limit(transaction)? {
        return Err("Commit evidence exceeds its size bound.".to_owned());
    }
    let path = transaction.artifact_directory.join(PROGRESS_FILE);
    let mut output = AtomicWriteFile::open(&path).map_err(|e| e.to_string())?;
    output.write_all(&bytes).map_err(|e| e.to_string())?;
    output.commit().map_err(|e| e.to_string())?;
    state_permissions::restrict_file(&path).map_err(|e| e.to_string())?;
    flush_directory(&transaction.artifact_directory).map_err(|e| e.to_string())
}

fn read_progress(
    transaction: &TransactionRecord,
    planner: &WorkspaceEditPlanner<'_>,
) -> Result<CommitProgress, String> {
    let dir = open_owned_artifacts(transaction, planner)?;
    let mut options = CapabilityOpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    let mut file = dir
        .open_with(PROGRESS_FILE, &options)
        .map_err(|e| e.to_string())?;
    let limit = progress_size_limit(transaction)?;
    let metadata = file.metadata().map_err(|e| e.to_string())?;
    if !metadata.is_file() || metadata.len() > limit {
        return Err("Invalid commit evidence size or kind.".to_owned());
    }
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > limit {
        return Err("Commit evidence exceeds its size bound.".to_owned());
    }
    let progress: CommitProgress = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    let binding = CommitProgress::new(transaction);
    if progress.version != binding.version
        || progress.transaction_id != binding.transaction_id
        || progress.operations_digest != binding.operations_digest
        || progress.before_digest != binding.before_digest
        || progress.effects.len() > transaction.operations.len()
    {
        return Err("Commit evidence does not bind this transaction.".to_owned());
    }
    let mut expected = initial_progress_manifest(transaction);
    for (position, effect) in progress.effects.iter().enumerate() {
        let operation = &transaction.operations[position];
        add_undo_manifest(&mut expected, transaction, operation);
        if effect.index != operation_index(operation) || effect.before != expected {
            return Err("Commit evidence has invalid operation order or preconditions.".to_owned());
        }
        if let Some(after) = &effect.after {
            let identity = match operation {
                CanonicalOperation::Text { path, .. } | CanonicalOperation::Create { path, .. } => {
                    manifest_for_path(after, path)?.identity_digest.as_deref()
                }
                _ => None,
            };
            expected = operation_after(transaction, operation, &expected, after, identity)?;
            if &expected != after {
                return Err("Commit evidence has an impossible result.".to_owned());
            }
        } else if position + 1 != progress.effects.len() {
            return Err("Commit evidence advances past an uncertain operation.".to_owned());
        }
    }
    Ok(progress)
}

struct MoveHandle {
    file: File,
    identity: String,
    #[cfg(windows)]
    security_descriptor: Vec<u8>,
    #[cfg(windows)]
    created: std::time::SystemTime,
}

fn retain_effect_handles(
    planner: &WorkspaceEditPlanner<'_>,
    operation: &CanonicalOperation,
    before: &[ManifestEntry],
) -> Result<Vec<MoveHandle>, String> {
    let paths = match operation {
        CanonicalOperation::Rename {
            old_path, new_path, ..
        } => vec![old_path, new_path],
        CanonicalOperation::Delete { path, .. } => vec![path],
        _ => vec![],
    };
    paths
        .into_iter()
        .filter(|path| manifest_for_path(before, path).is_ok_and(|entry| entry.exists))
        .map(|path| retain_move_handle(planner, manifest_for_path(before, path)?))
        .collect()
}

fn retain_move_handle(
    planner: &WorkspaceEditPlanner<'_>,
    expected: &ManifestEntry,
) -> Result<MoveHandle, String> {
    let mut options = CapabilityOpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    #[cfg(windows)]
    {
        use cap_std::fs::OpenOptionsExt;
        use windows_sys::Win32::{
            Foundation::GENERIC_READ,
            Storage::FileSystem::{FILE_FLAG_BACKUP_SEMANTICS, FILE_WRITE_ATTRIBUTES, WRITE_DAC},
        };
        // Keep share-delete enabled while retaining the right to restore the moved root's DACL.
        options
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .access_mode(GENERIC_READ | WRITE_DAC | FILE_WRITE_ATTRIBUTES);
    }
    let file = planner
        .capability_root()
        .open_with(planner.relative_path(&expected.path)?, &options)
        .map_err(|e| e.to_string())?
        .into_std();
    let identity = open_file_identity(&file).map_err(|e| e.to_string())?;
    if expected.identity_digest.as_deref() != Some(&identity) {
        return Err("Resource handle differs from commit preconditions.".to_owned());
    }
    Ok(MoveHandle {
        identity,
        #[cfg(windows)]
        security_descriptor: move_security_descriptor(&file).map_err(|e| e.to_string())?,
        #[cfg(windows)]
        created: file
            .metadata()
            .and_then(|metadata| metadata.created())
            .map_err(|e| e.to_string())?,
        file,
    })
}

#[cfg(windows)]
fn move_security_descriptor(file: &File) -> std::io::Result<Vec<u8>> {
    use std::{os::windows::io::AsRawHandle, ptr, slice};
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::{
            Authorization::{GetSecurityInfo, SE_FILE_OBJECT},
            DACL_SECURITY_INFORMATION, GROUP_SECURITY_INFORMATION, GetSecurityDescriptorLength,
            OWNER_SECURITY_INFORMATION,
        },
    };
    let mut descriptor = ptr::null_mut();
    let status = unsafe {
        GetSecurityInfo(
            file.as_raw_handle(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | GROUP_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            &mut descriptor,
        )
    };
    if status != 0 {
        return Err(std::io::Error::from_raw_os_error(status as i32));
    }
    let length = unsafe { GetSecurityDescriptorLength(descriptor) } as usize;
    let bytes = if length == 0 {
        Err(std::io::Error::other(
            "The move security descriptor is empty.",
        ))
    } else {
        Ok(unsafe { slice::from_raw_parts(descriptor.cast::<u8>(), length) }.to_vec())
    };
    unsafe {
        LocalFree(descriptor);
    }
    bytes
}

#[cfg(windows)]
fn without_inherited_ace_flags(mut descriptor: Vec<u8>) -> std::io::Result<Vec<u8>> {
    use std::ptr;
    use windows_sys::Win32::Security::{
        ACE_HEADER, GetAce, GetSecurityDescriptorDacl, INHERITED_ACE,
    };
    let mut present = 0;
    let mut defaulted = 0;
    let mut dacl = ptr::null_mut();
    if unsafe {
        GetSecurityDescriptorDacl(
            descriptor.as_mut_ptr().cast(),
            &mut present,
            &mut dacl,
            &mut defaulted,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error());
    }
    if present != 0 && !dacl.is_null() {
        for index in 0..u32::from(unsafe { (*dacl).AceCount }) {
            let mut ace = ptr::null_mut();
            if unsafe { GetAce(dacl, index, &mut ace) } == 0 {
                return Err(std::io::Error::last_os_error());
            }
            // Descriptors come directly from GetSecurityInfo, and GetAce locates each header.
            unsafe {
                (*ace.cast::<ACE_HEADER>()).AceFlags &= !(INHERITED_ACE as u8);
            }
        }
    }
    Ok(descriptor)
}

#[cfg(windows)]
fn preserve_move_metadata(handle: &MoveHandle) -> std::io::Result<()> {
    use std::os::windows::{fs::FileTimesExt, io::AsRawHandle};
    use windows_sys::Win32::Security::{DACL_SECURITY_INFORMATION, SetKernelObjectSecurity};
    let current = move_security_descriptor(&handle.file)?;
    let changed = current != handle.security_descriptor;
    if changed
        && without_inherited_ace_flags(current)?
            != without_inherited_ace_flags(handle.security_descriptor.clone())?
    {
        return Err(std::io::Error::other(
            "The moved security descriptor changed externally.",
        ));
    }
    // Narrow legacy API exception: SetSecurityInfo recomputes inheritance on filesystem objects.
    // Only the proven INHERITED_ACE-bit rewrite may be undone, through this retained handle,
    // without propagating the root's ACL to children or changing any other descriptor bytes.
    if changed
        && unsafe {
            SetKernelObjectSecurity(
                handle.file.as_raw_handle(),
                DACL_SECURITY_INFORMATION,
                handle.security_descriptor.as_ptr().cast_mut().cast(),
            )
        } == 0
    {
        return Err(std::io::Error::last_os_error());
    }
    if move_security_descriptor(&handle.file)? != handle.security_descriptor {
        return Err(std::io::Error::other(
            "The moved security descriptor could not be preserved.",
        ));
    }
    // NTFS tunneling can copy the vacated destination's creation time onto a renamed file.
    // Restore the time captured from the moved handle only after rejecting unrelated ACL changes.
    handle
        .file
        .set_times(fs::FileTimes::new().set_created(handle.created))
}

fn operation_after(
    transaction: &TransactionRecord,
    operation: &CanonicalOperation,
    before: &[ManifestEntry],
    observed: &[ManifestEntry],
    handle_identity: Option<&str>,
) -> Result<Vec<ManifestEntry>, String> {
    let mut after = before
        .iter()
        .map(|entry| (entry.path.clone(), entry.clone()))
        .collect::<BTreeMap<_, _>>();
    match operation {
        CanonicalOperation::Text {
            path,
            before_digest,
            after_digest,
            ..
        } => {
            let entry = after.get_mut(path).ok_or("Missing text precondition.")?;
            if entry.content_digest.as_ref() != Some(before_digest)
                || entry.identity_digest.as_deref() != handle_identity
            {
                return Err(
                    "Text handle or before content does not match the canonical operation."
                        .to_owned(),
                );
            }
            entry.content_digest = Some(after_digest.clone());
        }
        CanonicalOperation::Create {
            path,
            overwrite,
            ignore_if_exists,
            ..
        } => {
            let entry = after.get_mut(path).ok_or("Missing create precondition.")?;
            if entry.exists && !overwrite {
                if !ignore_if_exists {
                    return Err("Unexpected occupied create target.".to_owned());
                }
            } else if entry.exists {
                if entry.identity_digest.as_deref() != handle_identity {
                    return Err("Overwrite handle changed.".to_owned());
                }
                entry.content_digest = Some(digest_raw_bytes(b""));
            } else {
                let created = manifest_for_path(observed, path)?;
                if created.resource_kind != ResourceKind::File
                    || created.identity_digest.is_none()
                    || created.identity_digest.as_deref() != handle_identity
                    || created.content_digest != Some(digest_raw_bytes(b""))
                {
                    return Err(
                        "Created resource does not match its handle and canonical empty content."
                            .to_owned(),
                    );
                }
                *entry = created.clone();
            }
        }
        CanonicalOperation::Rename {
            index,
            old_path,
            new_path,
            overwrite,
            ignore_if_exists,
            ..
        } => {
            if after.get(new_path).is_some_and(|entry| entry.exists) {
                if *overwrite {
                    transfer_manifest(
                        &mut after,
                        new_path,
                        &undo_resource_path(&transaction.artifact_directory, *index),
                    )?;
                } else if *ignore_if_exists {
                    return Ok(before.to_vec());
                } else {
                    return Err("Unexpected occupied rename destination.".to_owned());
                }
            }
            transfer_manifest(&mut after, old_path, new_path)?;
        }
        CanonicalOperation::Delete {
            index,
            path,
            ignore_if_not_exists,
            ..
        } => {
            if after.get(path).is_some_and(|entry| entry.exists) {
                transfer_manifest(
                    &mut after,
                    path,
                    &undo_resource_path(&transaction.artifact_directory, *index),
                )?;
            } else if !ignore_if_not_exists {
                return Err("Unexpected missing delete target.".to_owned());
            }
        }
    }
    let mut after = after.into_values().collect::<Vec<_>>();
    refresh_membership(&mut after);
    Ok(after)
}

fn transfer_manifest(
    entries: &mut BTreeMap<PathBuf, ManifestEntry>,
    from: &Path,
    to: &Path,
) -> Result<(), String> {
    let tree = entries
        .values()
        .filter(|entry| entry.exists && entry.path.starts_with(from))
        .cloned()
        .collect::<Vec<_>>();
    if tree.is_empty() {
        return Err("Missing transferred resource.".to_owned());
    }
    for entry in tree {
        if entry.resource_kind == ResourceKind::Directory && entry.content_digest.is_none() {
            return Err("Directory transfer lacks a membership certificate.".to_owned());
        }
        entries.insert(entry.path.clone(), missing_manifest(&entry.path));
        let path = translated_path(&entry.path, from, to);
        let mut moved = entry;
        moved.path = path.clone();
        entries.insert(path, moved);
    }
    Ok(())
}

#[derive(Debug)]
struct CommitFailure {
    operation_index: u64,
    _reason: String,
}

fn commit_operations(
    planner: &WorkspaceEditPlanner<'_>,
    transaction: &TransactionRecord,
) -> Result<(), CommitFailure> {
    commit_operations_with_writer(planner, transaction, |progress| {
        write_progress(transaction, planner, progress)
    })
}

fn commit_operations_with_writer(
    planner: &WorkspaceEditPlanner<'_>,
    transaction: &TransactionRecord,
    mut persist: impl FnMut(&CommitProgress) -> Result<(), String>,
) -> Result<(), CommitFailure> {
    let artifact_directory = &transaction.artifact_directory;
    let mut progress = CommitProgress::new(transaction);
    let mut expected = initial_progress_manifest(transaction);
    for operation in &transaction.operations {
        let index = operation_index(operation);
        let result = (|| -> Result<(), String> {
            add_undo_manifest(&mut expected, transaction, operation);
            require_manifest(planner, &expected)?;
            progress.effects.push(CommitEffect {
                index,
                before: expected.clone(),
                after: None,
            });
            persist(&progress)?;
            let retained = retain_effect_handles(planner, operation, &expected)?;
            let identity = match operation {
                CanonicalOperation::Text {
                    index,
                    path,
                    before_digest,
                    after_digest,
                    ..
                } => apply_text_operation(
                    planner,
                    artifact_directory,
                    *index,
                    path,
                    before_digest,
                    after_digest,
                    manifest_for_path(&expected, path)
                        .unwrap()
                        .identity_digest
                        .as_deref(),
                ),
                CanonicalOperation::Create {
                    path,
                    overwrite,
                    ignore_if_exists,
                    ..
                } => apply_create_operation(
                    planner,
                    path,
                    *overwrite,
                    *ignore_if_exists,
                    manifest_for_path(&expected, path)
                        .unwrap()
                        .identity_digest
                        .as_deref(),
                ),
                CanonicalOperation::Rename {
                    index,
                    old_path,
                    new_path,
                    overwrite,
                    ignore_if_exists,
                    ..
                } => apply_rename_operation(
                    planner,
                    artifact_directory,
                    *index,
                    old_path,
                    new_path,
                    *overwrite,
                    *ignore_if_exists,
                )
                .map(|()| None),
                CanonicalOperation::Delete {
                    index,
                    path,
                    recursive,
                    ignore_if_not_exists,
                    ..
                } => apply_delete_operation(
                    planner,
                    artifact_directory,
                    *index,
                    path,
                    *recursive,
                    *ignore_if_not_exists,
                )
                .map(|()| None),
            }?;
            for handle in &retained {
                if open_file_identity(&handle.file).map_err(|e| e.to_string())? != handle.identity {
                    return Err("A moved resource handle changed.".to_owned());
                }
                #[cfg(windows)]
                preserve_move_metadata(handle).map_err(|e| e.to_string())?;
            }
            let observed = planner
                .inspect_manifest(&expected)
                .map_err(|problems| format!("Commit evidence cannot be inspected: {problems:?}"))?;
            expected = operation_after(
                transaction,
                operation,
                &expected,
                &observed,
                identity.as_deref(),
            )?;
            require_manifest(planner, &expected)?;
            progress.effects.last_mut().unwrap().after = Some(expected.clone());
            persist(&progress)
        })();
        result.map_err(|reason| CommitFailure {
            operation_index: index,
            _reason: reason,
        })?;
    }
    Ok(())
}

fn apply_text_operation(
    planner: &WorkspaceEditPlanner<'_>,
    artifact_directory: &Path,
    operation_index: u64,
    path: &Path,
    before_digest: &str,
    after_digest: &str,
    expected_identity: Option<&str>,
) -> Result<Option<String>, String> {
    let relative = planner.relative_path(path)?;
    let mut read_options = CapabilityOpenOptions::new();
    read_options
        .read(true)
        .write(true)
        .follow(FollowSymlinks::No);
    let mut file = planner
        .capability_root()
        .open_with(relative, &read_options)
        .map_err(|error| error.to_string())?
        .into_std();
    let accessed = file
        .metadata()
        .and_then(|metadata| metadata.accessed())
        .map_err(|error| format!("The text resource access time cannot be inspected: {error}"))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if digest_raw_bytes(&bytes) != before_digest {
        return Err("Text resource changed during commit.".to_owned());
    }
    let result = fs::read(staged_text_path(artifact_directory, operation_index))
        .map_err(|error| error.to_string())?;
    if digest_raw_bytes(&result) != after_digest {
        return Err("Canonical text edit digest does not match.".to_owned());
    }
    let identity = open_file_identity(&file).map_err(|e| e.to_string())?;
    if expected_identity != Some(identity.as_str())
        || inspect_manifest_path(planner, path)?
            .identity_digest
            .as_deref()
            != Some(&identity)
    {
        return Err("Text resource was replaced before truncation.".to_owned());
    }
    file.set_len(0).map_err(|error| error.to_string())?;
    file.seek(SeekFrom::Start(0))
        .and_then(|_| file.write_all(&result))
        .and_then(|_| file.sync_all())
        .map_err(|error| error.to_string())?;
    file.set_times(std::fs::FileTimes::new().set_accessed(accessed))
        .and_then(|()| file.sync_all())
        .map_err(|error| error.to_string())?;
    Ok(Some(identity))
}

fn apply_create_operation(
    planner: &WorkspaceEditPlanner<'_>,
    path: &Path,
    overwrite: bool,
    ignore_if_exists: bool,
    expected_identity: Option<&str>,
) -> Result<Option<String>, String> {
    let root = planner.capability_root();
    let relative = planner.relative_path(path)?;
    if root.symlink_metadata(relative).is_ok() {
        if overwrite {
            let mut options = CapabilityOpenOptions::new();
            options.write(true).follow(FollowSymlinks::No);
            let file = root
                .open_with(relative, &options)
                .map_err(|error| error.to_string())?
                .into_std();
            let identity = open_file_identity(&file).map_err(|e| e.to_string())?;
            if expected_identity != Some(identity.as_str())
                || inspect_manifest_path(planner, path)?
                    .identity_digest
                    .as_deref()
                    != Some(&identity)
            {
                return Err("CreateFile target was replaced before truncation.".to_owned());
            }
            file.set_len(0)
                .and_then(|()| file.sync_all())
                .map_err(|e| e.to_string())?;
            return Ok(Some(identity));
        }
        if ignore_if_exists {
            return Ok(None);
        }
        return Err("CreateFile target exists.".to_owned());
    }
    let mut options = CapabilityOpenOptions::new();
    options
        .write(true)
        .create_new(true)
        .follow(FollowSymlinks::No);
    let file = root
        .open_with(relative, &options)
        .map_err(|error| error.to_string())?;
    let file = file.into_std();
    let identity = open_file_identity(&file).map_err(|e| e.to_string())?;
    file.sync_all().map_err(|error| error.to_string())?;
    flush_parent(path).map_err(|error| error.to_string())?;
    Ok(Some(identity))
}

fn apply_rename_operation(
    planner: &WorkspaceEditPlanner<'_>,
    artifact_directory: &Path,
    operation_index: u64,
    old_path: &Path,
    new_path: &Path,
    overwrite: bool,
    ignore_if_exists: bool,
) -> Result<(), String> {
    let root = planner.capability_root();
    let old_relative = planner.relative_path(old_path)?;
    let new_relative = planner.relative_path(new_path)?;
    if root.symlink_metadata(new_relative).is_ok() {
        if overwrite {
            move_resource_to_undo(planner, artifact_directory, operation_index, new_path)?;
        } else if ignore_if_exists {
            return Ok(());
        } else {
            return Err("RenameFile destination exists.".to_owned());
        }
    }
    root.rename(old_relative, root, new_relative)
        .map_err(|error| error.to_string())?;
    flush_parent(old_path).map_err(|error| error.to_string())?;
    if old_path.parent() != new_path.parent() {
        flush_parent(new_path).map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn apply_delete_operation(
    planner: &WorkspaceEditPlanner<'_>,
    artifact_directory: &Path,
    operation_index: u64,
    path: &Path,
    recursive: bool,
    ignore_if_not_exists: bool,
) -> Result<(), String> {
    let root = planner.capability_root();
    let relative = planner.relative_path(path)?;
    let Ok(metadata) = root.symlink_metadata(relative) else {
        return if ignore_if_not_exists {
            Ok(())
        } else {
            Err("DeleteFile target is missing.".to_owned())
        };
    };
    if metadata.is_dir()
        && !recursive
        && root
            .read_dir(relative)
            .map_err(|error| error.to_string())?
            .next()
            .transpose()
            .map_err(|error| error.to_string())?
            .is_some()
    {
        return Err("DeleteFile directory is no longer empty.".to_owned());
    }
    move_resource_to_undo(planner, artifact_directory, operation_index, path)
}

fn move_resource_to_undo(
    planner: &WorkspaceEditPlanner<'_>,
    artifact_directory: &Path,
    operation_index: u64,
    path: &Path,
) -> Result<(), String> {
    let root = planner.capability_root();
    let relative = planner.relative_path(path)?;
    let undo_path = undo_resource_path(artifact_directory, operation_index);
    let undo_relative = planner.relative_path(&undo_path)?;
    root.rename(relative, root, undo_relative)
        .map_err(|error| error.to_string())?;
    flush_parent(path).map_err(|error| error.to_string())?;
    flush_parent(&undo_path).map_err(|error| error.to_string())
}

fn remove_capability_resource(root: &Dir, relative: &Path) -> std::io::Result<()> {
    if root.symlink_metadata(relative)?.is_dir() {
        root.remove_dir_all(relative)
    } else {
        root.remove_file(relative)
    }
}

fn planned_backups(before: &[ManifestEntry], artifact_directory: &Path) -> Vec<BackupEntry> {
    let paths = before
        .iter()
        .filter(|entry| entry.exists)
        .map(|entry| entry.path.clone())
        .collect::<BTreeSet<_>>();
    let roots = paths
        .iter()
        .filter(|path| {
            !paths
                .iter()
                .any(|candidate| *candidate != **path && path.starts_with(candidate))
        })
        .collect::<Vec<_>>();
    roots
        .into_iter()
        .enumerate()
        .map(|(index, path)| {
            let manifest = before.iter().find(|entry| &entry.path == path).unwrap();
            BackupEntry {
                path: path.clone(),
                backup_path: artifact_directory.join(format!("backup-{index}")),
                existed: true,
                resource_kind: match manifest.resource_kind {
                    ResourceKind::File => "file",
                    ResourceKind::Directory => "directory",
                    ResourceKind::Missing => "missing",
                }
                .to_owned(),
            }
        })
        .collect()
}

fn stage_transaction(
    transaction: &TransactionRecord,
    operations: &[CanonicalOperation],
    limits: &MutationSettings,
) -> Result<(), TransactionStageFailure> {
    create_private_directory(&transaction.artifact_directory).map_err(|error| {
        TransactionStageFailure {
            failure: stage_failure(
                &transaction.transaction_id,
                "The same-volume transaction directory cannot be created.",
                error.raw_os_error(),
            ),
            artifact_created: false,
        }
    })?;
    if let Err(failure) = write_artifact_owner(transaction) {
        let _ = fs::remove_file(transaction.artifact_directory.join(ARTIFACT_OWNER_FILE));
        let removed = fs::remove_dir(&transaction.artifact_directory).is_ok();
        return Err(TransactionStageFailure {
            failure,
            artifact_created: !removed,
        });
    }
    stage_transaction_contents(transaction, operations, limits).map_err(|failure| {
        TransactionStageFailure {
            failure,
            artifact_created: true,
        }
    })
}

#[derive(Debug)]
struct TransactionStageFailure {
    failure: ContractFailure,
    artifact_created: bool,
}

fn write_artifact_owner(transaction: &TransactionRecord) -> Result<(), ContractFailure> {
    let path = transaction.artifact_directory.join(ARTIFACT_OWNER_FILE);
    let mut owner = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|error| {
            stage_failure(
                &transaction.transaction_id,
                "The transaction ownership marker cannot be created.",
                error.raw_os_error(),
            )
        })?;
    state_permissions::restrict_file(&path).map_err(|error| {
        stage_failure(
            &transaction.transaction_id,
            "The transaction ownership marker cannot be made private.",
            error.raw_os_error(),
        )
    })?;
    owner
        .write_all(transaction.transaction_id.as_bytes())
        .and_then(|()| owner.sync_all())
        .map_err(|error| {
            stage_failure(
                &transaction.transaction_id,
                "The transaction ownership marker cannot be flushed.",
                error.raw_os_error(),
            )
        })
}

fn stage_transaction_contents(
    transaction: &TransactionRecord,
    operations: &[CanonicalOperation],
    limits: &MutationSettings,
) -> Result<(), ContractFailure> {
    state_permissions::restrict_directory(&transaction.artifact_directory).map_err(|error| {
        stage_failure(
            &transaction.transaction_id,
            &error.to_string(),
            error.raw_os_error(),
        )
    })?;
    let mut copied_bytes = 0_u64;
    for backup in &transaction.backups {
        copy_resource(&backup.path, &backup.backup_path, &mut copied_bytes).map_err(|error| {
            stage_failure(
                &transaction.transaction_id,
                "A rollback backup cannot be staged.",
                error.raw_os_error(),
            )
        })?;
        if copied_bytes > limits.max_rollback_bytes {
            return Err(ContractFailure {
                exit_code: 6,
                category: "mutation",
                code: "resource_limit_exceeded",
                message: "Rollback staging exceeds the configured byte limit.".to_owned(),
                stage: "stage",
                delivery: "not_applicable",
                retry: "after_change",
                data: json!({
                    "resource": "rollbackBytes",
                    "limit": limits.max_rollback_bytes,
                    "observed": copied_bytes
                }),
            });
        }
    }
    stage_text_outputs(transaction, operations, limits)?;
    flush_directory(&transaction.artifact_directory).map_err(|error| {
        stage_failure(
            &transaction.transaction_id,
            "The transaction directory cannot be flushed.",
            error.raw_os_error(),
        )
    })
}

#[cfg(unix)]
fn create_private_directory(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;

    fs::DirBuilder::new().mode(0o700).create(path)
}

#[cfg(not(unix))]
fn create_private_directory(path: &Path) -> std::io::Result<()> {
    fs::create_dir(path)?;
    if let Err(error) = state_permissions::restrict_directory(path) {
        let _ = fs::remove_dir(path);
        return Err(error);
    }
    Ok(())
}

fn stage_text_outputs(
    transaction: &TransactionRecord,
    operations: &[CanonicalOperation],
    limits: &MutationSettings,
) -> Result<(), ContractFailure> {
    let mut texts = BTreeMap::<PathBuf, Vec<u8>>::new();
    let mut unavailable = Vec::<PathBuf>::new();
    let mut aliases = Vec::<(PathBuf, PathBuf)>::new();
    let mut staged_bytes = 0_u64;
    for operation in operations {
        match operation {
            CanonicalOperation::Text {
                index,
                path,
                before_digest,
                after_digest,
                edits,
                ..
            } => {
                let before =
                    virtual_text(path, &texts, &unavailable, &aliases).map_err(|error| {
                        stage_failure(
                            &transaction.transaction_id,
                            "A text input cannot be staged.",
                            error.raw_os_error(),
                        )
                    })?;
                if digest_raw_bytes(&before) != *before_digest {
                    return Err(stage_failure(
                        &transaction.transaction_id,
                        "A staged text input no longer matches its canonical digest.",
                        None,
                    ));
                }
                let after = apply_canonical_text_edits(&before, edits)
                    .map_err(|reason| stage_failure(&transaction.transaction_id, &reason, None))?;
                if digest_raw_bytes(&after) != *after_digest {
                    return Err(stage_failure(
                        &transaction.transaction_id,
                        "A staged text output does not match its canonical digest.",
                        None,
                    ));
                }
                staged_bytes = staged_bytes.saturating_add(after.len() as u64);
                if staged_bytes > limits.max_staged_text_bytes {
                    return Err(ContractFailure {
                        exit_code: 6,
                        category: "mutation",
                        code: "resource_limit_exceeded",
                        message: "Text staging exceeds the configured byte limit.".to_owned(),
                        stage: "stage",
                        delivery: "not_applicable",
                        retry: "after_change",
                        data: json!({
                            "resource": "stagedTextBytes",
                            "limit": limits.max_staged_text_bytes,
                            "observed": staged_bytes,
                            "operationIndex": index
                        }),
                    });
                }
                let staged_path = staged_text_path(&transaction.artifact_directory, *index);
                let mut staged = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&staged_path)
                    .map_err(|error| {
                        stage_failure(
                            &transaction.transaction_id,
                            "A text output cannot be created in staging.",
                            error.raw_os_error(),
                        )
                    })?;
                state_permissions::restrict_file(&staged_path).map_err(|error| {
                    stage_failure(
                        &transaction.transaction_id,
                        "A staged text output cannot be made private.",
                        error.raw_os_error(),
                    )
                })?;
                staged
                    .write_all(&after)
                    .and_then(|()| staged.sync_all())
                    .map_err(|error| {
                        stage_failure(
                            &transaction.transaction_id,
                            "A staged text output cannot be flushed.",
                            error.raw_os_error(),
                        )
                    })?;
                texts.insert(path.clone(), after);
                unavailable.retain(|root| root != path);
            }
            CanonicalOperation::Create { path, .. } => {
                texts.retain(|candidate, _| !candidate.starts_with(path));
                texts.insert(path.clone(), Vec::new());
                unavailable.retain(|root| root != path);
            }
            CanonicalOperation::Rename {
                old_path, new_path, ..
            } => {
                let physical_source = resolve_physical_path(old_path, &aliases);
                let moved = texts
                    .iter()
                    .filter(|(path, _)| path.starts_with(old_path))
                    .map(|(path, text)| (path.clone(), text.clone()))
                    .collect::<Vec<_>>();
                texts.retain(|path, _| !path.starts_with(old_path) && !path.starts_with(new_path));
                for (path, text) in moved {
                    let relative = path.strip_prefix(old_path).unwrap();
                    let target = if relative.as_os_str().is_empty() {
                        new_path.clone()
                    } else {
                        new_path.join(relative)
                    };
                    texts.insert(target, text);
                }
                unavailable.retain(|root| !root.starts_with(new_path));
                unavailable.push(old_path.clone());
                aliases.push((new_path.clone(), physical_source));
            }
            CanonicalOperation::Delete { path, .. } => {
                texts.retain(|candidate, _| !candidate.starts_with(path));
                unavailable.push(path.clone());
            }
        }
    }
    Ok(())
}

fn virtual_text(
    path: &Path,
    texts: &BTreeMap<PathBuf, Vec<u8>>,
    unavailable: &[PathBuf],
    aliases: &[(PathBuf, PathBuf)],
) -> std::io::Result<Vec<u8>> {
    if let Some(text) = texts.get(path) {
        return Ok(text.clone());
    }
    if unavailable.iter().any(|root| path.starts_with(root)) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "the virtual text resource is unavailable",
        ));
    }
    read_source_preserving_access_time(&resolve_physical_path(path, aliases))
}

fn read_source_preserving_access_time(path: &Path) -> std::io::Result<Vec<u8>> {
    let metadata = fs::metadata(path)?;
    let bytes = fs::read(path)?;
    restore_source_access_time(path, &metadata)?;
    Ok(bytes)
}

fn resolve_physical_path(path: &Path, aliases: &[(PathBuf, PathBuf)]) -> PathBuf {
    let mut resolved = path.to_path_buf();
    for (virtual_root, physical_root) in aliases.iter().rev() {
        if let Ok(relative) = resolved.strip_prefix(virtual_root) {
            resolved = if relative.as_os_str().is_empty() {
                physical_root.clone()
            } else {
                physical_root.join(relative)
            };
        }
    }
    resolved
}

fn apply_canonical_text_edits(
    before: &[u8],
    edits: &[super::planner::CanonicalTextEdit],
) -> Result<Vec<u8>, String> {
    let mut after = Vec::with_capacity(before.len());
    let mut cursor = 0;
    for edit in edits {
        let start = edit.start_byte as usize;
        let end = edit.end_byte as usize;
        if start < cursor || end < start || end > before.len() {
            return Err("A canonical staged text edit range is invalid.".to_owned());
        }
        after.extend_from_slice(&before[cursor..start]);
        after.extend_from_slice(edit.new_text.as_bytes());
        cursor = end;
    }
    after.extend_from_slice(&before[cursor..]);
    Ok(after)
}

fn staged_text_path(artifact_directory: &Path, operation_index: u64) -> PathBuf {
    artifact_directory.join(format!("text-{operation_index}"))
}

fn undo_resource_path(artifact_directory: &Path, operation_index: u64) -> PathBuf {
    artifact_directory.join(format!("undo-{operation_index}"))
}

fn rollback_transaction(
    transaction: &TransactionRecord,
    planner: &WorkspaceEditPlanner<'_>,
) -> Result<Vec<ManifestEntry>, String> {
    let current = inspect_transaction_manifest(planner, transaction).map_err(|problems| {
        format!("The partial filesystem is unsafe for automatic rollback: {problems:?}")
    })?;
    if manifest_mismatches(&transaction.before_manifest, &current).is_empty() {
        return Ok(current);
    }
    let progress = read_progress(transaction, planner)?;
    if progress.effects.iter().any(|effect| effect.after.is_none()) {
        return Err("An uncertain commit effect requires Recovery.".to_owned());
    }
    let Some(last) = progress.effects.last() else {
        return Err("No completed Application effects are proven.".to_owned());
    };
    require_manifest(planner, last.after.as_ref().unwrap())?;
    // Preflight every reverse step, including its protected restore content, before changing bytes.
    let mut reversed = last.after.as_ref().unwrap().clone();
    for (operation, effect) in transaction.operations.iter().zip(&progress.effects).rev() {
        if reversed != *effect.after.as_ref().unwrap() {
            return Err("Commit effects do not form an undo chain.".to_owned());
        }
        if let CanonicalOperation::Text { path, .. } | CanonicalOperation::Create { path, .. } =
            operation
        {
            let before = manifest_for_path(&effect.before, path)?;
            if before.exists {
                rollback_text_bytes(transaction, planner, before)?;
            }
        }
        reversed = effect.before.clone();
        // Later operations can introduce only missing, derived undo paths into the chain.
        if let Some(previous) = progress
            .effects
            .iter()
            .take_while(|candidate| candidate.index != effect.index)
            .last()
        {
            reversed.retain(|entry| {
                previous
                    .after
                    .as_ref()
                    .unwrap()
                    .iter()
                    .any(|old| old.path == entry.path)
            });
        }
    }
    if !manifest_mismatches(&transaction.before_manifest, &reversed).is_empty() {
        return Err("Proven effects cannot restore the full before-manifest.".to_owned());
    }
    for (operation, effect) in transaction.operations.iter().zip(&progress.effects).rev() {
        require_manifest(planner, effect.after.as_ref().unwrap())?;
        match operation {
            CanonicalOperation::Text { path, .. } | CanonicalOperation::Create { path, .. } => {
                let before = manifest_for_path(&effect.before, path)?;
                let after = manifest_for_path(effect.after.as_ref().unwrap(), path)?;
                if before == after {
                    continue;
                }
                if before.exists {
                    restore_proven_text(transaction, planner, before, after)?;
                } else {
                    let relative = planner.relative_path(path)?;
                    remove_capability_resource(planner.capability_root(), relative)
                        .map_err(|e| e.to_string())?;
                    flush_parent(path).map_err(|e| e.to_string())?;
                }
            }
            CanonicalOperation::Rename {
                index,
                old_path,
                new_path,
                ..
            } => {
                if effect.before != *effect.after.as_ref().unwrap() {
                    let proof = effect.after.as_ref().unwrap();
                    rename_capability_resource(planner, new_path, old_path, proof)?;
                    let undo = undo_resource_path(&transaction.artifact_directory, *index);
                    if manifest_for_path(proof, &undo)?.exists {
                        rename_capability_resource(planner, &undo, new_path, proof)?;
                    }
                }
            }
            CanonicalOperation::Delete { index, path, .. } => {
                let undo = undo_resource_path(&transaction.artifact_directory, *index);
                if manifest_for_path(effect.after.as_ref().unwrap(), &undo)?.exists {
                    rename_capability_resource(
                        planner,
                        &undo,
                        path,
                        effect.after.as_ref().unwrap(),
                    )?;
                }
            }
        }
        require_manifest(planner, &effect.before)?;
    }
    let current = inspect_transaction_manifest(planner, transaction)
        .map_err(|_| "Restored state cannot be inspected.".to_owned())?;
    // The operation-order undo above must restore everything; never guess from an inode match.
    if !manifest_mismatches(&transaction.before_manifest, &current).is_empty() {
        return Err("The rollback manifest does not match its preconditions.".to_owned());
    }
    Ok(current)
}

fn rollback_text_bytes(
    transaction: &TransactionRecord,
    planner: &WorkspaceEditPlanner<'_>,
    expected: &ManifestEntry,
) -> Result<(Vec<u8>, Option<File>), String> {
    let digest = expected
        .content_digest
        .as_ref()
        .ok_or("Missing undo text digest.")?;
    if let Some(original) = transaction.before_manifest.iter().find(|entry| {
        entry.exists
            && entry.identity_digest == expected.identity_digest
            && entry.content_digest == expected.content_digest
    }) {
        let path = backup_path_for(&transaction.backups, &original.path)
            .ok_or("Missing original text backup.")?;
        let mut options = CapabilityOpenOptions::new();
        options.read(true).follow(FollowSymlinks::No);
        let mut source = planner
            .capability_root()
            .open_with(planner.relative_path(&path)?, &options)
            .map_err(|e| e.to_string())?
            .into_std();
        let mut bytes = Vec::new();
        source.read_to_end(&mut bytes).map_err(|e| e.to_string())?;
        if digest_raw_bytes(&bytes) != *digest {
            return Err("Original rollback backup content changed.".to_owned());
        }
        let actual = inspect_manifest_path(planner, &path)?;
        if actual.metadata_digest != expected.metadata_digest {
            return Err("Original rollback backup metadata changed.".to_owned());
        }
        return Ok((bytes, Some(source)));
    }
    if *digest == digest_raw_bytes(b"") {
        return Ok((Vec::new(), None));
    }
    for operation in &transaction.operations {
        if let CanonicalOperation::Text {
            index,
            after_digest,
            ..
        } = operation
            && after_digest == digest
        {
            let path = staged_text_path(&transaction.artifact_directory, *index);
            let mut options = CapabilityOpenOptions::new();
            options.read(true).follow(FollowSymlinks::No);
            let mut source = planner
                .capability_root()
                .open_with(planner.relative_path(&path)?, &options)
                .map_err(|e| e.to_string())?;
            let mut bytes = Vec::new();
            source.read_to_end(&mut bytes).map_err(|e| e.to_string())?;
            if digest_raw_bytes(&bytes) != *digest {
                return Err("Intermediate rollback text changed.".to_owned());
            }
            return Ok((bytes, None));
        }
    }
    Err("No canonical content proves the intermediate text state.".to_owned())
}

fn restore_proven_text(
    transaction: &TransactionRecord,
    planner: &WorkspaceEditPlanner<'_>,
    before: &ManifestEntry,
    after: &ManifestEntry,
) -> Result<(), String> {
    let (bytes, backup) = rollback_text_bytes(transaction, planner, before)?;
    let mut options = CapabilityOpenOptions::new();
    options.read(true).write(true).follow(FollowSymlinks::No);
    let mut output = planner
        .capability_root()
        .open_with(planner.relative_path(&before.path)?, &options)
        .map_err(|e| e.to_string())?
        .into_std();
    let identity = open_file_identity(&output).map_err(|e| e.to_string())?;
    let mut current = Vec::new();
    output
        .read_to_end(&mut current)
        .map_err(|e| e.to_string())?;
    if after.identity_digest.as_deref() != Some(&identity)
        || after.content_digest != Some(digest_raw_bytes(&current))
    {
        return Err("Text changed before rollback truncation.".to_owned());
    }
    require_manifest(planner, std::slice::from_ref(after))?;
    output
        .set_len(0)
        .and_then(|()| output.seek(SeekFrom::Start(0)))
        .and_then(|_| output.write_all(&bytes))
        .map_err(|e| e.to_string())?;
    if let Some(source) = backup {
        preserve_open_file_metadata(
            &source,
            &output,
            &source.metadata().map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
    }
    output.sync_all().map_err(|e| e.to_string())
}

fn manifest_for_path<'a>(
    manifest: &'a [ManifestEntry],
    path: &Path,
) -> Result<&'a ManifestEntry, String> {
    manifest
        .iter()
        .find(|entry| entry.path == path)
        .ok_or_else(|| "A rollback manifest path is missing.".to_owned())
}

fn inspect_manifest_path(
    planner: &WorkspaceEditPlanner<'_>,
    path: &Path,
) -> Result<ManifestEntry, String> {
    planner
        .inspect_manifest_paths(&[path.to_path_buf()])
        .map_err(|_| "A rollback resource cannot be inspected safely.".to_owned())?
        .into_iter()
        .next()
        .ok_or_else(|| "A rollback resource manifest is missing.".to_owned())
}

fn rename_capability_resource(
    planner: &WorkspaceEditPlanner<'_>,
    from: &Path,
    to: &Path,
    proof: &[ManifestEntry],
) -> Result<(), String> {
    let root = planner.capability_root();
    let from_relative = planner.relative_path(from)?;
    let to_relative = planner.relative_path(to)?;
    if !manifest_for_path(proof, from)?.exists {
        return Err("The rollback rename source lacks proof.".to_owned());
    }
    let mut expected = proof
        .iter()
        .filter(|entry| entry.path.starts_with(from))
        .cloned()
        .collect::<Vec<_>>();
    expected.push(missing_manifest(to));
    expected.sort_by(|a, b| a.path.cmp(&b.path));
    // Each leg needs a fresh check: the previous rename and its flush can admit an external writer.
    require_manifest(planner, &expected)?;
    #[cfg(windows)]
    let handle = retain_move_handle(planner, manifest_for_path(proof, from)?)?;
    root.rename(from_relative, root, to_relative)
        .map_err(|error| error.to_string())?;
    #[cfg(windows)]
    preserve_move_metadata(&handle).map_err(|e| e.to_string())?;
    flush_parent(from).map_err(|error| error.to_string())?;
    if from.parent() != to.parent() {
        flush_parent(to).map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn preserve_open_file_metadata(
    source: &File,
    destination: &File,
    metadata: &fs::Metadata,
) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::{fd::AsRawFd, unix::fs::MetadataExt};

        copy_open_file_extended_attributes(source, destination)?;
        if unsafe { libc::fchown(destination.as_raw_fd(), metadata.uid(), metadata.gid()) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    #[cfg(not(unix))]
    let _ = source;
    destination.set_permissions(metadata.permissions())?;
    let times = fs::FileTimes::new()
        .set_accessed(metadata.accessed()?)
        .set_modified(metadata.modified()?);
    #[cfg(windows)]
    let times = {
        use std::os::windows::fs::FileTimesExt;
        times.set_created(metadata.created()?)
    };
    destination.set_times(times)
}

#[cfg(unix)]
fn copy_open_file_extended_attributes(source: &File, destination: &File) -> std::io::Result<()> {
    use xattr::FileExt;

    let source_names = source.list_xattr()?.collect::<BTreeSet<_>>();
    for name in destination.list_xattr()? {
        if !source_names.contains(&name) {
            destination.remove_xattr(&name)?;
        }
    }
    for name in source_names {
        if let Some(value) = source.get_xattr(&name)? {
            destination.set_xattr(&name, &value)?;
        }
    }
    Ok(())
}

fn backup_path_for(backups: &[BackupEntry], path: &Path) -> Option<PathBuf> {
    backups.iter().find_map(|backup| {
        path.strip_prefix(&backup.path).ok().map(|relative| {
            if relative.as_os_str().is_empty() {
                backup.backup_path.clone()
            } else {
                backup.backup_path.join(relative)
            }
        })
    })
}

fn inspect_transaction_manifest(
    planner: &WorkspaceEditPlanner<'_>,
    transaction: &TransactionRecord,
) -> Result<Vec<ManifestEntry>, Vec<WorkspaceEditProblem>> {
    let mut template = transaction
        .before_manifest
        .iter()
        .chain(&transaction.intended_manifest)
        .map(|entry| (&entry.path, entry))
        .collect::<BTreeMap<_, _>>();
    // Retain certificates at either end of a rename, without enriching legacy or ignored directories.
    for entry in transaction
        .before_manifest
        .iter()
        .chain(&transaction.intended_manifest)
        .chain(&transaction.observed_manifest)
        .filter(|entry| {
            entry.resource_kind == ResourceKind::Directory && entry.content_digest.is_some()
        })
    {
        if let Some(bound) = template.get_mut(&entry.path) {
            *bound = entry;
        }
    }
    planner.inspect_manifest(&template.into_values().cloned().collect::<Vec<_>>())
}

fn copy_resource(source: &Path, destination: &Path, copied_bytes: &mut u64) -> std::io::Result<()> {
    let metadata = fs::symlink_metadata(source)?;
    if metadata.is_dir() {
        fs::create_dir(destination)?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            copy_resource(
                &entry.path(),
                &destination.join(entry.file_name()),
                copied_bytes,
            )?;
        }
        copy_required_metadata(source, destination, &metadata, copied_bytes)?;
        restore_source_access_time(source, &metadata)?;
        flush_directory(destination)?;
    } else {
        fs::copy(source, destination)?;
        let flush_file = open_copied_file_for_flush(destination, &metadata)?;
        *copied_bytes = copied_bytes.saturating_add(metadata.len());
        copy_required_metadata(source, destination, &metadata, copied_bytes)?;
        restore_source_access_time(source, &metadata)?;
        flush_file.sync_all()?;
    }
    Ok(())
}

#[cfg(windows)]
#[allow(clippy::permissions_set_readonly_false)]
fn open_copied_file_for_flush(path: &Path, metadata: &fs::Metadata) -> std::io::Result<File> {
    let mut permissions = metadata.permissions();
    permissions.set_readonly(false);
    fs::set_permissions(path, permissions)?;
    OpenOptions::new().write(true).open(path)
}

#[cfg(not(windows))]
fn open_copied_file_for_flush(path: &Path, _metadata: &fs::Metadata) -> std::io::Result<File> {
    File::open(path)
}

fn copy_required_metadata(
    source: &Path,
    destination: &Path,
    metadata: &fs::Metadata,
    copied_bytes: &mut u64,
) -> std::io::Result<()> {
    copy_native_owner(source, destination, metadata)?;
    *copied_bytes = copied_bytes.saturating_add(copy_extended_attributes(source, destination)?);
    fs::set_permissions(destination, metadata.permissions())?;
    copy_native_flags(source, destination, metadata)?;
    copy_file_times(destination, metadata)
}

#[cfg(unix)]
fn copy_native_owner(
    _source: &Path,
    destination: &Path,
    metadata: &fs::Metadata,
) -> std::io::Result<()> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt, os::unix::fs::MetadataExt};

    let path = CString::new(destination.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "path contains NUL"))?;
    if unsafe { libc::chown(path.as_ptr(), metadata.uid(), metadata.gid()) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(windows)]
fn copy_native_owner(
    source: &Path,
    destination: &Path,
    _metadata: &fs::Metadata,
) -> std::io::Result<()> {
    use std::{os::windows::ffi::OsStrExt, ptr};
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::{
            Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT},
            DACL_SECURITY_INFORMATION, GROUP_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION,
            SetFileSecurityW,
        },
    };

    let source_security_descriptor = windows_security_descriptor(source)?;
    let mut source_wide = source
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let mut destination_wide = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let mut descriptor = ptr::null_mut();
    let information =
        OWNER_SECURITY_INFORMATION | GROUP_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION;
    let status = unsafe {
        GetNamedSecurityInfoW(
            source_wide.as_mut_ptr(),
            SE_FILE_OBJECT,
            information,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            &mut descriptor,
        )
    };
    if status != 0 {
        return Err(std::io::Error::from_raw_os_error(status as i32));
    }
    let result = if unsafe {
        SetFileSecurityW(
            destination_wide.as_mut_ptr(),
            DACL_SECURITY_INFORMATION,
            descriptor,
        )
    } == 0
    {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    };
    unsafe {
        LocalFree(descriptor);
    }
    result?;
    if windows_security_descriptor(destination)? != source_security_descriptor {
        return Err(std::io::Error::other(
            "copied Windows security descriptor does not match the source",
        ));
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn copy_native_owner(
    _source: &Path,
    _destination: &Path,
    _metadata: &fs::Metadata,
) -> std::io::Result<()> {
    Ok(())
}

#[cfg(target_os = "macos")]
fn copy_native_flags(
    _source: &Path,
    destination: &Path,
    metadata: &fs::Metadata,
) -> std::io::Result<()> {
    use std::{ffi::CString, os::macos::fs::MetadataExt, os::unix::ffi::OsStrExt};

    let path = CString::new(destination.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "path contains NUL"))?;
    if unsafe { libc::chflags(path.as_ptr(), metadata.st_flags()) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn copy_native_flags(
    _source: &Path,
    _destination: &Path,
    _metadata: &fs::Metadata,
) -> std::io::Result<()> {
    Ok(())
}

fn copy_file_times(destination: &Path, metadata: &fs::Metadata) -> std::io::Result<()> {
    let times = std::fs::FileTimes::new()
        .set_accessed(metadata.accessed()?)
        .set_modified(metadata.modified()?);
    #[cfg(windows)]
    let times = {
        use std::os::windows::fs::FileTimesExt;
        times.set_created(metadata.created()?)
    };
    open_file_for_timestamp_update(destination)?.set_times(times)
}

#[cfg(windows)]
fn open_file_for_timestamp_update(path: &Path) -> std::io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_WRITE_ATTRIBUTES,
    };

    fs::OpenOptions::new()
        .access_mode(FILE_WRITE_ATTRIBUTES)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
}

#[cfg(not(windows))]
fn open_file_for_timestamp_update(path: &Path) -> std::io::Result<File> {
    File::open(path)
}

#[cfg(unix)]
fn restore_source_access_time(source: &Path, metadata: &fs::Metadata) -> std::io::Result<()> {
    File::open(source)?.set_times(std::fs::FileTimes::new().set_accessed(metadata.accessed()?))
}

#[cfg(not(unix))]
fn restore_source_access_time(_source: &Path, _metadata: &fs::Metadata) -> std::io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn copy_extended_attributes(source: &Path, destination: &Path) -> std::io::Result<u64> {
    let mut copied_bytes = 0_u64;
    for name in xattr::list(source)? {
        if let Some(value) = xattr::get(source, &name)? {
            xattr::set(destination, &name, &value)?;
            copied_bytes = copied_bytes
                .saturating_add(name.as_encoded_bytes().len() as u64)
                .saturating_add(value.len() as u64);
        }
    }
    Ok(copied_bytes)
}

#[cfg(not(unix))]
fn copy_extended_attributes(_source: &Path, _destination: &Path) -> std::io::Result<u64> {
    Ok(0)
}

fn cleanup_transaction_artifacts(transaction: &TransactionRecord) -> Result<(), String> {
    if transaction.artifact_directory.exists() {
        let expected_directory = transaction
            .workspace_path
            .join(format!(".lspctl-{}", transaction.transaction_id));
        if transaction.artifact_directory != expected_directory {
            return Err("The transaction artifact path is not canonical.".to_owned());
        }
        let marker = transaction.artifact_directory.join(ARTIFACT_OWNER_FILE);
        if fs::read_to_string(marker).ok().as_deref() != Some(transaction.transaction_id.as_str()) {
            return Err("The transaction artifact ownership cannot be verified.".to_owned());
        }
        fs::remove_dir_all(&transaction.artifact_directory).map_err(|error| error.to_string())?;
        flush_parent(&transaction.artifact_directory).map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn inspect_paths_best_effort(template: &[ManifestEntry]) -> Vec<ManifestEntry> {
    let mut entries = template
        .iter()
        .map(|entry| inspect_path_best_effort(&entry.path))
        .collect::<Vec<_>>();
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    entries
}

fn inspect_path_best_effort(path: &Path) -> ManifestEntry {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return ManifestEntry {
            path: path.to_path_buf(),
            exists: false,
            resource_kind: ResourceKind::Missing,
            identity_digest: None,
            content_digest: None,
            metadata_digest: None,
        };
    };
    let resource_kind = if metadata.is_file() {
        ResourceKind::File
    } else if metadata.is_dir() {
        ResourceKind::Directory
    } else {
        ResourceKind::Missing
    };
    ManifestEntry {
        path: path.to_path_buf(),
        exists: resource_kind != ResourceKind::Missing,
        resource_kind,
        identity_digest: None,
        content_digest: (resource_kind == ResourceKind::File)
            .then(|| fs::read(path).ok().map(|bytes| digest_raw_bytes(&bytes)))
            .flatten(),
        metadata_digest: None,
    }
}

pub(crate) fn preview_manifest_mismatches(
    plan: &CanonicalPlan,
    actual: &[ManifestEntry],
) -> Vec<Value> {
    let mut reasons = manifest_mismatches(&plan.before_manifest, actual);
    for entry in &plan.before_manifest {
        if entry.exists
            && entry.resource_kind == ResourceKind::Directory
            && entry.content_digest.is_none()
            && plan.operations.iter().any(|operation| match operation {
                CanonicalOperation::Delete { path, .. } => entry.path.starts_with(path),
                CanonicalOperation::Rename {
                    old_path,
                    new_path,
                    overwrite,
                    ..
                } => {
                    entry.path.starts_with(old_path)
                        || (*overwrite && entry.path.starts_with(new_path))
                }
                _ => false,
            })
        {
            reasons.push(json!({"code": "resource_content", "path": entry.path,
                "message": "Directory membership was not bound by this Preview; create a fresh Preview."}));
        }
    }
    reasons
}

pub(crate) fn manifest_mismatches(
    expected: &[ManifestEntry],
    actual: &[ManifestEntry],
) -> Vec<Value> {
    expected
        .iter()
        .filter_map(|expected| {
            let actual = actual.iter().find(|actual| actual.path == expected.path);
            let actual = actual.cloned().unwrap_or(ManifestEntry {
                path: expected.path.clone(),
                exists: false,
                resource_kind: ResourceKind::Missing,
                identity_digest: None,
                content_digest: None,
                metadata_digest: None,
            });
            let code = if expected.exists != actual.exists {
                Some("resource_existence")
            } else if expected.resource_kind != actual.resource_kind {
                Some("resource_kind")
            } else if expected.identity_digest.is_some()
                && expected.identity_digest != actual.identity_digest
            {
                Some("resource_identity")
            } else if expected.content_digest.is_some()
                && expected.content_digest != actual.content_digest
            {
                Some("resource_content")
            } else if expected.metadata_digest.is_some()
                && expected.metadata_digest != actual.metadata_digest
            {
                Some("resource_metadata")
            } else {
                None
            }?;
            Some(json!({
                "code": code,
                "message": "A filesystem precondition no longer matches.",
                "path": expected.path,
                "expectedDigest": expected.content_digest,
                "actualDigest": actual.content_digest
            }))
        })
        .collect()
}

fn preview_stale_failure(stored: &StoredPreview, reasons: Vec<Value>) -> ContractFailure {
    ContractFailure {
        exit_code: 6,
        category: "mutation",
        code: "preview_stale",
        message: "The Preview authorization or immutable inputs changed.".to_owned(),
        stage: "reserve",
        delivery: "not_applicable",
        retry: "after_change",
        data: json!({
            "previewId": stored.preview.preview_id,
            "reasons": reasons,
            "preconditions": stored.preview.preconditions
        }),
    }
}

fn application_success(receipt: &ReceiptRecord, outcome: &str) -> Value {
    json!({
        "schemaVersion": 1,
        "ok": true,
        "command": ["apply"],
        "outcome": outcome,
        "result": {
            "previewId": receipt.preview_id,
            "receiptId": receipt.receipt_id,
            "transactionId": receipt.transaction_id,
            "state": "terminal",
            "outcome": outcome,
            "filesystemState": receipt.filesystem_state,
            "sessionSynchronized": receipt.session_synchronized,
            "cleanupPending": receipt.cleanup_pending,
            "durability": {
                "directoryFlush": receipt.durability["directoryFlush"]
            },
            "manifest": receipt.observed_manifest
        }
    })
}

pub(crate) fn lock_workspace(
    file: &File,
    workspace_uri: &str,
    timeout_text: &str,
) -> Result<(), ContractFailure> {
    let timeout = parse_duration(timeout_text).unwrap_or(Duration::from_secs(30));
    let deadline = Instant::now() + timeout;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(()),
            Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(20));
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(ContractFailure {
                    exit_code: 4,
                    category: "unavailable",
                    code: "workspace_lock_timeout",
                    message: "The Workspace Application lock timed out.".to_owned(),
                    stage: "lock_workspace",
                    delivery: "not_applicable",
                    retry: "safe",
                    data: json!({"workspaceUri": workspace_uri, "timeout": timeout_text}),
                });
            }
            Err(std::fs::TryLockError::Error(error)) => {
                return Err(ContractFailure {
                    exit_code: 4,
                    category: "unavailable",
                    code: "state_unavailable",
                    message: "The Workspace Application lock failed.".to_owned(),
                    stage: "persist",
                    delivery: "not_applicable",
                    retry: "after_change",
                    data: json!({"recordType": "transaction", "path": "", "osCode": error.raw_os_error()}),
                });
            }
        }
    }
}

fn lock_workspace_for_application(
    file: &File,
    workspace_uri: &str,
    timeout_text: &str,
    caller_deadline: Option<Instant>,
    preview_id: &str,
) -> Result<(), ContractFailure> {
    let timeout = parse_duration(timeout_text).unwrap_or(Duration::from_secs(30));
    let lock_deadline = Instant::now() + timeout;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(()),
            Err(std::fs::TryLockError::WouldBlock)
                if caller_deadline.is_some_and(|deadline| Instant::now() >= deadline) =>
            {
                return Err(application_cancelled(preview_id));
            }
            Err(std::fs::TryLockError::WouldBlock) if Instant::now() < lock_deadline => {
                thread::sleep(Duration::from_millis(20));
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(ContractFailure {
                    exit_code: 4,
                    category: "unavailable",
                    code: "workspace_lock_timeout",
                    message: "The Workspace Application lock timed out.".to_owned(),
                    stage: "lock_workspace",
                    delivery: "not_applicable",
                    retry: "safe",
                    data: json!({"workspaceUri": workspace_uri, "timeout": timeout_text}),
                });
            }
            Err(std::fs::TryLockError::Error(error)) => {
                return Err(ContractFailure {
                    exit_code: 4,
                    category: "unavailable",
                    code: "state_unavailable",
                    message: "The Workspace Application lock failed.".to_owned(),
                    stage: "persist",
                    delivery: "not_applicable",
                    retry: "after_change",
                    data: json!({"recordType": "transaction", "path": "", "osCode": error.raw_os_error()}),
                });
            }
        }
    }
}

fn deadline_expired(deadline: Option<Instant>) -> bool {
    deadline.is_some_and(|deadline| Instant::now() >= deadline)
}

fn application_cancelled(preview_id: &str) -> ContractFailure {
    ContractFailure {
        exit_code: 6,
        category: "mutation",
        code: "application_cancelled",
        message: "The caller deadline expired before Application commit began.".to_owned(),
        stage: "lock_workspace",
        delivery: "not_applicable",
        retry: "safe",
        data: json!({"previewId": preview_id}),
    }
}

fn parse_duration(value: &str) -> Option<Duration> {
    if let Some(value) = value.strip_suffix("ms") {
        value.parse().ok().map(Duration::from_millis)
    } else if let Some(value) = value.strip_suffix('s') {
        value.parse().ok().map(Duration::from_secs)
    } else if let Some(value) = value.strip_suffix('m') {
        value
            .parse::<u64>()
            .ok()
            .map(|value| Duration::from_secs(value.saturating_mul(60)))
    } else {
        None
    }
}

fn parse_position_encoding(value: &str) -> PositionEncoding {
    if value == "utf-16" {
        PositionEncoding::Utf16
    } else {
        PositionEncoding::Utf8
    }
}

fn recovery_required_failure(transaction: &TransactionRecord) -> ContractFailure {
    ContractFailure {
        exit_code: 7,
        category: "recovery",
        code: "recovery_required",
        message: "Workspace Recovery is required before another Application can run.".to_owned(),
        stage: "recover",
        delivery: "not_applicable",
        retry: "after_change",
        data: json!({
            "transactionId": transaction.transaction_id,
            "receiptId": transaction.receipt_id,
            "manifestDigest": transaction.manifest_digest,
            "intended": transaction.intended_manifest,
            "observed": transaction.observed_manifest
        }),
    }
}

fn transaction_id_from_evidence(evidence: &Value) -> String {
    evidence
        .get("transactionId")
        .and_then(Value::as_str)
        .unwrap_or("txn_00000000000000000000000000000000")
        .to_owned()
}

fn unsupported_filesystem_failure(
    workspace_uri: &str,
    problems: &[WorkspaceEditProblem],
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

fn stage_failure(transaction_id: &str, message: &str, os_code: Option<i32>) -> ContractFailure {
    let mut data =
        json!({"recordType": "transaction", "path": "", "transactionId": transaction_id});
    if let Some(os_code) = os_code {
        data["osCode"] = json!(os_code);
    }
    ContractFailure {
        exit_code: 4,
        category: "unavailable",
        code: "state_unavailable",
        message: message.to_owned(),
        stage: "persist",
        delivery: "not_applicable",
        retry: "after_change",
        data,
    }
}

fn durability_value() -> Value {
    json!({
        "fileFlush": "complete",
        "directoryFlush": if cfg!(unix) { "complete" } else { "unsupported" }
    })
}

fn flush_parent(path: &Path) -> std::io::Result<()> {
    path.parent().map_or(Ok(()), flush_directory)
}

#[cfg(unix)]
fn flush_directory(path: &Path) -> std::io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn flush_directory(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs::OpenOptions;

    use tempfile::TempDir;

    use super::*;
    use crate::mutation::{PreviewRecordContext, create_preview_record};

    fn test_application_context<'a>(
        store: &'a MutationStateStore,
        previews: &'a PreviewSettings,
        receipts: &'a ReceiptSettings,
        mutation: &'a MutationSettings,
    ) -> ApplicationContext<'a> {
        ApplicationContext {
            store,
            preview_limits: previews,
            receipt_limits: receipts,
            mutation_limits: mutation,
            reauthorize: None,
            post_commit: None,
            preauthorized: false,
            caller_deadline: None,
        }
    }

    fn persist_test_preview(
        context: &ApplicationContext<'_>,
        workspace: &Path,
        edit: Value,
        planned: super::super::planner::PlannedWorkspaceEdit,
    ) -> String {
        let id = context.store.new_preview_id().unwrap();
        let record = create_preview_record(
            PreviewRecordContext {
                preview_id: &id,
                workspace_uri: url::Url::from_directory_path(workspace).unwrap().as_str(),
                server: None,
                session_identity: &format!("sid_{}", "0".repeat(64)),
                position_encoding: "utf-8",
                source: json!({"kind": "test"}),
                edit,
                command: None,
            },
            planned,
        );
        context
            .store
            .create_preview(
                record,
                workspace.to_path_buf(),
                "sha256:test".to_owned(),
                None,
                context.preview_limits,
            )
            .unwrap();
        id
    }

    #[test]
    fn directory_membership_rejects_added_descendant() {
        for kind in ["delete", "rename"] {
            let workspace = TempDir::new().unwrap();
            let state = TempDir::new().unwrap();
            let source = workspace.path().join("source");
            fs::create_dir_all(source.join("nested")).unwrap();
            let old = source.join("nested/old.txt");
            let added = source.join("nested/added.txt");
            fs::write(&old, b"previewed").unwrap();
            let store = MutationStateStore::open_at(state.path().join("state")).unwrap();
            let (previews, receipts, mutation) = super::super::default_mutation_settings();
            let mut context = test_application_context(&store, &previews, &receipts, &mutation);
            let uri = url::Url::from_file_path(&source).unwrap();
            let operation = if kind == "delete" {
                json!({"kind": kind, "uri": uri, "options": {"recursive": true}})
            } else {
                json!({"kind": kind, "oldUri": uri,
                    "newUri": url::Url::from_file_path(workspace.path().join("moved")).unwrap()})
            };
            let edit = json!({"documentChanges": [operation]});
            let planner = WorkspaceEditPlanner::open(
                workspace.path(),
                PositionEncoding::Utf8,
                &previews,
                &mutation,
            )
            .unwrap();
            let planned = planner.plan_workspace_edit(&edit).unwrap();
            let id = persist_test_preview(&context, workspace.path(), edit, planned);
            fs::write(&added, b"not previewed").unwrap();

            let failure = apply_preview(&mut context, &id).unwrap_err();

            assert_eq!(failure.code, "preview_stale", "{kind}");
            assert_eq!(fs::read(&old).unwrap(), b"previewed", "{kind}");
            assert_eq!(fs::read(&added).unwrap(), b"not previewed", "{kind}");
            assert!(store.list_transactions().unwrap().is_empty());
            assert!(store.list_receipts().unwrap().is_empty());
            assert!(!store.read_preview(&id).unwrap().preview.reserved);
        }
    }

    #[test]
    fn directory_membership_legacy_preview_requires_recreation() {
        for kind in ["delete", "rename", "overwrite", "text"] {
            let workspace = TempDir::new().unwrap();
            let state = TempDir::new().unwrap();
            let source = workspace.path().join("source");
            let destination = workspace.path().join("destination");
            fs::create_dir_all(source.join("nested")).unwrap();
            fs::create_dir_all(destination.join("nested")).unwrap();
            let file = source.join("nested/old.txt");
            fs::write(&file, b"old").unwrap();
            fs::write(destination.join("nested/extra.txt"), b"extra").unwrap();
            let store = MutationStateStore::open_at(state.path().join("state")).unwrap();
            let (previews, receipts, mutation) = super::super::default_mutation_settings();
            let mut context = test_application_context(&store, &previews, &receipts, &mutation);
            let operation = match kind {
                "text" => {
                    json!({"textDocument": {"uri": url::Url::from_file_path(&file).unwrap(), "version": null},
                    "edits": [{"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 3}}, "newText": "new"}]})
                }
                "delete" => {
                    json!({"kind": "delete", "uri": url::Url::from_file_path(&source).unwrap(), "options": {"recursive": true}})
                }
                _ => json!({"kind": "rename", "oldUri": url::Url::from_file_path(&source).unwrap(),
                    "newUri": url::Url::from_file_path(if kind == "overwrite" { destination.clone() } else { workspace.path().join("moved") }).unwrap(),
                    "options": {"overwrite": kind == "overwrite"}}),
            };
            let edit = json!({"documentChanges": [operation]});
            let planner = WorkspaceEditPlanner::open(
                workspace.path(),
                PositionEncoding::Utf8,
                &previews,
                &mutation,
            )
            .unwrap();
            let mut planned = planner.plan_workspace_edit(&edit).unwrap();
            for entry in planned
                .plan
                .before_manifest
                .iter_mut()
                .chain(&mut planned.plan.intended_manifest)
            {
                if entry.resource_kind == ResourceKind::Directory {
                    entry.content_digest = None;
                }
            }
            let id = persist_test_preview(&context, workspace.path(), edit, planned);
            let immutable =
                serde_json::to_value(&store.read_preview(&id).unwrap().preview.plan).unwrap();
            if kind == "text" {
                assert_eq!(
                    apply_preview(&mut context, &id).unwrap()["outcome"],
                    "applied"
                );
                assert_eq!(fs::read(&file).unwrap(), b"new");
            } else {
                let failure = apply_preview(&mut context, &id).unwrap_err();
                assert_eq!(failure.code, "preview_stale", "{kind}");
                assert_eq!(fs::read(&file).unwrap(), b"old");
                assert_eq!(
                    fs::read(destination.join("nested/extra.txt")).unwrap(),
                    b"extra"
                );
                assert!(store.list_transactions().unwrap().is_empty());
                assert!(store.list_receipts().unwrap().is_empty());
                assert_eq!(
                    serde_json::to_value(&store.read_preview(&id).unwrap().preview.plan).unwrap(),
                    immutable
                );
            }
        }
    }

    #[test]
    fn directory_membership_covers_overwritten_destination() {
        let workspace = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        let source = workspace.path().join("source");
        let destination = workspace.path().join("destination");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("old.txt"), b"source").unwrap();
        fs::create_dir_all(destination.join("nested")).unwrap();
        let extra = destination.join("nested/extra.txt");
        fs::write(&extra, b"previewed").unwrap();
        let store = MutationStateStore::open_at(state.path().join("state")).unwrap();
        let (previews, receipts, mutation) = super::super::default_mutation_settings();
        let mut context = test_application_context(&store, &previews, &receipts, &mutation);
        let edit = json!({"documentChanges": [{"kind": "rename",
            "oldUri": url::Url::from_file_path(&source).unwrap(),
            "newUri": url::Url::from_file_path(&destination).unwrap(), "options": {"overwrite": true}}]});
        let planner = WorkspaceEditPlanner::open(
            workspace.path(),
            PositionEncoding::Utf8,
            &previews,
            &mutation,
        )
        .unwrap();
        let id = persist_test_preview(
            &context,
            workspace.path(),
            edit.clone(),
            planner.plan_workspace_edit(&edit).unwrap(),
        );
        fs::write(&extra, b"external change").unwrap();
        assert_eq!(
            apply_preview(&mut context, &id).unwrap_err().code,
            "preview_stale"
        );
        assert_eq!(fs::read(source.join("old.txt")).unwrap(), b"source");
        assert_eq!(fs::read(&extra).unwrap(), b"external change");
        assert!(store.list_transactions().unwrap().is_empty());
        assert!(store.list_receipts().unwrap().is_empty());
    }

    #[test]
    fn directory_membership_intended_tree_matches_ordered_operations() {
        let workspace = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        let source = workspace.path().join("source");
        let destination = workspace.path().join("destination");
        let final_path = workspace.path().join("final");
        fs::create_dir_all(source.join("nested")).unwrap();
        fs::write(source.join("nested/old.txt"), b"old").unwrap();
        fs::write(source.join("nested/keep.txt"), b"keep").unwrap();
        fs::create_dir_all(destination.join("nested")).unwrap();
        fs::write(destination.join("nested/extra.txt"), b"extra").unwrap();
        let uri = |path: &Path| url::Url::from_file_path(path).unwrap();
        let edit = json!({"documentChanges": [
            {"kind": "rename", "oldUri": uri(&source), "newUri": uri(&destination), "options": {"overwrite": true}},
            {"kind": "delete", "uri": uri(&destination.join("nested/old.txt"))},
            {"kind": "create", "uri": uri(&destination.join("nested/new.txt"))},
            {"kind": "rename", "oldUri": uri(&destination.join("nested/keep.txt")), "newUri": uri(&destination.join("nested/renamed.txt"))},
            {"kind": "delete", "uri": uri(&source.join("nested/old.txt")), "options": {"ignoreIfNotExists": true}},
            {"kind": "rename", "oldUri": uri(&destination), "newUri": uri(&final_path)}
        ]});
        let store = MutationStateStore::open_at(state.path().join("state")).unwrap();
        let (previews, receipts, mutation) = super::super::default_mutation_settings();
        let mut context = test_application_context(&store, &previews, &receipts, &mutation);
        let planner = WorkspaceEditPlanner::open(
            workspace.path(),
            PositionEncoding::Utf8,
            &previews,
            &mutation,
        )
        .unwrap();
        let planned = planner.plan_workspace_edit(&edit).unwrap();
        assert_eq!(planned.plan.operations.len(), 5);
        let intended = planned.plan.intended_manifest.clone();
        let id = persist_test_preview(&context, workspace.path(), edit, planned);
        let first = apply_preview(&mut context, &id).unwrap();
        assert_eq!(first["outcome"], "applied");
        assert_eq!(
            apply_preview(&mut context, &id).unwrap()["outcome"],
            "already_applied"
        );
        assert_eq!(
            fs::read(final_path.join("nested/renamed.txt")).unwrap(),
            b"keep"
        );
        assert_eq!(fs::read(final_path.join("nested/new.txt")).unwrap(), b"");
        assert!(!final_path.join("nested/old.txt").exists());
        assert!(!final_path.join("nested/extra.txt").exists());
        assert!(!source.exists());
        assert!(!destination.exists());
        let observed = planner.inspect_manifest(&intended).unwrap();
        assert!(manifest_mismatches(&intended, &observed).is_empty());
        for entry in intended
            .iter()
            .filter(|entry| entry.resource_kind == ResourceKind::Directory)
        {
            assert!(entry.content_digest.is_some());
        }
        assert_eq!(store.list_receipts().unwrap().len(), 1);
        assert!(store.list_transactions().unwrap().is_empty());
    }

    #[test]
    fn directory_membership_legacy_recovery_preserves_digest_vocabulary() {
        for (committed, certified) in [(false, false), (true, false), (false, true), (true, true)] {
            for accept in [false, true] {
                let workspace = TempDir::new().unwrap();
                let state = TempDir::new().unwrap();
                let source = workspace.path().join("source");
                let destination = workspace.path().join("destination");
                fs::create_dir_all(source.join("nested")).unwrap();
                fs::write(source.join("nested/old.txt"), b"old").unwrap();
                let store = MutationStateStore::open_at(state.path().join("state")).unwrap();
                let (previews, receipts, mutation) = super::super::default_mutation_settings();
                let planner = WorkspaceEditPlanner::open(
                    workspace.path(),
                    PositionEncoding::Utf8,
                    &previews,
                    &mutation,
                )
                .unwrap();
                let mut planned = planner
                    .plan_workspace_edit(&json!({"documentChanges": [{"kind": "rename",
                    "oldUri": url::Url::from_file_path(&source).unwrap(),
                    "newUri": url::Url::from_file_path(&destination).unwrap()}]}))
                    .unwrap();
                for entry in planned
                    .plan
                    .before_manifest
                    .iter_mut()
                    .chain(&mut planned.plan.intended_manifest)
                {
                    if !certified && entry.resource_kind == ResourceKind::Directory {
                        entry.content_digest = None;
                    }
                }
                let id = store.new_transaction_id().unwrap();
                let preview_id = store.new_preview_id().unwrap();
                let artifact_directory = workspace.path().join(format!(".lspctl-{id}"));
                let transaction = TransactionRecord {
                    format_version: MUTATION_STATE_VERSION,
                    transaction_id: id.clone(),
                    preview_id: preview_id.clone(),
                    receipt_id: preview_id,
                    workspace_path: workspace.path().to_path_buf(),
                    workspace_uri: url::Url::from_directory_path(workspace.path())
                        .unwrap()
                        .to_string(),
                    state: if committed {
                        TransactionState::Committing
                    } else {
                        TransactionState::Staged
                    },
                    started_at: now_rfc3339(),
                    backups: planned_backups(&planned.plan.before_manifest, &artifact_directory),
                    artifact_directory,
                    operations: planned.plan.operations,
                    manifest_digest: manifest_digest(&planned.plan.before_manifest),
                    observed_manifest: planned.plan.before_manifest.clone(),
                    before_manifest: planned.plan.before_manifest,
                    intended_manifest: planned.plan.intended_manifest,
                    cleanup_pending: false,
                };
                store.write_transaction(&transaction).unwrap();
                stage_transaction(&transaction, &transaction.operations, &mutation).unwrap();
                if committed {
                    commit_operations(&planner, &transaction).unwrap();
                }
                let original_digest = transaction.manifest_digest.clone();
                let reconciled =
                    reconcile_recovery_status(&store, transaction, &previews, &mutation)
                        .unwrap()
                        .unwrap();
                if !committed {
                    assert_eq!(reconciled.manifest_digest, original_digest);
                }
                assert!(
                    reconciled
                        .observed_manifest
                        .iter()
                        .filter(|entry| entry.resource_kind == ResourceKind::Directory)
                        .all(|entry| entry.content_digest.is_some() == certified)
                );
                let recover = || {
                    if accept {
                        recover_accept_current(
                            &store,
                            &id,
                            &reconciled.manifest_digest,
                            &previews,
                            &receipts,
                            &mutation,
                        )
                    } else {
                        recover_rollback(
                            &store,
                            &id,
                            &reconciled.manifest_digest,
                            &previews,
                            &receipts,
                            &mutation,
                        )
                    }
                };
                if certified {
                    let retained = if committed { &destination } else { &source };
                    let added = retained.join("nested/added.txt");
                    fs::write(&added, b"external").unwrap();
                    assert_eq!(recover().unwrap_err().code, "recovery_conflict");
                    assert_eq!(fs::read(&added).unwrap(), b"external");
                    assert_eq!(fs::read(retained.join("nested/old.txt")).unwrap(), b"old");
                    fs::remove_file(&added).unwrap();
                }
                let result = recover().unwrap();
                assert_eq!(
                    result["outcome"],
                    if accept {
                        "accepted_current"
                    } else {
                        "restored"
                    }
                );
                let retained = if accept && committed {
                    &destination
                } else {
                    &source
                };
                assert_eq!(fs::read(retained.join("nested/old.txt")).unwrap(), b"old");
                let receipt = &store.list_receipts().unwrap()[0].receipt;
                assert!(
                    receipt
                        .observed_manifest
                        .iter()
                        .filter(|entry| entry.resource_kind == ResourceKind::Directory)
                        .all(|entry| entry.content_digest.is_some() == certified)
                );
                assert!(store.list_transactions().unwrap().is_empty());
            }
        }
    }

    #[test]
    fn directory_membership_preserves_ignored_destination() {
        let workspace = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        let source = workspace.path().join("source");
        let destination = workspace.path().join("destination");
        let file = workspace.path().join("main.txt");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&destination).unwrap();
        fs::write(&file, b"old").unwrap();
        for number in 0..5 {
            fs::write(destination.join(format!("extra-{number}")), b"untouched").unwrap();
        }
        let store = MutationStateStore::open_at(state.path().join("state")).unwrap();
        let (previews, receipts, mut mutation) = super::super::default_mutation_settings();
        mutation.max_entries = 4;
        let mut context = test_application_context(&store, &previews, &receipts, &mutation);
        let uri = |path: &Path| url::Url::from_file_path(path).unwrap();
        let edit = json!({"documentChanges": [
            {"kind": "rename", "oldUri": uri(&source), "newUri": uri(&destination), "options": {"ignoreIfExists": true}},
            {"textDocument": {"uri": uri(&file), "version": null}, "edits": [{
                "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 3}}, "newText": "new"
            }]}
        ]});
        let planner = WorkspaceEditPlanner::open(
            workspace.path(),
            PositionEncoding::Utf8,
            &previews,
            &mutation,
        )
        .unwrap();
        let planned = planner.plan_workspace_edit(&edit).unwrap();
        assert_eq!(planned.plan.operations.len(), 1);
        let id = persist_test_preview(&context, workspace.path(), edit, planned);
        let result = apply_preview(&mut context, &id).unwrap();
        assert_eq!(result["outcome"], "applied");
        assert_eq!(fs::read(&file).unwrap(), b"new");
        for number in 0..5 {
            assert_eq!(
                fs::read(destination.join(format!("extra-{number}"))).unwrap(),
                b"untouched"
            );
        }
        assert!(source.is_dir());
        assert!(store.list_transactions().unwrap().is_empty());
    }

    fn provenance_transaction(
        workspace: &Path,
        planner: &WorkspaceEditPlanner<'_>,
        edit: Value,
    ) -> TransactionRecord {
        let plan = planner.plan_workspace_edit(&edit).unwrap().plan;
        let id = "txn_00000000000000000000000000000000";
        let artifact_directory = workspace.join(format!(".lspctl-{id}"));
        TransactionRecord {
            format_version: MUTATION_STATE_VERSION,
            transaction_id: id.to_owned(),
            preview_id: "prv_00000000000000000000000000000000".to_owned(),
            receipt_id: "prv_00000000000000000000000000000000".to_owned(),
            workspace_path: workspace.to_path_buf(),
            workspace_uri: url::Url::from_directory_path(workspace)
                .unwrap()
                .to_string(),
            state: TransactionState::Committing,
            started_at: now_rfc3339(),
            backups: planned_backups(&plan.before_manifest, &artifact_directory),
            artifact_directory,
            operations: plan.operations,
            manifest_digest: manifest_digest(&plan.before_manifest),
            observed_manifest: plan.before_manifest.clone(),
            before_manifest: plan.before_manifest,
            intended_manifest: plan.intended_manifest,
            cleanup_pending: false,
        }
    }

    fn text_change(path: &Path, before_len: u64, text: &str) -> Value {
        json!({"textDocument": {"uri": url::Url::from_file_path(path).unwrap(), "version": null},
            "edits": [{"range": {"start": {"line": 0, "character": 0},
                "end": {"line": 0, "character": before_len}}, "newText": text}]})
    }

    fn assert_unproven_recovery_preserves_workspace(
        transaction: &TransactionRecord,
        planner: &WorkspaceEditPlanner<'_>,
    ) {
        let state = TempDir::new().unwrap();
        let store = MutationStateStore::open_at(state.path().join("state")).unwrap();
        let (previews, receipts, mutation) = super::super::default_mutation_settings();
        let observed = inspect_transaction_manifest(planner, transaction).unwrap();
        store.write_transaction(transaction).unwrap();
        let reconciled = reconcile_recovery_status(
            &store,
            store.read_transaction(&transaction.transaction_id).unwrap(),
            &previews,
            &mutation,
        )
        .unwrap()
        .unwrap();
        assert_eq!(reconciled.state, TransactionState::RecoveryRequired);
        assert_eq!(reconciled.observed_manifest, observed);
        assert_eq!(
            recover_rollback(
                &store,
                &reconciled.transaction_id,
                &reconciled.manifest_digest,
                &previews,
                &receipts,
                &mutation,
            )
            .unwrap_err()
            .code,
            "recovery_failed"
        );
        assert_eq!(
            inspect_transaction_manifest(planner, transaction).unwrap(),
            observed
        );
        assert!(transaction.artifact_directory.exists());
        assert_eq!(
            store
                .read_transaction(&transaction.transaction_id)
                .unwrap()
                .state,
            TransactionState::RecoveryRequired
        );
        let target = transaction.workspace_path.join("blocked-create");
        let edit = json!({"documentChanges": [{"kind": "create", "uri": url::Url::from_file_path(&target).unwrap()}]});
        let mut context = test_application_context(&store, &previews, &receipts, &mutation);
        let id = persist_test_preview(
            &context,
            &transaction.workspace_path,
            edit.clone(),
            planner.plan_workspace_edit(&edit).unwrap(),
        );
        assert_eq!(
            apply_preview(&mut context, &id).unwrap_err().code,
            "recovery_required"
        );
        assert!(!target.exists());
        assert!(store.list_receipts().unwrap().is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn rollback_provenance_windows_preserves_external_acl_change() {
        use std::{os::windows::io::AsRawHandle, ptr};
        use windows_sys::Win32::{
            Security::{
                ACCESS_ALLOWED_ACE, ACE_HEADER, DACL_SECURITY_INFORMATION, GetAce,
                GetSecurityDescriptorDacl, SetKernelObjectSecurity,
            },
            Storage::FileSystem::FILE_WRITE_DATA,
            System::SystemServices::ACCESS_ALLOWED_ACE_TYPE,
        };
        let workspace = TempDir::new().unwrap();
        let source = workspace.path().join("source");
        fs::write(&source, b"preserve these bytes").unwrap();
        let (previews, _, mutation) = super::super::default_mutation_settings();
        let planner = WorkspaceEditPlanner::open(
            workspace.path(),
            PositionEncoding::Utf8,
            &previews,
            &mutation,
        )
        .unwrap();
        let transaction = provenance_transaction(
            workspace.path(),
            &planner,
            json!({"documentChanges": [{"kind": "delete", "uri": url::Url::from_file_path(&source).unwrap()}]}),
        );
        stage_transaction(&transaction, &transaction.operations, &mutation).unwrap();
        let handle = retain_move_handle(
            &planner,
            manifest_for_path(&transaction.before_manifest, &source).unwrap(),
        )
        .unwrap();
        let undo = undo_resource_path(
            &transaction.artifact_directory,
            operation_index(&transaction.operations[0]),
        );
        planner
            .capability_root()
            .rename(
                planner.relative_path(&source).unwrap(),
                planner.capability_root(),
                planner.relative_path(&undo).unwrap(),
            )
            .unwrap();

        // Change an actual permission, not the inheritance marker altered by the move.
        let mut external = move_security_descriptor(&handle.file).unwrap();
        let mut present = 0;
        let mut defaulted = 0;
        let mut dacl = ptr::null_mut();
        assert_ne!(
            unsafe {
                GetSecurityDescriptorDacl(
                    external.as_mut_ptr().cast(),
                    &mut present,
                    &mut dacl,
                    &mut defaulted,
                )
            },
            0
        );
        assert_ne!(present, 0);
        assert!(!dacl.is_null());
        let mut changed = false;
        for index in 0..u32::from(unsafe { (*dacl).AceCount }) {
            let mut ace = ptr::null_mut();
            assert_ne!(unsafe { GetAce(dacl, index, &mut ace) }, 0);
            if u32::from(unsafe { (*ace.cast::<ACE_HEADER>()).AceType }) == ACCESS_ALLOWED_ACE_TYPE
            {
                unsafe {
                    (*ace.cast::<ACCESS_ALLOWED_ACE>()).Mask ^= FILE_WRITE_DATA;
                }
                changed = true;
                break;
            }
        }
        assert!(changed, "the isolated file must have an allowed ACE");
        assert_ne!(
            unsafe {
                SetKernelObjectSecurity(
                    handle.file.as_raw_handle(),
                    DACL_SECURITY_INFORMATION,
                    external.as_mut_ptr().cast(),
                )
            },
            0
        );
        let external = move_security_descriptor(&handle.file).unwrap();
        assert_ne!(
            without_inherited_ace_flags(external.clone()).unwrap(),
            without_inherited_ace_flags(handle.security_descriptor.clone()).unwrap()
        );
        let observed = inspect_manifest_path(&planner, &undo).unwrap();

        assert!(preserve_move_metadata(&handle).is_err());
        assert_eq!(move_security_descriptor(&handle.file).unwrap(), external);
        assert_eq!(inspect_manifest_path(&planner, &undo).unwrap(), observed);
        assert_eq!(fs::read(&undo).unwrap(), b"preserve these bytes");
        assert!(transaction.artifact_directory.exists());
    }

    #[cfg(windows)]
    #[test]
    fn rollback_provenance_windows_preserves_copy_and_move_metadata() {
        use std::{os::windows::fs::FileTimesExt, time::SystemTime};

        for directory in [false, true] {
            let workspace = TempDir::new().unwrap();
            let source = workspace.path().join("source");
            let destination = workspace.path().join("destination");
            for (path, seconds) in [(&source, 946_684_800), (&destination, 1_262_304_000)] {
                if directory {
                    fs::create_dir(path).unwrap();
                    fs::write(path.join("child"), b"content").unwrap();
                } else {
                    fs::write(path, b"content").unwrap();
                }
                // Distinct known creation times expose both copy loss and NTFS rename tunneling.
                open_file_for_timestamp_update(path)
                    .unwrap()
                    .set_times(
                        fs::FileTimes::new()
                            .set_created(SystemTime::UNIX_EPOCH + Duration::from_secs(seconds)),
                    )
                    .unwrap();
            }
            let (previews, _, mutation) = super::super::default_mutation_settings();
            let planner = WorkspaceEditPlanner::open(
                workspace.path(),
                PositionEncoding::Utf8,
                &previews,
                &mutation,
            )
            .unwrap();
            let transaction = provenance_transaction(
                workspace.path(),
                &planner,
                json!({"documentChanges": [{"kind": "rename",
                    "oldUri": url::Url::from_file_path(&source).unwrap(),
                    "newUri": url::Url::from_file_path(&destination).unwrap(),
                    "options": {"overwrite": true}}]}),
            );
            stage_transaction(&transaction, &transaction.operations, &mutation).unwrap();
            for before in transaction
                .before_manifest
                .iter()
                .filter(|entry| entry.exists)
            {
                let backup = backup_path_for(&transaction.backups, &before.path).unwrap();
                assert_eq!(
                    inspect_manifest_path(&planner, &backup)
                        .unwrap()
                        .metadata_digest,
                    before.metadata_digest,
                    "backup metadata: {}",
                    before.path.display()
                );
            }
            commit_operations(&planner, &transaction).unwrap();
            let restored = rollback_transaction(&transaction, &planner).unwrap();
            assert!(manifest_mismatches(&transaction.before_manifest, &restored).is_empty());
        }
    }

    #[test]
    fn rollback_provenance_rechecks_each_rename_leg() {
        for conflict in ["destination", "undo"] {
            let workspace = TempDir::new().unwrap();
            let source = workspace.path().join("source");
            let destination = workspace.path().join("destination");
            fs::write(&source, b"source").unwrap();
            fs::write(&destination, b"destination").unwrap();
            let (previews, _, mutation) = super::super::default_mutation_settings();
            let planner = WorkspaceEditPlanner::open(
                workspace.path(),
                PositionEncoding::Utf8,
                &previews,
                &mutation,
            )
            .unwrap();
            let transaction = provenance_transaction(
                workspace.path(),
                &planner,
                json!({"documentChanges": [{"kind": "rename",
                    "oldUri": url::Url::from_file_path(&source).unwrap(),
                    "newUri": url::Url::from_file_path(&destination).unwrap(),
                    "options": {"overwrite": true}}]}),
            );
            stage_transaction(&transaction, &transaction.operations, &mutation).unwrap();
            commit_operations(&planner, &transaction).unwrap();
            let undo = undo_resource_path(
                &transaction.artifact_directory,
                operation_index(&transaction.operations[0]),
            );
            let progress = read_progress(&transaction, &planner).unwrap();
            let proof = progress.effects[0].after.as_ref().unwrap();
            // The first undo leg completes; an external writer acts before the second.
            rename_capability_resource(&planner, &destination, &source, proof).unwrap();
            let changed = if conflict == "destination" {
                &destination
            } else {
                &undo
            };
            fs::write(changed, b"external").unwrap();
            let external = inspect_manifest_path(&planner, changed).unwrap();
            let result = rename_capability_resource(&planner, &undo, &destination, proof);
            assert!(result.is_err(), "the second rename must recheck {conflict}");
            assert_eq!(fs::read(changed).unwrap(), b"external");
            assert_eq!(inspect_manifest_path(&planner, changed).unwrap(), external);
            assert_eq!(fs::read(&source).unwrap(), b"source");
            assert!(transaction.artifact_directory.exists());
        }
    }

    #[test]
    fn rollback_provenance_preserves_unwritten_text() {
        for after_first in [false, true] {
            let workspace = TempDir::new().unwrap();
            let first = workspace.path().join("first");
            let second = workspace.path().join("second");
            fs::write(&first, b"old").unwrap();
            fs::write(&second, b"old").unwrap();
            let (previews, _, mutation) = super::super::default_mutation_settings();
            let planner = WorkspaceEditPlanner::open(
                workspace.path(),
                PositionEncoding::Utf8,
                &previews,
                &mutation,
            )
            .unwrap();
            let transaction = provenance_transaction(
                workspace.path(),
                &planner,
                json!({"documentChanges": [text_change(&first, 3, "new"), text_change(&second, 3, "new")]}),
            );
            stage_transaction(&transaction, &transaction.operations, &mutation).unwrap();
            if !after_first {
                fs::write(&second, b"external").unwrap();
            }
            let result = commit_operations_with_writer(&planner, &transaction, |progress| {
                write_progress(&transaction, &planner, progress)?;
                if after_first && progress.effects.len() == 1 && progress.effects[0].after.is_some()
                {
                    fs::write(&second, b"external").unwrap();
                }
                Ok(())
            });
            assert!(result.is_err());
            let external = inspect_manifest_path(&planner, &second).unwrap();
            let result = rollback_transaction(&transaction, &planner);
            assert_eq!(fs::read(&second).unwrap(), b"external");
            assert_eq!(inspect_manifest_path(&planner, &second).unwrap(), external);
            assert!(result.is_err(), "external content requires Recovery");
            assert_eq!(
                fs::read(&first).unwrap(),
                if after_first { b"new" } else { b"old" }
            );
            assert_unproven_recovery_preserves_workspace(&transaction, &planner);
        }
    }

    #[test]
    fn rollback_provenance_preserves_uncreated_target() {
        let workspace = TempDir::new().unwrap();
        let target = workspace.path().join("created");
        let (previews, _, mutation) = super::super::default_mutation_settings();
        let planner = WorkspaceEditPlanner::open(
            workspace.path(),
            PositionEncoding::Utf8,
            &previews,
            &mutation,
        )
        .unwrap();
        let transaction = provenance_transaction(
            workspace.path(),
            &planner,
            json!({"documentChanges": [{"kind": "create", "uri": url::Url::from_file_path(&target).unwrap()}]}),
        );
        stage_transaction(&transaction, &transaction.operations, &mutation).unwrap();
        fs::write(&target, b"external").unwrap();
        let external = inspect_manifest_path(&planner, &target).unwrap();
        assert!(commit_operations(&planner, &transaction).is_err());
        let result = rollback_transaction(&transaction, &planner);
        assert_eq!(fs::read(&target).unwrap(), b"external");
        assert_eq!(inspect_manifest_path(&planner, &target).unwrap(), external);
        assert!(result.is_err());
        assert_unproven_recovery_preserves_workspace(&transaction, &planner);
    }

    #[test]
    fn rollback_provenance_preserves_postcommit_editor_change() {
        for (create, replace) in [(false, false), (false, true), (true, false), (true, true)] {
            let workspace = TempDir::new().unwrap();
            let target = workspace.path().join("target");
            if !create {
                fs::write(&target, b"old").unwrap();
            }
            let (previews, _, mutation) = super::super::default_mutation_settings();
            let planner = WorkspaceEditPlanner::open(
                workspace.path(),
                PositionEncoding::Utf8,
                &previews,
                &mutation,
            )
            .unwrap();
            let operation = if create {
                json!({"kind": "create", "uri": url::Url::from_file_path(&target).unwrap()})
            } else {
                text_change(&target, 3, "new")
            };
            let transaction = provenance_transaction(
                workspace.path(),
                &planner,
                json!({"documentChanges": [operation]}),
            );
            stage_transaction(&transaction, &transaction.operations, &mutation).unwrap();
            commit_operations(&planner, &transaction).unwrap();
            if replace {
                fs::rename(&target, workspace.path().join("replaced-original")).unwrap();
            }
            fs::write(&target, b"external").unwrap();
            let external = inspect_manifest_path(&planner, &target).unwrap();
            let result = rollback_transaction(&transaction, &planner);
            assert_eq!(fs::read(&target).unwrap(), b"external");
            assert_eq!(inspect_manifest_path(&planner, &target).unwrap(), external);
            assert!(result.is_err());
            assert_unproven_recovery_preserves_workspace(&transaction, &planner);
        }
    }

    #[test]
    fn rollback_provenance_progress_round_trips_and_rejects_invalid_evidence() {
        let workspace = TempDir::new().unwrap();
        let file = workspace.path().join("file");
        fs::write(&file, b"old").unwrap();
        let (previews, _, mutation) = super::super::default_mutation_settings();
        let planner = WorkspaceEditPlanner::open(
            workspace.path(),
            PositionEncoding::Utf8,
            &previews,
            &mutation,
        )
        .unwrap();
        let transaction = provenance_transaction(
            workspace.path(),
            &planner,
            json!({"documentChanges": [text_change(&file, 3, "new")]}),
        );
        stage_transaction(&transaction, &transaction.operations, &mutation).unwrap();
        commit_operations(&planner, &transaction).unwrap();
        let path = transaction.artifact_directory.join(PROGRESS_FILE);
        let bytes = fs::read(&path).unwrap();
        let valid: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            read_progress(&transaction, &planner).unwrap().effects.len(),
            1
        );
        for field in [
            "version",
            "transaction_id",
            "operations_digest",
            "before_digest",
            "unknown",
        ] {
            let mut invalid = valid.clone();
            invalid[field] = json!(42);
            fs::write(&path, serde_json::to_vec(&invalid).unwrap()).unwrap();
            assert!(read_progress(&transaction, &planner).is_err(), "{field}");
        }
        for invalid in [
            {
                let mut value = valid.clone();
                value["effects"][0]["index"] = json!(99);
                value
            },
            {
                let mut value = valid.clone();
                value["effects"]
                    .as_array_mut()
                    .unwrap()
                    .push(valid["effects"][0].clone());
                value
            },
            {
                let mut value = valid.clone();
                value["effects"][0]["after"][0]["path"] = json!("/outside");
                value
            },
            {
                let mut value = valid.clone();
                value["effects"][0]["after"][0]["contentDigest"] = json!("sha256:fake");
                value
            },
        ] {
            fs::write(&path, serde_json::to_vec(&invalid).unwrap()).unwrap();
            assert!(read_progress(&transaction, &planner).is_err());
            assert!(rollback_transaction(&transaction, &planner).is_err());
            assert_eq!(fs::read(&file).unwrap(), b"new");
        }
        File::create(&path)
            .unwrap()
            .set_len(progress_size_limit(&transaction).unwrap() + 1)
            .unwrap();
        assert!(read_progress(&transaction, &planner).is_err());
        fs::write(&path, &bytes).unwrap();
        fs::write(
            transaction.artifact_directory.join(ARTIFACT_OWNER_FILE),
            b"wrong owner",
        )
        .unwrap();
        assert!(read_progress(&transaction, &planner).is_err());
        fs::write(
            transaction.artifact_directory.join(ARTIFACT_OWNER_FILE),
            &transaction.transaction_id,
        )
        .unwrap();
        #[cfg(unix)]
        {
            fs::remove_file(&path).unwrap();
            let external = workspace.path().join("external-proof");
            fs::write(&external, &bytes).unwrap();
            std::os::unix::fs::symlink(&external, &path).unwrap();
            assert!(read_progress(&transaction, &planner).is_err());
            fs::remove_file(&path).unwrap();
            fs::write(&path, &bytes).unwrap();
        }
        assert!(read_progress(&transaction, &planner).is_ok());
        rollback_transaction(&transaction, &planner).unwrap();
        assert_eq!(fs::read(&file).unwrap(), b"old");
    }

    #[test]
    fn rollback_provenance_uncertain_commit_requires_recovery() {
        for boundary in ["before_effect", "after_effect", "after_durability"] {
            let workspace = TempDir::new().unwrap();
            let state = TempDir::new().unwrap();
            let file = workspace.path().join("file");
            fs::write(&file, b"old").unwrap();
            let (previews, receipts, mutation) = super::super::default_mutation_settings();
            let planner = WorkspaceEditPlanner::open(
                workspace.path(),
                PositionEncoding::Utf8,
                &previews,
                &mutation,
            )
            .unwrap();
            let transaction = provenance_transaction(
                workspace.path(),
                &planner,
                json!({"documentChanges": [text_change(&file, 3, "new")]}),
            );
            let store = MutationStateStore::open_at(state.path().join("state")).unwrap();
            store.write_transaction(&transaction).unwrap();
            stage_transaction(&transaction, &transaction.operations, &mutation).unwrap();
            let result = commit_operations_with_writer(&planner, &transaction, |progress| {
                let completed = progress.effects.last().unwrap().after.is_some();
                if (!completed && boundary == "before_effect")
                    || (completed && boundary == "after_effect")
                {
                    if !completed {
                        write_progress(&transaction, &planner, progress)?;
                    }
                    return Err("simulated interrupted progress write".to_owned());
                }
                write_progress(&transaction, &planner, progress)?;
                if completed && boundary == "after_durability" {
                    return Err("simulated crash after durable completion".to_owned());
                }
                Ok(())
            });
            assert!(result.is_err());
            let id = transaction.transaction_id.clone();
            let artifacts = transaction.artifact_directory.clone();
            let transaction = reconcile_recovery_status(
                &store,
                store.read_transaction(&id).unwrap(),
                &previews,
                &mutation,
            )
            .unwrap()
            .unwrap();
            assert_eq!(transaction.state, TransactionState::RecoveryRequired);
            let result = recover_rollback(
                &store,
                &id,
                &transaction.manifest_digest,
                &previews,
                &receipts,
                &mutation,
            );
            if boundary == "after_effect" {
                assert_eq!(result.unwrap_err().code, "recovery_failed");
                assert_eq!(fs::read(&file).unwrap(), b"new");
                assert!(artifacts.exists());
                assert_eq!(
                    store.read_transaction(&id).unwrap().state,
                    TransactionState::RecoveryRequired
                );
            } else {
                assert_eq!(result.unwrap()["outcome"], "restored");
                assert_eq!(fs::read(&file).unwrap(), b"old");
                assert!(!artifacts.exists());
                assert!(store.list_transactions().unwrap().is_empty());
            }
        }
    }

    #[test]
    fn rollback_provenance_restores_completed_ordered_effects() {
        for case in [
            "text",
            "repeated",
            "create",
            "overwrite",
            "rename_overwrite",
            "delete",
            "create_text_rename",
            "text_rename_text",
            "delete_create",
            "directory",
        ] {
            let workspace = TempDir::new().unwrap();
            let first = workspace.path().join("first");
            let second = workspace.path().join("second");
            if case != "create" && case != "create_text_rename" {
                fs::write(&first, b"old").unwrap();
            }
            if case == "rename_overwrite" {
                fs::write(&second, b"destination").unwrap();
            }
            if case == "directory" {
                fs::remove_file(&first).unwrap();
                fs::create_dir_all(first.join("nested")).unwrap();
                fs::write(first.join("nested/file"), b"old").unwrap();
                fs::create_dir(&second).unwrap();
                fs::write(second.join("other"), b"other").unwrap();
            }
            let uri = |path: &Path| url::Url::from_file_path(path).unwrap();
            let create =
                json!({"kind": "create", "uri": uri(&first), "options": {"overwrite": true}});
            let rename = json!({"kind": "rename", "oldUri": uri(&first), "newUri": uri(&second), "options": {"overwrite": true}});
            let delete = json!({"kind": "delete", "uri": uri(&first)});
            let operations = match case {
                "text" => vec![text_change(&first, 3, "new")],
                "repeated" => vec![text_change(&first, 3, "one"), text_change(&first, 3, "two")],
                "create" | "overwrite" => vec![create],
                "rename_overwrite" => vec![rename],
                "delete" => vec![delete],
                "create_text_rename" => vec![create, text_change(&first, 0, "new"), rename],
                "text_rename_text" => vec![
                    text_change(&first, 3, "one"),
                    rename,
                    text_change(&second, 3, "two"),
                ],
                "delete_create" => vec![delete, create, text_change(&first, 0, "new")],
                "directory" => vec![
                    rename,
                    json!({"kind": "delete", "uri": uri(&second.join("nested/file"))}),
                ],
                _ => unreachable!(),
            };
            let (previews, _, mutation) = super::super::default_mutation_settings();
            let planner = WorkspaceEditPlanner::open(
                workspace.path(),
                PositionEncoding::Utf8,
                &previews,
                &mutation,
            )
            .unwrap();
            let transaction = provenance_transaction(
                workspace.path(),
                &planner,
                json!({"documentChanges": operations}),
            );
            stage_transaction(&transaction, &transaction.operations, &mutation).unwrap();
            commit_operations(&planner, &transaction).unwrap();
            let restored = rollback_transaction(&transaction, &planner)
                .unwrap_or_else(|error| panic!("{case}: {error}"));
            assert!(
                manifest_mismatches(&transaction.before_manifest, &restored).is_empty(),
                "{case}"
            );
            if case == "create" || case == "create_text_rename" {
                assert!(!first.exists());
            } else if case == "directory" {
                assert_eq!(fs::read(first.join("nested/file")).unwrap(), b"old");
                assert_eq!(fs::read(second.join("other")).unwrap(), b"other");
            } else {
                assert_eq!(fs::read(&first).unwrap(), b"old");
            }
            if case == "rename_overwrite" {
                assert_eq!(fs::read(&second).unwrap(), b"destination");
            }
            cleanup_transaction_artifacts(&transaction).unwrap();
        }
    }

    #[test]
    fn rollback_provenance_legacy_journal_is_readable_but_not_authority() {
        for changed in [false, true] {
            let workspace = TempDir::new().unwrap();
            let state = TempDir::new().unwrap();
            let file = workspace.path().join("file");
            fs::write(&file, b"old").unwrap();
            let (previews, receipts, mutation) = super::super::default_mutation_settings();
            let planner = WorkspaceEditPlanner::open(
                workspace.path(),
                PositionEncoding::Utf8,
                &previews,
                &mutation,
            )
            .unwrap();
            let transaction = provenance_transaction(
                workspace.path(),
                &planner,
                json!({"documentChanges": [text_change(&file, 3, "new")]}),
            );
            let store = MutationStateStore::open_at(state.path().join("state")).unwrap();
            store.write_transaction(&transaction).unwrap();
            stage_transaction(&transaction, &transaction.operations, &mutation).unwrap();
            assert!(!transaction.artifact_directory.join(PROGRESS_FILE).exists());
            if changed {
                fs::write(&file, b"external").unwrap();
            }
            let external = inspect_manifest_path(&planner, &file).unwrap();
            let id = transaction.transaction_id.clone();
            let artifacts = transaction.artifact_directory.clone();
            let transaction = reconcile_recovery_status(
                &store,
                store.read_transaction(&id).unwrap(),
                &previews,
                &mutation,
            )
            .unwrap()
            .unwrap();
            let result = recover_rollback(
                &store,
                &id,
                &transaction.manifest_digest,
                &previews,
                &receipts,
                &mutation,
            );
            if changed {
                assert_eq!(result.unwrap_err().code, "recovery_failed");
                assert_eq!(fs::read(&file).unwrap(), b"external");
                assert_eq!(inspect_manifest_path(&planner, &file).unwrap(), external);
                assert!(artifacts.exists());
                assert_eq!(
                    store.read_transaction(&id).unwrap().state,
                    TransactionState::RecoveryRequired
                );
                let accepted = recover_accept_current(
                    &store,
                    &id,
                    &transaction.manifest_digest,
                    &previews,
                    &receipts,
                    &mutation,
                )
                .unwrap();
                assert_eq!(accepted["outcome"], "accepted_current");
                assert_eq!(fs::read(&file).unwrap(), b"external");
            } else {
                assert_eq!(result.unwrap()["outcome"], "restored");
            }
            assert!(!artifacts.exists());
            assert!(store.list_transactions().unwrap().is_empty());
        }
    }

    #[test]
    fn exact_text_preview_applies_once_and_returns_same_receipt() {
        let workspace = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        let file = workspace.path().join("main.rs");
        fs::write(&file, "old\n").unwrap();
        let original_accessed =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        open_file_for_timestamp_update(&file)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_accessed(original_accessed))
            .unwrap();
        let store = MutationStateStore::open_at(state.path().join("state")).unwrap();
        let preview_limits = PreviewSettings {
            max_count: 64,
            max_total_bytes: 1_000_000,
            max_document_text_bytes: 1_000_000,
            max_text_bytes: 1_000_000,
        };
        let receipt_limits = ReceiptSettings { max_count: 100 };
        let mutation_limits = MutationSettings {
            application_lock_timeout: "1s".to_owned(),
            max_entries: 100,
            max_recursion_depth: 20,
            max_rollback_bytes: 1_000_000,
            max_staged_text_bytes: 1_000_000,
            max_preauthorized_callbacks: 64,
        };
        let workspace_uri = url::Url::from_directory_path(workspace.path())
            .unwrap()
            .to_string();
        let file_uri = url::Url::from_file_path(&file).unwrap().to_string();
        let planner = WorkspaceEditPlanner::open(
            workspace.path(),
            PositionEncoding::Utf8,
            &preview_limits,
            &mutation_limits,
        )
        .unwrap();
        let edit = json!({"changes": {file_uri: [{"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 3}}, "newText": "new"}]}});
        let planned = planner.plan_workspace_edit(&edit).unwrap();
        let id = store.new_preview_id().unwrap();
        let record = create_preview_record(
            PreviewRecordContext {
                preview_id: &id,
                workspace_uri: &workspace_uri,
                server: None,
                session_identity: &format!("sid_{}", "0".repeat(64)),
                position_encoding: "utf-8",
                source: json!({"kind": "test"}),
                edit,
                command: None,
            },
            planned,
        );
        store
            .create_preview(
                record,
                workspace.path().to_path_buf(),
                "sha256:test".to_owned(),
                None,
                &preview_limits,
            )
            .unwrap();
        let mut context = ApplicationContext {
            store: &store,
            preview_limits: &preview_limits,
            receipt_limits: &receipt_limits,
            mutation_limits: &mutation_limits,
            reauthorize: None,
            post_commit: None,
            preauthorized: false,
            caller_deadline: None,
        };
        let first = apply_preview(&mut context, &id).unwrap();
        let second = apply_preview(&mut context, &id).unwrap();
        assert_eq!(
            fs::metadata(workspace.path().join("main.rs"))
                .unwrap()
                .accessed()
                .unwrap(),
            original_accessed
        );
        assert_eq!(fs::read_to_string(file).unwrap(), "new\n");
        assert_eq!(first["outcome"], "applied");
        assert_eq!(second["outcome"], "already_applied");
    }

    #[test]
    fn caller_deadline_cancels_application_while_waiting_for_workspace_lock() {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join("application.lock");
        let holder = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        holder.lock().unwrap();
        let waiter = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let preview_id = format!("prv_{}", "1".repeat(32));
        let failure = lock_workspace_for_application(
            &waiter,
            "file:///workspace/",
            "1s",
            Some(Instant::now() + Duration::from_millis(10)),
            &preview_id,
        )
        .unwrap_err();
        assert_eq!(failure.code, "application_cancelled");
        assert_eq!(failure.data["previewId"], preview_id);
    }

    #[test]
    fn staging_never_cleans_an_unowned_artifact_path() {
        let workspace = TempDir::new().unwrap();
        let artifact_directory = workspace.path().join(".lspctl-collision");
        fs::create_dir(&artifact_directory).unwrap();
        let sentinel = artifact_directory.join("sentinel");
        fs::write(&sentinel, "owned by the workspace").unwrap();
        let transaction = TransactionRecord {
            format_version: MUTATION_STATE_VERSION,
            transaction_id: "txn_00000000000000000000000000000000".to_owned(),
            preview_id: "prv_00000000000000000000000000000000".to_owned(),
            receipt_id: "prv_00000000000000000000000000000000".to_owned(),
            workspace_path: workspace.path().to_path_buf(),
            workspace_uri: url::Url::from_directory_path(workspace.path())
                .unwrap()
                .to_string(),
            state: TransactionState::Staged,
            started_at: now_rfc3339(),
            artifact_directory,
            backups: Vec::new(),
            operations: Vec::new(),
            before_manifest: Vec::new(),
            intended_manifest: Vec::new(),
            observed_manifest: Vec::new(),
            manifest_digest: manifest_digest(&[]),
            cleanup_pending: false,
        };
        let failure = stage_transaction(
            &transaction,
            &[],
            &MutationSettings {
                application_lock_timeout: "1s".to_owned(),
                max_entries: 1,
                max_recursion_depth: 1,
                max_rollback_bytes: 1,
                max_staged_text_bytes: 1,
                max_preauthorized_callbacks: 1,
            },
        )
        .unwrap_err();

        assert!(!failure.artifact_created);
        assert!(cleanup_transaction_artifacts(&transaction).is_err());
        assert_eq!(
            fs::read_to_string(sentinel).unwrap(),
            "owned by the workspace"
        );
    }

    #[test]
    fn text_output_is_complete_private_staging_before_commit() {
        let workspace = TempDir::new().unwrap();
        let file = workspace.path().join("main.rs");
        fs::write(&file, "old\n").unwrap();
        let original_accessed =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        open_file_for_timestamp_update(&file)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_accessed(original_accessed))
            .unwrap();
        let preview_limits = PreviewSettings {
            max_count: 64,
            max_total_bytes: 1_000_000,
            max_document_text_bytes: 1_000_000,
            max_text_bytes: 1_000_000,
        };
        let mutation_limits = MutationSettings {
            application_lock_timeout: "1s".to_owned(),
            max_entries: 100,
            max_recursion_depth: 20,
            max_rollback_bytes: 1_000_000,
            max_staged_text_bytes: 1_000_000,
            max_preauthorized_callbacks: 64,
        };
        let workspace_uri = url::Url::from_directory_path(workspace.path())
            .unwrap()
            .to_string();
        let file_uri = url::Url::from_file_path(&file).unwrap().to_string();
        let planner = WorkspaceEditPlanner::open(
            workspace.path(),
            PositionEncoding::Utf8,
            &preview_limits,
            &mutation_limits,
        )
        .unwrap();
        let planned = planner
            .plan_workspace_edit(&json!({"changes": {file_uri: [{"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 3}}, "newText": "longer"}]}}))
            .unwrap();
        let transaction_id = "txn_00000000000000000000000000000000";
        let artifact_directory = workspace.path().join(format!(".lspctl-{transaction_id}"));
        let transaction = TransactionRecord {
            format_version: MUTATION_STATE_VERSION,
            transaction_id: transaction_id.to_owned(),
            preview_id: "prv_00000000000000000000000000000000".to_owned(),
            receipt_id: "prv_00000000000000000000000000000000".to_owned(),
            workspace_path: workspace.path().to_path_buf(),
            workspace_uri: workspace_uri.clone(),
            state: TransactionState::Staged,
            started_at: now_rfc3339(),
            artifact_directory: artifact_directory.clone(),
            backups: planned_backups(&planned.plan.before_manifest, &artifact_directory),
            operations: planned.plan.operations.clone(),
            before_manifest: planned.plan.before_manifest.clone(),
            intended_manifest: planned.plan.intended_manifest.clone(),
            observed_manifest: planned.plan.before_manifest.clone(),
            manifest_digest: manifest_digest(&planned.plan.before_manifest),
            cleanup_pending: false,
        };

        stage_transaction(&transaction, &transaction.operations, &mutation_limits).unwrap();

        assert_eq!(
            fs::read(staged_text_path(&artifact_directory, 0)).unwrap(),
            b"longer\n"
        );
        assert_eq!(
            fs::metadata(&file).unwrap().accessed().unwrap(),
            original_accessed
        );
        assert_eq!(fs::read(&file).unwrap(), b"old\n");
        cleanup_transaction_artifacts(&transaction).unwrap();
    }

    #[test]
    fn rollback_restores_resource_identities_after_rename_delete_and_create() {
        let workspace = TempDir::new().unwrap();
        let source = workspace.path().join("source.txt");
        let destination = workspace.path().join("destination.txt");
        let deleted = workspace.path().join("deleted.txt");
        let created = workspace.path().join("created.txt");
        fs::write(&source, "source").unwrap();
        fs::write(&destination, "destination").unwrap();
        fs::write(&deleted, "deleted").unwrap();
        let preview_limits = PreviewSettings {
            max_count: 64,
            max_total_bytes: 1_000_000,
            max_document_text_bytes: 1_000_000,
            max_text_bytes: 1_000_000,
        };
        let mutation_limits = MutationSettings {
            application_lock_timeout: "1s".to_owned(),
            max_entries: 100,
            max_recursion_depth: 20,
            max_rollback_bytes: 1_000_000,
            max_staged_text_bytes: 1_000_000,
            max_preauthorized_callbacks: 64,
        };
        let workspace_uri = url::Url::from_directory_path(workspace.path())
            .unwrap()
            .to_string();
        let planner = WorkspaceEditPlanner::open(
            workspace.path(),
            PositionEncoding::Utf8,
            &preview_limits,
            &mutation_limits,
        )
        .unwrap();
        let planned = planner
            .plan_workspace_edit(&json!({"documentChanges": [
                {
                    "kind": "rename",
                    "oldUri": url::Url::from_file_path(&source).unwrap(),
                    "newUri": url::Url::from_file_path(&destination).unwrap(),
                    "options": {"overwrite": true}
                },
                {
                    "kind": "delete",
                    "uri": url::Url::from_file_path(&deleted).unwrap()
                },
                {
                    "kind": "create",
                    "uri": url::Url::from_file_path(&created).unwrap()
                }
            ]}))
            .unwrap();
        let transaction_id = "txn_00000000000000000000000000000000";
        let artifact_directory = workspace.path().join(format!(".lspctl-{transaction_id}"));
        let transaction = TransactionRecord {
            format_version: MUTATION_STATE_VERSION,
            transaction_id: transaction_id.to_owned(),
            preview_id: "prv_00000000000000000000000000000000".to_owned(),
            receipt_id: "prv_00000000000000000000000000000000".to_owned(),
            workspace_path: workspace.path().to_path_buf(),
            workspace_uri: workspace_uri.clone(),
            state: TransactionState::Committing,
            started_at: now_rfc3339(),
            artifact_directory: artifact_directory.clone(),
            backups: planned_backups(&planned.plan.before_manifest, &artifact_directory),
            operations: planned.plan.operations.clone(),
            before_manifest: planned.plan.before_manifest.clone(),
            intended_manifest: planned.plan.intended_manifest.clone(),
            observed_manifest: planned.plan.before_manifest.clone(),
            manifest_digest: manifest_digest(&planned.plan.before_manifest),
            cleanup_pending: false,
        };
        stage_transaction(&transaction, &transaction.operations, &mutation_limits).unwrap();
        commit_operations(&planner, &transaction).unwrap();

        let restored = rollback_transaction(&transaction, &planner).unwrap();

        assert!(manifest_mismatches(&transaction.before_manifest, &restored).is_empty());
        assert_eq!(fs::read_to_string(source).unwrap(), "source");
        assert_eq!(fs::read_to_string(destination).unwrap(), "destination");
        assert_eq!(fs::read_to_string(deleted).unwrap(), "deleted");
        assert!(!created.exists());
        cleanup_transaction_artifacts(&transaction).unwrap();
    }

    #[test]
    fn abandoned_commit_is_sealed_and_rolled_back_without_replaying_writes() {
        for durable in [false, true] {
            let workspace = TempDir::new().unwrap();
            let state = TempDir::new().unwrap();
            let file = workspace.path().join("main.rs");
            fs::write(&file, "old\n").unwrap();
            let store = MutationStateStore::open_at(state.path().join("state")).unwrap();
            let preview_limits = PreviewSettings {
                max_count: 64,
                max_total_bytes: 1_000_000,
                max_document_text_bytes: 1_000_000,
                max_text_bytes: 1_000_000,
            };
            let receipt_limits = ReceiptSettings { max_count: 100 };
            let mutation_limits = MutationSettings {
                application_lock_timeout: "1s".to_owned(),
                max_entries: 100,
                max_recursion_depth: 20,
                max_rollback_bytes: 1_000_000,
                max_staged_text_bytes: 1_000_000,
                max_preauthorized_callbacks: 64,
            };
            let workspace_uri = url::Url::from_directory_path(workspace.path())
                .unwrap()
                .to_string();
            let file_uri = url::Url::from_file_path(&file).unwrap().to_string();
            let planner = WorkspaceEditPlanner::open(
                workspace.path(),
                PositionEncoding::Utf8,
                &preview_limits,
                &mutation_limits,
            )
            .unwrap();
            let planned = planner
            .plan_workspace_edit(&json!({"changes": {file_uri: [{"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 3}}, "newText": "longer"}]}}))
            .unwrap();
            let transaction_id = "txn_00000000000000000000000000000000";
            let artifact_directory = workspace.path().join(format!(".lspctl-{transaction_id}"));
            let transaction = TransactionRecord {
                format_version: MUTATION_STATE_VERSION,
                transaction_id: transaction_id.to_owned(),
                preview_id: "prv_00000000000000000000000000000000".to_owned(),
                receipt_id: "prv_00000000000000000000000000000000".to_owned(),
                workspace_path: workspace.path().to_path_buf(),
                workspace_uri: workspace_uri.clone(),
                state: TransactionState::Committing,
                started_at: now_rfc3339(),
                artifact_directory: artifact_directory.clone(),
                backups: planned_backups(&planned.plan.before_manifest, &artifact_directory),
                operations: planned.plan.operations.clone(),
                before_manifest: planned.plan.before_manifest.clone(),
                intended_manifest: planned.plan.intended_manifest.clone(),
                observed_manifest: planned.plan.before_manifest.clone(),
                manifest_digest: manifest_digest(&planned.plan.before_manifest),
                cleanup_pending: false,
            };
            store.write_transaction(&transaction).unwrap();
            stage_transaction(&transaction, &transaction.operations, &mutation_limits).unwrap();
            let commit = commit_operations_with_writer(&planner, &transaction, |progress| {
                if !durable && progress.effects.last().unwrap().after.is_some() {
                    return Err("simulated crash before completed evidence".to_owned());
                }
                write_progress(&transaction, &planner, progress)
            });
            assert_eq!(commit.is_ok(), durable);

            let transaction = reconcile_recovery_status(
                &store,
                store.read_transaction(transaction_id).unwrap(),
                &preview_limits,
                &mutation_limits,
            )
            .unwrap()
            .unwrap();
            assert_eq!(transaction.state, TransactionState::RecoveryRequired);
            assert_eq!(fs::read_to_string(&file).unwrap(), "longer\n");

            let result = recover_rollback(
                &store,
                transaction_id,
                &transaction.manifest_digest,
                &preview_limits,
                &receipt_limits,
                &mutation_limits,
            );

            if durable {
                assert_eq!(result.unwrap()["outcome"], "restored");
                assert_eq!(fs::read_to_string(file).unwrap(), "old\n");
                assert!(store.read_transaction(transaction_id).is_err());
                assert!(!artifact_directory.exists());
            } else {
                assert_eq!(result.unwrap_err().code, "recovery_failed");
                assert_eq!(fs::read_to_string(&file).unwrap(), "longer\n");
                assert!(artifact_directory.exists());
                assert_unproven_recovery_preserves_workspace(&transaction, &planner);
            }
        }
    }
}
