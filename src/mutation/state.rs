#![allow(clippy::result_large_err)]

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

use atomic_write_file::AtomicWriteFile;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use time::{Duration, OffsetDateTime, format_description::well_known::Rfc3339};

use crate::{
    canonical_value::digest_canonical_value,
    configuration::{PreviewSettings, ReceiptSettings, resolved_user_state_directory},
    contract::ContractFailure,
    state_permissions,
};

use super::planner::{
    CanonicalOperation, CanonicalPlan, ManifestEntry, PreviewSummary, WorkspaceEditProblem,
};

pub(crate) const MUTATION_STATE_VERSION: u32 = 1;
const PREVIEW_RETENTION_HOURS: i64 = 24;
const RECEIPT_RETENTION_HOURS: i64 = 720;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PreviewRecord {
    pub(crate) preview_id: String,
    pub(crate) workspace_uri: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) server: Option<String>,
    pub(crate) session_identity: String,
    pub(crate) position_encoding: String,
    pub(crate) expires_at: String,
    pub(crate) source: Value,
    pub(crate) summary: PreviewSummary,
    pub(crate) edit: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) command: Option<Value>,
    pub(crate) annotations: Value,
    pub(crate) plan: CanonicalPlan,
    pub(crate) preconditions: Vec<ManifestEntry>,
    pub(crate) conflicts: Vec<WorkspaceEditProblem>,
    pub(crate) stale_reasons: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) diff: Option<String>,
    pub(crate) reserved: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct StoredPreview {
    pub(crate) format_version: u32,
    pub(crate) created_unix_seconds: i64,
    pub(crate) expires_unix_seconds: i64,
    pub(crate) workspace_path: PathBuf,
    pub(crate) authorization_digest: String,
    pub(crate) recovery_manifest_digest: Option<String>,
    pub(crate) preview: PreviewRecord,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ReceiptRecord {
    pub(crate) receipt_id: String,
    pub(crate) kind: String,
    pub(crate) transaction_id: String,
    pub(crate) workspace_uri: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) server: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) session_identity: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) preview_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) linked_receipt_id: Option<String>,
    pub(crate) preauthorized: bool,
    pub(crate) started_at: String,
    pub(crate) completed_at: String,
    pub(crate) outcome: String,
    pub(crate) filesystem_state: String,
    pub(crate) summary: PreviewSummary,
    pub(crate) before_manifest: Vec<ManifestEntry>,
    pub(crate) intended_manifest: Vec<ManifestEntry>,
    pub(crate) observed_manifest: Vec<ManifestEntry>,
    pub(crate) session_synchronized: bool,
    pub(crate) cleanup_pending: bool,
    pub(crate) durability: Value,
    pub(crate) manifest_digest: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) failure_stage: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) failed_change: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct StoredReceipt {
    pub(crate) format_version: u32,
    pub(crate) completed_unix_seconds: i64,
    pub(crate) expires_unix_seconds: i64,
    pub(crate) receipt: ReceiptRecord,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct BackupEntry {
    pub(crate) path: PathBuf,
    pub(crate) backup_path: PathBuf,
    pub(crate) existed: bool,
    pub(crate) resource_kind: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TransactionState {
    Staged,
    Committing,
    RecoveryRequired,
    CleanupPending,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct TransactionRecord {
    pub(crate) format_version: u32,
    pub(crate) transaction_id: String,
    pub(crate) preview_id: String,
    pub(crate) receipt_id: String,
    pub(crate) workspace_path: PathBuf,
    pub(crate) workspace_uri: String,
    pub(crate) state: TransactionState,
    pub(crate) started_at: String,
    pub(crate) artifact_directory: PathBuf,
    pub(crate) backups: Vec<BackupEntry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) operations: Vec<CanonicalOperation>,
    pub(crate) before_manifest: Vec<ManifestEntry>,
    pub(crate) intended_manifest: Vec<ManifestEntry>,
    pub(crate) observed_manifest: Vec<ManifestEntry>,
    pub(crate) manifest_digest: String,
    pub(crate) cleanup_pending: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct MutationStateStore {
    root: PathBuf,
}

impl MutationStateStore {
    /// Opens the versioned user-private Preview, Receipt, and Recovery store.
    pub(crate) fn open() -> Result<Self, ContractFailure> {
        let root = resolved_user_state_directory().ok_or_else(|| {
            state_failure(
                "mutation",
                Path::new(""),
                "The user state directory is unavailable.",
                None,
            )
        })?;
        Self::open_at(root.join("mutation-v1"))
    }

    pub(crate) fn open_at(root: PathBuf) -> Result<Self, ContractFailure> {
        for directory in [
            root.clone(),
            root.join("previews"),
            root.join("receipts"),
            root.join("transactions"),
            root.join("locks"),
        ] {
            fs::create_dir_all(&directory).map_err(|error| {
                state_failure(
                    "mutation",
                    &directory,
                    "A Mutation state directory cannot be created.",
                    error.raw_os_error(),
                )
            })?;
            restrict_directory(&directory)?;
        }
        Ok(Self { root })
    }

    pub(crate) fn new_preview_id(&self) -> Result<String, ContractFailure> {
        random_identifier("prv_")
    }

    pub(crate) fn new_transaction_id(&self) -> Result<String, ContractFailure> {
        random_identifier("txn_")
    }

    pub(crate) fn new_receipt_id(&self) -> Result<String, ContractFailure> {
        random_identifier("rcp_")
    }

    pub(crate) fn create_preview(
        &self,
        mut preview: PreviewRecord,
        workspace_path: PathBuf,
        authorization_digest: String,
        recovery_manifest_digest: Option<String>,
        limits: &PreviewSettings,
    ) -> Result<StoredPreview, ContractFailure> {
        self.prune_expired_records()?;
        let now = OffsetDateTime::now_utc();
        let expires = now + Duration::hours(PREVIEW_RETENTION_HOURS);
        preview.expires_at = expires.format(&Rfc3339).unwrap();
        let stored = StoredPreview {
            format_version: MUTATION_STATE_VERSION,
            created_unix_seconds: now.unix_timestamp(),
            expires_unix_seconds: expires.unix_timestamp(),
            workspace_path,
            authorization_digest,
            recovery_manifest_digest,
            preview,
        };
        let bytes = serialize_record(
            &stored,
            "preview",
            &self.preview_path(&stored.preview.preview_id),
        )?;
        let previews = self.preview_files()?;
        let current_bytes = previews
            .iter()
            .filter_map(|path| fs::metadata(path).ok())
            .map(|metadata| metadata.len())
            .sum::<u64>();
        if previews.len() as u64 >= limits.max_count {
            return Err(capacity_failure(
                "previewCount",
                limits.max_count,
                previews.len() as u64,
                1,
                self.expired_preview_ids()?,
            ));
        }
        if current_bytes.saturating_add(bytes.len() as u64) > limits.max_total_bytes {
            return Err(capacity_failure(
                "previewBytes",
                limits.max_total_bytes,
                current_bytes,
                bytes.len() as u64,
                self.expired_preview_ids()?,
            ));
        }
        write_record(
            &self.preview_path(&stored.preview.preview_id),
            &bytes,
            "preview",
        )?;
        Ok(stored)
    }

    pub(crate) fn read_preview(&self, preview_id: &str) -> Result<StoredPreview, ContractFailure> {
        let path = self.preview_path(preview_id);
        let preview: StoredPreview = read_record(&path, "preview", preview_id)?;
        validate_record_version(preview.format_version, "preview", &path)?;
        if preview.expires_unix_seconds <= OffsetDateTime::now_utc().unix_timestamp()
            && !preview.preview.reserved
        {
            let _ = fs::remove_file(path);
            return Err(preview_unknown(preview_id, "inspect"));
        }
        Ok(preview)
    }

    pub(crate) fn write_preview(&self, preview: &StoredPreview) -> Result<(), ContractFailure> {
        let path = self.preview_path(&preview.preview.preview_id);
        let bytes = serialize_record(preview, "preview", &path)?;
        write_record(&path, &bytes, "preview")
    }

    /// The caller must hold this Preview's Workspace Application lock.
    pub(crate) fn reserve_preview(
        &self,
        preview_id: &str,
    ) -> Result<StoredPreview, ContractFailure> {
        self.reserve_preview_with_writer(preview_id, |preview| self.write_preview(preview))
    }

    fn reserve_preview_with_writer(
        &self,
        preview_id: &str,
        write: impl FnOnce(&StoredPreview) -> Result<(), ContractFailure>,
    ) -> Result<StoredPreview, ContractFailure> {
        let mut preview = self.read_preview(preview_id)?;
        if preview.preview.reserved {
            return Err(ContractFailure {
                exit_code: 6,
                category: "mutation",
                code: "application_busy",
                message: "The Preview is already reserved by an Application.".to_owned(),
                stage: "reserve",
                delivery: "not_applicable",
                retry: "safe",
                data: json!({"previewId": preview_id}),
            });
        }
        preview.preview.reserved = true;
        if let Err(failure) = write(&preview) {
            // This invocation saw an unreserved record under the Application lock;
            // no journal can own its reservation yet, even if the write committed.
            self.release_preview(&mut preview)?;
            return Err(failure);
        }
        Ok(preview)
    }

    pub(crate) fn release_preview(
        &self,
        preview: &mut StoredPreview,
    ) -> Result<(), ContractFailure> {
        preview.preview.reserved = false;
        self.write_preview(preview)
    }

    pub(crate) fn discard_preview(&self, preview_id: &str) -> Result<(), ContractFailure> {
        let preview = self.read_preview(preview_id)?;
        if preview.preview.reserved {
            let lock = self.open_application_lock(&preview.preview.workspace_uri)?;
            match lock.try_lock() {
                Ok(()) => {}
                Err(std::fs::TryLockError::WouldBlock) => {
                    return Err(application_busy(preview_id));
                }
                Err(std::fs::TryLockError::Error(error)) => {
                    return Err(state_failure(
                        "transaction",
                        &self.application_lock_path(&preview.preview.workspace_uri),
                        "The Workspace Application lock failed.",
                        error.raw_os_error(),
                    ));
                }
            }
            if self
                .list_transactions()?
                .into_iter()
                .any(|transaction| match transaction {
                    Ok(transaction) => transaction.preview_id == preview_id,
                    Err(_) => true,
                })
            {
                return Err(application_busy(preview_id));
            }
        }
        fs::remove_file(self.preview_path(preview_id)).map_err(|error| {
            state_failure(
                "preview",
                &self.preview_path(preview_id),
                "The Preview cannot be discarded.",
                error.raw_os_error(),
            )
        })
    }

    pub(crate) fn retire_preview_after_recovery(
        &self,
        preview_id: &str,
    ) -> Result<(), ContractFailure> {
        let path = self.preview_path(preview_id);
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(state_failure(
                "preview",
                &path,
                "The recovered Preview cannot be retired.",
                error.raw_os_error(),
            )),
        }
    }

    pub(crate) fn list_previews(&self) -> Result<Vec<StoredPreview>, ContractFailure> {
        self.prune_expired_records()?;
        let mut records = self
            .preview_files()?
            .into_iter()
            .map(|path| {
                let id = file_stem(&path);
                self.read_preview(&id)
            })
            .collect::<Result<Vec<_>, _>>()?;
        records.sort_by_key(|record| std::cmp::Reverse(record.created_unix_seconds));
        Ok(records)
    }

    pub(crate) fn write_receipt(
        &self,
        receipt: ReceiptRecord,
        limits: &ReceiptSettings,
    ) -> Result<StoredReceipt, ContractFailure> {
        self.prune_expired_records()?;
        let now = OffsetDateTime::now_utc();
        let stored = StoredReceipt {
            format_version: MUTATION_STATE_VERSION,
            completed_unix_seconds: now.unix_timestamp(),
            expires_unix_seconds: (now + Duration::hours(RECEIPT_RETENTION_HOURS)).unix_timestamp(),
            receipt,
        };
        let receipts = self.receipt_files()?;
        if receipts.len() as u64 >= limits.max_count {
            return Err(capacity_failure(
                "receiptCount",
                limits.max_count,
                receipts.len() as u64,
                1,
                self.expired_receipt_ids()?,
            ));
        }
        let path = self.receipt_path(&stored.receipt.receipt_id);
        let bytes = serialize_record(&stored, "receipt", &path)?;
        write_record(&path, &bytes, "receipt")?;
        Ok(stored)
    }

    pub(crate) fn ensure_receipt_capacity(
        &self,
        limits: &ReceiptSettings,
    ) -> Result<(), ContractFailure> {
        self.prune_expired_records()?;
        let receipts = self.receipt_files()?;
        if receipts.len() as u64 >= limits.max_count {
            return Err(capacity_failure(
                "receiptCount",
                limits.max_count,
                receipts.len() as u64,
                1,
                self.expired_receipt_ids()?,
            ));
        }
        Ok(())
    }

    pub(crate) fn read_receipt(&self, receipt_id: &str) -> Result<StoredReceipt, ContractFailure> {
        let path = self.receipt_path(receipt_id);
        let receipt: StoredReceipt =
            read_record(&path, "receipt", receipt_id).map_err(|mut failure| {
                if failure.code == "preview_unknown" {
                    failure.exit_code = 3;
                    failure.category = "blocked";
                    failure.code = "state_record_unknown";
                    failure.message = "The requested Receipt is unknown or expired.".to_owned();
                    failure.data = json!({"recordType": "receipt", "id": receipt_id});
                }
                failure
            })?;
        validate_record_version(receipt.format_version, "receipt", &path)?;
        Ok(receipt)
    }

    pub(crate) fn mark_receipt_session_synchronized(
        &self,
        receipt_id: &str,
    ) -> Result<(), ContractFailure> {
        let mut stored = self.read_receipt(receipt_id)?;
        if stored.receipt.session_synchronized {
            return Ok(());
        }
        stored.receipt.session_synchronized = true;
        let path = self.receipt_path(receipt_id);
        let bytes = serialize_record(&stored, "receipt", &path)?;
        write_record(&path, &bytes, "receipt")
    }

    pub(crate) fn mark_receipt_cleanup_complete(
        &self,
        receipt_id: &str,
    ) -> Result<(), ContractFailure> {
        let mut stored = self.read_receipt(receipt_id)?;
        if !stored.receipt.cleanup_pending {
            return Ok(());
        }
        stored.receipt.cleanup_pending = false;
        let path = self.receipt_path(receipt_id);
        let bytes = serialize_record(&stored, "receipt", &path)?;
        write_record(&path, &bytes, "receipt")
    }

    pub(crate) fn list_receipts(&self) -> Result<Vec<StoredReceipt>, ContractFailure> {
        self.prune_expired_records()?;
        let mut records = self.read_all_receipts()?;
        records.sort_by_key(|record| std::cmp::Reverse(record.completed_unix_seconds));
        Ok(records)
    }

    pub(crate) fn already_applied(
        &self,
        preview_id: &str,
    ) -> Result<Option<StoredReceipt>, ContractFailure> {
        let path = self.receipt_path(preview_id);
        if !path.exists() {
            return Ok(None);
        }
        self.read_receipt(preview_id).map(Some)
    }

    pub(crate) fn convert_preview_to_receipt(
        &self,
        preview_id: &str,
        receipt: ReceiptRecord,
        limits: &ReceiptSettings,
    ) -> Result<StoredReceipt, ContractFailure> {
        let mut stored = self.write_receipt(receipt, limits)?;
        if let Err(error) = fs::remove_file(self.preview_path(preview_id))
            && error.kind() != std::io::ErrorKind::NotFound
        {
            stored.receipt.cleanup_pending = true;
            let path = self.receipt_path(&stored.receipt.receipt_id);
            let bytes = serialize_record(&stored, "receipt", &path)?;
            write_record(&path, &bytes, "receipt")?;
        }
        Ok(stored)
    }

    pub(crate) fn write_transaction(
        &self,
        transaction: &TransactionRecord,
    ) -> Result<(), ContractFailure> {
        let path = self.transaction_path(&transaction.transaction_id);
        let bytes = serialize_record(transaction, "transaction", &path)?;
        write_record(&path, &bytes, "transaction")
    }

    pub(crate) fn read_transaction(
        &self,
        transaction_id: &str,
    ) -> Result<TransactionRecord, ContractFailure> {
        let path = self.transaction_path(transaction_id);
        let transaction: TransactionRecord = read_record(&path, "transaction", transaction_id)
            .map_err(|failure| {
                if failure.code == "preview_unknown" {
                    recovery_not_found(transaction_id)
                } else {
                    failure
                }
            })?;
        validate_record_version(transaction.format_version, "transaction", &path)?;
        Ok(transaction)
    }

    pub(crate) fn list_transactions(
        &self,
    ) -> Result<Vec<Result<TransactionRecord, Value>>, ContractFailure> {
        let mut results = Vec::new();
        for path in self.transaction_files()? {
            match fs::read(&path).ok().and_then(|bytes| serde_json::from_slice::<TransactionRecord>(&bytes).ok()) {
                Some(record) if record.format_version == MUTATION_STATE_VERSION => results.push(Ok(record)),
                _ => results.push(Err(json!({
                    "kind": "corrupt",
                    "protected": true,
                    "path": path,
                    "problems": [{"code": "corrupt_journal", "message": "The transaction journal is invalid."}]
                }))),
            }
        }
        Ok(results)
    }

    pub(crate) fn remove_transaction(&self, transaction_id: &str) -> Result<(), ContractFailure> {
        fs::remove_file(self.transaction_path(transaction_id)).map_err(|error| {
            state_failure(
                "transaction",
                &self.transaction_path(transaction_id),
                "The transaction journal cannot be removed.",
                error.raw_os_error(),
            )
        })
    }

    pub(crate) fn application_lock_path(&self, workspace_uri: &str) -> PathBuf {
        let digest = digest_canonical_value(
            "lspctl-workspace-application-lock-v1",
            &json!(workspace_uri),
        );
        self.root
            .join("locks")
            .join(format!("{}.lock", digest.trim_start_matches("sha256:")))
    }

    pub(crate) fn open_application_lock(
        &self,
        workspace_uri: &str,
    ) -> Result<fs::File, ContractFailure> {
        let path = self.application_lock_path(workspace_uri);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|error| {
                state_failure(
                    "transaction",
                    &path,
                    "The Workspace Application lock cannot be opened.",
                    error.raw_os_error(),
                )
            })?;
        restrict_file(&path)?;
        Ok(file)
    }

    pub(crate) fn prune_state(
        &self,
        requested_ids: &[String],
    ) -> Result<Vec<String>, ContractFailure> {
        if requested_ids.is_empty() {
            return self.prune_expired_records();
        }
        let receipts = self.read_all_receipts()?;
        let mut paths = BTreeMap::new();
        let mut problems = Vec::new();
        for id in requested_ids {
            let path = if id.starts_with("prv_") && self.preview_path(id).exists() {
                match self.read_preview(id) {
                    Ok(record) if record.preview.reserved => {
                        problems.push(json!({"id": id, "code": "protected", "message": "A reserved Preview is protected."}));
                        continue;
                    }
                    Ok(_) => self.preview_path(id),
                    Err(_) => {
                        problems.push(json!({"id": id, "code": "unknown", "message": "The state identifier is unknown."}));
                        continue;
                    }
                }
            } else if id.starts_with("prv_") || id.starts_with("rcp_") {
                let Some(_) = receipts
                    .iter()
                    .find(|record| record.receipt.receipt_id == *id)
                else {
                    problems.push(json!({"id": id, "code": "unknown", "message": "The state identifier is unknown."}));
                    continue;
                };
                let chain = receipt_chain(&receipts, id);
                if receipt_chain_protected(&chain) {
                    problems.push(json!({"id": id, "code": "protected", "message": "A nonterminal or cleanup-pending Receipt chain is protected."}));
                    continue;
                }
                for receipt in chain {
                    let receipt_id = receipt.receipt.receipt_id.clone();
                    paths.insert(receipt_id.clone(), self.receipt_path(&receipt_id));
                }
                continue;
            } else if id.starts_with("txn_") && self.transaction_path(id).exists() {
                problems.push(json!({"id": id, "code": "protected", "message": "Recovery transaction evidence is protected."}));
                continue;
            } else {
                problems.push(json!({"id": id, "code": "unknown", "message": "The state identifier is unknown."}));
                continue;
            };
            paths.insert(id.clone(), path);
        }
        if !problems.is_empty() {
            return Err(ContractFailure {
                exit_code: 3,
                category: "blocked",
                code: "state_prune_rejected",
                message: "The requested state prune is not safe.".to_owned(),
                stage: "prune",
                delivery: "not_applicable",
                retry: "after_change",
                data: json!({"problems": problems}),
            });
        }
        for path in paths.values() {
            fs::remove_file(path).map_err(|error| {
                state_failure(
                    "mutation",
                    path,
                    "A state record cannot be pruned.",
                    error.raw_os_error(),
                )
            })?;
        }
        Ok(paths.into_keys().collect())
    }

    fn prune_expired_records(&self) -> Result<Vec<String>, ContractFailure> {
        let now = OffsetDateTime::now_utc().unix_timestamp();
        let mut removed = Vec::new();
        for path in self.preview_files()? {
            let bytes = fs::read(&path).map_err(|error| {
                state_failure(
                    "preview",
                    &path,
                    "A Preview cannot be inspected for pruning.",
                    error.raw_os_error(),
                )
            })?;
            let record: StoredPreview = serde_json::from_slice(&bytes)
                .map_err(|_| stored_version_failure("preview", &path, 0))?;
            validate_record_version(record.format_version, "preview", &path)?;
            if record.expires_unix_seconds <= now && !record.preview.reserved {
                fs::remove_file(&path).map_err(|error| {
                    state_failure(
                        "preview",
                        &path,
                        "An expired Preview cannot be pruned.",
                        error.raw_os_error(),
                    )
                })?;
                removed.push(record.preview.preview_id);
            }
        }
        let receipts = self.read_all_receipts()?;
        let mut visited = BTreeSet::new();
        for record in &receipts {
            if visited.contains(&record.receipt.receipt_id) {
                continue;
            }
            let chain = receipt_chain(&receipts, &record.receipt.receipt_id);
            visited.extend(chain.iter().map(|record| record.receipt.receipt_id.clone()));
            if chain
                .iter()
                .all(|record| record.expires_unix_seconds <= now)
                && !receipt_chain_protected(&chain)
            {
                for record in chain {
                    let id = &record.receipt.receipt_id;
                    let path = self.receipt_path(id);
                    fs::remove_file(&path).map_err(|error| {
                        state_failure(
                            "receipt",
                            &path,
                            "An expired Receipt chain cannot be pruned.",
                            error.raw_os_error(),
                        )
                    })?;
                    removed.push(id.clone());
                }
            }
        }
        Ok(removed)
    }

    fn expired_preview_ids(&self) -> Result<Vec<String>, ContractFailure> {
        let now = OffsetDateTime::now_utc().unix_timestamp();
        Ok(self
            .list_previews()?
            .into_iter()
            .filter(|record| record.expires_unix_seconds <= now && !record.preview.reserved)
            .map(|record| record.preview.preview_id)
            .collect())
    }

    fn expired_receipt_ids(&self) -> Result<Vec<String>, ContractFailure> {
        let now = OffsetDateTime::now_utc().unix_timestamp();
        let receipts = self.read_all_receipts()?;
        let mut expired = Vec::new();
        let mut visited = BTreeSet::new();
        for record in &receipts {
            if visited.contains(&record.receipt.receipt_id) {
                continue;
            }
            let chain = receipt_chain(&receipts, &record.receipt.receipt_id);
            visited.extend(chain.iter().map(|record| record.receipt.receipt_id.clone()));
            if chain
                .iter()
                .all(|record| record.expires_unix_seconds <= now)
                && !receipt_chain_protected(&chain)
            {
                expired.extend(chain.iter().map(|record| record.receipt.receipt_id.clone()));
            }
        }
        Ok(expired)
    }

    fn read_all_receipts(&self) -> Result<Vec<StoredReceipt>, ContractFailure> {
        self.receipt_files()?
            .into_iter()
            .map(|path| {
                let id = file_stem(&path);
                self.read_receipt(&id)
            })
            .collect()
    }

    fn preview_path(&self, id: &str) -> PathBuf {
        self.root.join("previews").join(format!("{id}.json"))
    }
    fn receipt_path(&self, id: &str) -> PathBuf {
        self.root.join("receipts").join(format!("{id}.json"))
    }
    fn transaction_path(&self, id: &str) -> PathBuf {
        self.root.join("transactions").join(format!("{id}.json"))
    }
    fn preview_files(&self) -> Result<Vec<PathBuf>, ContractFailure> {
        record_files(&self.root.join("previews"), "preview")
    }
    fn receipt_files(&self) -> Result<Vec<PathBuf>, ContractFailure> {
        record_files(&self.root.join("receipts"), "receipt")
    }
    fn transaction_files(&self) -> Result<Vec<PathBuf>, ContractFailure> {
        record_files(&self.root.join("transactions"), "transaction")
    }
}

fn receipt_chain<'a>(receipts: &'a [StoredReceipt], start: &str) -> Vec<&'a StoredReceipt> {
    let mut identifiers = BTreeSet::from([start.to_owned()]);
    loop {
        let before = identifiers.len();
        for record in receipts {
            let id = &record.receipt.receipt_id;
            let linked = record.receipt.linked_receipt_id.as_ref();
            if identifiers.contains(id) || linked.is_some_and(|linked| identifiers.contains(linked))
            {
                identifiers.insert(id.clone());
                if let Some(linked) = linked {
                    identifiers.insert(linked.clone());
                }
            }
        }
        if identifiers.len() == before {
            break;
        }
    }
    receipts
        .iter()
        .filter(|record| identifiers.contains(&record.receipt.receipt_id))
        .collect()
}

