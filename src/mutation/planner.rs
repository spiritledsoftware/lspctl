#![allow(clippy::result_large_err)]

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, Metadata},
    io::Read,
    path::{Component, Path, PathBuf},
};

use cap_fs_ext::DirExt;
use cap_std::{ambient_authority, fs::Dir};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use url::Url;

use crate::{
    canonical_value::{digest_canonical_value, digest_raw_bytes},
    configuration::{MutationSettings, PreviewSettings},
    workspace::{PositionEncoding, ProtocolPosition},
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ManifestEntry {
    pub(crate) path: PathBuf,
    pub(crate) exists: bool,
    pub(crate) resource_kind: ResourceKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) identity_digest: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) content_digest: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) metadata_digest: Option<String>,
}

pub(crate) struct DocumentVersionPrecondition {
    pub(crate) version: i64,
    pub(crate) digest: String,
}

fn document_version_precondition<'a>(
    preconditions: &'a BTreeMap<String, DocumentVersionPrecondition>,
    uri: &str,
    version: i64,
) -> Option<&'a DocumentVersionPrecondition> {
    if let Some(precondition) = preconditions.get(uri) {
        return (precondition.version == version).then_some(precondition);
    }
    #[cfg(windows)]
    {
        preconditions.iter().find_map(|(known_uri, precondition)| {
            (windows_file_uri_drive_alias(known_uri, uri) && precondition.version == version)
                .then_some(precondition)
        })
    }
    #[cfg(not(windows))]
    {
        None
    }
}

