#![allow(clippy::result_large_err)]

mod trust;

use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    ffi::{OsStr, OsString},
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use url::Url;

use crate::{
    canonical_value::digest_canonical_value, cli::ParsedInvocation, contract::ContractFailure,
};

pub(crate) use trust::authorize_server;
pub(crate) use trust::dispatch_trust_command;

const DEFAULT_PUBLISHED_DIAGNOSTICS_WAIT: &str = "2s";

#[derive(Debug)]
pub(crate) struct LoadedConfiguration {
    pub(crate) workspace: PathBuf,
    pub(crate) workspace_uri: String,
    pub(crate) project_path: PathBuf,
    pub(crate) default_server: Option<String>,
    pub(crate) routes: Vec<RouteConfig>,
    pub(crate) servers: BTreeMap<String, EffectiveServer>,
    pub(crate) session: SessionSettings,
    pub(crate) synchronization: SynchronizationSettings,
    pub(crate) protocol: ProtocolSettings,
    pub(crate) previews: PreviewSettings,
    pub(crate) receipts: ReceiptSettings,
    pub(crate) mutation: MutationSettings,
}

#[derive(Debug, Clone)]
pub(crate) struct EffectiveServer {
    pub(crate) name: String,
    pub(crate) executable: Option<Sourced<String>>,
    pub(crate) args: Sourced<Vec<String>>,
    pub(crate) cwd: Option<Sourced<String>>,
    pub(crate) environment: BTreeMap<String, Sourced<String>>,
    pub(crate) initialization_options: Option<Sourced<Value>>,
    pub(crate) settings: Sourced<Value>,
    pub(crate) request_timeout: String,
    pub(crate) published_diagnostics_wait: String,
    pub(crate) project_fields: BTreeSet<String>,
}

#[derive(Debug)]
pub(crate) struct AuthorizedServer {
    pub(crate) server: EffectiveServer,
    pub(crate) executable: PathBuf,
    pub(crate) cwd: PathBuf,
    pub(crate) child_environment: Vec<(OsString, OsString)>,
    pub(crate) declaration_digest: Option<String>,
    pub(crate) session_identity: String,
}