fn receipt_chain_protected(chain: &[&StoredReceipt]) -> bool {
    let terminal_recovery = chain.iter().any(|record| {
        record.receipt.kind == "recovery_receipt"
            && matches!(
                record.receipt.outcome.as_str(),
                "restored" | "accepted_current"
            )
            && !record.receipt.cleanup_pending
    });
    !terminal_recovery && chain.iter().any(|record| record.receipt.cleanup_pending)
}

fn application_busy(preview_id: &str) -> ContractFailure {
    ContractFailure {
        exit_code: 6,
        category: "mutation",
        code: "application_busy",
        message: "A reserved Preview is owned by an active or recoverable Application.".to_owned(),
        stage: "reserve",
        delivery: "not_applicable",
        retry: "safe",
        data: json!({"previewId": preview_id}),
    }
}

pub(crate) fn now_rfc3339() -> String {
    OffsetDateTime::now_utc().format(&Rfc3339).unwrap()
}

pub(crate) fn manifest_digest(manifest: &[ManifestEntry]) -> String {
    digest_canonical_value(
        "lspctl-mutation-manifest-v1",
        &serde_json::to_value(manifest).unwrap(),
    )
}

fn random_identifier(prefix: &str) -> Result<String, ContractFailure> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|error| {
        state_failure(
            "mutation",
            Path::new(""),
            &format!("A random state identifier cannot be generated: {error}"),
            None,
        )
    })?;
    Ok(format!("{prefix}{}", hex::encode(bytes)))
}