#[cfg(any(test, windows))]
fn windows_file_uri_drive_alias(left: &str, right: &str) -> bool {
    const PREFIX: &[u8] = b"file:///";
    let left = left.as_bytes();
    let right = right.as_bytes();
    left.len() > PREFIX.len() + 1
        && left.len() == right.len()
        && left.starts_with(PREFIX)
        && right.starts_with(PREFIX)
        && left[PREFIX.len()].eq_ignore_ascii_case(&right[PREFIX.len()])
        && left[PREFIX.len()].is_ascii_alphabetic()
        && left[PREFIX.len() + 1..] == right[PREFIX.len() + 1..]
        && left[PREFIX.len() + 1] == b':'
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ResourceKind {
    File,
    Directory,
    Missing,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct CanonicalTextEdit {
    pub(crate) start_byte: u64,
    pub(crate) end_byte: u64,
    pub(crate) new_text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) annotation_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub(crate) enum CanonicalOperation {
    Text {
        index: u64,
        uri: String,
        path: PathBuf,
        before_digest: String,
        after_digest: String,
        edits: Vec<CanonicalTextEdit>,
    },
    Create {
        index: u64,
        uri: String,
        path: PathBuf,
        overwrite: bool,
        ignore_if_exists: bool,
    },
    Rename {
        index: u64,
        old_uri: String,
        new_uri: String,
        old_path: PathBuf,
        new_path: PathBuf,
        overwrite: bool,
        ignore_if_exists: bool,
    },
    Delete {
        index: u64,
        uri: String,
        path: PathBuf,
        recursive: bool,
        ignore_if_not_exists: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct CanonicalPlan {
    pub(crate) operations: Vec<CanonicalOperation>,
    pub(crate) before_manifest: Vec<ManifestEntry>,
    pub(crate) intended_manifest: Vec<ManifestEntry>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PreviewSummary {
    pub(crate) files_changed: u64,
    pub(crate) text_edits: u64,
    pub(crate) creates: u64,
    pub(crate) renames: u64,
    pub(crate) deletes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct WorkspaceEditProblem {
    pub(crate) code: String,
    pub(crate) message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) operation_index: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) path: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) annotation_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) data: Option<Value>,
}

#[derive(Debug)]
pub(crate) struct PlannedWorkspaceEdit {
    pub(crate) plan: CanonicalPlan,
    pub(crate) annotations: Value,
    pub(crate) summary: PreviewSummary,
}

#[derive(Debug)]
pub(crate) struct WorkspaceEditPlanner<'a> {
    workspace: &'a Path,
    position_encoding: PositionEncoding,
    preview_limits: &'a PreviewSettings,
    mutation_limits: &'a MutationSettings,
    root_device: Option<u64>,
    _workspace_dir: Dir,
}

#[derive(Debug, Clone)]
struct ResourceState {
    manifest: ManifestEntry,
    text: Option<Vec<u8>>,
}

#[derive(Default)]
struct VirtualWorkspace {
    entries: BTreeMap<PathBuf, ResourceState>,
    before: BTreeMap<PathBuf, ManifestEntry>,
    affected: BTreeSet<PathBuf>,
    rollback_bytes: u64,
}

impl<'a> WorkspaceEditPlanner<'a> {
    /// Opens the canonical Workspace as the capability root for Mutation planning.
    pub(crate) fn open(
        workspace: &'a Path,
        position_encoding: PositionEncoding,
        preview_limits: &'a PreviewSettings,
        mutation_limits: &'a MutationSettings,
    ) -> Result<Self, WorkspaceEditProblem> {
        if workspace.to_str().is_none() {
            return Err(problem(
                "filesystem_capability_unavailable",
                "The Workspace path must be representable as a UTF-8 JSON string.",
                None,
                None,
                None,
            ));
        }
        let workspace_dir =
            Dir::open_ambient_dir(workspace, ambient_authority()).map_err(|error| {
                problem(
                    "filesystem_capability_unavailable",
                    "The Workspace cannot be opened as a filesystem capability.",
                    None,
                    Some(workspace),
                    Some(json!({"reason": error.to_string()})),
                )
            })?;
        let metadata = fs::metadata(workspace).map_err(|error| {
            problem(
                "filesystem_capability_unavailable",
                "The Workspace filesystem cannot be inspected.",
                None,
                Some(workspace),
                Some(json!({"reason": error.to_string()})),
            )
        })?;
        Ok(Self {
            workspace,
            position_encoding,
            preview_limits,
            mutation_limits,
            root_device: metadata_device(&metadata),
            _workspace_dir: workspace_dir,
        })
    }

    /// Validates one lossless LSP WorkspaceEdit and produces its exact ordered byte plan.
    pub(crate) fn plan_workspace_edit(
        &self,
        edit: &Value,
    ) -> Result<PlannedWorkspaceEdit, Vec<WorkspaceEditProblem>> {
        self.plan_workspace_edit_with_document_preconditions(edit, &BTreeMap::new())
    }

    pub(crate) fn plan_workspace_edit_with_document_preconditions(
        &self,
        edit: &Value,
        document_version_preconditions: &BTreeMap<String, DocumentVersionPrecondition>,
    ) -> Result<PlannedWorkspaceEdit, Vec<WorkspaceEditProblem>> {
        let Some(object) = edit.as_object() else {
            return Err(vec![problem(
                "unknown_operation",
                "WorkspaceEdit must be a JSON object.",
                None,
                None,
                None,
            )]);
        };
        if let Err(problem) = validate_object_keys(
            object,
            &["changes", "documentChanges", "changeAnnotations"],
            None,
        ) {
            return Err(vec![problem]);
        }
        let annotations = object
            .get("changeAnnotations")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let annotation_map = match annotations.as_object() {
            Some(map) => map,
            None => {
                return Err(vec![problem(
                    "missing_annotation",
                    "changeAnnotations must be an object.",
                    None,
                    None,
                    None,
                )]);
            }
        };
        let has_changes = object.get("changes").is_some_and(|value| !value.is_null());
        let has_document_changes = object
            .get("documentChanges")
            .is_some_and(|value| !value.is_null());
        if has_changes && has_document_changes {
            return Err(vec![problem(
                "both_changes_and_document_changes",
                "WorkspaceEdit cannot contain both changes and documentChanges.",
                None,
                None,
                None,
            )]);
        }

        let mut virtual_workspace = VirtualWorkspace::default();
        let mut operations = Vec::new();
        let mut summary = PreviewSummary::default();
        let mut problems = Vec::new();
        let mut total_new_text_bytes = 0_u64;

        if has_changes {
            let Some(changes) = object.get("changes").and_then(Value::as_object) else {
                return Err(vec![problem(
                    "unknown_operation",
                    "WorkspaceEdit.changes must be an object.",
                    None,
                    None,
                    None,
                )]);
            };
            let mut entries = changes.iter().collect::<Vec<_>>();
            entries.sort_by_key(|(uri, _)| *uri);
            for (index, (uri, edits)) in entries.into_iter().enumerate() {
                self.plan_text_operation(
                    index as u64,
                    uri,
                    edits,
                    annotation_map,
                    &mut virtual_workspace,
                    &mut operations,
                    &mut summary,
                    &mut total_new_text_bytes,
                    &mut problems,
                    None,
                );
            }
        } else if has_document_changes {
            let Some(changes) = object.get("documentChanges").and_then(Value::as_array) else {
                return Err(vec![problem(
                    "unknown_operation",
                    "WorkspaceEdit.documentChanges must be an array.",
                    None,
                    None,
                    None,
                )]);
            };
            for (index, change) in changes.iter().enumerate() {
                let index = index as u64;
                let Some(change) = change.as_object() else {
                    problems.push(problem(
                        "unknown_operation",
                        "A documentChanges entry must be an object.",
                        Some(index),
                        None,
                        None,
                    ));
                    continue;
                };
                if let Some(text_document) = change.get("textDocument") {
                    if let Err(problem) =
                        validate_object_keys(change, &["textDocument", "edits"], Some(index))
                    {
                        problems.push(problem);
                        continue;
                    }
                    let Some(text_document) = text_document.as_object() else {
                        problems.push(problem(
                            "unknown_operation",
                            "TextDocumentEdit.textDocument must be an object.",
                            Some(index),
                            None,
                            None,
                        ));
                        continue;
                    };
                    if let Err(problem) =
                        validate_object_keys(text_document, &["uri", "version"], Some(index))
                    {
                        problems.push(problem);
                        continue;
                    }
                    let Some(uri) = text_document.get("uri").and_then(Value::as_str) else {
                        problems.push(problem(
                            "malformed_uri",
                            "TextDocumentEdit is missing a string uri.",
                            Some(index),
                            None,
                            None,
                        ));
                        continue;
                    };
                    let version = text_document.get("version");
                    let document_precondition = version
                        .filter(|version| !version.is_null())
                        .and_then(Value::as_i64)
                        .and_then(|version| {
                            document_version_precondition(
                                document_version_preconditions,
                                uri,
                                version,
                            )
                        });
                    if version.is_some_and(|version| !version.is_null())
                        && document_precondition.is_none()
                    {
                        problems.push(problem(
                            "invalid_document_version",
                            "A local Workspace Edit cannot prove the originating Document version.",
                            Some(index),
                            None,
                            None,
                        ));
                        continue;
                    }
                    self.plan_text_operation(
                        index,
                        uri,
                        change.get("edits").unwrap_or(&Value::Null),
                        annotation_map,
                        &mut virtual_workspace,
                        &mut operations,
                        &mut summary,
                        &mut total_new_text_bytes,
                        &mut problems,
                        document_precondition.map(|known| known.digest.as_str()),
                    );
                    continue;
                }
                match change.get("kind").and_then(Value::as_str) {
                    Some("create") => self.plan_create_operation(
                        index,
                        change,
                        annotation_map,
                        &mut virtual_workspace,
                        &mut operations,
                        &mut summary,
                        &mut problems,
                    ),
                    Some("rename") => self.plan_rename_operation(
                        index,
                        change,
                        annotation_map,
                        &mut virtual_workspace,
                        &mut operations,
                        &mut summary,
                        &mut problems,
                    ),
                    Some("delete") => self.plan_delete_operation(
                        index,
                        change,
                        annotation_map,
                        &mut virtual_workspace,
                        &mut operations,
                        &mut summary,
                        &mut problems,
                    ),
                    _ => problems.push(problem(
                        "unknown_operation",
                        "The documentChanges operation kind is unsupported.",
                        Some(index),
                        None,
                        Some(Value::Object(change.clone())),
                    )),
                }
            }
        }

        if virtual_workspace.affected.len() as u64 > self.mutation_limits.max_entries {
            problems.push(problem(
                "resource_limit_exceeded",
                "The Mutation affects too many filesystem entries.",
                None,
                None,
                Some(json!({
                    "resource": "entries",
                    "limit": self.mutation_limits.max_entries,
                    "observed": virtual_workspace.affected.len()
                })),
            ));
        }
        if virtual_workspace.rollback_bytes > self.mutation_limits.max_rollback_bytes {
            problems.push(problem(
                "resource_limit_exceeded",
                "The Mutation needs too much rollback storage.",
                None,
                None,
                Some(json!({
                    "resource": "rollbackBytes",
                    "limit": self.mutation_limits.max_rollback_bytes,
                    "observed": virtual_workspace.rollback_bytes
                })),
            ));
        }
        if total_new_text_bytes > self.preview_limits.max_text_bytes
            || total_new_text_bytes > self.mutation_limits.max_staged_text_bytes
        {
            problems.push(problem(
                "resource_limit_exceeded",
                "The Mutation contains too much replacement text.",
                None,
                None,
                Some(json!({
                    "resource": "stagedTextBytes",
                    "limit": self.preview_limits.max_text_bytes.min(self.mutation_limits.max_staged_text_bytes),
                    "observed": total_new_text_bytes
                })),
            ));
        }
        if !problems.is_empty() {
            return Err(problems);
        }

        for path in virtual_workspace.affected.clone() {
            if let std::collections::btree_map::Entry::Vacant(entry) =
                virtual_workspace.before.entry(path)
            {
                let before = self
                    .inspect_resource(entry.key(), 0, false)
                    .map_err(|problem| vec![problem])?
                    .manifest;
                entry.insert(before);
            }
        }
        let before_manifest = virtual_workspace.before.into_values().collect::<Vec<_>>();
        let intended_manifest = virtual_workspace
            .affected
            .iter()
            .map(|path| {
                let mut entry = virtual_workspace
                    .entries
                    .get(path)
                    .map(|state| state.manifest.clone())
                    .unwrap_or_else(|| missing_manifest(path));
                if entry.resource_kind == ResourceKind::Directory {
                    let children = virtual_workspace
                        .entries
                        .range(path.clone()..)
                        .take_while(|(child, _)| child.starts_with(path))
                        .filter(|(child, state)| {
                            child.parent() == Some(path.as_path()) && state.manifest.exists
                        })
                        .map(|(child, state)| {
                            (
                                child.file_name().unwrap().to_str().unwrap().to_owned(),
                                state.manifest.resource_kind,
                            )
                        })
                        .collect::<BTreeMap<_, _>>();
                    entry.content_digest = Some(directory_membership_digest(&children));
                }
                entry
            })
            .collect::<Vec<_>>();
        let before_by_path = before_manifest
            .iter()
            .map(|entry| (&entry.path, entry))
            .collect::<BTreeMap<_, _>>();
        summary.files_changed = intended_manifest
            .iter()
            .filter(|after| {
                before_by_path
                    .get(&after.path)
                    .is_none_or(|before| *before != *after)
            })
            .count() as u64;
        Ok(PlannedWorkspaceEdit {
            plan: CanonicalPlan {
                operations,
                before_manifest,
                intended_manifest,
            },
            annotations,
            summary,
        })
    }

    /// Re-reads exact manifest paths through the same no-follow validation used by planning.
    pub(crate) fn inspect_manifest_paths(
        &self,
        paths: &[PathBuf],
    ) -> Result<Vec<ManifestEntry>, Vec<WorkspaceEditProblem>> {
        let mut manifest = Vec::with_capacity(paths.len());
        let mut problems = Vec::new();
        for path in paths {
            if let Err(problem) = self.validate_existing_ancestors(path, 0) {
                problems.push(problem);
                continue;
            }
            match self.inspect_resource(path, 0, false) {
                Ok(state) => manifest.push(state.manifest),
                Err(problem) => problems.push(problem),
            }
        }
        manifest.sort_by(|left, right| left.path.cmp(&right.path));
        if problems.is_empty() {
            Ok(manifest)
        } else {
            Err(problems)
        }
    }

    /// Observes only bound directory child sets; an omitted certificate never authorizes enumeration.
    pub(crate) fn inspect_manifest(
        &self,
        template: &[ManifestEntry],
    ) -> Result<Vec<ManifestEntry>, Vec<WorkspaceEditProblem>> {
        let paths = template
            .iter()
            .map(|entry| entry.path.clone())
            .collect::<Vec<_>>();
        let certified = template
            .iter()
            .filter(|entry| {
                entry.resource_kind == ResourceKind::Directory && entry.content_digest.is_some()
            })
            .map(|entry| &entry.path)
            .collect::<BTreeSet<_>>();
        let mut observed = self.inspect_manifest_paths(&paths)?;
        for entry in observed.iter_mut().filter(|entry| {
            entry.resource_kind == ResourceKind::Directory && certified.contains(&entry.path)
        }) {
            entry.content_digest = Some(directory_membership_digest(
                &self
                    .directory_children(&entry.path, 0)
                    .map_err(|problem| vec![problem])?,
            ));
        }
        Ok(observed)
    }

    pub(crate) fn capability_root(&self) -> &Dir {
        &self._workspace_dir
    }

    pub(crate) fn relative_path<'p>(&self, path: &'p Path) -> Result<&'p Path, String> {
        path.strip_prefix(self.workspace)
            .map_err(|_| "A canonical operation path escaped the Workspace capability.".to_owned())
    }

    #[allow(clippy::too_many_arguments)]
    fn plan_text_operation(
        &self,
        index: u64,
        uri: &str,
        edits_value: &Value,
        annotations: &Map<String, Value>,
        workspace: &mut VirtualWorkspace,
        operations: &mut Vec<CanonicalOperation>,
        summary: &mut PreviewSummary,
        total_new_text_bytes: &mut u64,
        problems: &mut Vec<WorkspaceEditProblem>,
        expected_digest: Option<&str>,
    ) {
        let path = match self.path_from_file_uri(uri, index) {
            Ok(path) => path,
            Err(problem) => {
                problems.push(problem);
                return;
            }
        };
        if let Err(problem) = self.load_exact_resource(workspace, &path, index, true) {
            problems.push(problem);
            return;
        }
        if let Some(expected_digest) = expected_digest {
            let actual_digest = workspace
                .before
                .get(&path)
                .and_then(|entry| entry.content_digest.as_deref());
            if actual_digest != Some(expected_digest) {
                problems.push(problem(
                    "invalid_document_version",
                    "The synchronized Document content no longer matches the originating version.",
                    Some(index),
                    Some(&path),
                    Some(json!({
                        "expectedDigest": expected_digest,
                        "actualDigest": actual_digest
                    })),
                ));
                return;
            }
        }
        let state = workspace.entries.get_mut(&path).unwrap();
        if state.manifest.resource_kind != ResourceKind::File {
            problems.push(problem(
                "unsupported_resource_kind",
                "Text edits require a regular file.",
                Some(index),
                Some(&path),
                None,
            ));
            return;
        }
        let Some(bytes) = state.text.as_ref() else {
            problems.push(problem(
                "resource_limit_exceeded",
                "The text Document exceeds the Preview per-Document limit.",
                Some(index),
                Some(&path),
                None,
            ));
            return;
        };
        let Ok(text) = std::str::from_utf8(bytes) else {
            problems.push(problem(
                "unsupported_resource_kind",
                "Text edits require a UTF-8 regular file.",
                Some(index),
                Some(&path),
                None,
            ));
            return;
        };
        let Some(raw_edits) = edits_value.as_array() else {
            problems.push(problem(
                "unknown_operation",
                "TextDocumentEdit.edits must be an array.",
                Some(index),
                Some(&path),
                None,
            ));
            return;
        };
        let mut edits = Vec::new();
        for (ordinal, edit) in raw_edits.iter().enumerate() {
            match parse_text_edit(
                text,
                edit,
                annotations,
                self.position_encoding,
                index,
                &path,
                ordinal,
            ) {
                Ok(edit) => edits.push(edit),
                Err(problem) => problems.push(problem),
            }
        }
        edits.sort_by_key(|edit| (edit.start_byte, edit.end_byte, edit.ordinal));
        for pair in edits.windows(2) {
            let left = &pair[0];
            let right = &pair[1];
            if right.start_byte < left.end_byte
                || (right.start_byte == left.start_byte
                    && (right.end_byte != right.start_byte || left.end_byte != left.start_byte))
            {
                problems.push(problem(
                    "overlapping_edits",
                    "Text edits overlap or ambiguously share a start position.",
                    Some(index),
                    Some(&path),
                    None,
                ));
                return;
            }
        }
        if problems
            .iter()
            .any(|problem| problem.operation_index == Some(index))
        {
            return;
        }
        let mut result = Vec::with_capacity(text.len());
        let mut cursor = 0;
        let mut canonical_edits = Vec::new();
        for edit in edits {
            result.extend_from_slice(&bytes[cursor..edit.start_byte]);
            result.extend_from_slice(edit.new_text.as_bytes());
            cursor = edit.end_byte;
            *total_new_text_bytes = total_new_text_bytes.saturating_add(edit.new_text.len() as u64);
            canonical_edits.push(CanonicalTextEdit {
                start_byte: edit.start_byte as u64,
                end_byte: edit.end_byte as u64,
                new_text: edit.new_text,
                annotation_id: edit.annotation_id,
            });
        }
        result.extend_from_slice(&bytes[cursor..]);
        let before_digest = state.manifest.content_digest.clone().unwrap();
        let after_digest = digest_raw_bytes(&result);
        if after_digest == before_digest {
            return;
        }
        state.text = Some(result);
        state.manifest.content_digest = Some(after_digest.clone());
        workspace.affected.insert(path.clone());
        summary.text_edits = summary
            .text_edits
            .saturating_add(canonical_edits.len() as u64);
        operations.push(CanonicalOperation::Text {
            index,
            uri: uri.to_owned(),
            path,
            before_digest,
            after_digest,
            edits: canonical_edits,
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn plan_create_operation(
        &self,
        index: u64,
        operation: &Map<String, Value>,
        annotations: &Map<String, Value>,
        workspace: &mut VirtualWorkspace,
        operations: &mut Vec<CanonicalOperation>,
        summary: &mut PreviewSummary,
        problems: &mut Vec<WorkspaceEditProblem>,
    ) {
        if let Err(problem) = validate_resource_operation(
            operation,
            &["kind", "uri", "options", "annotationId"],
            &["overwrite", "ignoreIfExists"],
            index,
        ) {
            problems.push(problem);
            return;
        }
        if let Err(problem) = validate_annotation(operation, annotations, index) {
            problems.push(problem);
            return;
        }
        let Some(uri) = operation.get("uri").and_then(Value::as_str) else {
            problems.push(problem(
                "malformed_uri",
                "CreateFile is missing uri.",
                Some(index),
                None,
                None,
            ));
            return;
        };
        let path = match self.path_from_file_uri(uri, index) {
            Ok(path) => path,
            Err(problem) => {
                problems.push(problem);
                return;
            }
        };
        if let Err(problem) = self.load_exact_resource(workspace, &path, index, false) {
            problems.push(problem);
            return;
        }
        let options = operation.get("options").and_then(Value::as_object);
        let overwrite = option_bool(options, "overwrite");
        let ignore_if_exists = option_bool(options, "ignoreIfExists");
        let current = workspace.entries.get(&path).unwrap().clone();
        if current.manifest.exists && !overwrite {
            if ignore_if_exists {
                return;
            }
            problems.push(problem(
                "target_exists",
                "CreateFile target already exists.",
                Some(index),
                Some(&path),
                None,
            ));
            return;
        }
        if current.manifest.exists && current.manifest.resource_kind != ResourceKind::File {
            problems.push(problem(
                "resource_kind_mismatch",
                "CreateFile overwrite requires a regular file target.",
                Some(index),
                Some(&path),
                None,
            ));
            return;
        }
        if !current.manifest.exists
            && let Err(problem) = self.require_existing_parent(workspace, &path, index)
        {
            problems.push(problem);
            return;
        }
        let mut next = if current.manifest.exists {
            current
        } else {
            missing_resource(&path)
        };
        next.manifest.exists = true;
        next.manifest.resource_kind = ResourceKind::File;
        next.manifest.content_digest = Some(digest_raw_bytes(&[]));
        next.text = Some(Vec::new());
        workspace.entries.insert(path.clone(), next);
        workspace.affected.insert(path.clone());
        summary.creates = summary.creates.saturating_add(1);
        operations.push(CanonicalOperation::Create {
            index,
            uri: uri.to_owned(),
            path,
            overwrite,
            ignore_if_exists,
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn plan_rename_operation(
        &self,
        index: u64,
        operation: &Map<String, Value>,
        annotations: &Map<String, Value>,
        workspace: &mut VirtualWorkspace,
        operations: &mut Vec<CanonicalOperation>,
        summary: &mut PreviewSummary,
        problems: &mut Vec<WorkspaceEditProblem>,
    ) {
        if let Err(problem) = validate_resource_operation(
            operation,
            &["kind", "oldUri", "newUri", "options", "annotationId"],
            &["overwrite", "ignoreIfExists"],
            index,
        ) {
            problems.push(problem);
            return;
        }
        if let Err(problem) = validate_annotation(operation, annotations, index) {
            problems.push(problem);
            return;
        }
        let (Some(old_uri), Some(new_uri)) = (
            operation.get("oldUri").and_then(Value::as_str),
            operation.get("newUri").and_then(Value::as_str),
        ) else {
            problems.push(problem(
                "malformed_uri",
                "RenameFile requires oldUri and newUri.",
                Some(index),
                None,
                None,
            ));
            return;
        };
        let old_path = match self.path_from_file_uri(old_uri, index) {
            Ok(path) => path,
            Err(problem) => {
                problems.push(problem);
                return;
            }
        };
        let new_path = match self.path_from_file_uri(new_uri, index) {
            Ok(path) => path,
            Err(problem) => {
                problems.push(problem);
                return;
            }
        };
        if old_path == new_path {
            problems.push(problem(
                "resource_alias",
                "RenameFile source and destination identify the same path.",
                Some(index),
                Some(&old_path),
                None,
            ));
            return;
        }
        if old_path.starts_with(&new_path) || new_path.starts_with(&old_path) {
            problems.push(problem(
                "ambiguous_path_sequence",
                "RenameFile cannot move a path into its own ancestor or descendant.",
                Some(index),
                Some(&old_path),
                None,
            ));
            return;
        }
        if let Err(problem) = self.load_resource_tree(workspace, &old_path, index) {
            problems.push(problem);
            return;
        }
        if let Err(problem) = self.load_exact_resource(workspace, &new_path, index, false) {
            problems.push(problem);
            return;
        }
        let options = operation.get("options").and_then(Value::as_object);
        let overwrite = option_bool(options, "overwrite");
        let ignore_if_exists = option_bool(options, "ignoreIfExists");
        let source = workspace.entries.get(&old_path).unwrap().clone();
        if !source.manifest.exists {
            problems.push(problem(
                "source_missing",
                "RenameFile source does not exist.",
                Some(index),
                Some(&old_path),
                None,
            ));
            return;
        }
        let destination = workspace.entries.get(&new_path).unwrap().clone();
        if destination.manifest.exists && !overwrite {
            if ignore_if_exists {
                return;
            }
            problems.push(problem(
                "target_exists",
                "RenameFile destination already exists.",
                Some(index),
                Some(&new_path),
                None,
            ));
            return;
        }
        if destination.manifest.exists
            && destination.manifest.resource_kind != source.manifest.resource_kind
        {
            problems.push(problem(
                "resource_kind_mismatch",
                "RenameFile overwrite requires matching resource kinds.",
                Some(index),
                Some(&new_path),
                None,
            ));
            return;
        }
        if destination.manifest.resource_kind == ResourceKind::Directory
            && let Err(problem) = self.load_resource_tree(workspace, &new_path, index)
        {
            problems.push(problem);
            return;
        }
        if let Err(problem) = self.require_existing_parent(workspace, &new_path, index) {
            problems.push(problem);
            return;
        }
        if nearest_existing_device(&old_path) != nearest_existing_device(&new_path) {
            problems.push(problem(
                "cross_volume_rename",
                "RenameFile cannot cross filesystem volumes.",
                Some(index),
                Some(&new_path),
                None,
            ));
            return;
        }
        let moved = workspace
            .entries
            .range(old_path.clone()..)
            .take_while(|(path, _)| path.starts_with(&old_path))
            .map(|(path, state)| (path.clone(), state.clone()))
            .collect::<Vec<_>>();
        let destination_tree = workspace
            .entries
            .range(new_path.clone()..)
            .take_while(|(path, _)| path.starts_with(&new_path))
            .map(|(path, _)| path.clone())
            .collect::<Vec<_>>();
        for path in destination_tree {
            workspace
                .entries
                .insert(path.clone(), missing_resource(&path));
            workspace.affected.insert(path);
        }
        for (path, state) in &moved {
            workspace
                .entries
                .insert(path.clone(), missing_resource(path));
            workspace.affected.insert(path.clone());
            let relative = path.strip_prefix(&old_path).unwrap();
            let target = if relative.as_os_str().is_empty() {
                new_path.clone()
            } else {
                new_path.join(relative)
            };
            let mut state = state.clone();
            state.manifest.path = target.clone();
            workspace.entries.insert(target.clone(), state);
            workspace.affected.insert(target);
        }
        summary.renames = summary.renames.saturating_add(1);
        operations.push(CanonicalOperation::Rename {
            index,
            old_uri: old_uri.to_owned(),
            new_uri: new_uri.to_owned(),
            old_path,
            new_path,
            overwrite,
            ignore_if_exists,
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn plan_delete_operation(
        &self,
        index: u64,
        operation: &Map<String, Value>,
        annotations: &Map<String, Value>,
        workspace: &mut VirtualWorkspace,
        operations: &mut Vec<CanonicalOperation>,
        summary: &mut PreviewSummary,
        problems: &mut Vec<WorkspaceEditProblem>,
    ) {
        if let Err(problem) = validate_resource_operation(
            operation,
            &["kind", "uri", "options", "annotationId"],
            &["recursive", "ignoreIfNotExists"],
            index,
        ) {
            problems.push(problem);
            return;
        }
        if let Err(problem) = validate_annotation(operation, annotations, index) {
            problems.push(problem);
            return;
        }
        let Some(uri) = operation.get("uri").and_then(Value::as_str) else {
            problems.push(problem(
                "malformed_uri",
                "DeleteFile is missing uri.",
                Some(index),
                None,
                None,
            ));
            return;
        };
        let path = match self.path_from_file_uri(uri, index) {
            Ok(path) => path,
            Err(problem) => {
                problems.push(problem);
                return;
            }
        };
        if let Err(problem) = self.load_resource_tree(workspace, &path, index) {
            problems.push(problem);
            return;
        }
        let options = operation.get("options").and_then(Value::as_object);
        let recursive = option_bool(options, "recursive");
        let ignore_if_not_exists = option_bool(options, "ignoreIfNotExists");
        let source = workspace.entries.get(&path).unwrap();
        if !source.manifest.exists {
            if ignore_if_not_exists {
                return;
            }
            problems.push(problem(
                "source_missing",
                "DeleteFile target does not exist.",
                Some(index),
                Some(&path),
                None,
            ));
            return;
        }
        let subtree = workspace
            .entries
            .range(path.clone()..)
            .take_while(|(candidate, _)| candidate.starts_with(&path))
            .filter(|(_, state)| state.manifest.exists)
            .map(|(candidate, _)| candidate.clone())
            .collect::<Vec<_>>();
        if source.manifest.resource_kind == ResourceKind::Directory
            && subtree.len() > 1
            && !recursive
        {
            problems.push(problem(
                "directory_not_empty",
                "Deleting a non-empty directory requires recursive=true.",
                Some(index),
                Some(&path),
                None,
            ));
            return;
        }
        for target in subtree {
            workspace
                .entries
                .insert(target.clone(), missing_resource(&target));
            workspace.affected.insert(target);
        }
        summary.deletes = summary.deletes.saturating_add(1);
        operations.push(CanonicalOperation::Delete {
            index,
            uri: uri.to_owned(),
            path,
            recursive,
            ignore_if_not_exists,
        });
    }

    fn path_from_file_uri(&self, raw: &str, index: u64) -> Result<PathBuf, WorkspaceEditProblem> {
        if uri_contains_path_traversal(raw) {
            return Err(problem(
                "path_traversal",
                "Mutation URIs cannot contain dot path segments.",
                Some(index),
                None,
                Some(json!({"uri": raw})),
            ));
        }
        let uri = Url::parse(raw).map_err(|_| {
            problem(
                "malformed_uri",
                "The operation URI is malformed.",
                Some(index),
                None,
                Some(json!({"uri": raw})),
            )
        })?;
        if uri.scheme() != "file" {
            return Err(problem(
                "unsupported_uri_scheme",
                "Mutation supports only file: URIs.",
                Some(index),
                None,
                Some(json!({"uri": raw})),
            ));
        }
        let path = uri.to_file_path().map_err(|()| {
            problem(
                "malformed_uri",
                "The file URI cannot be converted to a native path.",
                Some(index),
                None,
                Some(json!({"uri": raw})),
            )
        })?;
        if path.to_str().is_none() {
            return Err(problem(
                "malformed_uri",
                "Mutation paths must be representable as UTF-8 JSON strings.",
                Some(index),
                None,
                Some(json!({"uri": raw})),
            ));
        }
        if path == self.workspace {
            return Err(problem(
                "workspace_root_mutation",
                "The Workspace root cannot be mutated.",
                Some(index),
                Some(&path),
                None,
            ));
        }
        if !lexically_beneath(self.workspace, &path) {
            return Err(problem(
                "outside_workspace",
                "The Mutation path is outside the Workspace.",
                Some(index),
                Some(&path),
                None,
            ));
        }
        self.validate_existing_ancestors(&path, index)?;
        Ok(path)
    }

    fn validate_existing_ancestors(
        &self,
        path: &Path,
        index: u64,
    ) -> Result<(), WorkspaceEditProblem> {
        let relative = path.strip_prefix(self.workspace).map_err(|_| {
            problem(
                "outside_workspace",
                "The Mutation path is outside the Workspace.",
                Some(index),
                Some(path),
                None,
            )
        })?;
        let mut current = self.workspace.to_path_buf();
        for component in relative.components() {
            current.push(component.as_os_str());
            match fs::symlink_metadata(&current) {
                Ok(metadata) => {
                    if metadata.file_type().is_symlink() || metadata_is_reparse_point(&metadata) {
                        return Err(problem(
                            "symlink_or_reparse_point",
                            "Mutation paths cannot traverse symlinks or reparse points.",
                            Some(index),
                            Some(&current),
                            None,
                        ));
                    }
                    if metadata_device(&metadata) != self.root_device {
                        return Err(problem(
                            "volume_boundary",
                            "Mutation paths cannot cross a nested filesystem volume.",
                            Some(index),
                            Some(&current),
                            None,
                        ));
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
                Err(error) => {
                    return Err(problem(
                        "filesystem_capability_unavailable",
                        "A Mutation path cannot be inspected.",
                        Some(index),
                        Some(&current),
                        Some(json!({"reason": error.to_string()})),
                    ));
                }
            }
        }
        Ok(())
    }

    fn load_exact_resource(
        &self,
        workspace: &mut VirtualWorkspace,
        path: &Path,
        index: u64,
        load_text: bool,
    ) -> Result<(), WorkspaceEditProblem> {
        if workspace.entries.contains_key(path) {
            if load_text
                && workspace.entries[path].text.is_none()
                && workspace.entries[path].manifest.exists
                && workspace.entries[path].manifest.resource_kind == ResourceKind::File
            {
                let bytes =
                    read_text_file(path, self.preview_limits.max_document_text_bytes, index)?;
                workspace.entries.get_mut(path).unwrap().text = Some(bytes);
            }
            return Ok(());
        }
        let state = if path.ancestors().skip(1).any(|ancestor| {
            workspace
                .entries
                .get(ancestor)
                .is_some_and(|state| !state.manifest.exists)
        }) {
            missing_resource(path)
        } else {
            self.inspect_resource(path, index, load_text)?
        };
        if state.manifest.exists {
            workspace.rollback_bytes = workspace
                .rollback_bytes
                .saturating_add(resource_rollback_bytes(path, &state.manifest));
        }
        workspace
            .before
            .insert(path.to_path_buf(), state.manifest.clone());
        workspace.entries.insert(path.to_path_buf(), state);
        Ok(())
    }

    fn load_resource_tree(
        &self,
        workspace: &mut VirtualWorkspace,
        path: &Path,
        index: u64,
    ) -> Result<(), WorkspaceEditProblem> {
        self.load_exact_resource(workspace, path, index, false)?;
        if workspace.entries[path].manifest.resource_kind != ResourceKind::Directory {
            return Ok(());
        }
        let mut pending = vec![(path.to_path_buf(), 0_u64)];
        while let Some((directory, depth)) = pending.pop() {
            if depth > self.mutation_limits.max_recursion_depth {
                return Err(problem(
                    "resource_limit_exceeded",
                    "A recursive operation exceeds the configured depth.",
                    Some(index),
                    Some(&directory),
                    Some(
                        json!({"resource": "recursionDepth", "limit": self.mutation_limits.max_recursion_depth, "observed": depth}),
                    ),
                ));
            }
            if workspace.entries[&directory]
                .manifest
                .content_digest
                .is_none()
            {
                let children = self.directory_children(&directory, index)?;
                let digest = directory_membership_digest(&children);
                workspace.before.get_mut(&directory).unwrap().content_digest = Some(digest.clone());
                workspace
                    .entries
                    .get_mut(&directory)
                    .unwrap()
                    .manifest
                    .content_digest = Some(digest);
                for name in children.keys() {
                    self.load_exact_resource(workspace, &directory.join(name), index, false)?;
                }
            }
            let children = workspace
                .entries
                .range(directory.clone()..)
                .take_while(|(path, _)| path.starts_with(&directory))
                .filter(|(path, state)| {
                    path.parent() == Some(directory.as_path()) && state.manifest.exists
                })
                .map(|(path, _)| path.clone())
                .collect::<Vec<_>>();
            for path in children {
                if workspace.entries[&path].manifest.resource_kind == ResourceKind::Directory {
                    pending.push((path, depth.saturating_add(1)));
                }
                if workspace.entries.len() as u64 > self.mutation_limits.max_entries {
                    return Err(problem(
                        "resource_limit_exceeded",
                        "A recursive operation affects too many entries.",
                        Some(index),
                        Some(&directory),
                        Some(
                            json!({"resource": "entries", "limit": self.mutation_limits.max_entries, "observed": workspace.entries.len()}),
                        ),
                    ));
                }
            }
        }
        Ok(())
    }

    fn directory_children(
        &self,
        path: &Path,
        index: u64,
    ) -> Result<BTreeMap<String, ResourceKind>, WorkspaceEditProblem> {
        self.validate_existing_ancestors(path, index)?;
        let unavailable = |error: std::io::Error| {
            problem(
                "filesystem_capability_unavailable",
                "A directory cannot be enumerated.",
                Some(index),
                Some(path),
                Some(json!({"reason": error.to_string()})),
            )
        };
        let directory = self
            ._workspace_dir
            .open_dir_nofollow(path.strip_prefix(self.workspace).unwrap())
            .map_err(unavailable)?;
        let mut children = BTreeMap::new();
        for entry in directory.entries().map_err(unavailable)? {
            let entry = entry.map_err(unavailable)?;
            let name = entry.file_name().into_string().map_err(|_| {
                problem(
                    "unsupported_resource_kind",
                    "Directory child names must be representable as UTF-8 JSON strings.",
                    Some(index),
                    Some(path),
                    None,
                )
            })?;
            let child = path.join(&name);
            self.validate_existing_ancestors(&child, index)?;
            let kind = entry.file_type().map_err(unavailable)?;
            let kind = if kind.is_file() {
                ResourceKind::File
            } else if kind.is_dir() {
                ResourceKind::Directory
            } else {
                return Err(problem(
                    "unsupported_resource_kind",
                    "The Mutation resource is not a regular file or directory.",
                    Some(index),
                    Some(&child),
                    None,
                ));
            };
            children.insert(name, kind);
            if children.len() as u64 >= self.mutation_limits.max_entries {
                return Err(problem(
                    "resource_limit_exceeded",
                    "A directory has too many entries.",
                    Some(index),
                    Some(path),
                    Some(json!({
                        "resource": "entries", "limit": self.mutation_limits.max_entries, "observed": children.len() + 1,
                    })),
                ));
            }
        }
        Ok(children)
    }

    fn inspect_resource(
        &self,
        path: &Path,
        index: u64,
        load_text: bool,
    ) -> Result<ResourceState, WorkspaceEditProblem> {
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(missing_resource(path));
            }
            Err(error) => {
                return Err(problem(
                    "filesystem_capability_unavailable",
                    "A filesystem resource cannot be inspected.",
                    Some(index),
                    Some(path),
                    Some(json!({"reason": error.to_string()})),
                ));
            }
        };
        if metadata.file_type().is_symlink() || metadata_is_reparse_point(&metadata) {
            return Err(problem(
                "symlink_or_reparse_point",
                "Symlinks and reparse points are unsupported Mutation resources.",
                Some(index),
                Some(path),
                None,
            ));
        }
        let resource_kind = if metadata.is_file() {
            ResourceKind::File
        } else if metadata.is_dir() {
            ResourceKind::Directory
        } else {
            return Err(problem(
                "unsupported_resource_kind",
                "The Mutation resource is not a regular file or directory.",
                Some(index),
                Some(path),
                None,
            ));
        };
        if resource_kind == ResourceKind::File && metadata_link_count(path, &metadata, index)? > 1 {
            return Err(problem(
                "hard_link",
                "Regular files with multiple hard links are unsupported.",
                Some(index),
                Some(path),
                None,
            ));
        }
        if resource_kind == ResourceKind::File && metadata_is_sparse(&metadata) {
            return Err(problem(
                "sparse_file_unsupported",
                "Sparse files are unsupported Mutation resources.",
                Some(index),
                Some(path),
                None,
            ));
        }
        let content_digest = if resource_kind == ResourceKind::File {
            Some(hash_file(path, index)?)
        } else {
            None
        };
        let text = if load_text {
            Some(read_text_file(
                path,
                self.preview_limits.max_document_text_bytes,
                index,
            )?)
        } else {
            None
        };
        Ok(ResourceState {
            manifest: ManifestEntry {
                path: path.to_path_buf(),
                exists: true,
                resource_kind,
                identity_digest: Some(identity_digest(path, &metadata, index)?),
                content_digest,
                metadata_digest: Some(metadata_digest(path, &metadata, index)?),
            },
            text,
        })
    }

    fn require_existing_parent(
        &self,
        workspace: &VirtualWorkspace,
        path: &Path,
        index: u64,
    ) -> Result<(), WorkspaceEditProblem> {
        let parent = path.parent().unwrap_or(self.workspace);
        if let Some(state) = workspace.entries.get(parent) {
            return if state.manifest.resource_kind == ResourceKind::Directory {
                Ok(())
            } else {
                Err(problem(
                    "parent_missing",
                    "The operation parent is not a directory.",
                    Some(index),
                    Some(parent),
                    None,
                ))
            };
        }
        let metadata = fs::symlink_metadata(parent).map_err(|_| {
            problem(
                "parent_missing",
                "The operation parent directory does not exist.",
                Some(index),
                Some(parent),
                None,
            )
        })?;
        if !metadata.is_dir() {
            return Err(problem(
                "parent_missing",
                "The operation parent is not a directory.",
                Some(index),
                Some(parent),
                None,
            ));
        }
        Ok(())
    }
}

#[derive(Debug)]
struct ParsedTextEdit {
    start_byte: usize,
    end_byte: usize,
    new_text: String,
    annotation_id: Option<String>,
    ordinal: usize,
}

fn parse_text_edit(
    text: &str,
    edit: &Value,
    annotations: &Map<String, Value>,
    encoding: PositionEncoding,
    index: u64,
    path: &Path,
    ordinal: usize,
) -> Result<ParsedTextEdit, WorkspaceEditProblem> {
    let object = edit.as_object().ok_or_else(|| {
        problem(
            "unknown_operation",
            "A text edit must be an object.",
            Some(index),
            Some(path),
            None,
        )
    })?;
    validate_object_keys(
        object,
        &[
            "range",
            "newText",
            "annotationId",
            "insertTextFormat",
            "snippet",
        ],
        Some(index),
    )?;
    if object.contains_key("insertTextFormat") || object.contains_key("snippet") {
        return Err(problem(
            "snippet_edit_unsupported",
            "Snippet text edits are unsupported.",
            Some(index),
            Some(path),
            None,
        ));
    }
    let new_text = object
        .get("newText")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            problem(
                "snippet_edit_unsupported",
                "Text edit newText must be a string.",
                Some(index),
                Some(path),
                None,
            )
        })?
        .to_owned();
    let annotation_id = object
        .get("annotationId")
        .and_then(Value::as_str)
        .map(str::to_owned);
    if let Some(annotation_id) = &annotation_id
        && !annotations.contains_key(annotation_id)
    {
        return Err(WorkspaceEditProblem {
            code: "missing_annotation".to_owned(),
            message: "A text edit references a missing change annotation.".to_owned(),
            operation_index: Some(index),
            path: Some(path.to_path_buf()),
            annotation_id: Some(annotation_id.clone()),
            data: None,
        });
    }
    let range = object
        .get("range")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            problem(
                "invalid_range",
                "A text edit requires a range.",
                Some(index),
                Some(path),
                None,
            )
        })?;
    let start = parse_position(range.get("start")).ok_or_else(|| {
        problem(
            "invalid_range",
            "A text edit start position is invalid.",
            Some(index),
            Some(path),
            None,
        )
    })?;
    let end = parse_position(range.get("end")).ok_or_else(|| {
        problem(
            "invalid_range",
            "A text edit end position is invalid.",
            Some(index),
            Some(path),
            None,
        )
    })?;
    let start_byte = position_to_offset(text, start, encoding).ok_or_else(|| {
        problem(
            "invalid_range",
            "A text edit start position is outside the UTF-8 Document.",
            Some(index),
            Some(path),
            None,
        )
    })?;
    let end_byte = position_to_offset(text, end, encoding).ok_or_else(|| {
        problem(
            "invalid_range",
            "A text edit end position is outside the UTF-8 Document.",
            Some(index),
            Some(path),
            None,
        )
    })?;
    if end_byte < start_byte {
        return Err(problem(
            "invalid_range",
            "A text edit range ends before it starts.",
            Some(index),
            Some(path),
            None,
        ));
    }
    Ok(ParsedTextEdit {
        start_byte,
        end_byte,
        new_text,
        annotation_id,
        ordinal,
    })
}

fn parse_position(value: Option<&Value>) -> Option<ProtocolPosition> {
    let object = value?.as_object()?;
    Some(ProtocolPosition {
        line: u32::try_from(object.get("line")?.as_u64()?).ok()?,
        character: u32::try_from(object.get("character")?.as_u64()?).ok()?,
    })
}

fn position_to_offset(
    text: &str,
    position: ProtocolPosition,
    encoding: PositionEncoding,
) -> Option<usize> {
    let mut offset = 0;
    for (line_number, segment) in text.split_inclusive('\n').enumerate() {
        if line_number == position.line as usize {
            let without_newline = segment.strip_suffix('\n').unwrap_or(segment);
            let line = without_newline
                .strip_suffix('\r')
                .unwrap_or(without_newline);
            return offset_in_line(line, position.character, encoding)
                .map(|line_offset| offset + line_offset);
        }
        offset += segment.len();
    }
    ((text.is_empty() && position.line == 0 && position.character == 0)
        || (text.ends_with('\n')
            && position.line == text.split_terminator('\n').count() as u32
            && position.character == 0))
        .then_some(text.len())
}

fn offset_in_line(line: &str, character: u32, encoding: PositionEncoding) -> Option<usize> {
    let wanted = character as usize;
    match encoding {
        PositionEncoding::Utf8 => line
            .is_char_boundary(wanted)
            .then_some(wanted)
            .filter(|offset| *offset <= line.len()),
        PositionEncoding::Utf16 => {
            let mut units = 0;
            for (offset, scalar) in line.char_indices() {
                if units == wanted {
                    return Some(offset);
                }
                units += scalar.len_utf16();
                if units > wanted {
                    return None;
                }
            }
            (units == wanted).then_some(line.len())
        }
    }
}

fn validate_annotation(
    operation: &Map<String, Value>,
    annotations: &Map<String, Value>,
    index: u64,
) -> Result<(), WorkspaceEditProblem> {
    let Some(annotation_id) = operation.get("annotationId").and_then(Value::as_str) else {
        return Ok(());
    };
    if annotations.contains_key(annotation_id) {
        Ok(())
    } else {
        Err(WorkspaceEditProblem {
            code: "missing_annotation".to_owned(),
            message: "A resource operation references a missing change annotation.".to_owned(),
            operation_index: Some(index),
            path: None,
            annotation_id: Some(annotation_id.to_owned()),
            data: None,
        })
    }
}

fn option_bool(options: Option<&Map<String, Value>>, name: &str) -> bool {
    options
        .and_then(|options| options.get(name))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn validate_resource_operation(
    operation: &Map<String, Value>,
    allowed_keys: &[&str],
    allowed_option_keys: &[&str],
    index: u64,
) -> Result<(), WorkspaceEditProblem> {
    validate_object_keys(operation, allowed_keys, Some(index))?;
    let Some(options) = operation.get("options") else {
        return Ok(());
    };
    if options.is_null() {
        return Ok(());
    }
    let options = options.as_object().ok_or_else(|| {
        problem(
            "unknown_operation",
            "A resource operation options value must be an object.",
            Some(index),
            None,
            Some(options.clone()),
        )
    })?;
    validate_object_keys(options, allowed_option_keys, Some(index))?;
    if let Some((name, value)) = options.iter().find(|(_, value)| !value.is_boolean()) {
        return Err(problem(
            "unknown_operation",
            "A resource operation option must be boolean.",
            Some(index),
            None,
            Some(json!({"option": name, "value": value})),
        ));
    }
    Ok(())
}

fn validate_object_keys(
    object: &Map<String, Value>,
    allowed_keys: &[&str],
    index: Option<u64>,
) -> Result<(), WorkspaceEditProblem> {
    if let Some(key) = object
        .keys()
        .find(|key| !allowed_keys.contains(&key.as_str()))
    {
        return Err(problem(
            "unknown_operation",
            "The Workspace Edit contains an unknown field.",
            index,
            None,
            Some(json!({"field": key})),
        ));
    }
    Ok(())
}

fn lexically_beneath(root: &Path, path: &Path) -> bool {
    if !path.is_absolute() || !path.starts_with(root) {
        return false;
    }
    !path
        .strip_prefix(root)
        .unwrap()
        .components()
        .any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
}

fn uri_contains_path_traversal(raw: &str) -> bool {
    let path = raw
        .split_once("://")
        .map(|(_, path)| path)
        .unwrap_or(raw)
        .split(['?', '#'])
        .next()
        .unwrap_or_default();
    path.split('/').any(|segment| {
        let mut decoded_dots = String::new();
        let bytes = segment.as_bytes();
        let mut index = 0;
        while index < bytes.len() {
            if index + 2 < bytes.len()
                && bytes[index] == b'%'
                && bytes[index + 1] == b'2'
                && matches!(bytes[index + 2], b'e' | b'E')
            {
                decoded_dots.push('.');
                index += 3;
            } else {
                decoded_dots.push(bytes[index] as char);
                index += 1;
            }
        }
        matches!(decoded_dots.as_str(), "." | "..")
    })
}

fn directory_membership_digest(children: &BTreeMap<String, ResourceKind>) -> String {
    digest_canonical_value("lspctl-directory-membership-v1", &json!(children))
}

fn missing_resource(path: &Path) -> ResourceState {
    ResourceState {
        manifest: missing_manifest(path),
        text: None,
    }
}

fn missing_manifest(path: &Path) -> ManifestEntry {
    ManifestEntry {
        path: path.to_path_buf(),
        exists: false,
        resource_kind: ResourceKind::Missing,
        identity_digest: None,
        content_digest: None,
        metadata_digest: None,
    }
}

fn problem(
    code: &str,
    message: &str,
    operation_index: Option<u64>,
    path: Option<&Path>,
    data: Option<Value>,
) -> WorkspaceEditProblem {
    WorkspaceEditProblem {
        code: code.to_owned(),
        message: message.to_owned(),
        operation_index,
        path: path.map(Path::to_path_buf),
        annotation_id: None,
        data,
    }
}

fn hash_file(path: &Path, index: u64) -> Result<String, WorkspaceEditProblem> {
    let mut file = open_file_for_inspection(path).map_err(|error| {
        problem(
            "filesystem_capability_unavailable",
            "A regular file cannot be opened.",
            Some(index),
            Some(path),
            Some(json!({"reason": error.to_string()})),
        )
    })?;
    let accessed = file
        .metadata()
        .and_then(|metadata| metadata.accessed())
        .map_err(|error| {
            problem(
                "metadata_unsupported",
                "A regular file access time cannot be inspected.",
                Some(index),
                Some(path),
                Some(json!({"reason": error.to_string()})),
            )
        })?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    let read_result = loop {
        let read = file.read(&mut buffer).map_err(|error| {
            problem(
                "filesystem_capability_unavailable",
                "A regular file cannot be read.",
                Some(index),
                Some(path),
                Some(json!({"reason": error.to_string()})),
            )
        })?;
        if read == 0 {
            break Ok(());
        }
        hasher.update(&buffer[..read]);
    };
    let restore_result = file.set_times(std::fs::FileTimes::new().set_accessed(accessed));
    read_result?;
    restore_result.map_err(|error| {
        problem(
            "metadata_unsupported",
            "A regular file access time cannot be restored after inspection.",
            Some(index),
            Some(path),
            Some(json!({"reason": error.to_string()})),
        )
    })?;
    Ok(format!("sha256:{}", hex::encode(hasher.finalize())))
}

fn read_text_file(path: &Path, limit: u64, index: u64) -> Result<Vec<u8>, WorkspaceEditProblem> {
    let mut file = open_file_for_inspection(path).map_err(|error| {
        problem(
            "filesystem_capability_unavailable",
            "A text Document cannot be opened.",
            Some(index),
            Some(path),
            Some(json!({"reason": error.to_string()})),
        )
    })?;
    let metadata = file.metadata().map_err(|error| {
        problem(
            "filesystem_capability_unavailable",
            "A text Document cannot be inspected.",
            Some(index),
            Some(path),
            Some(json!({"reason": error.to_string()})),
        )
    })?;
    if metadata.len() > limit {
        return Err(problem(
            "resource_limit_exceeded",
            "A text Document exceeds the configured Preview limit.",
            Some(index),
            Some(path),
            Some(
                json!({"resource": "documentTextBytes", "limit": limit, "observed": metadata.len()}),
            ),
        ));
    }
    let accessed = metadata.accessed().map_err(|error| {
        problem(
            "metadata_unsupported",
            "A text Document access time cannot be inspected.",
            Some(index),
            Some(path),
            Some(json!({"reason": error.to_string()})),
        )
    })?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    let read_result = file.read_to_end(&mut bytes);
    let restore_result = file.set_times(std::fs::FileTimes::new().set_accessed(accessed));
    read_result.map_err(|error| {
        problem(
            "filesystem_capability_unavailable",
            "A text Document cannot be read.",
            Some(index),
            Some(path),
            Some(json!({"reason": error.to_string()})),
        )
    })?;
    restore_result.map_err(|error| {
        problem(
            "metadata_unsupported",
            "A text Document access time cannot be restored after inspection.",
            Some(index),
            Some(path),
            Some(json!({"reason": error.to_string()})),
        )
    })?;
    Ok(bytes)
}

fn identity_digest(
    path: &Path,
    metadata: &Metadata,
    index: u64,
) -> Result<String, WorkspaceEditProblem> {
    let mut identity = json!({});
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        identity["device"] = json!(metadata.dev());
        identity["inode"] = json!(metadata.ino());
        let _ = (path, index);
    }
    #[cfg(windows)]
    {
        let _ = metadata;
        let (volume_serial, file_index) = windows_file_identity(path).map_err(|error| {
            problem(
                "filesystem_capability_unavailable",
                "A Mutation resource identity cannot be inspected.",
                Some(index),
                Some(path),
                Some(json!({"reason": error.to_string()})),
            )
        })?;
        identity["volumeSerial"] = json!(volume_serial);
        identity["fileIndex"] = json!(file_index);
    }
    Ok(digest_canonical_value(
        "lspctl-resource-identity-v1",
        &identity,
    ))
}

#[cfg(windows)]
fn windows_file_identity(path: &Path) -> std::io::Result<(u32, u64)> {
    let information = windows_file_information(path)?;
    let file_index =
        (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow);
    Ok((information.dwVolumeSerialNumber, file_index))
}

#[cfg(windows)]
fn windows_file_information(
    path: &Path,
) -> std::io::Result<windows_sys::Win32::Storage::FileSystem::BY_HANDLE_FILE_INFORMATION> {
    use std::{os::windows::ffi::OsStrExt, ptr};
    use windows_sys::Win32::{
        Foundation::{CloseHandle, INVALID_HANDLE_VALUE},
        Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_FLAG_BACKUP_SEMANTICS,
            FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
            GetFileInformationByHandle, OPEN_EXISTING,
        },
    };

    let wide = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }
    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    let result = unsafe { GetFileInformationByHandle(handle, &mut information) };
    let close_result = unsafe { CloseHandle(handle) };
    if result == 0 {
        return Err(std::io::Error::last_os_error());
    }
    if close_result == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(information)
}

fn metadata_digest(
    path: &Path,
    metadata: &Metadata,
    index: u64,
) -> Result<String, WorkspaceEditProblem> {
    let mut value = json!({});
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        value["mode"] = json!(metadata.mode());
        value["uid"] = json!(metadata.uid());
        value["gid"] = json!(metadata.gid());
        let mut attributes = Map::new();
        for name in xattr::list(path).map_err(|error| {
            problem(
                "metadata_unsupported",
                "Extended attributes cannot be enumerated.",
                Some(index),
                Some(path),
                Some(json!({"reason": error.to_string()})),
            )
        })? {
            let name_text = name.to_string_lossy().into_owned();
            let bytes = xattr::get(path, &name)
                .map_err(|error| {
                    problem(
                        "metadata_unsupported",
                        "An extended attribute cannot be read.",
                        Some(index),
                        Some(path),
                        Some(json!({"reason": error.to_string()})),
                    )
                })?
                .unwrap_or_default();
            attributes.insert(name_text, Value::String(hex::encode(bytes)));
        }
        value["extendedAttributes"] = Value::Object(attributes);
    }
    #[cfg(target_os = "macos")]
    {
        use std::os::macos::fs::MetadataExt;
        value["bsdFlags"] = json!(metadata.st_flags());
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        value["fileAttributes"] = json!(metadata.file_attributes());
        value["creationTime"] = json!(metadata.creation_time());
        value["securityDescriptor"] = Value::String(hex::encode(
            windows_security_descriptor(path).map_err(|error| {
                problem(
                    "metadata_unsupported",
                    "The Windows security descriptor cannot be inspected.",
                    Some(index),
                    Some(path),
                    Some(json!({"reason": error.to_string()})),
                )
            })?,
        ));
        if metadata.is_file() {
            value["alternateStreams"] =
                Value::Object(windows_alternate_streams(path).map_err(|error| {
                    problem(
                        "metadata_unsupported",
                        "Windows alternate data streams cannot be inspected.",
                        Some(index),
                        Some(path),
                        Some(json!({"reason": error.to_string()})),
                    )
                })?);
        }
    }
    Ok(digest_canonical_value(
        "lspctl-resource-metadata-v1",
        &value,
    ))
}

#[cfg(windows)]
pub(super) fn windows_security_descriptor(path: &Path) -> std::io::Result<Vec<u8>> {
    use std::{os::windows::ffi::OsStrExt, ptr, slice};
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::{
            Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT},
            DACL_SECURITY_INFORMATION, GROUP_SECURITY_INFORMATION, GetSecurityDescriptorLength,
            OWNER_SECURITY_INFORMATION,
        },
    };

    let mut wide = path
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let mut descriptor = ptr::null_mut();
    let status = unsafe {
        GetNamedSecurityInfoW(
            wide.as_mut_ptr(),
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
    if length == 0 {
        unsafe {
            LocalFree(descriptor);
        }
        return Err(std::io::Error::last_os_error());
    }
    let bytes = unsafe { slice::from_raw_parts(descriptor.cast::<u8>(), length) }.to_vec();
    unsafe {
        LocalFree(descriptor);
    }
    Ok(bytes)
}

#[cfg(windows)]
fn windows_alternate_streams(path: &Path) -> std::io::Result<Map<String, Value>> {
    use std::{
        ffi::OsString,
        os::windows::ffi::{OsStrExt, OsStringExt},
    };
    use windows_sys::Win32::{
        Foundation::{ERROR_HANDLE_EOF, INVALID_HANDLE_VALUE},
        Storage::FileSystem::{
            FindClose, FindFirstStreamW, FindNextStreamW, FindStreamInfoStandard,
            WIN32_FIND_STREAM_DATA,
        },
    };

    let base_wide = path.as_os_str().encode_wide().collect::<Vec<_>>();
    let mut terminated = base_wide.clone();
    terminated.push(0);
    let mut data = WIN32_FIND_STREAM_DATA::default();
    let handle = unsafe {
        FindFirstStreamW(
            terminated.as_ptr(),
            FindStreamInfoStandard,
            std::ptr::addr_of_mut!(data).cast(),
            0,
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }
    let mut streams = Vec::new();
    loop {
        let end = data
            .cStreamName
            .iter()
            .position(|character| *character == 0)
            .unwrap_or(data.cStreamName.len());
        let name_wide = &data.cStreamName[..end];
        let name = OsString::from_wide(name_wide)
            .to_string_lossy()
            .into_owned();
        if name != "::$DATA" {
            streams.push((name, name_wide.to_vec(), data.StreamSize));
        }
        data = WIN32_FIND_STREAM_DATA::default();
        if unsafe { FindNextStreamW(handle, std::ptr::addr_of_mut!(data).cast()) } == 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(ERROR_HANDLE_EOF as i32) {
                unsafe {
                    FindClose(handle);
                }
                return Err(error);
            }
            break;
        }
    }
    if unsafe { FindClose(handle) } == 0 {
        return Err(std::io::Error::last_os_error());
    }

    let accessed = fs::metadata(path)?.accessed()?;
    let mut result = Map::new();
    for (name, name_wide, declared_size) in streams {
        let mut stream_path = base_wide.clone();
        stream_path.extend(name_wide);
        let stream_path = PathBuf::from(OsString::from_wide(&stream_path));
        let bytes = fs::read(&stream_path)?;
        result.insert(
            name,
            json!({
                "size": declared_size.max(0) as u64,
                "digest": digest_raw_bytes(&bytes)
            }),
        );
    }
    open_file_for_inspection(path)?.set_times(std::fs::FileTimes::new().set_accessed(accessed))?;
    Ok(result)
}

#[cfg(windows)]
fn open_file_for_inspection(path: &Path) -> std::io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::{
        Foundation::GENERIC_READ, Storage::FileSystem::FILE_WRITE_ATTRIBUTES,
    };

    fs::OpenOptions::new()
        .access_mode(GENERIC_READ | FILE_WRITE_ATTRIBUTES)
        .open(path)
}

