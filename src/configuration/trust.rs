use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

use atomic_write_file::AtomicWriteFile;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use super::{
    AuthorizedServer, ConfigSource, EffectiveServer, LoadedConfiguration, canonical_workspace,
    effective_child_environment, load_configuration, resolve_server_cwd, resolve_server_executable,
    resolved_user_state_directory, session_identity,
};
use crate::{
    canonical_value::digest_canonical_value, cli::ParsedInvocation, contract::ContractFailure,
};

const TRUST_STATE_FORMAT_VERSION: u32 = 1;

#[derive(Debug)]
struct CurrentDeclaration {
    server: String,
    digest: String,
    field_digests: BTreeMap<String, String>,
    executable_path: String,
    source_path: String,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TrustStateFile {
    format_version: u32,
    records: Vec<StoredTrustRecord>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredTrustRecord {
    workspace_uri: String,
    server: String,
    state: StoredTrustState,
    declaration_digest: Option<String>,
    #[serde(default)]
    field_digests: BTreeMap<String, String>,
    executable_path: Option<String>,
    source_path: Option<String>,
    updated_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum StoredTrustState {
    Trusted,
    Denied,
}

/// Handles one Trust command without starting a language server.
pub(crate) fn dispatch_trust_command(
    invocation: &ParsedInvocation,
) -> Result<Value, ContractFailure> {
    match invocation.command_path().get(1).map(String::as_str) {
        Some("grant") => grant_trust(invocation),
        Some("deny") => deny_trust(invocation),
        Some("revoke") => revoke_trust(invocation),
        Some("status") => trust_status(invocation),
        Some("list") => trust_list(invocation),
        _ => unreachable!("the CLI catalog limits Trust command paths"),
    }
}

/// Recomputes launch identity and enforces declaration-bound project Trust.
pub(crate) fn authorize_server(
    configuration: &LoadedConfiguration,
    server: EffectiveServer,
) -> Result<AuthorizedServer, ContractFailure> {
    let executable = resolve_server_executable(&server)?;
    let cwd = resolve_server_cwd(&server)?;
    let child_environment = effective_child_environment(&server, &cwd);
    let session_identity = session_identity(
        configuration,
        &server,
        &executable,
        &cwd,
        &child_environment,
    );
    let declaration = if server.project_fields.is_empty() {
        None
    } else {
        Some(current_declaration(configuration, &server.name, &server)?)
    };
    if let Some(declaration) = &declaration {
        let state = read_trust_state()?;
        let record = state.records.iter().find(|record| {
            record.workspace_uri == configuration.workspace_uri && record.server == server.name
        });
        match record {
            Some(record) if record.state == StoredTrustState::Denied => {
                return Err(project_trust_failure(
                    "project_trust_denied",
                    "Project-controlled language-server configuration has been denied.",
                    json!({
                        "workspaceUri": configuration.workspace_uri,
                        "server": server.name,
                        "digest": declaration.digest,
                        "requiredCommand": ["trust", "revoke", "--workspace", configuration.workspace, "--server", server.name]
                    }),
                ));
            }
            Some(record)
                if record.state == StoredTrustState::Trusted
                    && record.declaration_digest.as_ref() == Some(&declaration.digest) => {}
            Some(record) => {
                return Err(project_trust_failure(
                    "project_trust_changed",
                    "Project-controlled language-server configuration changed after it was trusted.",
                    json!({
                        "workspaceUri": configuration.workspace_uri,
                        "server": server.name,
                        "recordedDigest": record.declaration_digest,
                        "currentDigest": declaration.digest,
                        "changedFields": changed_fields(&record.field_digests, Some(&declaration.field_digests)),
                        "requiredCommand": ["trust", "grant", "--workspace", configuration.workspace, "--server", server.name, "--digest", declaration.digest]
                    }),
                ));
            }
            None => {
                return Err(project_trust_failure(
                    "project_trust_required",
                    "Project-controlled language-server configuration requires Trust.",
                    json!({
                        "workspaceUri": configuration.workspace_uri,
                        "server": server.name,
                        "digest": declaration.digest,
                        "requiredCommand": ["trust", "grant", "--workspace", configuration.workspace, "--server", server.name, "--digest", declaration.digest]
                    }),
                ));
            }
        }
    }
    Ok(AuthorizedServer {
        server,
        executable,
        cwd,
        child_environment,
        declaration_digest: declaration.map(|declaration| declaration.digest),
        session_identity,
    })
}

fn grant_trust(invocation: &ParsedInvocation) -> Result<Value, ContractFailure> {
    let configuration = command_configuration(invocation)?;
    let declarations = if invocation.has_option("--all") {
        current_declarations(&configuration)?
    } else {
        let name = invocation.option_string("--server").unwrap();
        let server = configuration
            .servers
            .get(&name)
            .filter(|server| !server.project_fields.is_empty())
            .ok_or_else(|| {
                super::server_selection_failure(
                    "The named server has no project-controlled declaration.",
                )
            })?;
        BTreeMap::from([(
            name.clone(),
            current_declaration(&configuration, &name, server)?,
        )])
    };
    let expected = invocation.option_string("--digest").unwrap();
    let selected = if invocation.has_option("--all") {
        let actual = aggregate_digest(&declarations);
        if expected != actual {
            return Err(digest_mismatch_failure(
                &configuration,
                None,
                &expected,
                &actual,
            ));
        }
        declarations.values().collect::<Vec<_>>()
    } else {
        let server = invocation.option_string("--server").unwrap();
        let declaration = &declarations[&server];
        if expected != declaration.digest {
            return Err(digest_mismatch_failure(
                &configuration,
                Some(&server),
                &expected,
                &declaration.digest,
            ));
        }
        vec![declaration]
    };
    // Named grants need only their declaration; optional metadata must remain a full aggregate.
    let aggregate = if invocation.has_option("--all") {
        Some(aggregate_digest(&declarations))
    } else {
        current_declarations(&configuration)
            .ok()
            .map(|declarations| aggregate_digest(&declarations))
    };
    let replace_denial =
        invocation.has_option("--replace-denial") || invocation.has_option("--replace-denials");
    let affected_servers = selected
        .iter()
        .map(|declaration| declaration.server.clone())
        .collect::<Vec<_>>();
    let _application_lock = crate::mutation::acquire_workspace_application_lock(
        &configuration.workspace_uri,
        &configuration.mutation.application_lock_timeout,
    )?;

    update_trust_state(|state| {
        let denied = selected
            .iter()
            .filter(|declaration| {
                state.records.iter().any(|record| {
                    record.workspace_uri == configuration.workspace_uri
                        && record.server == declaration.server
                        && record.state == StoredTrustState::Denied
                })
            })
            .map(|declaration| declaration.server.clone())
            .collect::<Vec<_>>();
        if !denied.is_empty() && !replace_denial {
            return Err(trust_failure(
                "denial_replacement_required",
                "An explicit Denial replacement flag is required.",
                json!({
                    "servers": denied,
                    "requiredCommand": if invocation.has_option("--all") {
                        vec!["trust", "grant", "--all", "--replace-denials"]
                    } else {
                        vec!["trust", "grant", "--server", "--replace-denial"]
                    }
                }),
            ));
        }
        let now = now_rfc3339();
        for declaration in &selected {
            upsert_record(
                state,
                StoredTrustRecord {
                    workspace_uri: configuration.workspace_uri.clone(),
                    server: declaration.server.clone(),
                    state: StoredTrustState::Trusted,
                    declaration_digest: Some(declaration.digest.clone()),
                    field_digests: declaration.field_digests.clone(),
                    executable_path: Some(declaration.executable_path.clone()),
                    source_path: Some(declaration.source_path.clone()),
                    updated_at: now.clone(),
                },
            );
        }
        Ok(())
    })?;

    let state = read_trust_state()?;
    let (owners_signalled, owner_signal_failures) =
        crate::session::signal_workspace_owners(&configuration.workspace_uri, &affected_servers);
    Ok(trust_change_envelope(
        invocation,
        &configuration,
        aggregate,
        selected
            .iter()
            .filter_map(|declaration| {
                state.records.iter().find(|record| {
                    record.workspace_uri == configuration.workspace_uri
                        && record.server == declaration.server
                })
            })
            .map(|record| render_record(record, declarations.get(&record.server)))
            .collect(),
        owners_signalled,
        owner_signal_failures,
    ))
}

fn deny_trust(invocation: &ParsedInvocation) -> Result<Value, ContractFailure> {
    let configuration = command_configuration(invocation)?;
    let server = invocation.option_string("--server").unwrap();
    if !configuration.servers.contains_key(&server) {
        return Err(super::server_selection_failure(
            "The named server has no current declaration.",
        ));
    }
    let record = StoredTrustRecord {
        workspace_uri: configuration.workspace_uri.clone(),
        server: server.clone(),
        state: StoredTrustState::Denied,
        declaration_digest: None,
        field_digests: BTreeMap::new(),
        executable_path: None,
        source_path: Some(configuration.project_path.to_string_lossy().into_owned()),
        updated_at: now_rfc3339(),
    };
    let _application_lock = crate::mutation::acquire_workspace_application_lock(
        &configuration.workspace_uri,
        &configuration.mutation.application_lock_timeout,
    )?;
    update_trust_state(|state| {
        upsert_record(state, record.clone());
        Ok(())
    })?;
    let (owners_signalled, owner_signal_failures) = crate::session::signal_workspace_owners(
        &configuration.workspace_uri,
        std::slice::from_ref(&server),
    );
    Ok(trust_change_envelope(
        invocation,
        &configuration,
        None,
        vec![render_record(&record, None)],
        owners_signalled,
        owner_signal_failures,
    ))
}

fn revoke_trust(invocation: &ParsedInvocation) -> Result<Value, ContractFailure> {
    let configuration = command_configuration(invocation)?;
    let server = invocation.option_string("--server").unwrap();
    let _application_lock = crate::mutation::acquire_workspace_application_lock(
        &configuration.workspace_uri,
        &configuration.mutation.application_lock_timeout,
    )?;
    update_trust_state(|state| {
        state.records.retain(|record| {
            record.workspace_uri != configuration.workspace_uri || record.server != server
        });
        Ok(())
    })?;
    let record = render_untrusted_record(&configuration, &server, None);
    let (owners_signalled, owner_signal_failures) = crate::session::signal_workspace_owners(
        &configuration.workspace_uri,
        std::slice::from_ref(&server),
    );
    Ok(trust_change_envelope(
        invocation,
        &configuration,
        None,
        vec![record],
        owners_signalled,
        owner_signal_failures,
    ))
}

fn trust_status(invocation: &ParsedInvocation) -> Result<Value, ContractFailure> {
    let configuration = command_configuration(invocation)?;
    let declarations = current_declarations(&configuration)?;
    let state = read_trust_state()?;
    let server_filter = invocation.option_string("--server");
    let names = declarations
        .keys()
        .chain(
            state
                .records
                .iter()
                .filter(|record| record.workspace_uri == configuration.workspace_uri)
                .map(|record| &record.server),
        )
        .filter(|server| {
            server_filter
                .as_ref()
                .is_none_or(|filter| filter == *server)
        })
        .cloned()
        .collect::<BTreeSet<_>>();
    let records = names
        .iter()
        .map(|server| {
            if let Some(record) = state.records.iter().find(|record| {
                record.workspace_uri == configuration.workspace_uri && record.server == *server
            }) {
                render_record(record, declarations.get(server))
            } else {
                render_untrusted_record(&configuration, server, declarations.get(server))
            }
        })
        .collect::<Vec<_>>();
    Ok(json!({
        "schemaVersion": 1,
        "ok": true,
        "command": invocation.command_path(),
        "result": {
            "workspaceUri": configuration.workspace_uri,
            "aggregateDigest": aggregate_digest(&declarations),
            "records": records
        }
    }))
}

fn trust_list(invocation: &ParsedInvocation) -> Result<Value, ContractFailure> {
    let workspace_filter = invocation
        .option_path("--workspace")
        .map(|path| {
            canonical_workspace(&path).and_then(|path| {
                url::Url::from_directory_path(path)
                    .map(|uri| uri.to_string())
                    .map_err(|()| state_failure("The Workspace path is not a file URI."))
            })
        })
        .transpose()?;
    let state = read_trust_state()?;
    let records = state
        .records
        .iter()
        .filter(|record| {
            workspace_filter
                .as_ref()
                .is_none_or(|workspace| workspace == &record.workspace_uri)
        })
        .map(|record| render_record(record, None))
        .collect::<Vec<_>>();
    Ok(json!({
        "schemaVersion": 1,
        "ok": true,
        "command": invocation.command_path(),
        "result": records
    }))
}

fn command_configuration(
    invocation: &ParsedInvocation,
) -> Result<LoadedConfiguration, ContractFailure> {
    let workspace = invocation.option_path("--workspace").unwrap();
    load_configuration(&workspace, false)
}

fn current_declarations(
    configuration: &LoadedConfiguration,
) -> Result<BTreeMap<String, CurrentDeclaration>, ContractFailure> {
    configuration
        .servers
        .iter()
        .filter(|(_, server)| !server.project_fields.is_empty())
        .map(|(name, server)| {
            current_declaration(configuration, name, server)
                .map(|declaration| (name.clone(), declaration))
        })
        .collect()
}

fn current_declaration(
    configuration: &LoadedConfiguration,
    name: &str,
    server: &EffectiveServer,
) -> Result<CurrentDeclaration, ContractFailure> {
    let executable = resolve_server_executable(server)?;
    let executable_path = executable.to_string_lossy().into_owned();
    let fields = project_field_values(server)?;
    let field_digests = fields
        .iter()
        .map(|(name, value)| {
            (
                name.clone(),
                digest_canonical_value("lspctl-trust-field-v1", value),
            )
        })
        .collect();
    let digest = digest_canonical_value(
        "lspctl-trust-declaration-v1",
        &json!({
            "server": name,
            "resolvedExecutablePath": executable_path,
            "projectFields": fields
        }),
    );
    Ok(CurrentDeclaration {
        server: name.to_owned(),
        digest,
        field_digests,
        executable_path,
        source_path: configuration.project_path.to_string_lossy().into_owned(),
    })
}

fn project_field_values(
    server: &EffectiveServer,
) -> Result<BTreeMap<String, Value>, ContractFailure> {
    let mut fields = BTreeMap::new();
    for field in &server.project_fields {
        let value = match field.as_str() {
            "executable" => json!(resolve_server_executable(server)?),
            "args" => json!(server.args.value),
            "cwd" => json!(resolve_server_cwd(server)?),
            "environment" => Value::Object(
                server
                    .environment
                    .iter()
                    .filter(|(_, value)| value.source == ConfigSource::Project)
                    .map(|(key, value)| (key.clone(), Value::String(value.value.clone())))
                    .collect(),
            ),
            "initialization_options" => server
                .initialization_options
                .as_ref()
                .map(|value| value.value.clone())
                .unwrap_or(Value::Null),
            "settings" => server.settings.value.clone(),
            _ => unreachable!(),
        };
        fields.insert(field.clone(), value);
    }
    Ok(fields)
}

fn aggregate_digest(declarations: &BTreeMap<String, CurrentDeclaration>) -> String {
    digest_canonical_value(
        "lspctl-trust-aggregate-v1",
        &Value::Object(
            declarations
                .iter()
                .map(|(name, declaration)| {
                    (name.clone(), Value::String(declaration.digest.clone()))
                })
                .collect(),
        ),
    )
}

fn render_record(record: &StoredTrustRecord, current: Option<&CurrentDeclaration>) -> Value {
    let (state, changed_fields) = match record.state {
        StoredTrustState::Denied => ("denied", Vec::new()),
        StoredTrustState::Trusted
            if current.is_some_and(|current| {
                record.declaration_digest.as_ref() == Some(&current.digest)
            }) =>
        {
            ("trusted", Vec::new())
        }
        StoredTrustState::Trusted => (
            "changed",
            changed_fields(
                &record.field_digests,
                current.map(|value| &value.field_digests),
            ),
        ),
    };
    let mut value = json!({
        "workspaceUri": record.workspace_uri,
        "server": record.server,
        "state": state,
        "declarationDigest": record.declaration_digest,
        "currentDigest": current.map(|current| &current.digest),
        "executablePath": current.map_or(record.executable_path.as_ref(), |value| Some(&value.executable_path)),
        "sourcePath": current.map_or(record.source_path.as_ref(), |value| Some(&value.source_path)),
        "changedFields": changed_fields,
        "updatedAt": record.updated_at
    });
    if state != "trusted" {
        value["requiredCommand"] = json!(["trust", "grant"]);
    }
    remove_null_members(&mut value);
    value
}

fn render_untrusted_record(
    configuration: &LoadedConfiguration,
    server: &str,
    current: Option<&CurrentDeclaration>,
) -> Value {
    let mut value = json!({
        "workspaceUri": configuration.workspace_uri,
        "server": server,
        "state": "untrusted",
        "currentDigest": current.map(|current| &current.digest),
        "executablePath": current.map(|current| &current.executable_path),
        "sourcePath": current.map(|current| &current.source_path),
        "changedFields": [],
        "updatedAt": now_rfc3339(),
        "requiredCommand": ["trust", "grant"]
    });
    remove_null_members(&mut value);
    value
}

fn changed_fields(
    recorded: &BTreeMap<String, String>,
    current: Option<&BTreeMap<String, String>>,
) -> Vec<String> {
    recorded
        .keys()
        .chain(current.into_iter().flat_map(|fields| fields.keys()))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|field| recorded.get(*field) != current.and_then(|fields| fields.get(*field)))
        .cloned()
        .collect()
}

fn trust_change_envelope(
    invocation: &ParsedInvocation,
    configuration: &LoadedConfiguration,
    aggregate_digest: Option<String>,
    records: Vec<Value>,
    owners_signalled: Vec<String>,
    owner_signal_failures: Vec<String>,
) -> Value {
    let mut result = json!({
        "workspaceUri": configuration.workspace_uri,
        "aggregateDigest": aggregate_digest,
        "records": records,
        "ownersSignalled": owners_signalled,
        "ownerSignalFailures": owner_signal_failures
    });
    remove_null_members(&mut result);
    json!({
        "schemaVersion": 1,
        "ok": true,
        "command": invocation.command_path(),
        "result": result
    })
}

fn upsert_record(state: &mut TrustStateFile, record: StoredTrustRecord) {
    state.records.retain(|existing| {
        existing.workspace_uri != record.workspace_uri || existing.server != record.server
    });
    state.records.push(record);
    state.records.sort_by(|left, right| {
        (&left.workspace_uri, &left.server).cmp(&(&right.workspace_uri, &right.server))
    });
}

fn update_trust_state(
    update: impl FnOnce(&mut TrustStateFile) -> Result<(), ContractFailure>,
) -> Result<(), ContractFailure> {
    let paths = trust_state_paths()?;
    fs::create_dir_all(&paths.directory).map_err(|error| {
        state_failure(&format!(
            "The Trust state directory cannot be created: {error}"
        ))
    })?;
    restrict_directory(&paths.directory)?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&paths.lock)
        .map_err(|error| state_failure(&format!("The Trust lock cannot be opened: {error}")))?;
    lock_with_timeout(&lock, Duration::from_secs(30))?;
    let mut state = read_trust_state_from(&paths.file)?;
    update(&mut state)?;
    write_trust_state(&paths.file, &state)?;
    lock.unlock().ok();
    Ok(())
}

fn read_trust_state() -> Result<TrustStateFile, ContractFailure> {
    let paths = trust_state_paths()?;
    read_trust_state_from(&paths.file)
}

fn read_trust_state_from(path: &Path) -> Result<TrustStateFile, ContractFailure> {
    let mut contents = Vec::new();
    match fs::File::open(path) {
        Ok(mut file) => {
            file.read_to_end(&mut contents)
                .map_err(|error| state_failure(&format!("Trust state cannot be read: {error}")))?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(TrustStateFile {
                format_version: TRUST_STATE_FORMAT_VERSION,
                records: Vec::new(),
            });
        }
        Err(error) => {
            return Err(state_failure(&format!(
                "Trust state cannot be opened: {error}"
            )));
        }
    }
    let state: TrustStateFile = serde_json::from_slice(&contents)
        .map_err(|_| stored_state_failure(path, "Trust state is invalid."))?;
    if state.format_version != TRUST_STATE_FORMAT_VERSION {
        return Err(stored_state_failure(
            path,
            "Trust state version is unsupported.",
        ));
    }
    Ok(state)
}

fn write_trust_state(path: &Path, state: &TrustStateFile) -> Result<(), ContractFailure> {
    let bytes = serde_json::to_vec(state)
        .map_err(|error| state_failure(&format!("Trust state cannot be serialized: {error}")))?;
    let mut file = AtomicWriteFile::open(path)
        .map_err(|error| state_failure(&format!("Trust state cannot be staged: {error}")))?;
    file.write_all(&bytes)
        .and_then(|()| file.commit())
        .map_err(|error| state_failure(&format!("Trust state cannot be committed: {error}")))?;
    restrict_file(path)
}

struct TrustStatePaths {
    directory: PathBuf,
    file: PathBuf,
    lock: PathBuf,
}

fn trust_state_paths() -> Result<TrustStatePaths, ContractFailure> {
    let directory = resolved_user_state_directory()
        .ok_or_else(|| state_failure("The user state directory is unavailable."))?;
    Ok(TrustStatePaths {
        file: directory.join("trust-v1.json"),
        lock: directory.join("trust-v1.lock"),
        directory,
    })
}

fn lock_with_timeout(file: &fs::File, timeout: Duration) -> Result<(), ContractFailure> {
    let deadline = Instant::now() + timeout;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(()),
            Err(std::fs::TryLockError::WouldBlock) => {
                if Instant::now() >= deadline {
                    return Err(state_failure("The Trust state lock timed out."));
                }
                thread::sleep(Duration::from_millis(20));
            }
            Err(std::fs::TryLockError::Error(error)) => {
                return Err(state_failure(&format!(
                    "The Trust state lock failed: {error}"
                )));
            }
        }
    }
}