#[derive(Debug, Clone)]
pub(crate) struct Sourced<T> {
    pub(crate) value: T,
    pub(crate) source: ConfigSource,
    pub(crate) declaring_directory: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConfigSource {
    BuiltIn,
    User,
    Project,
    Cli,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RouteConfig {
    pub(crate) server: String,
    pub(crate) language_id: String,
    pub(crate) extensions: Vec<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct SessionSettings {
    pub(crate) owner_startup_timeout: String,
    pub(crate) initialization_timeout: String,
    pub(crate) request_timeout: String,
    pub(crate) cancellation_grace: String,
    pub(crate) shutdown_timeout: String,
    pub(crate) idle_timeout: String,
}

#[derive(Debug, Clone)]
pub(crate) struct SynchronizationSettings {
    pub(crate) max_open_documents: u64,
    pub(crate) max_document_bytes: u64,
    pub(crate) max_total_text_bytes: u64,
    pub(crate) max_diagnostic_snapshots: u64,
    pub(crate) max_diagnostic_bytes: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct ProtocolSettings {
    pub(crate) max_message_bytes: u64,
    pub(crate) max_partial_result_bytes: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PreviewSettings {
    pub(crate) max_count: u64,
    pub(crate) max_total_bytes: u64,
    pub(crate) max_document_text_bytes: u64,
    pub(crate) max_text_bytes: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ReceiptSettings {
    pub(crate) max_count: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct MutationSettings {
    pub(crate) application_lock_timeout: String,
    pub(crate) max_entries: u64,
    pub(crate) max_recursion_depth: u64,
    pub(crate) max_rollback_bytes: u64,
    pub(crate) max_staged_text_bytes: u64,
    pub(crate) max_preauthorized_callbacks: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UserConfigFile {
    version: u32,
    default_server: Option<String>,
    routes: Option<Vec<RouteConfig>>,
    #[serde(default)]
    servers: BTreeMap<String, UserServerConfig>,
    session: Option<SessionConfig>,
    synchronization: Option<SynchronizationConfig>,
    protocol: Option<ProtocolConfig>,
    previews: Option<PreviewConfig>,
    receipts: Option<ReceiptConfig>,
    mutation: Option<MutationConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectConfigFile {
    version: u32,
    default_server: Option<String>,
    routes: Option<Vec<RouteConfig>>,
    #[serde(default)]
    servers: BTreeMap<String, ProjectServerConfig>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct UserServerConfig {
    executable: Option<String>,
    args: Option<Vec<String>>,
    cwd: Option<String>,
    environment: Option<BTreeMap<String, String>>,
    request_timeout: Option<String>,
    published_diagnostics_wait: Option<String>,
    initialization_options: Option<toml::Value>,
    settings: Option<toml::Table>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectServerConfig {
    executable: Option<String>,
    args: Option<Vec<String>>,
    cwd: Option<String>,
    environment: Option<BTreeMap<String, String>>,
    initialization_options: Option<toml::Value>,
    settings: Option<toml::Table>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionConfig {
    owner_startup_timeout: Option<String>,
    initialization_timeout: Option<String>,
    request_timeout: Option<String>,
    cancellation_grace: Option<String>,
    shutdown_timeout: Option<String>,
    idle_timeout: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SynchronizationConfig {
    max_open_documents: Option<u64>,
    max_document_bytes: Option<u64>,
    max_total_text_bytes: Option<u64>,
    max_diagnostic_snapshots: Option<u64>,
    max_diagnostic_bytes: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProtocolConfig {
    max_message_bytes: Option<u64>,
    max_partial_result_bytes: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PreviewConfig {
    max_count: Option<u64>,
    max_total_bytes: Option<u64>,
    max_document_text_bytes: Option<u64>,
    max_text_bytes: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReceiptConfig {
    max_count: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MutationConfig {
    application_lock_timeout: Option<String>,
    max_entries: Option<u64>,
    max_recursion_depth: Option<u64>,
    max_rollback_bytes: Option<u64>,
    max_staged_text_bytes: Option<u64>,
    max_preauthorized_callbacks: Option<u64>,
}

/// Loads strict user and project configuration for one canonical Workspace.
pub(crate) fn load_configuration(
    workspace: &Path,
    ignore_project: bool,
) -> Result<LoadedConfiguration, ContractFailure> {
    let workspace = canonical_workspace(workspace)?;
    let workspace_uri = Url::from_directory_path(&workspace)
        .map_err(|()| workspace_failure("The Workspace path cannot be represented as a file URI."))?
        .to_string();
    let user_path = resolved_user_config_path()?;
    let project_path = workspace.join(".lspctl.toml");
    let user = read_optional_toml::<UserConfigFile>(&user_path)?;
    let project = if ignore_project {
        None
    } else {
        read_optional_toml::<ProjectConfigFile>(&project_path)?
    };
    if let Some(user) = &user {
        validate_version(user.version, &user_path)?;
        validate_user_file(user, &user_path)
            .map_err(|failure| ensure_configuration_path(failure, &user_path))?;
    }
    if let Some(project) = &project {
        validate_version(project.version, &project_path)?;
        validate_project_file(project, &project_path)
            .map_err(|failure| ensure_configuration_path(failure, &project_path))?;
    }

    let user_directory = user_path.parent().unwrap().to_path_buf();
    let servers = merge_servers(user.as_ref(), project.as_ref(), &user_directory, &workspace)
        .map_err(|failure| {
            ensure_configuration_path(
                failure,
                project.as_ref().map_or(&user_path, |_| &project_path),
            )
        })?;
    let routes = project
        .as_ref()
        .and_then(|config| config.routes.clone())
        .or_else(|| user.as_ref().and_then(|config| config.routes.clone()))
        .unwrap_or_default();
    validate_routes(
        &routes,
        &servers,
        project.as_ref().map_or(&user_path, |_| &project_path),
    )?;
    let default_server = project
        .as_ref()
        .and_then(|config| config.default_server.clone())
        .or_else(|| {
            user.as_ref()
                .and_then(|config| config.default_server.clone())
        });
    if let Some(server) = &default_server
        && !servers.contains_key(server)
    {
        return Err(configuration_failure(
            project.as_ref().map_or(&user_path, |_| &project_path),
            "missing_server_reference",
            "default_server references an unknown server",
            Some("default_server"),
        ));
    }

    let settings = user_settings(user.as_ref())
        .map_err(|failure| ensure_configuration_path(failure, &user_path))?;
    Ok(LoadedConfiguration {
        workspace,
        workspace_uri,
        project_path,
        default_server,
        routes,
        servers,
        session: settings.0,
        synchronization: settings.1,
        protocol: settings.2,
        previews: settings.3,
        receipts: settings.4,
        mutation: settings.5,
    })
}

/// Selects and completes one server, including explicit CLI launch overrides.
pub(crate) fn select_server(
    configuration: &LoadedConfiguration,
    invocation: &ParsedInvocation,
) -> Result<EffectiveServer, ContractFailure> {
    let name = if let Some(server) = invocation.option_string("--server") {
        server
    } else if let Some(language_id) = invocation.option_string("--language-id") {
        unique_route_server(&configuration.routes, |route| {
            route.language_id == language_id
        })?
    } else if let Some(file) = invocation.option_path("--file") {
        longest_extension_server(&configuration.routes, &file)?
    } else {
        configuration.default_server.clone().ok_or_else(|| {
            server_selection_failure(
                "A Workspace-wide command requires --server or default_server.",
            )
        })?
    };
    select_named_server(configuration, &name, invocation)
}

/// Selects one exact stored server name while still applying invocation-scoped launch fields.
pub(crate) fn select_named_server(
    configuration: &LoadedConfiguration,
    name: &str,
    invocation: &ParsedInvocation,
) -> Result<EffectiveServer, ContractFailure> {
    let mut server = configuration
        .servers
        .get(name)
        .cloned()
        .ok_or_else(|| server_selection_failure("The selected server has no declaration."))?;
    apply_cli_overrides(&mut server, invocation)?;
    if server.executable.is_none() {
        return Err(incomplete_server_declaration(name));
    }
    Ok(server)
}

/// Selects one declared server without applying invocation-scoped launch overrides.
pub(crate) fn select_configured_server(
    configuration: &LoadedConfiguration,
    name: &str,
) -> Result<EffectiveServer, ContractFailure> {
    let server = configuration
        .servers
        .get(name)
        .cloned()
        .ok_or_else(|| server_selection_failure("The selected server has no declaration."))?;
    if server.executable.is_none() {
        return Err(incomplete_server_declaration(name));
    }
    Ok(server)
}

fn incomplete_server_declaration(name: &str) -> ContractFailure {
    ContractFailure {
        exit_code: 3,
        category: "blocked",
        code: "server_declaration_incomplete",
        message: "The selected server declaration is incomplete.".to_owned(),
        stage: "select_server",
        delivery: "not_sent",
        retry: "after_change",
        data: json!({"server": name, "missingFields": ["executable"]}),
    }
}

/// Derives one synchronized Document language ID for the selected server.
pub(crate) fn document_language_id(
    configuration: &LoadedConfiguration,
    server: &str,
    file: &Path,
    explicit: Option<&str>,
) -> Result<String, ContractFailure> {
    if let Some(explicit) = explicit {
        return Ok(explicit.to_owned());
    }
    let file = file.to_string_lossy().to_ascii_lowercase();
    let matching = configuration
        .routes
        .iter()
        .filter(|route| route.server == server)
        .flat_map(|route| {
            route
                .extensions
                .iter()
                .map(move |extension| (route, extension))
        })
        .filter(|(_, extension)| file.ends_with(&extension.to_ascii_lowercase()))
        .collect::<Vec<_>>();
    let longest = matching
        .iter()
        .map(|(_, extension)| extension.len())
        .max()
        .ok_or_else(|| {
            server_selection_failure(
                "No language route for the selected server matches a synchronized Document.",
            )
        })?;
    let languages = matching
        .into_iter()
        .filter(|(_, extension)| extension.len() == longest)
        .map(|(route, _)| route.language_id.clone())
        .collect::<BTreeSet<_>>();
    if languages.len() != 1 {
        return Err(server_selection_failure(
            "Equally specific routes disagree on the synchronized Document language ID.",
        ));
    }
    Ok(languages.into_iter().next().unwrap())
}

/// Resolves a server executable without invoking a shell.
pub(crate) fn resolve_server_executable(
    server: &EffectiveServer,
) -> Result<PathBuf, ContractFailure> {
    let executable = server
        .executable
        .as_ref()
        .ok_or_else(|| incomplete_server_declaration(&server.name))?;
    let declared = &executable.value;
    let resolved = if declared.contains('/') || declared.contains('\\') {
        let path = PathBuf::from(declared);
        let path = if path.is_absolute() {
            path
        } else {
            executable.declaring_directory.join(path)
        };
        dunce::canonicalize(path).ok()
    } else {
        let cwd = resolve_server_cwd(server)?;
        which::which_in(declared, effective_path(server), cwd).ok()
    };
    resolved.ok_or_else(|| ContractFailure {
        exit_code: 4,
        category: "unavailable",
        code: "server_executable_unavailable",
        message: "The configured language-server executable is unavailable.".to_owned(),
        stage: "resolve_executable",
        delivery: "not_sent",
        retry: "after_change",
        data: json!({"server": server.name, "declared": declared}),
    })
}

fn effective_path(server: &EffectiveServer) -> Option<OsString> {
    #[cfg(windows)]
    let configured = server
        .environment
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("PATH"))
        .map(|(_, value)| OsString::from(&value.value));
    #[cfg(not(windows))]
    let configured = server
        .environment
        .get("PATH")
        .map(|value| OsString::from(&value.value));

    configured.or_else(|| env::var_os("PATH"))
}

pub(crate) fn resolve_server_cwd(server: &EffectiveServer) -> Result<PathBuf, ContractFailure> {
    let cwd = server
        .cwd
        .as_ref()
        .expect("merged servers always have a cwd");
    let path = PathBuf::from(&cwd.value);
    let path = if path.is_absolute() {
        path
    } else {
        cwd.declaring_directory.join(path)
    };
    dunce::canonicalize(path).map_err(|_| {
        server_selection_failure("The selected server working directory is unavailable.")
    })
}

/// Builds the stable environment inherited by the language-server process.
pub(crate) fn effective_child_environment(
    server: &EffectiveServer,
    _cwd: &Path,
) -> Vec<(OsString, OsString)> {
    let mut environment = env::vars_os()
        .filter(|(key, _)| !is_transient_inherited_environment_key(key))
        .collect::<BTreeMap<_, _>>();
    #[cfg(unix)]
    environment.insert(OsString::from("PWD"), _cwd.as_os_str().to_os_string());
    for (key, value) in &server.environment {
        #[cfg(windows)]
        if let Some(existing) = environment
            .keys()
            .find(|existing| existing.to_string_lossy().eq_ignore_ascii_case(key))
            .cloned()
        {
            environment.remove(&existing);
        }
        environment.insert(OsString::from(key), OsString::from(&value.value));
    }
    environment.into_iter().collect()
}

fn is_transient_inherited_environment_key(key: &OsStr) -> bool {
    let key = key.to_string_lossy();
    ["_", "OLDPWD", "PWD", "PS1", "PS2", "PS4", "SHLVL"]
        .iter()
        .any(|transient| key.eq_ignore_ascii_case(transient))
        || key
            .get(..3)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("PI_"))
        || key.starts_with("__MISE_")
}

pub(crate) fn session_identity(
    configuration: &LoadedConfiguration,
    server: &EffectiveServer,
    executable: &Path,
    cwd: &Path,
    child_environment: &[(OsString, OsString)],
) -> String {
    let environment = child_environment
        .iter()
        .map(|(key, value)| {
            json!({
                "key": encode_os_string(key),
                "value": encode_os_string(value)
            })
        })
        .collect::<Vec<_>>();
    let digest = digest_canonical_value(
        "lspctl-session-identity-v1",
        &json!({
            "workspaceUri": configuration.workspace_uri,
            "server": server.name,
            "resolvedExecutablePath": encode_os_string(executable.as_os_str()),
            "args": server.args.value,
            "cwd": encode_os_string(cwd.as_os_str()),
            "completeChildEnvironment": environment,
            "initializationOptions": server.initialization_options.as_ref().map(|value| &value.value),
            "settings": server.settings.value,
            "capabilityProfileVersion": 1,
            "ownerProtocolVersion": 1,
            "initializationTimeout": configuration.session.initialization_timeout,
            "cancellationGrace": configuration.session.cancellation_grace,
            "shutdownTimeout": configuration.session.shutdown_timeout,
            "idleTimeout": configuration.session.idle_timeout,
            "maxOpenDocuments": configuration.synchronization.max_open_documents,
            "maxDocumentBytes": configuration.synchronization.max_document_bytes,
            "maxTotalTextBytes": configuration.synchronization.max_total_text_bytes,
            "maxDiagnosticSnapshots": configuration.synchronization.max_diagnostic_snapshots,
            "maxDiagnosticBytes": configuration.synchronization.max_diagnostic_bytes,
            "maxMessageBytes": configuration.protocol.max_message_bytes,
            "maxPartialResultBytes": configuration.protocol.max_partial_result_bytes
        }),
    );
    format!("sid_{}", digest.trim_start_matches("sha256:"))
}

#[cfg(unix)]
fn encode_os_string(value: &OsStr) -> Value {
    use std::os::unix::ffi::OsStrExt;

    match value.to_str() {
        Some(value) => json!({"utf8": value}),
        None => json!({"unixBytes": hex::encode(value.as_bytes())}),
    }
}

#[cfg(windows)]
fn encode_os_string(value: &OsStr) -> Value {
    use std::os::windows::ffi::OsStrExt;

    match value.to_str() {
        Some(value) => json!({"utf8": value}),
        None => json!({"windowsWide": value.encode_wide().collect::<Vec<_>>() }),
    }
}

#[cfg(not(any(unix, windows)))]
fn encode_os_string(value: &OsStr) -> Value {
    json!({"utf8Lossy": value.to_string_lossy()})
}

pub(crate) fn canonical_workspace(path: &Path) -> Result<PathBuf, ContractFailure> {
    let path = if path.as_os_str().is_empty() {
        env::current_dir()
            .map_err(|_| workspace_failure("The current directory is unavailable."))?
    } else {
        path.to_path_buf()
    };
    let canonical = dunce::canonicalize(path).map_err(|_| {
        workspace_failure("The Workspace path does not exist or cannot be resolved.")
    })?;
    if !canonical.is_dir() {
        return Err(workspace_failure("The Workspace path is not a directory."));
    }
    Ok(canonical)
}

pub(crate) fn resolved_user_config_path() -> Result<PathBuf, ContractFailure> {
    #[cfg(target_os = "linux")]
    if let Some(xdg) = env::var_os("XDG_CONFIG_HOME") {
        let path = PathBuf::from(xdg);
        if !path.is_absolute() {
            return Err(user_path_failure("XDG_CONFIG_HOME is not absolute."));
        }
        return Ok(path.join("lspctl/config.toml"));
    }
    #[cfg(windows)]
    if let Some(app_data) = env::var_os("APPDATA") {
        let path = PathBuf::from(app_data);
        if !path.is_absolute() {
            return Err(user_path_failure("APPDATA is not absolute."));
        }
        return Ok(path.join("lspctl/config.toml"));
    }
    directories::ProjectDirs::from("", "", "lspctl")
        .map(|directories| directories.config_dir().join("config.toml"))
        .ok_or_else(|| {
            user_path_failure("The operating system user configuration directory is unavailable.")
        })
}

pub(crate) fn resolved_user_state_directory() -> Option<PathBuf> {
    #[cfg(windows)]
    if let Some(local_app_data) = env::var_os("LOCALAPPDATA") {
        let path = PathBuf::from(local_app_data);
        return path.is_absolute().then(|| path.join("lspctl"));
    }
    directories::ProjectDirs::from("", "", "lspctl").map(|directories| {
        directories
            .state_dir()
            .unwrap_or_else(|| directories.data_local_dir())
            .to_path_buf()
    })
}

fn merge_servers(
    user: Option<&UserConfigFile>,
    project: Option<&ProjectConfigFile>,
    user_directory: &Path,
    workspace: &Path,
) -> Result<BTreeMap<String, EffectiveServer>, ContractFailure> {
    let names = user
        .into_iter()
        .flat_map(|config| config.servers.keys())
        .chain(project.into_iter().flat_map(|config| config.servers.keys()))
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut servers = BTreeMap::new();
    for name in names {
        let user_server = user.and_then(|config| config.servers.get(&name));
        let project_server = project.and_then(|config| config.servers.get(&name));
        let request_timeout = user_server
            .and_then(|server| server.request_timeout.clone())
            .or_else(|| user.and_then(|config| config.session.as_ref()?.request_timeout.clone()))
            .unwrap_or_else(|| "30s".to_owned());
        let published_diagnostics_wait = user_server
            .and_then(|server| server.published_diagnostics_wait.clone())
            .unwrap_or_else(|| DEFAULT_PUBLISHED_DIAGNOSTICS_WAIT.to_owned());
        validate_duration(&request_timeout, "servers.request_timeout")?;
        validate_duration(
            &published_diagnostics_wait,
            "servers.published_diagnostics_wait",
        )?;

        let executable = sourced_field(
            user_server.and_then(|server| server.executable.clone()),
            project_server.and_then(|server| server.executable.clone()),
            user_directory,
            workspace,
        );
        let args = sourced_field(
            user_server.and_then(|server| server.args.clone()),
            project_server.and_then(|server| server.args.clone()),
            user_directory,
            workspace,
        )
        .unwrap_or_else(|| sourced(Vec::new(), ConfigSource::BuiltIn, workspace));
        let cwd = sourced_field(
            user_server.and_then(|server| server.cwd.clone()),
            project_server.and_then(|server| server.cwd.clone()),
            user_directory,
            workspace,
        )
        .or_else(|| {
            Some(sourced(
                workspace.to_string_lossy().into_owned(),
                ConfigSource::BuiltIn,
                workspace,
            ))
        });
        let mut environment = BTreeMap::new();
        if let Some(values) = user_server.and_then(|server| server.environment.as_ref()) {
            for (key, value) in values {
                environment.insert(
                    key.clone(),
                    sourced(value.clone(), ConfigSource::User, user_directory),
                );
            }
        }
        if let Some(values) = project_server.and_then(|server| server.environment.as_ref()) {
            for (key, value) in values {
                environment.insert(
                    key.clone(),
                    sourced(value.clone(), ConfigSource::Project, workspace),
                );
            }
        }
        let initialization_options = sourced_field(
            user_server
                .and_then(|server| server.initialization_options.as_ref())
                .map(toml_to_json)
                .transpose()?,
            project_server
                .and_then(|server| server.initialization_options.as_ref())
                .map(toml_to_json)
                .transpose()?,
            user_directory,
            workspace,
        );
        let settings = sourced_field(
            user_server
                .and_then(|server| server.settings.as_ref())
                .map(table_to_json)
                .transpose()?,
            project_server
                .and_then(|server| server.settings.as_ref())
                .map(table_to_json)
                .transpose()?,
            user_directory,
            workspace,
        )
        .unwrap_or_else(|| sourced(json!({}), ConfigSource::BuiltIn, workspace));
        let mut project_fields = BTreeSet::new();
        if executable
            .as_ref()
            .is_some_and(|value| value.source == ConfigSource::Project)
        {
            project_fields.insert("executable".to_owned());
        }
        if args.source == ConfigSource::Project {
            project_fields.insert("args".to_owned());
        }
        if cwd
            .as_ref()
            .is_some_and(|value| value.source == ConfigSource::Project)
        {
            project_fields.insert("cwd".to_owned());
        }
        if environment
            .values()
            .any(|value| value.source == ConfigSource::Project)
        {
            project_fields.insert("environment".to_owned());
        }
        if initialization_options
            .as_ref()
            .is_some_and(|value| value.source == ConfigSource::Project)
        {
            project_fields.insert("initialization_options".to_owned());
        }
        if settings.source == ConfigSource::Project {
            project_fields.insert("settings".to_owned());
        }
        servers.insert(
            name.clone(),
            EffectiveServer {
                name,
                executable,
                args,
                cwd,
                environment,
                initialization_options,
                settings,
                request_timeout,
                published_diagnostics_wait,
                project_fields,
            },
        );
    }
    Ok(servers)
}

fn sourced_field<T>(
    user: Option<T>,
    project: Option<T>,
    user_directory: &Path,
    workspace: &Path,
) -> Option<Sourced<T>> {
    project
        .map(|value| sourced(value, ConfigSource::Project, workspace))
        .or_else(|| user.map(|value| sourced(value, ConfigSource::User, user_directory)))
}

fn sourced<T>(value: T, source: ConfigSource, directory: &Path) -> Sourced<T> {
    Sourced {
        value,
        source,
        declaring_directory: directory.to_path_buf(),
    }
}

fn apply_cli_overrides(
    server: &mut EffectiveServer,
    invocation: &ParsedInvocation,
) -> Result<(), ContractFailure> {
    let directory = env::current_dir()
        .map_err(|_| workspace_failure("The current directory is unavailable."))?;
    if let Some(executable) = invocation.option_string("--server-executable") {
        server.executable = Some(sourced(executable, ConfigSource::Cli, &directory));
        server.project_fields.remove("executable");
    }
    if let Some(args) = invocation.option_strings("--server-arg") {
        server.args = sourced(args, ConfigSource::Cli, &directory);
        server.project_fields.remove("args");
    }
    if let Some(cwd) = invocation.option_string("--server-cwd") {
        server.cwd = Some(sourced(cwd, ConfigSource::Cli, &directory));
        server.project_fields.remove("cwd");
    }
    let mut environment_keys = BTreeSet::new();
    for entry in invocation
        .option_strings("--server-env")
        .unwrap_or_default()
    {
        let (key, value) = entry.split_once('=').unwrap();
        if !environment_keys.insert(key.to_owned()) {
            return Err(input_failure(
                "invalid_json_input",
                "A CLI environment key may be supplied only once.",
                json!({"source": "--server-env"}),
            ));
        }
        server.environment.insert(
            key.to_owned(),
            sourced(value.to_owned(), ConfigSource::Cli, &directory),
        );
    }
    if invocation.has_option("--server-env")
        && !server
            .environment
            .values()
            .any(|value| value.source == ConfigSource::Project)
    {
        server.project_fields.remove("environment");
    }
    let initialization_options = invocation
        .option_string("--initialization-options-json")
        .map(|value| serde_json::from_str(&value).expect("Clap validates inline JSON"))
        .map(Ok)
        .or_else(|| {
            invocation
                .option_path("--initialization-options-file")
                .map(|path| read_json_input(&path, false))
        })
        .transpose()?;
    if let Some(value) = initialization_options {
        server.initialization_options = Some(sourced(value, ConfigSource::Cli, &directory));
        server.project_fields.remove("initialization_options");
    }
    let server_settings = invocation
        .option_string("--server-settings-json")
        .map(|value| serde_json::from_str(&value).expect("Clap validates inline JSON"))
        .map(Ok)
        .or_else(|| {
            invocation
                .option_path("--server-settings-file")
                .map(|path| read_json_input(&path, true))
        })
        .transpose()?;
    if let Some(value) = server_settings {
        server.settings = sourced(value, ConfigSource::Cli, &directory);
        server.project_fields.remove("settings");
    }
    if let Some(request_timeout) = invocation.option_string("--request-timeout") {
        server.request_timeout = request_timeout;
    }
    Ok(())
}

fn read_json_input(path: &Path, object_required: bool) -> Result<Value, ContractFailure> {
    let bytes = fs::read(path).map_err(|error| {
        input_failure(
            "input_read_failed",
            "A JSON input file could not be read.",
            json!({
                "source": "file",
                "path": path,
                "osCode": error.raw_os_error()
            }),
        )
    })?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|error| {
        input_failure(
            "invalid_json_input",
            "A JSON input file is invalid.",
            json!({
                "source": "file",
                "path": path,
                "line": error.line(),
                "column": error.column()
            }),
        )
    })?;
    if object_required && !value.is_object() {
        return Err(input_failure(
            "invalid_json_input",
            "The JSON input must be an object.",
            json!({"source": "file", "path": path, "jsonPath": ""}),
        ));
    }
    Ok(value)
}

fn user_settings(
    user: Option<&UserConfigFile>,
) -> Result<
    (
        SessionSettings,
        SynchronizationSettings,
        ProtocolSettings,
        PreviewSettings,
        ReceiptSettings,
        MutationSettings,
    ),
    ContractFailure,
> {
    let session = user.and_then(|config| config.session.as_ref());
    let session = SessionSettings {
        owner_startup_timeout: session
            .and_then(|value| value.owner_startup_timeout.clone())
            .unwrap_or_else(|| "30s".to_owned()),
        initialization_timeout: session
            .and_then(|value| value.initialization_timeout.clone())
            .unwrap_or_else(|| "30s".to_owned()),
        request_timeout: session
            .and_then(|value| value.request_timeout.clone())
            .unwrap_or_else(|| "30s".to_owned()),
        cancellation_grace: session
            .and_then(|value| value.cancellation_grace.clone())
            .unwrap_or_else(|| "5s".to_owned()),
        shutdown_timeout: session
            .and_then(|value| value.shutdown_timeout.clone())
            .unwrap_or_else(|| "5s".to_owned()),
        idle_timeout: session
            .and_then(|value| value.idle_timeout.clone())
            .unwrap_or_else(|| "10m".to_owned()),
    };
    for (field, duration) in [
        (
            "session.owner_startup_timeout",
            &session.owner_startup_timeout,
        ),
        (
            "session.initialization_timeout",
            &session.initialization_timeout,
        ),
        ("session.request_timeout", &session.request_timeout),
        ("session.cancellation_grace", &session.cancellation_grace),
        ("session.shutdown_timeout", &session.shutdown_timeout),
        ("session.idle_timeout", &session.idle_timeout),
    ] {
        validate_duration(duration, field)?;
    }
    let synchronization = user.and_then(|config| config.synchronization.as_ref());
    let synchronization = SynchronizationSettings {
        max_open_documents: positive_or_default(
            synchronization.and_then(|value| value.max_open_documents),
            128,
        )?,
        max_document_bytes: positive_or_default(
            synchronization.and_then(|value| value.max_document_bytes),
            16_777_216,
        )?,
        max_total_text_bytes: positive_or_default(
            synchronization.and_then(|value| value.max_total_text_bytes),
            67_108_864,
        )?,
        max_diagnostic_snapshots: positive_or_default(
            synchronization.and_then(|value| value.max_diagnostic_snapshots),
            128,
        )?,
        max_diagnostic_bytes: positive_or_default(
            synchronization.and_then(|value| value.max_diagnostic_bytes),
            134_217_728,
        )?,
    };
    if synchronization.max_total_text_bytes < synchronization.max_document_bytes {
        return Err(simple_configuration_failure(
            "synchronization.max_total_text_bytes must be at least max_document_bytes",
        ));
    }
    let protocol = user.and_then(|config| config.protocol.as_ref());
    let protocol = ProtocolSettings {
        max_message_bytes: positive_or_default(
            protocol.and_then(|value| value.max_message_bytes),
            67_108_864,
        )?,
        max_partial_result_bytes: positive_or_default(
            protocol.and_then(|value| value.max_partial_result_bytes),
            67_108_864,
        )?,
    };

    let previews = user.and_then(|config| config.previews.as_ref());
    let previews = PreviewSettings {
        max_count: positive_or_default(previews.and_then(|value| value.max_count), 64)?,
        max_total_bytes: positive_or_default(
            previews.and_then(|value| value.max_total_bytes),
            268_435_456,
        )?,
        max_document_text_bytes: positive_or_default(
            previews.and_then(|value| value.max_document_text_bytes),
            16_777_216,
        )?,
        max_text_bytes: positive_or_default(
            previews.and_then(|value| value.max_text_bytes),
            67_108_864,
        )?,
    };
    if previews.max_total_bytes < previews.max_text_bytes
        || previews.max_text_bytes < previews.max_document_text_bytes
    {
        return Err(simple_configuration_failure(
            "Preview byte limits are inconsistent.",
        ));
    }
    let receipts = ReceiptSettings {
        max_count: positive_or_default(
            user.and_then(|config| config.receipts.as_ref()?.max_count),
            1024,
        )?,
    };
    let mutation = user.and_then(|config| config.mutation.as_ref());
    let mutation = MutationSettings {
        application_lock_timeout: mutation
            .and_then(|value| value.application_lock_timeout.clone())
            .unwrap_or_else(|| "30s".to_owned()),
        max_entries: positive_or_default(mutation.and_then(|value| value.max_entries), 10_000)?,
        max_recursion_depth: positive_or_default(
            mutation.and_then(|value| value.max_recursion_depth),
            64,
        )?,
        max_rollback_bytes: positive_or_default(
            mutation.and_then(|value| value.max_rollback_bytes),
            1_073_741_824,
        )?,
        max_staged_text_bytes: positive_or_default(
            mutation.and_then(|value| value.max_staged_text_bytes),
            67_108_864,
        )?,
        max_preauthorized_callbacks: positive_or_default(
            mutation.and_then(|value| value.max_preauthorized_callbacks),
            64,
        )?,
    };
    validate_duration(
        &mutation.application_lock_timeout,
        "mutation.application_lock_timeout",
    )?;
    Ok((
        session,
        synchronization,
        protocol,
        previews,
        receipts,
        mutation,
    ))
}

fn validate_user_file(config: &UserConfigFile, path: &Path) -> Result<(), ContractFailure> {
    for (name, server) in &config.servers {
        validate_server_name(name, path)?;
        if let Some(duration) = &server.request_timeout {
            validate_duration(duration, "servers.request_timeout")?;
        }
        if let Some(duration) = &server.published_diagnostics_wait {
            validate_duration(duration, "servers.published_diagnostics_wait")?;
        }
        if let Some(value) = &server.initialization_options {
            reject_toml_datetime(value)?;
        }
        if let Some(value) = &server.settings {
            table_to_json(value)?;
        }
    }
    Ok(())
}

fn validate_project_file(config: &ProjectConfigFile, path: &Path) -> Result<(), ContractFailure> {
    for (name, server) in &config.servers {
        validate_server_name(name, path)?;
        if let Some(value) = &server.initialization_options {
            reject_toml_datetime(value)?;
        }
        if let Some(value) = &server.settings {
            table_to_json(value)?;
        }
    }
    Ok(())
}

fn validate_routes(
    routes: &[RouteConfig],
    servers: &BTreeMap<String, EffectiveServer>,
    path: &Path,
) -> Result<(), ContractFailure> {
    let mut selectors = BTreeSet::new();
    for route in routes {
        validate_server_name(&route.server, path)?;
        if route.language_id.is_empty()
            || route.extensions.is_empty()
            || route.extensions.iter().any(|extension| {
                !extension.starts_with('.') || extension.contains('/') || extension.contains('\\')
            })
        {
            return Err(configuration_failure(
                path,
                "invalid_value",
                "route has an invalid language_id or extension",
                Some("routes"),
            ));
        }
        if !servers.contains_key(&route.server) {
            return Err(configuration_failure(
                path,
                "missing_server_reference",
                "route references an unknown server",
                Some("routes.server"),
            ));
        }
        for extension in &route.extensions {
            let selector = (
                route.server.clone(),
                route.language_id.clone(),
                extension.to_ascii_lowercase(),
            );
            if !selectors.insert(selector) {
                return Err(configuration_failure(
                    path,
                    "ambiguous_route",
                    "route repeats a selector for the same server",
                    Some("routes"),
                ));
            }
        }
    }
    Ok(())
}

fn unique_route_server(
    routes: &[RouteConfig],
    predicate: impl Fn(&RouteConfig) -> bool,
) -> Result<String, ContractFailure> {
    let matches = routes
        .iter()
        .filter(|route| predicate(route))
        .map(|route| route.server.clone())
        .collect::<BTreeSet<_>>();
    if matches.len() == 1 {
        Ok(matches.into_iter().next().unwrap())
    } else {
        Err(server_selection_failure(
            "Language or extension routing did not select exactly one server.",
        ))
    }
}

fn longest_extension_server(
    routes: &[RouteConfig],
    file: &Path,
) -> Result<String, ContractFailure> {
    let file = file.to_string_lossy().to_ascii_lowercase();
    let longest = routes
        .iter()
        .flat_map(|route| {
            route
                .extensions
                .iter()
                .map(move |extension| (route, extension))
        })
        .filter(|(_, extension)| file.ends_with(&extension.to_ascii_lowercase()))
        .map(|(_, extension)| extension.len())
        .max()
        .ok_or_else(|| server_selection_failure("No route matches the target file."))?;
    unique_route_server(routes, |route| {
        route.extensions.iter().any(|extension| {
            extension.len() == longest && file.ends_with(&extension.to_ascii_lowercase())
        })
    })
}

fn read_optional_toml<T: for<'de> Deserialize<'de>>(
    path: &Path,
) -> Result<Option<T>, ContractFailure> {
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(configuration_failure(
                path,
                "invalid_value",
                &format!("configuration file cannot be read: {error}"),
                None,
            ));
        }
    };
    toml::from_str(&contents).map(Some).map_err(|error| {
        let (line, column) = error
            .span()
            .map(|span| line_column(&contents, span.start))
            .unwrap_or((1, 1));
        let message = error.to_string();
        let code = if message.contains("unknown field") {
            "unknown_field"
        } else if message.contains("duplicate key") || message.contains("duplicate table") {
            "duplicate_definition"
        } else if message.contains("missing field") {
            "missing_required_field"
        } else {
            "invalid_type"
        };
        let mut failure = configuration_failure(path, code, &message, None);
        failure.data["problems"][0]["line"] = json!(line);
        failure.data["problems"][0]["column"] = json!(column);
        failure
    })
}

fn line_column(contents: &str, offset: usize) -> (usize, usize) {
    let prefix = &contents[..offset.min(contents.len())];
    let line = prefix.bytes().filter(|byte| *byte == b'\n').count() + 1;
    let column = prefix
        .rsplit_once('\n')
        .map_or(prefix.len(), |(_, tail)| tail.len())
        + 1;
    (line, column)
}

fn toml_to_json(value: &toml::Value) -> Result<Value, ContractFailure> {
    match value {
        toml::Value::String(value) => Ok(Value::String(value.clone())),
        toml::Value::Integer(value) => Ok(json!(value)),
        toml::Value::Float(value) if value.is_finite() => Ok(json!(value)),
        toml::Value::Boolean(value) => Ok(json!(value)),
        toml::Value::Array(values) => values
            .iter()
            .map(toml_to_json)
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
        toml::Value::Table(values) => values
            .iter()
            .map(|(key, value)| Ok((key.clone(), toml_to_json(value)?)))
            .collect::<Result<Map<_, _>, _>>()
            .map(Value::Object),
        _ => Err(simple_configuration_failure(
            "TOML date and time values are not allowed in opaque server settings.",
        )),
    }
}

fn table_to_json(table: &toml::Table) -> Result<Value, ContractFailure> {
    toml_to_json(&toml::Value::Table(table.clone()))
}

fn reject_toml_datetime(value: &toml::Value) -> Result<(), ContractFailure> {
    toml_to_json(value).map(|_| ())
}

fn validate_version(version: u32, path: &Path) -> Result<(), ContractFailure> {
    if version == 1 {
        Ok(())
    } else {
        Err(configuration_failure(
            path,
            "unsupported_version",
            "only configuration version 1 is supported",
            Some("version"),
        ))
    }
}

fn validate_server_name(name: &str, path: &Path) -> Result<(), ContractFailure> {
    if crate::cli::valid_server_name(name) {
        Ok(())
    } else {
        Err(configuration_failure(
            path,
            "invalid_value",
            "server name is invalid",
            Some("servers"),
        ))
    }
}

fn validate_duration(value: &str, field: &str) -> Result<Duration, ContractFailure> {
    let (digits, multiplier) = if let Some(digits) = value.strip_suffix("ms") {
        (digits, 1)
    } else if let Some(digits) = value.strip_suffix('s') {
        (digits, 1_000)
    } else if let Some(digits) = value.strip_suffix('m') {
        (digits, 60_000)
    } else {
        return Err(simple_configuration_failure(&format!(
            "{field} is not a valid duration"
        )));
    };
    let amount = digits
        .parse::<u64>()
        .ok()
        .filter(|amount| *amount > 0)
        .ok_or_else(|| simple_configuration_failure(&format!("{field} is not a valid duration")))?;
    amount
        .checked_mul(multiplier)
        .map(Duration::from_millis)
        .ok_or_else(|| simple_configuration_failure(&format!("{field} is too large")))
}

fn positive_or_default(value: Option<u64>, default: u64) -> Result<u64, ContractFailure> {
    let value = value.unwrap_or(default);
    if value > 0 {
        Ok(value)
    } else {
        Err(simple_configuration_failure(
            "Configuration limits must be positive.",
        ))
    }
}

fn configuration_failure(
    path: &Path,
    problem_code: &'static str,
    message: &str,
    field_path: Option<&str>,
) -> ContractFailure {
    let mut problem = json!({"code": problem_code, "message": message});
    if let Some(field_path) = field_path {
        problem["fieldPath"] = json!(field_path);
    }
    ContractFailure {
        exit_code: 3,
        category: "blocked",
        code: "invalid_configuration",
        message: "Configuration is invalid.".to_owned(),
        stage: "load_configuration",
        delivery: "not_sent",
        retry: "after_change",
        data: json!({"path": path, "problems": [problem]}),
    }
}

fn simple_configuration_failure(message: &str) -> ContractFailure {
    ContractFailure {
        exit_code: 3,
        category: "blocked",
        code: "invalid_configuration",
        message: "Configuration is invalid.".to_owned(),
        stage: "load_configuration",
        delivery: "not_sent",
        retry: "after_change",
        data: json!({"path": "", "problems": [{"code": "invalid_value", "message": message}]}),
    }
}

fn ensure_configuration_path(mut failure: ContractFailure, path: &Path) -> ContractFailure {
    if failure.code == "invalid_configuration"
        && failure
            .data
            .get("path")
            .and_then(Value::as_str)
            .is_some_and(str::is_empty)
    {
        failure.data["path"] = json!(path);
    }
    failure
}

fn workspace_failure(message: &str) -> ContractFailure {
    ContractFailure {
        exit_code: 3,
        category: "blocked",
        code: "workspace_selection_failed",
        message: message.to_owned(),
        stage: "resolve_workspace",
        delivery: "not_sent",
        retry: "after_change",
        data: json!({"reason": message}),
    }
}

fn server_selection_failure(message: &str) -> ContractFailure {
    ContractFailure {
        exit_code: 3,
        category: "blocked",
        code: "server_selection_failed",
        message: message.to_owned(),
        stage: "select_server",
        delivery: "not_sent",
        retry: "after_change",
        data: json!({"reason": message}),
    }
}

fn input_failure(code: &'static str, message: &str, data: Value) -> ContractFailure {
    ContractFailure {
        exit_code: 2,
        category: "input",
        code,
        message: message.to_owned(),
        stage: "read_input",
        delivery: "not_applicable",
        retry: if code == "input_read_failed" {
            "after_change"
        } else {
            "never"
        },
        data,
    }
}

fn user_path_failure(reason: &str) -> ContractFailure {
    ContractFailure {
        exit_code: 4,
        category: "unavailable",
        code: "user_path_unavailable",
        message: "The user configuration path is unavailable.".to_owned(),
        stage: "load_configuration",
        delivery: "not_applicable",
        retry: "after_change",
        data: json!({"kind": "configuration", "reason": reason}),
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use tempfile::TempDir;

    use super::*;

    #[test]
    fn strict_project_config_merges_with_user_independently() {
        let workspace = TempDir::new().unwrap();
        let mut project = fs::File::create(workspace.path().join(".lspctl.toml")).unwrap();
        writeln!(
            project,
            "version = 1\n[servers.rust]\nexecutable = \"rust-analyzer\"\nargs = [\"--stdio\"]"
        )
        .unwrap();

        let loaded = load_configuration(workspace.path(), false).unwrap();
        let server = loaded.servers.get("rust").unwrap();
        assert_eq!(server.args.value, ["--stdio"]);
        assert!(server.project_fields.contains("executable"));
        assert!(server.project_fields.contains("args"));
    }

    #[test]
    fn canonical_toml_conversion_rejects_dates() {
        let value = toml::from_str::<toml::Table>("date = 2026-01-01")
            .unwrap()
            .remove("date")
            .unwrap();
        assert!(toml_to_json(&value).is_err());
    }
}