#[cfg(not(windows))]
fn open_file_for_inspection(path: &Path) -> std::io::Result<File> {
    File::open(path)
}

fn resource_rollback_bytes(_path: &Path, manifest: &ManifestEntry) -> u64 {
    if manifest.resource_kind == ResourceKind::File {
        fs::metadata(&manifest.path)
            .map(|metadata| metadata.len())
            .unwrap_or(u64::MAX)
    } else {
        0
    }
}

#[cfg(unix)]
fn metadata_device(metadata: &Metadata) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    Some(metadata.dev())
}
#[cfg(not(unix))]
fn metadata_device(_metadata: &Metadata) -> Option<u64> {
    None
}

#[cfg(unix)]
fn nearest_existing_device(path: &Path) -> Option<u64> {
    let mut candidate = Some(path);
    while let Some(path) = candidate {
        if let Ok(metadata) = fs::metadata(path) {
            return metadata_device(&metadata);
        }
        candidate = path.parent();
    }
    None
}
#[cfg(not(unix))]
fn nearest_existing_device(_path: &Path) -> Option<u64> {
    None
}

#[cfg(unix)]
fn metadata_link_count(
    _path: &Path,
    metadata: &Metadata,
    _index: u64,
) -> Result<u64, WorkspaceEditProblem> {
    use std::os::unix::fs::MetadataExt;
    Ok(metadata.nlink())
}
#[cfg(windows)]
fn metadata_link_count(
    path: &Path,
    _metadata: &Metadata,
    index: u64,
) -> Result<u64, WorkspaceEditProblem> {
    windows_file_information(path)
        .map(|information| u64::from(information.nNumberOfLinks))
        .map_err(|error| {
            problem(
                "filesystem_capability_unavailable",
                "A Mutation resource link count cannot be inspected.",
                Some(index),
                Some(path),
                Some(json!({"reason": error.to_string()})),
            )
        })
}
#[cfg(not(any(unix, windows)))]
fn metadata_link_count(
    _path: &Path,
    _metadata: &Metadata,
    _index: u64,
) -> Result<u64, WorkspaceEditProblem> {
    Ok(1)
}