fn record_files(directory: &Path, record_type: &str) -> Result<Vec<PathBuf>, ContractFailure> {
    let entries = fs::read_dir(directory).map_err(|error| {
        state_failure(
            record_type,
            directory,
            "A state directory cannot be read.",
            error.raw_os_error(),
        )
    })?;
    let mut paths = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .collect::<Vec<_>>();
    paths.sort();
    Ok(paths)
}

fn file_stem(path: &Path) -> String {
    path.file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

fn serialize_record<T: Serialize>(
    record: &T,
    record_type: &str,
    path: &Path,
) -> Result<Vec<u8>, ContractFailure> {
    serde_json::to_vec(record).map_err(|_| {
        state_failure(
            record_type,
            path,
            "A state record cannot be serialized.",
            None,
        )
    })
}

fn read_record<T: DeserializeOwned>(
    path: &Path,
    record_type: &str,
    id: &str,
) -> Result<T, ContractFailure> {
    let mut bytes = Vec::new();
    match fs::File::open(path) {
        Ok(mut file) => file.read_to_end(&mut bytes).map_err(|error| {
            state_failure(
                record_type,
                path,
                "A state record cannot be read.",
                error.raw_os_error(),
            )
        })?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(preview_unknown(id, "inspect"));
        }
        Err(error) => {
            return Err(state_failure(
                record_type,
                path,
                "A state record cannot be opened.",
                error.raw_os_error(),
            ));
        }
    };
    serde_json::from_slice(&bytes).map_err(|_| stored_version_failure(record_type, path, 0))
}