fn restrict_directory(path: &Path) -> Result<(), ContractFailure> {
    crate::state_permissions::restrict_directory(path)
        .map_err(|error| state_failure(&format!("State directory permissions failed: {error}")))
}

fn restrict_file(path: &Path) -> Result<(), ContractFailure> {
    crate::state_permissions::restrict_file(path)
        .map_err(|error| state_failure(&format!("State file permissions failed: {error}")))
}

fn now_rfc3339() -> String {
    OffsetDateTime::now_utc().format(&Rfc3339).unwrap()
}

fn remove_null_members(value: &mut Value) {
    if let Some(object) = value.as_object_mut() {
        object.retain(|_, value| !value.is_null());
    }
}

fn digest_mismatch_failure(
    configuration: &LoadedConfiguration,
    server: Option<&str>,
    expected: &str,
    actual: &str,
) -> ContractFailure {
    let mut data = json!({
        "workspaceUri": configuration.workspace_uri,
        "expectedDigest": expected,
        "actualDigest": actual
    });
    if let Some(server) = server {
        data["server"] = json!(server);
    }
    trust_failure(
        "trust_digest_mismatch",
        "The supplied Trust digest does not match current project configuration.",
        data,
    )
}

fn trust_failure(code: &'static str, message: &str, data: Value) -> ContractFailure {
    ContractFailure {
        exit_code: 3,
        category: "blocked",
        code,
        message: message.to_owned(),
        stage: "authorize",
        delivery: "not_applicable",
        retry: "after_change",
        data,
    }
}