#[cfg(unix)]
fn metadata_is_sparse(metadata: &Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    metadata.len() > 0 && metadata.blocks().saturating_mul(512) < metadata.len()
}
#[cfg(windows)]
fn metadata_is_sparse(metadata: &Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    metadata.file_attributes() & 0x200 != 0
}
#[cfg(not(any(unix, windows)))]
fn metadata_is_sparse(_metadata: &Metadata) -> bool {
    false
}

#[cfg(windows)]
fn metadata_is_reparse_point(metadata: &Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    metadata.file_attributes() & 0x400 != 0
}
#[cfg(not(windows))]
fn metadata_is_reparse_point(_metadata: &Metadata) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn settings() -> (PreviewSettings, MutationSettings) {
        (
            PreviewSettings {
                max_count: 64,
                max_total_bytes: 1_000_000,
                max_document_text_bytes: 1_000_000,
                max_text_bytes: 1_000_000,
            },
            MutationSettings {
                application_lock_timeout: "1s".to_owned(),
                max_entries: 100,
                max_recursion_depth: 10,
                max_rollback_bytes: 1_000_000,
                max_staged_text_bytes: 1_000_000,
                max_preauthorized_callbacks: 64,
            },
        )
    }

    #[test]
    fn directory_membership_covers_overwritten_destination() {
        let workspace = TempDir::new().unwrap();
        let source = workspace.path().join("source");
        let destination = workspace.path().join("destination");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("source.txt"), b"source").unwrap();
        fs::create_dir_all(destination.join("nested")).unwrap();
        let extra = destination.join("nested/destination-only.txt");
        fs::write(&extra, b"destination-only").unwrap();
        let edit = json!({"documentChanges": [{
            "kind": "rename", "oldUri": Url::from_file_path(&source).unwrap(),
            "newUri": Url::from_file_path(&destination).unwrap(),
            "options": {"overwrite": true}
        }]});
        let (previews, mut mutation) = settings();
        let planned = WorkspaceEditPlanner::open(
            workspace.path(),
            PositionEncoding::Utf8,
            &previews,
            &mutation,
        )
        .unwrap()
        .plan_workspace_edit(&edit)
        .unwrap();
        for path in [destination.join("nested"), extra] {
            assert!(
                planned
                    .plan
                    .before_manifest
                    .iter()
                    .any(|entry| entry.path == path && entry.exists),
                "{}",
                path.display()
            );
            assert!(
                planned
                    .plan
                    .intended_manifest
                    .iter()
                    .any(|entry| entry.path == path && !entry.exists),
                "{}",
                path.display()
            );
        }
        mutation.max_entries = 4;
        let problems = WorkspaceEditPlanner::open(
            workspace.path(),
            PositionEncoding::Utf8,
            &previews,
            &mutation,
        )
        .unwrap()
        .plan_workspace_edit(&edit)
        .unwrap_err();
        assert!(problems.iter().any(|problem| {
            problem
                .data
                .as_ref()
                .is_some_and(|data| data["resource"] == "entries")
        }));
        mutation.max_entries = 100;
        mutation.max_rollback_bytes = b"source".len() as u64;
        let problems = WorkspaceEditPlanner::open(
            workspace.path(),
            PositionEncoding::Utf8,
            &previews,
            &mutation,
        )
        .unwrap()
        .plan_workspace_edit(&edit)
        .unwrap_err();
        assert!(problems.iter().any(|problem| {
            problem
                .data
                .as_ref()
                .is_some_and(|data| data["resource"] == "rollbackBytes")
        }));
    }

    #[test]
    fn directory_membership_limits_and_no_follow() {
        let workspace = TempDir::new().unwrap();
        let source = workspace.path().join("source");
        let destination = workspace.path().join("destination");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("source.txt"), b"source").unwrap();
        fs::create_dir_all(destination.join("one/two")).unwrap();
        let extra = destination.join("one/two/extra.txt");
        fs::write(&extra, b"untouched").unwrap();
        let edit = json!({"documentChanges": [{"kind": "rename",
            "oldUri": Url::from_file_path(&source).unwrap(), "newUri": Url::from_file_path(&destination).unwrap(),
            "options": {"overwrite": true}}]});
        let (previews, mut mutation) = settings();
        for (entries, depth, resource) in [(4, 10, "entries"), (100, 1, "recursionDepth")] {
            mutation.max_entries = entries;
            mutation.max_recursion_depth = depth;
            let problems = WorkspaceEditPlanner::open(
                workspace.path(),
                PositionEncoding::Utf8,
                &previews,
                &mutation,
            )
            .unwrap()
            .plan_workspace_edit(&edit)
            .unwrap_err();
            assert!(
                problems.iter().any(|problem| problem
                    .data
                    .as_ref()
                    .is_some_and(|data| data["resource"] == resource)),
                "{problems:?}"
            );
            assert_eq!(fs::read(&extra).unwrap(), b"untouched");
        }
        let mut ignored = edit.clone();
        ignored["documentChanges"][0]["options"] = json!({"ignoreIfExists": true});
        assert!(
            WorkspaceEditPlanner::open(
                workspace.path(),
                PositionEncoding::Utf8,
                &previews,
                &mutation
            )
            .unwrap()
            .plan_workspace_edit(&ignored)
            .unwrap()
            .plan
            .operations
            .is_empty()
        );
        mutation.max_recursion_depth = 10;
        mutation.max_entries = 6;
        let problems = WorkspaceEditPlanner::open(
            workspace.path(),
            PositionEncoding::Utf8,
            &previews,
            &mutation,
        )
        .unwrap()
        .plan_workspace_edit(&edit)
        .unwrap_err();
        // The six before entries fit; the destination-only tombstones and moved target also count.
        assert!(problems.iter().any(|problem| {
            problem
                .data
                .as_ref()
                .is_some_and(|data| data["resource"] == "entries")
        }));
        mutation.max_entries = 100;
        let planned = WorkspaceEditPlanner::open(
            workspace.path(),
            PositionEncoding::Utf8,
            &previews,
            &mutation,
        )
        .unwrap()
        .plan_workspace_edit(&edit)
        .unwrap();
        assert!(!planned.plan.before_manifest.is_empty());

        #[cfg(any(unix, windows))]
        {
            let outside = TempDir::new().unwrap();
            fs::write(outside.path().join("sentinel"), b"outside").unwrap();
            let link = destination.join("outside");
            #[cfg(unix)]
            std::os::unix::fs::symlink(outside.path(), &link).unwrap();
            #[cfg(windows)]
            {
                // Junctions exercise reparse rejection without requiring symlink privileges.
                let result = std::process::Command::new("cmd")
                    .args(["/C", "mklink", "/J"])
                    .arg(&link)
                    .arg(outside.path())
                    .output()
                    .unwrap();
                assert!(
                    result.status.success(),
                    "{}",
                    String::from_utf8_lossy(&result.stderr)
                );
            }
            let planner = WorkspaceEditPlanner::open(
                workspace.path(),
                PositionEncoding::Utf8,
                &previews,
                &mutation,
            )
            .unwrap();
            let problems = planner.plan_workspace_edit(&edit).unwrap_err();
            assert!(
                problems
                    .iter()
                    .any(|problem| problem.code == "symlink_or_reparse_point")
            );
            assert!(
                planner
                    .inspect_manifest(&planned.plan.before_manifest)
                    .is_err()
            );
            // Editing one file must not enumerate its siblings or the whole Workspace.
            assert!(planner.plan_workspace_edit(&json!({"changes": {Url::from_file_path(&extra).unwrap().to_string(): [{
                "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}}, "newText": "x"
            }]}})).is_ok());
            assert_eq!(
                fs::read(outside.path().join("sentinel")).unwrap(),
                b"outside"
            );
            #[cfg(windows)]
            fs::remove_dir(&link).unwrap();
        }
    }

    #[test]
    fn plans_utf16_text_edits_and_preserves_same_position_order() {
        let workspace = TempDir::new().unwrap();
        let file = workspace.path().join("main.rs");
        fs::write(&file, "a😀b\r\n").unwrap();
        let uri = Url::from_file_path(&file).unwrap().to_string();
        let (previews, mutation) = settings();
        let planner = WorkspaceEditPlanner::open(
            workspace.path(),
            PositionEncoding::Utf16,
            &previews,
            &mutation,
        )
        .unwrap();
        let planned = planner.plan_workspace_edit(&json!({"changes": {uri: [
            {"range": {"start": {"line": 0, "character": 1}, "end": {"line": 0, "character": 1}}, "newText": "x"},
            {"range": {"start": {"line": 0, "character": 1}, "end": {"line": 0, "character": 1}}, "newText": "y"}
        ]}})).unwrap();
        let CanonicalOperation::Text { after_digest, .. } = &planned.plan.operations[0] else {
            panic!()
        };
        assert_eq!(after_digest, &digest_raw_bytes("axy😀b\r\n".as_bytes()));
    }

    #[test]
    fn rejects_overlap_and_workspace_escape() {
        let workspace = TempDir::new().unwrap();
        let file = workspace.path().join("main.rs");
        fs::write(&file, "abc").unwrap();
        let uri = Url::from_file_path(&file).unwrap().to_string();
        let (previews, mutation) = settings();
        let planner = WorkspaceEditPlanner::open(
            workspace.path(),
            PositionEncoding::Utf8,
            &previews,
            &mutation,
        )
        .unwrap();
        let problems = planner.plan_workspace_edit(&json!({"changes": {uri: [
            {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}}, "newText": "x"},
            {"range": {"start": {"line": 0, "character": 1}, "end": {"line": 0, "character": 3}}, "newText": "y"}
        ]}})).unwrap_err();
        assert!(
            problems
                .iter()
                .any(|problem| problem.code == "overlapping_edits")
        );
    }

    #[test]
    fn positions_require_a_real_or_trailing_empty_line() {
        assert_eq!(
            position_to_offset(
                "abc\nxyz",
                ProtocolPosition {
                    line: 1,
                    character: 3
                },
                PositionEncoding::Utf8
            ),
            Some(7)
        );
        assert_eq!(
            position_to_offset(
                "abc\nxyz",
                ProtocolPosition {
                    line: 2,
                    character: 0
                },
                PositionEncoding::Utf8
            ),
            None
        );
        assert_eq!(
            position_to_offset(
                "abc\n",
                ProtocolPosition {
                    line: 1,
                    character: 0
                },
                PositionEncoding::Utf8
            ),
            Some(4)
        );
    }

    #[test]
    fn rejects_unbound_versions_unknown_fields_and_dot_segments() {
        let workspace = TempDir::new().unwrap();
        let file = workspace.path().join("main.rs");
        fs::write(&file, "abc").unwrap();
        let uri = Url::from_file_path(&file).unwrap().to_string();
        let (previews, mutation) = settings();
        let planner = WorkspaceEditPlanner::open(
            workspace.path(),
            PositionEncoding::Utf8,
            &previews,
            &mutation,
        )
        .unwrap();

        let versioned = planner
            .plan_workspace_edit(&json!({"documentChanges": [{
                "textDocument": {"uri": uri.clone(), "version": 1},
                "edits": []
            }]}))
            .unwrap_err();
        assert_eq!(versioned[0].code, "invalid_document_version");
        assert!(
            planner
                .plan_workspace_edit_with_document_preconditions(
                    &json!({"documentChanges": [{
                        "textDocument": {"uri": uri.clone(), "version": 1},
                        "edits": []
                    }]}),
                    &BTreeMap::from([(
                        uri.clone(),
                        DocumentVersionPrecondition {
                            version: 1,
                            digest: digest_raw_bytes(b"abc"),
                        },
                    )]),
                )
                .is_ok()
        );
        #[cfg(windows)]
        {
            let drive = uri.as_bytes()[8];
            let alias_drive = if drive.is_ascii_lowercase() {
                drive.to_ascii_uppercase()
            } else {
                drive.to_ascii_lowercase()
            };
            let drive_alias_uri = format!("{}{}{}", &uri[..8], char::from(alias_drive), &uri[9..]);
            assert!(
                planner
                    .plan_workspace_edit_with_document_preconditions(
                        &json!({"documentChanges": [{
                            "textDocument": {"uri": drive_alias_uri, "version": 1},
                            "edits": []
                        }]}),
                        &BTreeMap::from([(
                            uri.clone(),
                            DocumentVersionPrecondition {
                                version: 1,
                                digest: digest_raw_bytes(b"abc"),
                            },
                        )]),
                    )
                    .is_ok()
            );
        }
        let mismatched = planner
            .plan_workspace_edit_with_document_preconditions(
                &json!({"documentChanges": [{
                    "textDocument": {"uri": uri.clone(), "version": 2},
                    "edits": []
                }]}),
                &BTreeMap::from([(
                    uri.clone(),
                    DocumentVersionPrecondition {
                        version: 1,
                        digest: digest_raw_bytes(b"abc"),
                    },
                )]),
            )
            .unwrap_err();
        assert_eq!(mismatched[0].code, "invalid_document_version");
        let stale = planner
            .plan_workspace_edit_with_document_preconditions(
                &json!({"documentChanges": [{
                    "textDocument": {"uri": uri.clone(), "version": 1},
                    "edits": []
                }]}),
                &BTreeMap::from([(
                    uri.clone(),
                    DocumentVersionPrecondition {
                        version: 1,
                        digest: digest_raw_bytes(b"different"),
                    },
                )]),
            )
            .unwrap_err();
        assert_eq!(stale[0].code, "invalid_document_version");

        let unknown = planner
            .plan_workspace_edit(&json!({"changes": {}, "unexpected": true}))
            .unwrap_err();
        assert_eq!(unknown[0].code, "unknown_operation");

        let traversal_uri = format!(
            "file://{}/child/../main.rs",
            workspace.path().to_string_lossy()
        );
        let traversal = planner
            .plan_workspace_edit(&json!({"changes": {traversal_uri: []}}))
            .unwrap_err();
        assert_eq!(traversal[0].code, "path_traversal");
    }

    #[test]
    fn windows_file_uri_alias_only_ignores_drive_case() {
        assert!(windows_file_uri_drive_alias(
            "file:///C:/workspace/main.rs",
            "file:///c:/workspace/main.rs"
        ));
        assert!(!windows_file_uri_drive_alias(
            "file:///C:/Workspace/main.rs",
            "file:///c:/workspace/main.rs"
        ));
    }

    #[test]
    fn omits_exact_no_op_text_edits_from_the_plan() {
        let workspace = TempDir::new().unwrap();
        let file = workspace.path().join("main.rs");
        fs::write(&file, "abc").unwrap();
        let uri = Url::from_file_path(&file).unwrap().to_string();
        let (previews, mutation) = settings();
        let planner = WorkspaceEditPlanner::open(
            workspace.path(),
            PositionEncoding::Utf8,
            &previews,
            &mutation,
        )
        .unwrap();
        let planned = planner
            .plan_workspace_edit(&json!({"changes": {uri: [{
                "range": {
                    "start": {"line": 0, "character": 0},
                    "end": {"line": 0, "character": 3}
                },
                "newText": "abc"
            }]}}))
            .unwrap();
        assert!(planned.plan.operations.is_empty());
        assert_eq!(planned.summary.files_changed, 0);
    }
}