fn write_record(path: &Path, bytes: &[u8], record_type: &str) -> Result<(), ContractFailure> {
    let mut file = AtomicWriteFile::open(path).map_err(|error| {
        state_failure(
            record_type,
            path,
            "A state record cannot be staged.",
            error.raw_os_error(),
        )
    })?;
    file.write_all(bytes)
        .and_then(|()| file.commit())
        .map_err(|error| {
            state_failure(
                record_type,
                path,
                "A state record cannot be committed.",
                error.raw_os_error(),
            )
        })?;
    restrict_file(path)
}

fn validate_record_version(
    version: u32,
    record_type: &str,
    path: &Path,
) -> Result<(), ContractFailure> {
    if version == MUTATION_STATE_VERSION {
        Ok(())
    } else {
        Err(stored_version_failure(record_type, path, version))
    }
}

fn preview_unknown(id: &str, stage: &'static str) -> ContractFailure {
    ContractFailure {
        exit_code: 6,
        category: "mutation",
        code: "preview_unknown",
        message: "The Preview is unknown or expired.".to_owned(),
        stage,
        delivery: "not_applicable",
        retry: "never",
        data: json!({"previewId": id}),
    }
}

fn recovery_not_found(id: &str) -> ContractFailure {
    ContractFailure {
        exit_code: 7,
        category: "recovery",
        code: "recovery_not_found",
        message: "The Recovery transaction is unknown.".to_owned(),
        stage: "recover",
        delivery: "not_applicable",
        retry: "never",
        data: json!({"transactionId": id}),
    }
}