fn project_trust_failure(code: &'static str, message: &str, data: Value) -> ContractFailure {
    ContractFailure {
        exit_code: 3,
        category: "blocked",
        code,
        message: message.to_owned(),
        stage: "authorize",
        delivery: "not_sent",
        retry: "after_change",
        data,
    }
}

fn state_failure(message: &str) -> ContractFailure {
    ContractFailure {
        exit_code: 4,
        category: "unavailable",
        code: "state_unavailable",
        message: message.to_owned(),
        stage: "persist",
        delivery: "not_applicable",
        retry: "after_change",
        data: json!({"recordType": "trust", "path": trust_state_paths_unchecked()}),
    }
}

fn stored_state_failure(path: &Path, message: &str) -> ContractFailure {
    ContractFailure {
        exit_code: 3,
        category: "blocked",
        code: "stored_state_version_unsupported",
        message: message.to_owned(),
        stage: "inspect",
        delivery: "not_applicable",
        retry: "never",
        data: json!({
            "recordType": "trust",
            "path": path,
            "foundVersion": 0,
            "supportedVersions": [TRUST_STATE_FORMAT_VERSION]
        }),
    }
}

fn trust_state_paths_unchecked() -> String {
    resolved_user_state_directory()
        .map(|directory| {
            directory
                .join("trust-v1.json")
                .to_string_lossy()
                .into_owned()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_major_trust_state_fixtures_remain_readable() {
        for fixture in [
            include_str!("../../tests/fixtures/stored-state/v1/trust.json"),
            include_str!("../../tests/fixtures/stored-state/v0.1.1/trust.json"),
        ] {
            let state: TrustStateFile = serde_json::from_str(fixture).unwrap();
            assert_eq!(state.format_version, TRUST_STATE_FORMAT_VERSION);
            assert_eq!(state.records.len(), 2);
        }
    }

    #[test]
    fn changed_fields_reports_added_removed_and_changed_fields() {
        let recorded = BTreeMap::from([
            ("args".to_owned(), "a".to_owned()),
            ("cwd".to_owned(), "same".to_owned()),
        ]);
        let current = BTreeMap::from([
            ("cwd".to_owned(), "same".to_owned()),
            ("settings".to_owned(), "b".to_owned()),
        ]);
        assert_eq!(
            changed_fields(&recorded, Some(&current)),
            ["args", "settings"]
        );
    }
}