fn stored_version_failure(record_type: &str, path: &Path, version: u32) -> ContractFailure {
    ContractFailure {
        exit_code: 3,
        category: "blocked",
        code: "stored_state_version_unsupported",
        message: "Stored Mutation state is invalid or uses unsupported semantics.".to_owned(),
        stage: "inspect",
        delivery: "not_applicable",
        retry: "never",
        data: json!({"recordType": record_type, "path": path, "foundVersion": version, "supportedVersions": [MUTATION_STATE_VERSION]}),
    }
}

fn state_failure(
    record_type: &str,
    path: &Path,
    message: &str,
    os_code: Option<i32>,
) -> ContractFailure {
    let mut data = json!({"recordType": record_type, "path": path});
    if let Some(code) = os_code {
        data["osCode"] = json!(code);
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

fn capacity_failure(
    record_type: &str,
    limit: u64,
    current: u64,
    required: u64,
    prunable_ids: Vec<String>,
) -> ContractFailure {
    ContractFailure {
        exit_code: 4,
        category: "unavailable",
        code: "state_capacity_exceeded",
        message: "Mutation state capacity is exhausted.".to_owned(),
        stage: "reserve",
        delivery: "not_applicable",
        retry: "after_change",
        data: json!({"recordType": record_type, "limit": limit, "current": current, "required": required, "prunableIds": prunable_ids}),
    }
}

fn restrict_directory(path: &Path) -> Result<(), ContractFailure> {
    state_permissions::restrict_directory(path).map_err(|error| {
        state_failure(
            "mutation",
            path,
            "Mutation state directory permissions cannot be restricted.",
            error.raw_os_error(),
        )
    })
}

fn restrict_file(path: &Path) -> Result<(), ContractFailure> {
    state_permissions::restrict_file(path).map_err(|error| {
        state_failure(
            "mutation",
            path,
            "Mutation state file permissions cannot be restricted.",
            error.raw_os_error(),
        )
    })
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn preview_record(id: String) -> PreviewRecord {
        PreviewRecord {
            preview_id: id,
            workspace_uri: "file:///workspace/".to_owned(),
            server: None,
            session_identity: format!("sid_{}", "0".repeat(64)),
            position_encoding: "utf-8".to_owned(),
            expires_at: String::new(),
            source: json!({}),
            summary: PreviewSummary::default(),
            edit: json!({}),
            command: None,
            annotations: json!({}),
            plan: CanonicalPlan {
                operations: vec![],
                before_manifest: vec![],
                intended_manifest: vec![],
            },
            preconditions: vec![],
            conflicts: vec![],
            stale_reasons: vec![],
            diff: None,
            reserved: false,
        }
    }

    fn receipt_record(id: String) -> ReceiptRecord {
        ReceiptRecord {
            receipt_id: id,
            kind: "receipt".to_owned(),
            transaction_id: "txn_test".to_owned(),
            workspace_uri: "file:///workspace/".to_owned(),
            server: None,
            session_identity: None,
            preview_id: None,
            linked_receipt_id: None,
            preauthorized: false,
            started_at: "2026-01-01T00:00:00Z".to_owned(),
            completed_at: "2026-01-01T00:00:01Z".to_owned(),
            outcome: "applied".to_owned(),
            filesystem_state: "unchanged".to_owned(),
            summary: PreviewSummary::default(),
            before_manifest: Vec::new(),
            intended_manifest: Vec::new(),
            observed_manifest: Vec::new(),
            session_synchronized: false,
            cleanup_pending: false,
            durability: json!({}),
            manifest_digest: "sha256:test".to_owned(),
            failure_stage: None,
            failed_change: None,
        }
    }

    #[test]
    fn same_major_stored_state_fixtures_remain_readable() {
        for (preview, receipt, recovery) in [
            (
                include_str!("../../tests/fixtures/stored-state/v1/preview.json"),
                include_str!("../../tests/fixtures/stored-state/v1/receipt.json"),
                include_str!("../../tests/fixtures/stored-state/v1/recovery.json"),
            ),
            (
                include_str!("../../tests/fixtures/stored-state/v0.1.1/preview.json"),
                include_str!("../../tests/fixtures/stored-state/v0.1.1/receipt.json"),
                include_str!("../../tests/fixtures/stored-state/v0.1.1/recovery.json"),
            ),
        ] {
            let preview: StoredPreview = serde_json::from_str(preview).unwrap();
            let receipt: StoredReceipt = serde_json::from_str(receipt).unwrap();
            let recovery: TransactionRecord = serde_json::from_str(recovery).unwrap();

            assert_eq!(preview.format_version, MUTATION_STATE_VERSION);
            assert_eq!(receipt.format_version, MUTATION_STATE_VERSION);
            assert_eq!(recovery.format_version, MUTATION_STATE_VERSION);
        }
    }

    #[test]
    fn preview_store_reserves_and_discards_exact_records() {
        let directory = TempDir::new().unwrap();
        let store = MutationStateStore::open_at(directory.path().join("state")).unwrap();
        let id = store.new_preview_id().unwrap();
        let preview = preview_record(id.clone());
        let limits = PreviewSettings {
            max_count: 2,
            max_total_bytes: 100_000,
            max_document_text_bytes: 100,
            max_text_bytes: 100,
        };
        store
            .create_preview(
                preview,
                directory.path().to_path_buf(),
                "sha256:test".to_owned(),
                None,
                &limits,
            )
            .unwrap();
        let lock = store.open_application_lock("file:///workspace/").unwrap();
        lock.lock().unwrap();
        let _reserved = store.reserve_preview(&id).unwrap();
        assert!(store.discard_preview(&id).is_err());
        drop(lock);
        store.discard_preview(&id).unwrap();
        assert!(store.read_preview(&id).is_err());
    }

    #[test]
    fn preview_reservation_write_failure_preserves_existing_ownership() {
        for case in [
            "before_commit",
            "after_commit",
            "cleanup_failure",
            "already_reserved",
        ] {
            let directory = TempDir::new().unwrap();
            let store = MutationStateStore::open_at(directory.path().join("state")).unwrap();
            let id = store.new_preview_id().unwrap();
            let (limits, _, _) = super::super::default_mutation_settings();
            store
                .create_preview(
                    preview_record(id.clone()),
                    directory.path().to_path_buf(),
                    "sha256:test".to_owned(),
                    None,
                    &limits,
                )
                .unwrap();
            let lock = store.open_application_lock("file:///workspace/").unwrap();
            lock.lock().unwrap();
            if case == "already_reserved" {
                store.reserve_preview(&id).unwrap();
            }
            let path = store.preview_path(&id);
            let failure = store
                .reserve_preview_with_writer(&id, |preview| {
                    assert_ne!(
                        case, "already_reserved",
                        "must reject before attempting a write"
                    );
                    if case != "before_commit" {
                        store.write_preview(preview)?;
                        assert!(store.read_preview(&id)?.preview.reserved);
                    }
                    if case == "cleanup_failure" {
                        fs::rename(&path, path.with_extension("retained")).unwrap();
                        fs::create_dir(&path).unwrap();
                    }
                    Err(state_failure(
                        "preview",
                        &path,
                        "simulated reservation write failure",
                        None,
                    ))
                })
                .unwrap_err();
            if case == "cleanup_failure" {
                assert_ne!(failure.message, "simulated reservation write failure");
                fs::remove_dir(&path).unwrap();
                fs::rename(path.with_extension("retained"), &path).unwrap();
            } else if case != "already_reserved" {
                assert_eq!(failure.message, "simulated reservation write failure");
            }
            assert_eq!(
                failure.code,
                if case == "already_reserved" {
                    "application_busy"
                } else {
                    "state_unavailable"
                }
            );
            assert_eq!(
                store.read_preview(&id).unwrap().preview.reserved,
                case == "already_reserved" || case == "cleanup_failure",
                "{case}"
            );
            assert!(store.list_transactions().unwrap().is_empty());
        }
    }

    #[test]
    fn state_churn_respects_preview_and_receipt_capacity() {
        let directory = TempDir::new().unwrap();
        let store = MutationStateStore::open_at(directory.path().join("state")).unwrap();
        let preview_limits = PreviewSettings {
            max_count: 1,
            max_total_bytes: 100_000,
            max_document_text_bytes: 100,
            max_text_bytes: 100,
        };
        for index in 0..2 {
            let result = store.create_preview(
                preview_record(format!("prv_{index:032}")),
                directory.path().to_path_buf(),
                "sha256:test".to_owned(),
                None,
                &preview_limits,
            );
            assert_eq!(result.is_ok(), index == 0);
            if let Err(failure) = result {
                assert_eq!(failure.code, "state_capacity_exceeded");
            }
        }

        let receipt_limits = ReceiptSettings { max_count: 1 };
        for index in 0..2 {
            let result =
                store.write_receipt(receipt_record(format!("rcp_{index:032}")), &receipt_limits);
            assert_eq!(result.is_ok(), index == 0);
            if let Err(failure) = result {
                assert_eq!(failure.code, "state_capacity_exceeded");
            }
        }
    }

    #[test]
    fn explicit_prune_selects_the_complete_terminal_recovery_chain() {
        let directory = TempDir::new().unwrap();
        let store = MutationStateStore::open_at(directory.path().join("state")).unwrap();
        let limits = ReceiptSettings { max_count: 4 };
        let original_id = format!("prv_{}", "1".repeat(32));
        let recovery_id = format!("rcp_{}", "2".repeat(32));

        let mut original = receipt_record(original_id.clone());
        original.outcome = "recovery_required".to_owned();
        original.cleanup_pending = true;
        store.write_receipt(original, &limits).unwrap();

        let mut recovery = receipt_record(recovery_id.clone());
        recovery.kind = "recovery_receipt".to_owned();
        recovery.outcome = "restored".to_owned();
        recovery.linked_receipt_id = Some(original_id.clone());
        store.write_receipt(recovery, &limits).unwrap();

        let removed = store
            .prune_state(std::slice::from_ref(&original_id))
            .unwrap();
        assert_eq!(removed, vec![original_id.clone(), recovery_id.clone()]);
        assert!(store.read_receipt(&original_id).is_err());
        assert!(store.read_receipt(&recovery_id).is_err());
    }

    #[test]
    fn prune_protects_a_nonterminal_recovery_chain() {
        let directory = TempDir::new().unwrap();
        let store = MutationStateStore::open_at(directory.path().join("state")).unwrap();
        let limits = ReceiptSettings { max_count: 2 };
        let original_id = format!("prv_{}", "3".repeat(32));
        let mut original = receipt_record(original_id.clone());
        original.outcome = "recovery_required".to_owned();
        original.cleanup_pending = true;
        store.write_receipt(original, &limits).unwrap();

        let failure = store
            .prune_state(std::slice::from_ref(&original_id))
            .unwrap_err();
        assert_eq!(failure.code, "state_prune_rejected");
        assert_eq!(failure.data["problems"][0]["code"], "protected");
        assert!(store.read_receipt(&original_id).is_ok());
    }
}
