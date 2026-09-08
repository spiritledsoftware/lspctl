#![cfg(feature = "fake-server")]

use std::{
    fs,
    path::PathBuf,
    process::{Child, Command, Output, Stdio},
    time::{Duration, Instant},
};

use serde_json::{Value, json};
use tempfile::TempDir;

struct Fixture {
    _root: TempDir,
    workspace: PathBuf,
    environment: Vec<(String, PathBuf)>,
}

impl Fixture {
    fn new() -> Self {
        Self::with_server_arguments(&[])
    }

    fn with_server_arguments(arguments: &[&str]) -> Self {
        Self::with_configuration(arguments, "")
    }

    fn with_configuration(arguments: &[&str], configuration: &str) -> Self {
        let root = TempDir::new().unwrap();
        let workspace = root.path().join("workspace");
        fs::create_dir(&workspace).unwrap();

        #[cfg(target_os = "linux")]
        let (config, environment) = {
            let config = root.path().join("config");
            let state = root.path().join("state");
            (
                config.join("lspctl/config.toml"),
                vec![
                    ("XDG_CONFIG_HOME".to_owned(), config),
                    ("XDG_STATE_HOME".to_owned(), state),
                    ("HOME".to_owned(), root.path().join("home")),
                ],
            )
        };

        #[cfg(target_os = "macos")]
        let (config, environment) = {
            let home = root.path().join("home");
            (
                home.join("Library/Application Support/lspctl/config.toml"),
                vec![("HOME".to_owned(), home)],
            )
        };

        #[cfg(windows)]
        let (config, environment) = {
            let home = root.path().join("home");
            let roaming = home.join("AppData/Roaming");
            let local = home.join("AppData/Local");
            (
                roaming.join("lspctl/config.toml"),
                vec![
                    ("APPDATA".to_owned(), roaming),
                    ("LOCALAPPDATA".to_owned(), local),
                    ("USERPROFILE".to_owned(), home),
                ],
            )
        };

        for (_, directory) in &environment {
            fs::create_dir_all(directory).unwrap();
        }
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        let arguments = serde_json::to_string(arguments).unwrap();
        fs::write(
            config,
            format!(
                "version = 1\ndefault_server = \"fake\"\nroutes = [{{ server = \"fake\", language_id = \"rust\", extensions = [\".rs\"] }}]\n[servers.fake]\nexecutable = {:?}\nargs = {}\n{configuration}",
                env!("CARGO_BIN_EXE_lspctl-fake-server"), arguments
            ),
        )
        .unwrap();
        Self {
            _root: root,
            workspace,
            environment,
        }
    }

    fn command(&self, arguments: &[&str]) -> Value {
        self.command_with_environment(arguments, &[])
    }

    fn command_with_environment(&self, arguments: &[&str], environment: &[(&str, &str)]) -> Value {
        let output = self.output_with_environment(arguments, environment);
        assert!(
            output.status.success(),
            "command failed: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        assert!(output.stderr.is_empty());
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn project_configuration(&self, declarations: &str) {
        fs::write(
            self.workspace.join(".lspctl.toml"),
            format!("version = 1\n{declarations}"),
        )
        .unwrap();
    }

    fn trust(&self, arguments: &[&str]) -> Value {
        let mut command = vec!["trust"];
        command.extend_from_slice(arguments);
        command.extend(["--workspace", self.workspace.to_str().unwrap()]);
        let result = self.command(&command);
        assert_eq!(result["schemaVersion"], 1);
        assert_eq!(result["ok"], true);
        assert_eq!(result["command"], json!(["trust", arguments[0]]));
        result
    }

    fn trust_failure(&self, arguments: &[&str], code: &str) -> Value {
        let mut command = vec!["trust"];
        command.extend_from_slice(arguments);
        command.extend(["--workspace", self.workspace.to_str().unwrap()]);
        let output = self.output(&command);
        assert_eq!(
            output.status.code(),
            Some(if code == "server_executable_unavailable" {
                4
            } else {
                3
            })
        );
        assert!(output.stderr.is_empty());
        let result: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(result["schemaVersion"], 1);
        assert_eq!(result["ok"], false);
        assert_eq!(result["error"]["code"], code);
        assert_eq!(result["error"]["retry"], "after_change");
        assert!(result.get("result").is_none());
        result["error"].clone()
    }

    fn assert_no_sessions(&self) {
        assert_eq!(
            self.command(&[
                "session",
                "list",
                "--workspace",
                self.workspace.to_str().unwrap()
            ])["result"],
            json!([])
        );
    }

    fn stop(&self, workspace: &str) -> Value {
        self.command(&[
            "session",
            "stop",
            "--workspace",
            workspace,
            "--server",
            "fake",
        ])
    }

    fn output(&self, arguments: &[&str]) -> Output {
        self.output_with_environment(arguments, &[])
    }

    fn output_in_workspace(&self, arguments: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_lspctl"))
            .args(arguments)
            .current_dir(&self.workspace)
            .envs(self.environment.iter().cloned())
            .output()
            .unwrap()
    }

    fn output_with_environment(&self, arguments: &[&str], environment: &[(&str, &str)]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_lspctl"));
        command.args(arguments);
        for (name, value) in &self.environment {
            command.env(name, value);
        }
        command.envs(environment.iter().copied());
        command.output().unwrap()
    }
}

fn trust_declaration(name: &str) -> String {
    format!(
        "[servers.{name}]\nexecutable = {:?}\n",
        env!("CARGO_BIN_EXE_lspctl-fake-server")
    )
}

fn assert_trust_change(value: &Value, server: &str, state: &str) {
    let result = &value["result"];
    assert!(
        result["workspaceUri"]
            .as_str()
            .unwrap()
            .starts_with("file:")
    );
    assert!(result["ownersSignalled"].is_array());
    assert_eq!(result["ownerSignalFailures"], json!([]));
    assert_eq!(result["records"].as_array().unwrap().len(), 1);
    let record = &result["records"][0];
    assert_eq!(record["workspaceUri"], result["workspaceUri"]);
    assert_eq!(record["server"], server);
    assert_eq!(record["state"], state);
    assert!(!record["updatedAt"].as_str().unwrap().is_empty());
    assert!(record["changedFields"].is_array());
    assert!(
        record
            .as_object()
            .unwrap()
            .values()
            .all(|value| !value.is_null())
    );
}

#[test]
fn trust_administration_grant_ignores_unrelated_unavailable_server() {
    for sibling in [
        "[servers.other]\nexecutable = './absent-server'\n",
        "[servers.other]\nargs = []\n",
    ] {
        let fixture = Fixture::new();
        let selected = trust_declaration("project");
        fixture.project_configuration(&selected);
        let status = fixture.trust(&["status", "--server", "project"]);
        let digest = status["result"]["records"][0]["currentDigest"]
            .as_str()
            .unwrap();
        fixture.project_configuration(&format!("{selected}{sibling}"));

        for name in ["undeclared", "fake"] {
            fixture.trust_failure(
                &["grant", "--server", name, "--digest", digest],
                "server_selection_failed",
            );
            assert_eq!(fixture.trust(&["list"])["result"], json!([]));
        }
        let mut wrong = digest.to_owned();
        wrong.pop();
        wrong.push(if digest.ends_with('0') { '1' } else { '0' });
        fixture.trust_failure(
            &["grant", "--server", "project", "--digest", &wrong],
            "trust_digest_mismatch",
        );
        assert_eq!(fixture.trust(&["list"])["result"], json!([]));
        let granted = fixture.trust(&["grant", "--server", "project", "--digest", digest]);
        assert_trust_change(&granted, "project", "trusted");
        assert!(granted["result"].get("aggregateDigest").is_none());
        assert_eq!(granted["result"]["ownersSignalled"], json!([]));
        assert_eq!(granted["result"]["records"][0]["currentDigest"], digest);
        assert_eq!(granted["result"]["records"][0]["declarationDigest"], digest);
        let stored = fixture.trust(&["list"]);
        assert_eq!(stored["result"].as_array().unwrap().len(), 1);
        assert_eq!(stored["result"][0]["server"], "project");
        assert_eq!(stored["result"][0]["declarationDigest"], digest);
        fixture.assert_no_sessions();
    }
}

#[test]
fn trust_administration_revoke_ignores_executable_availability() {
    for selected in [
        "[servers.project]\nexecutable = './absent-server'\n",
        "[servers.project]\nargs = []\n",
        "",
    ] {
        let fixture = Fixture::new();
        let _cleanup = StopOwnerOnPanic(&fixture, "project");
        fixture.project_configuration(&trust_declaration("project"));
        let status = fixture.trust(&["status"]);
        let digest = status["result"]["records"][0]["currentDigest"]
            .as_str()
            .unwrap();
        fixture.trust(&["grant", "--server", "project", "--digest", digest]);
        let started = fixture.command(&[
            "raw",
            "--workspace",
            fixture.workspace.to_str().unwrap(),
            "--server",
            "project",
            "--method",
            "fixture/start",
        ]);
        let generation = started["context"]["ownerGeneration"].as_str().unwrap();
        fixture.project_configuration(&format!("{selected}[servers.missing]\nexecutable = './absent-sibling'\n[servers.z_incomplete]\nargs = []\n"));
        // Always stop the deliberate Owner before assertions, including on the red path.
        let output = fixture.output(&[
            "trust",
            "revoke",
            "--workspace",
            fixture.workspace.to_str().unwrap(),
            "--server",
            "project",
        ]);
        let listed = fixture.command(&[
            "session",
            "list",
            "--workspace",
            fixture.workspace.to_str().unwrap(),
        ]);
        let _ = fixture.output(&["session", "stop", generation]);
        assert!(
            output.status.success(),
            "revoke failed: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        assert!(output.stderr.is_empty());
        let revoked: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(revoked["schemaVersion"], 1);
        assert_eq!(revoked["ok"], true);
        assert_eq!(revoked["command"], json!(["trust", "revoke"]));
        assert_trust_change(&revoked, "project", "untrusted");
        assert_eq!(revoked["result"]["ownersSignalled"], json!([generation]));
        assert_eq!(listed["result"], json!([]));
        assert!(revoked["result"].get("aggregateDigest").is_none());
        assert!(
            revoked["result"]["records"][0]
                .get("currentDigest")
                .is_none()
        );
        assert_eq!(
            revoked["result"]["records"][0]["requiredCommand"],
            json!(["trust", "grant"])
        );
        assert_eq!(fixture.trust(&["list"])["result"], json!([]));
        let repeated = fixture.trust(&["revoke", "--server", "project"]);
        assert_trust_change(&repeated, "project", "untrusted");
        assert_eq!(repeated["result"]["ownersSignalled"], json!([]));
        assert!(repeated["result"].get("aggregateDigest").is_none());
        assert!(
            repeated["result"]["records"][0]
                .get("currentDigest")
                .is_none()
        );
        assert_eq!(fixture.trust(&["list"])["result"], json!([]));
        fixture.assert_no_sessions();
    }
}

#[test]
fn trust_administration_incomplete_declarations_return_json() {
    let fixture = Fixture::new();
    fixture.project_configuration(&trust_declaration("project"));
    let status = fixture.trust(&["status"]);
    let digest = status["result"]["records"][0]["currentDigest"]
        .as_str()
        .unwrap();
    for (declaration, code, category, stage) in [
        (
            "[servers.project]\nargs = []\n",
            "server_declaration_incomplete",
            "blocked",
            "select_server",
        ),
        (
            "[servers.project]\nexecutable = './absent-server'\n",
            "server_executable_unavailable",
            "unavailable",
            "resolve_executable",
        ),
    ] {
        fixture.project_configuration(declaration);
        for arguments in [
            vec!["status", "--server", "project"],
            vec!["grant", "--server", "project", "--digest", digest],
        ] {
            let error = fixture.trust_failure(&arguments, code);
            assert_eq!(error["category"], category);
            assert_eq!(error["stage"], stage);
            assert_eq!(error["delivery"], "not_sent");
            if code == "server_declaration_incomplete" {
                assert_eq!(
                    error["data"],
                    json!({"server": "project", "missingFields": ["executable"]})
                );
            } else {
                assert_eq!(
                    error["data"],
                    json!({"server": "project", "declared": "./absent-server"})
                );
            }
            assert_eq!(fixture.trust(&["list"])["result"], json!([]));
            fixture.assert_no_sessions();
        }
    }
}

#[test]
fn trust_administration_preserves_all_grant_digest_and_denials() {
    let fixture = Fixture::new();
    let declarations = format!(
        "{}{}",
        trust_declaration("project"),
        trust_declaration("other")
    );
    fixture.project_configuration(&declarations);
    let status = fixture.trust(&["status"]);
    let aggregate = status["result"]["aggregateDigest"].as_str().unwrap();
    assert_eq!(status["result"]["records"].as_array().unwrap().len(), 2);
    let granted = fixture.trust(&["grant", "--all", "--digest", aggregate]);
    assert_eq!(granted["result"]["aggregateDigest"], aggregate);
    assert_eq!(granted["result"]["records"].as_array().unwrap().len(), 2);
    assert!(
        granted["result"]["records"]
            .as_array()
            .unwrap()
            .iter()
            .all(|record| record["state"] == "trusted"
                && record["declarationDigest"] == record["currentDigest"])
    );
    let stored = fixture.trust(&["list"]);
    assert_eq!(stored["result"].as_array().unwrap().len(), 2);

    fixture.project_configuration(&format!("{declarations}args = ['--changed']\n"));
    fixture.trust_failure(
        &["grant", "--all", "--digest", aggregate],
        "trust_digest_mismatch",
    );
    assert_eq!(fixture.trust(&["list"]), stored);
    fixture.project_configuration(&declarations);
    fixture.trust(&["deny", "--server", "project"]);
    let denied = fixture.trust(&["list"]);
    fixture.trust_failure(
        &["grant", "--all", "--digest", aggregate],
        "denial_replacement_required",
    );
    assert_eq!(fixture.trust(&["list"]), denied);
    let project_digest = status["result"]["records"]
        .as_array()
        .unwrap()
        .iter()
        .find(|record| record["server"] == "project")
        .unwrap()["currentDigest"]
        .as_str()
        .unwrap();
    fixture.trust_failure(
        &["grant", "--server", "project", "--digest", project_digest],
        "denial_replacement_required",
    );
    assert_eq!(fixture.trust(&["list"]), denied);
    let replaced = fixture.trust(&[
        "grant",
        "--server",
        "project",
        "--digest",
        project_digest,
        "--replace-denial",
    ]);
    assert_trust_change(&replaced, "project", "trusted");
    fixture.trust(&["deny", "--server", "project"]);
    let replaced_all =
        fixture.trust(&["grant", "--all", "--digest", aggregate, "--replace-denials"]);
    assert!(
        replaced_all["result"]["records"]
            .as_array()
            .unwrap()
            .iter()
            .all(|record| record["state"] == "trusted")
    );
    let stored = fixture.trust(&["list"]);
    for (sibling, code) in [
        (
            "[servers.missing]\nexecutable = './absent-server'\n",
            "server_executable_unavailable",
        ),
        (
            "[servers.incomplete]\nargs = []\n",
            "server_declaration_incomplete",
        ),
    ] {
        fixture.project_configuration(&format!("{declarations}{sibling}"));
        fixture.trust_failure(
            &["grant", "--all", "--digest", aggregate, "--replace-denials"],
            code,
        );
        assert_eq!(fixture.trust(&["list"]), stored);
    }
    fixture.assert_no_sessions();
}

#[test]
fn trust_administration_status_preserves_full_aggregate_contract() {
    let fixture = Fixture::new();
    let selected = trust_declaration("project");
    fixture.project_configuration(&selected);
    let selected_only = fixture.trust(&["status"]);
    let declarations = format!("{selected}{}", trust_declaration("other"));
    fixture.project_configuration(&declarations);
    let full = fixture.trust(&["status"]);
    let named = fixture.trust(&["status", "--server", "project"]);
    let aggregate = named["result"]["aggregateDigest"].as_str().unwrap();
    assert!(aggregate.starts_with("sha256:"));
    assert_eq!(aggregate.len(), 71);
    assert_eq!(
        named["result"]["aggregateDigest"],
        full["result"]["aggregateDigest"]
    );
    assert_ne!(
        named["result"]["aggregateDigest"],
        selected_only["result"]["aggregateDigest"]
    );
    assert_eq!(named["result"]["records"].as_array().unwrap().len(), 1);
    assert_eq!(named["result"]["records"][0]["server"], "project");
    assert!(
        named["result"]["workspaceUri"]
            .as_str()
            .unwrap()
            .starts_with("file:")
    );
    for (sibling, code) in [
        (
            "[servers.other]\nexecutable = './absent-server'\n",
            "server_executable_unavailable",
        ),
        (
            "[servers.other]\nargs = []\n",
            "server_declaration_incomplete",
        ),
    ] {
        fixture.project_configuration(&format!("{selected}{sibling}"));
        let error = fixture.trust_failure(&["status", "--server", "project"], code);
        assert_eq!(error["data"]["server"], "other");
        assert_eq!(fixture.trust(&["list"])["result"], json!([]));
        fixture.assert_no_sessions();
    }
}

#[test]
fn owner_reuses_session_across_transient_agent_environment_changes() {
    let fixture = Fixture::new();
    let workspace = fixture.workspace.to_str().unwrap();
    let query = [
        "raw",
        "--workspace",
        workspace,
        "--server",
        "fake",
        "--method",
        "fixture/environment",
    ];
    let first = fixture.command_with_environment(
        &query,
        &[
            ("PI_MODEL", "first-model"),
            ("PI_SESSION_ID", "first-session"),
            ("PWD", "/first/caller"),
            ("SHLVL", "1"),
            ("__MISE_SESSION", "first-shell-session"),
        ],
    );
    let second = fixture.command_with_environment(
        &query,
        &[
            ("PI_MODEL", "second-model"),
            ("PI_SESSION_ID", "second-session"),
            ("PWD", "/second/caller"),
            ("SHLVL", "2"),
            ("__MISE_SESSION", "second-shell-session"),
        ],
    );

    assert_eq!(
        first["context"]["ownerGeneration"],
        second["context"]["ownerGeneration"]
    );

    let changed_server_environment = fixture.command_with_environment(
        &query,
        &[
            ("PI_SESSION_ID", "third-session"),
            ("RUSTUP_TOOLCHAIN", "different-toolchain"),
        ],
    );
    assert_ne!(
        first["context"]["ownerGeneration"],
        changed_server_environment["context"]["ownerGeneration"]
    );

    fixture.command(&[
        "session",
        "stop",
        first["context"]["ownerGeneration"].as_str().unwrap(),
    ]);
    fixture.command(&[
        "session",
        "stop",
        changed_server_environment["context"]["ownerGeneration"]
            .as_str()
            .unwrap(),
    ]);
}

#[test]
fn owner_serializes_simultaneous_agent_operations_in_fifo_order() {
    let fixture = Fixture::with_server_arguments(&["--scenario=delayed"]);
    let workspace = fixture.workspace.to_str().unwrap();
    fixture.command(&[
        "raw",
        "--workspace",
        workspace,
        "--server",
        "fake",
        "--method",
        "fixture/start",
    ]);

    std::thread::scope(|scope| {
        let first = scope.spawn(|| {
            fixture.command(&[
                "raw",
                "--workspace",
                workspace,
                "--server",
                "fake",
                "--method",
                "fixture/first",
            ])
        });
        std::thread::sleep(std::time::Duration::from_millis(10));
        let started = std::time::Instant::now();
        let second = fixture.command(&[
            "raw",
            "--workspace",
            workspace,
            "--server",
            "fake",
            "--method",
            "fixture/second",
        ]);
        assert_eq!(first.join().unwrap()["result"], json!({"fixture": true}));
        assert_eq!(second["result"], json!({"fixture": true}));
        assert!(started.elapsed() >= std::time::Duration::from_millis(60));
    });

    let synchronized = fixture.workspace.join("raw-stale.rs");
    let marker = fixture.workspace.join("request-started");
    fs::write(&synchronized, "old\n").unwrap();
    let raw = std::thread::scope(|scope| {
        let query = scope.spawn(|| {
            fixture.command(&[
                "raw",
                "--workspace",
                workspace,
                "--server",
                "fake",
                "--method",
                "test/await-file-change",
                "--params-json",
                &json!({"marker": marker}).to_string(),
                "--sync-file",
                synchronized.to_str().unwrap(),
            ])
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !marker.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(marker.exists());
        fs::write(&synchronized, "new\n").unwrap();
        query.join().unwrap()
    });
    assert_eq!(raw["result"], json!({"fixture": true}));
    assert_eq!(
        raw["context"]["synchronization"]["postResponseChanged"][0]["uri"],
        url::Url::from_file_path(dunce::canonicalize(&synchronized).unwrap())
            .unwrap()
            .to_string()
    );

    fixture.stop(workspace);
}

#[test]
fn queued_operation_deadline_removes_work_before_dispatch() {
    let fixture = Fixture::new();
    let workspace = fixture.workspace.to_str().unwrap();
    fixture.command(&[
        "raw",
        "--workspace",
        workspace,
        "--server",
        "fake",
        "--method",
        "fixture/start",
    ]);

    let marker = fixture.workspace.join("active-request-started");
    std::thread::scope(|scope| {
        let active = scope.spawn(|| {
            fixture.command(&[
                "raw",
                "--workspace",
                workspace,
                "--server",
                "fake",
                "--method",
                "test/await-file-change",
                "--params-json",
                &json!({"marker": marker, "sleepMs": 500}).to_string(),
            ])
        });
        let marker_deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !marker.exists() && std::time::Instant::now() < marker_deadline {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(marker.exists());
        let expired = fixture.output(&[
            "raw",
            "--workspace",
            workspace,
            "--server",
            "fake",
            "--method",
            "fixture/never-dispatched",
            "--deadline",
            "10ms",
        ]);
        assert_eq!(
            expired.status.code(),
            Some(4),
            "unexpected response: {}",
            String::from_utf8_lossy(&expired.stdout)
        );
        let failure: Value = serde_json::from_slice(&expired.stdout).unwrap();
        assert_eq!(failure["error"]["code"], "queue_deadline_exceeded");
        assert_eq!(failure["error"]["delivery"], "not_sent");
        assert_eq!(active.join().unwrap()["result"], json!({"fixture": true}));
    });

    fixture.stop(workspace);
}

#[test]
fn owner_accepts_status_and_queues_work_during_initialization() {
    let initialization = TempDir::new().unwrap();
    let gate = initialization.path().join("release");
    let gate_argument = format!("--initialization-gate={}", gate.display());
    let fixture =
        Fixture::with_server_arguments(&["--scenario=delayed-initialization", &gate_argument]);
    let workspace = fixture.workspace.to_str().unwrap();

    std::thread::scope(|scope| {
        let query = scope.spawn(|| {
            fixture.command(&[
                "raw",
                "--workspace",
                workspace,
                "--server",
                "fake",
                "--method",
                "fixture/queued-during-initialization",
            ])
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut observed_initializing = false;
        loop {
            let status = fixture.output(&[
                "session",
                "status",
                "--workspace",
                workspace,
                "--server",
                "fake",
            ]);
            if status.status.success() {
                let status: Value = serde_json::from_slice(&status.stdout).unwrap();
                if status["result"]["state"] == "initializing" {
                    observed_initializing = true;
                    break;
                }
            }
            if std::time::Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        fs::write(&gate, "release").unwrap();
        assert_eq!(query.join().unwrap()["result"], json!({"fixture": true}));
        assert!(
            observed_initializing,
            "Owner never exposed its initializing state"
        );
    });

    fixture.stop(workspace);
}

#[test]
fn initialization_failure_rejects_queued_work_with_the_same_cause() {
    let fixture = Fixture::with_server_arguments(&["--scenario=crash"]);
    let workspace = fixture.workspace.to_str().unwrap();

    let output = fixture.output(&[
        "raw",
        "--workspace",
        workspace,
        "--server",
        "fake",
        "--method",
        "fixture/never-dispatched",
    ]);
    assert_eq!(
        output.status.code(),
        Some(5),
        "unexpected response: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let failure: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(failure["error"]["code"], "initialization_failed");
    assert_eq!(failure["error"]["delivery"], "not_sent");
}

#[test]
fn force_stop_cancels_an_active_query_without_waiting_for_it() {
    let fixture = Fixture::new();
    let workspace = fixture.workspace.to_str().unwrap();
    fixture.command(&[
        "raw",
        "--workspace",
        workspace,
        "--server",
        "fake",
        "--method",
        "fixture/start",
    ]);

    let marker = fixture.workspace.join("active-request-started");
    std::thread::scope(|scope| {
        let active = scope.spawn(|| {
            fixture.output(&[
                "raw",
                "--workspace",
                workspace,
                "--server",
                "fake",
                "--method",
                "test/await-file-change",
                "--params-json",
                &json!({"marker": marker, "sleepMs": 5_000}).to_string(),
            ])
        });
        let marker_deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !marker.exists() && std::time::Instant::now() < marker_deadline {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(marker.exists());

        let started = std::time::Instant::now();
        let stopped = fixture.command(&[
            "session",
            "stop",
            "--force",
            "--workspace",
            workspace,
            "--server",
            "fake",
        ]);
        assert_eq!(stopped["result"]["outcome"], "force_stopped");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "force stop waited for the active request"
        );

        let active = active.join().unwrap();
        assert!(!active.status.success());
        let failure: Value = serde_json::from_slice(&active.stdout).unwrap();
        assert_eq!(failure["error"]["code"], "request_cancelled");
        assert_eq!(failure["error"]["data"]["source"], "force_stop");
    });
}

struct KillChildOnDrop(Child);

impl Drop for KillChildOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn wait_for_child(child: &mut Child, deadline: Instant) {
    while child.try_wait().unwrap().is_none() {
        assert!(
            Instant::now() < deadline,
            "CLI exceeded the 5-second watchdog"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn check_maintenance_under_notifications(disconnect: bool) {
    let root = TempDir::new().unwrap();
    let events = root.path().join("events.log");
    let event_argument = format!("--event-log={}", events.display());
    let fixture = Fixture::with_configuration(
        &["--scenario=notification-flood", &event_argument],
        "[session]\ncancellation_grace = \"250ms\"\n",
    );
    let _cleanup = StopOwnerOnPanic(&fixture, "fake");
    let marker = fixture.workspace.join("flood-ready");
    let stdout = root.path().join("stdout.json");
    let stderr = root.path().join("stderr.log");
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut caller = KillChildOnDrop(
        Command::new(env!("CARGO_BIN_EXE_lspctl"))
            .args([
                "raw",
                "--workspace",
                fixture.workspace.to_str().unwrap(),
                "--server",
                "fake",
                "--method",
                "test/notification-flood",
                "--params-json",
                &json!({"marker": marker}).to_string(),
                "--request-timeout",
                if disconnect { "30s" } else { "100ms" },
            ])
            .envs(fixture.environment.iter().cloned())
            .stdout(fs::File::create(&stdout).unwrap())
            .stderr(fs::File::create(&stderr).unwrap())
            .spawn()
            .unwrap(),
    );
    while !marker.exists() && Instant::now() < deadline {
        assert!(
            caller.0.try_wait().unwrap().is_none(),
            "CLI exited before dispatch: {} {}",
            fs::read_to_string(&stdout).unwrap(),
            fs::read_to_string(&stderr).unwrap()
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(
        marker.exists(),
        "fixture did not dispatch within the watchdog"
    );

    if disconnect {
        caller.0.kill().unwrap();
        wait_for_child(&mut caller.0, deadline);
    }
    while !fs::read_to_string(&events)
        .unwrap()
        .contains("$/cancelRequest")
    {
        assert!(
            Instant::now() < deadline,
            "Owner did not cancel under notification traffic within the 5-second watchdog"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let lifetime = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(marker.with_extension("lock"))
        .unwrap();
    loop {
        match lifetime.try_lock() {
            Ok(()) => break,
            Err(std::fs::TryLockError::WouldBlock) => {
                assert!(
                    Instant::now() < deadline,
                    "Owner did not terminate the server after cancellation grace within the watchdog"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) => panic!("could not check fixture lifetime: {error}"),
        }
    }
    let events = fs::read_to_string(&events).unwrap();
    assert!(
        events.find("$/cancelRequest").unwrap() < events.find("notification-after-cancel").unwrap(),
        "notification traffic must continue after ignored cancellation"
    );
    wait_for_child(&mut caller.0, deadline);
    if !disconnect {
        assert_eq!(caller.0.try_wait().unwrap().unwrap().code(), Some(5));
        assert!(fs::read(&stderr).unwrap().is_empty());
        let failure: Value = serde_json::from_slice(&fs::read(&stdout).unwrap()).unwrap();
        assert_eq!(failure["error"]["code"], "protocol_failed");
        assert_eq!(failure["error"]["delivery"], "uncertain");
        assert_eq!(failure["error"]["retry"], "unsafe");
    }

    // Server exit precedes Owner record cleanup; observe both without an unbounded CLI call.
    loop {
        let mut listed = KillChildOnDrop(
            Command::new(env!("CARGO_BIN_EXE_lspctl"))
                .args(["session", "list", "--workspace"])
                .arg(&fixture.workspace)
                .envs(fixture.environment.iter().cloned())
                .stdout(fs::File::create(&stdout).unwrap())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        wait_for_child(&mut listed.0, deadline);
        assert!(listed.0.try_wait().unwrap().unwrap().success());
        let sessions: Value = serde_json::from_slice(&fs::read(&stdout).unwrap()).unwrap();
        if sessions["result"] == json!([]) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "Owner remained after server cleanup"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn owner_maintenance_times_out_under_notifications() {
    check_maintenance_under_notifications(false);
}

#[test]
fn owner_maintenance_cancels_disconnected_caller_under_notifications() {
    check_maintenance_under_notifications(true);
}

#[test]
fn owner_reports_bounded_stderr_after_an_unexpected_server_exit() {
    let fixture = Fixture::new();
    let workspace = fixture.workspace.to_str().unwrap();
    let output = fixture.output(&[
        "raw",
        "--workspace",
        workspace,
        "--server",
        "fake",
        "--method",
        "test/crash",
    ]);

    assert_eq!(
        output.status.code(),
        Some(5),
        "unexpected response: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let failure: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(failure["error"]["code"], "server_exited");
    assert_eq!(failure["error"]["data"]["status"]["code"], 42);
    assert!(
        failure["error"]["data"]["stderrTail"]
            .as_str()
            .is_some_and(|tail| tail.contains("fixture server crashed while handling test/crash"))
    );
}

#[test]
fn owner_bounds_tracks_and_cancels_server_requests() {
    let fixture = Fixture::new();
    let workspace = fixture.workspace.to_str().unwrap();

    let bounded = fixture.command(&[
        "raw",
        "--workspace",
        workspace,
        "--server",
        "fake",
        "--method",
        "test/server-request-limit",
    ]);
    assert_eq!(bounded["result"], json!({"accepted": 64, "busy": 1}));

    let cancelled = fixture.command(&[
        "raw",
        "--workspace",
        workspace,
        "--server",
        "fake",
        "--method",
        "test/cancel-server-request",
    ]);
    assert_eq!(
        cancelled["result"]["callbackResponse"]["error"]["code"],
        -32800
    );

    fixture.stop(workspace);
}

#[test]
fn duplicate_active_server_request_id_terminates_the_owner() {
    let fixture = Fixture::new();
    let workspace = fixture.workspace.to_str().unwrap();
    let output = fixture.output(&[
        "raw",
        "--workspace",
        workspace,
        "--server",
        "fake",
        "--method",
        "test/duplicate-server-request-id",
    ]);

    assert_eq!(output.status.code(), Some(5));
    let failure: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(failure["error"]["code"], "protocol_failed");
    assert!(
        failure["error"]["data"]["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("reused an active request identifier"))
    );
}

#[test]
fn query_failure_preserves_server_error_partial_results_context_and_trace() {
    let fixture = Fixture::new();
    let workspace = fixture.workspace.to_str().unwrap();
    let output = fixture.output(&[
        "workspace-symbols",
        "--workspace",
        workspace,
        "--server",
        "fake",
        "--query",
        "error-with-partial",
        "--trace-protocol",
    ]);

    assert_eq!(output.status.code(), Some(5));
    let failure: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(failure["error"]["code"], "server_error");
    assert_eq!(failure["error"]["serverError"]["code"], -32603);
    assert_eq!(failure["method"], "workspace/symbol");
    assert_eq!(failure["partialResult"]["complete"], false);
    assert_eq!(
        failure["partialResult"]["items"][0]["name"],
        "partial-symbol"
    );
    assert_eq!(
        failure["context"]["workspaceUri"],
        url::Url::from_directory_path(dunce::canonicalize(&fixture.workspace).unwrap())
            .unwrap()
            .to_string()
    );
    assert!(failure["trace"]["frames"].as_array().is_some());

    fixture.stop(workspace);
}

struct StopOwnerOnPanic<'a>(&'a Fixture, &'a str);

impl Drop for StopOwnerOnPanic<'_> {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            return;
        }
        let Ok(mut child) = Command::new(env!("CARGO_BIN_EXE_lspctl"))
            .args([
                "session",
                "stop",
                "--server",
                self.1,
                "--force",
                "--workspace",
            ])
            .arg(&self.0.workspace)
            .envs(self.0.environment.iter().cloned())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        else {
            return;
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while matches!(child.try_wait(), Ok(None)) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let _ = child.kill();
        let _ = child.wait();
    }
}

#[test]
fn owner_partial_results_chunks_merge_success() {
    let fixture = Fixture::new();
    let _cleanup = StopOwnerOnPanic(&fixture, "fake");
    let workspace = fixture.workspace.to_str().unwrap();
    fs::write(fixture.workspace.join("partial.rs"), "fn partial() {}\n").unwrap();
    let response = fixture.command(&[
        "workspace-symbols",
        "--workspace",
        workspace,
        "--server",
        "fake",
        "--query",
        "partial-chunks-success",
    ]);

    let items = response["result"].as_array().unwrap();
    assert_eq!(
        items
            .iter()
            .map(|item| item["name"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [
            "partial-first",
            "partial-second",
            "partial-third",
            "final-symbol"
        ]
    );
    fixture.stop(workspace);
}

#[test]
fn owner_partial_results_chunk_failure_remains_flat() {
    let fixture = Fixture::new();
    let _cleanup = StopOwnerOnPanic(&fixture, "fake");
    let workspace = fixture.workspace.to_str().unwrap();
    fs::write(fixture.workspace.join("partial.rs"), "fn partial() {}\n").unwrap();
    let output = fixture.output(&[
        "workspace-symbols",
        "--workspace",
        workspace,
        "--server",
        "fake",
        "--query",
        "partial-chunks-error",
        "--trace-protocol",
    ]);

    assert_eq!(output.status.code(), Some(5));
    assert!(output.stderr.is_empty());
    let failure: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(failure["error"]["code"], "server_error");
    assert_eq!(failure["error"]["serverError"]["code"], -32603);
    assert_eq!(failure["method"], "workspace/symbol");
    assert_eq!(failure["partialResult"]["complete"], false);
    let items = failure["partialResult"]["items"].as_array().unwrap();
    assert!(items.iter().all(Value::is_object));
    assert_eq!(
        items
            .iter()
            .map(|item| item["name"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["partial-first", "partial-second", "partial-third"]
    );
    assert_eq!(
        failure["context"]["workspaceUri"],
        url::Url::from_directory_path(dunce::canonicalize(&fixture.workspace).unwrap())
            .unwrap()
            .to_string()
    );
    assert!(failure["trace"]["frames"].as_array().is_some());
    fixture.stop(workspace);
}

#[test]
fn owner_partial_results_limit_keeps_flat_count() {
    let fixture = Fixture::with_configuration(&[], "[protocol]\nmax_partial_result_bytes = 64\n");
    let _cleanup = StopOwnerOnPanic(&fixture, "fake");
    let workspace = fixture.workspace.to_str().unwrap();
    fs::write(fixture.workspace.join("partial.rs"), "fn partial() {}\n").unwrap();
    let output = fixture.output(&[
        "workspace-symbols",
        "--workspace",
        workspace,
        "--server",
        "fake",
        "--query",
        "partial-chunks-limit",
        "--trace-protocol",
    ]);

    assert_eq!(output.status.code(), Some(5));
    assert!(output.stderr.is_empty());
    let failure: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(failure["error"]["code"], "partial_result_too_large");
    assert_eq!(failure["partialResult"]["complete"], false);
    let items = failure["partialResult"]["items"].as_array().unwrap();
    assert!(items.iter().all(Value::is_object));
    assert_eq!(
        items
            .iter()
            .map(|item| item["name"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["partial-first", "partial-second"]
    );
    assert_eq!(failure["error"]["data"]["partialItemCount"], 2);
    assert_eq!(failure["error"]["data"]["limit"], 64);
    // The retained items are the single wire chunk, including its array delimiters.
    let chunk_bytes = serde_json::to_vec(items).unwrap().len();
    assert!(chunk_bytes > 64);
    assert_eq!(failure["error"]["data"]["collectedBytes"], chunk_bytes);
    let frames = failure["trace"]["frames"].as_array().unwrap();
    assert!(
        frames
            .iter()
            .any(|frame| frame["message"]["error"]["code"] == -32800)
    );
    fixture.stop(workspace);
}

#[test]
fn graceful_stop_drains_the_active_query_and_rejects_new_work() {
    let fixture = Fixture::new();
    let workspace = fixture.workspace.to_str().unwrap();
    fixture.command(&[
        "raw",
        "--workspace",
        workspace,
        "--server",
        "fake",
        "--method",
        "fixture/start",
    ]);

    let marker = fixture.workspace.join("active-request-started");
    std::thread::scope(|scope| {
        let active = scope.spawn(|| {
            fixture.command(&[
                "raw",
                "--workspace",
                workspace,
                "--server",
                "fake",
                "--method",
                "test/await-file-change",
                "--params-json",
                &json!({"marker": marker, "sleepMs": 500}).to_string(),
            ])
        });
        let marker_deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !marker.exists() && std::time::Instant::now() < marker_deadline {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(marker.exists());

        let stop_started = std::time::Instant::now();
        let stop = scope.spawn(|| fixture.stop(workspace));
        let state_deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let status = fixture.command(&[
                "session",
                "status",
                "--workspace",
                workspace,
                "--server",
                "fake",
            ]);
            if status["result"]["state"] == "draining" {
                break;
            }
            assert!(std::time::Instant::now() < state_deadline);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }

        let rejected = fixture.output(&[
            "raw",
            "--workspace",
            workspace,
            "--server",
            "fake",
            "--method",
            "fixture/rejected",
        ]);
        assert_eq!(rejected.status.code(), Some(4));
        let failure: Value = serde_json::from_slice(&rejected.stdout).unwrap();
        assert_eq!(failure["error"]["code"], "owner_unavailable");
        assert_eq!(failure["error"]["data"]["reason"], "draining");

        assert_eq!(active.join().unwrap()["result"], json!({"fixture": true}));
        let stopped = stop.join().unwrap();
        assert_eq!(stopped["result"]["outcome"], "stopped");
        assert!(stop_started.elapsed() >= std::time::Duration::from_millis(300));
    });
}

#[test]
fn raw_document_scope_requires_explicit_workspace_and_server() {
    let fixture = Fixture::new();
    let _cleanup = StopOwnerOnPanic(&fixture, "fake");
    let workspace = fixture.workspace.to_str().unwrap();
    fs::write(fixture.workspace.join("inside.rs"), "fn inside() {}\n").unwrap();
    let outside = fixture._root.path().join("outside.rs");
    fs::write(&outside, "fn outside() {}\n").unwrap();
    let outside = outside.to_str().unwrap();

    for query in [
        vec!["hover", "--file", outside, "--line", "0", "--column", "0"],
        vec!["raw", "--method", "fixture/scope", "--sync-file", outside],
    ] {
        for explicit_workspace in [false, true] {
            for explicit_server in [false, true] {
                let mut arguments = query.clone();
                if explicit_workspace {
                    arguments.extend(["--workspace", workspace]);
                }
                if explicit_server {
                    arguments.extend(["--server", "fake"]);
                }
                let output = fixture.output_in_workspace(&arguments);
                // Scope denial happens after startup; stop the Owner even for rejected Queries.
                fixture.stop(workspace);
                assert_eq!(
                    fixture.command(&["session", "list", "--workspace", workspace])["result"],
                    json!([])
                );
                assert!(output.stderr.is_empty(), "{arguments:?}: {output:?}");
                let response: Value = serde_json::from_slice(&output.stdout).unwrap();
                if explicit_workspace && explicit_server {
                    assert!(output.status.success(), "{arguments:?}: {response}");
                    assert_eq!(
                        response["context"]["workspaceUri"],
                        url::Url::from_directory_path(
                            dunce::canonicalize(&fixture.workspace).unwrap()
                        )
                        .unwrap()
                        .to_string()
                    );
                } else {
                    assert_eq!(output.status.code(), Some(3), "{arguments:?}: {response}");
                    assert_eq!(response["error"]["code"], "workspace_selection_failed");
                    assert_eq!(response["error"]["delivery"], "not_sent");
                    assert_eq!(
                        response["error"]["data"]["reason"],
                        "An outside-Workspace Document requires explicit Workspace and server selection."
                    );
                }
            }
        }
    }
}

#[test]
fn raw_document_scope_allows_in_workspace_documents() {
    let fixture = Fixture::new();
    let _cleanup = StopOwnerOnPanic(&fixture, "fake");
    let workspace = fixture.workspace.to_str().unwrap();
    let inside = fixture.workspace.join("inside.rs");
    fs::write(&inside, "fn inside() {}\n").unwrap();
    let inside = inside.to_str().unwrap();

    for arguments in [
        vec!["raw", "--method", "fixture/scope", "--sync-file", inside],
        vec!["hover", "--file", inside, "--line", "0", "--column", "0"],
    ] {
        let output = fixture.output_in_workspace(&arguments);
        fixture.stop(workspace);
        assert_eq!(
            fixture.command(&["session", "list", "--workspace", workspace])["result"],
            json!([])
        );
        assert!(output.stderr.is_empty(), "{arguments:?}: {output:?}");
        let response: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert!(output.status.success(), "{arguments:?}: {response}");
        assert_eq!(
            response["context"]["workspaceUri"],
            url::Url::from_directory_path(dunce::canonicalize(&fixture.workspace).unwrap())
                .unwrap()
                .to_string()
        );
    }
}

#[test]
fn owner_starts_reuses_dispatches_and_stops_without_leaking_output() {
    let fixture = Fixture::new();
    let workspace = fixture.workspace.to_str().unwrap();
    let first = fixture.command(&[
        "raw",
        "--workspace",
        workspace,
        "--server",
        "fake",
        "--method",
        "fixture/first",
        "--params-json",
        "null",
    ]);
    assert_eq!(first["result"], json!({"fixture": true}));
    let generation = first["context"]["ownerGeneration"]
        .as_str()
        .unwrap()
        .to_owned();

    let second = fixture.command(&[
        "raw",
        "--workspace",
        workspace,
        "--server",
        "fake",
        "--method",
        "fixture/second",
    ]);
    assert_eq!(second["context"]["ownerGeneration"], generation);

    let file = fixture.workspace.join("main.rs");
    fs::write(&file, "fn main() {}\n").unwrap();
    let definition = fixture.command(&[
        "definition",
        "--workspace",
        workspace,
        "--server",
        "fake",
        "--file",
        file.to_str().unwrap(),
        "--line",
        "0",
        "--column",
        "3",
    ]);
    assert_eq!(definition["result"], json!([]));
    assert_eq!(definition["context"]["ownerGeneration"], generation);

    let renamed = fixture.workspace.join("rename.rs");
    let original = format!(
        "fn old() {{}}\n{}",
        (1..=1_000)
            .map(|line| format!("// unchanged source line {line}\n"))
            .collect::<String>()
    );
    fs::write(&renamed, &original).unwrap();
    let rename = fixture.command(&[
        "rename",
        "--workspace",
        workspace,
        "--server",
        "fake",
        "--file",
        renamed.to_str().unwrap(),
        "--line",
        "0",
        "--column",
        "4",
        "--new-name",
        "new_name",
    ]);
    assert_eq!(rename["outcome"], "previewed");
    assert_eq!(fs::read_to_string(&renamed).unwrap(), original);
    let diff = rename["result"]["diff"].as_str().unwrap();
    assert!(diff.contains("-fn old() {}"));
    assert!(diff.contains("+fn new_name() {}"));
    assert!(!diff.contains("unchanged source line 10"));
    assert!(diff.len() < 1_024);
    let preview_id = rename["result"]["previewId"].as_str().unwrap();
    let applied = fixture.command(&["apply", preview_id]);
    assert_eq!(applied["outcome"], "applied");
    assert_eq!(applied["result"]["sessionSynchronized"], true);
    assert_eq!(
        fs::read_to_string(&renamed).unwrap(),
        original.replacen("fn old()", "fn new_name()", 1)
    );
    let receipt = fixture.command(&["receipt", "show", preview_id]);
    assert_eq!(receipt["result"]["outcome"], "applied");
    assert_eq!(receipt["result"]["sessionSynchronized"], true);

    let callback_target = fixture.workspace.join("callback.rs");
    fs::write(&callback_target, "old\n").unwrap();
    let callback_uri = url::Url::from_file_path(dunce::canonicalize(&callback_target).unwrap())
        .unwrap()
        .to_string();
    let callback = fixture.command(&[
        "raw",
        "--workspace",
        workspace,
        "--server",
        "fake",
        "--method",
        "test/request-apply-edit",
        "--params-json",
        &json!({"uri": callback_uri}).to_string(),
    ]);
    assert_eq!(callback["result"]["callbackResponse"]["applied"], false);
    assert_eq!(
        callback["result"]["callbackResponse"]["failureReason"],
        "preview_required"
    );
    let callback_preview = callback["result"]["callbackResponse"]["previewId"]
        .as_str()
        .unwrap();
    assert_eq!(
        callback["applyEditLedger"][0]["previewId"],
        callback_preview
    );
    assert_eq!(fs::read_to_string(&callback_target).unwrap(), "old\n");
    assert_eq!(
        fixture.command(&["preview", "show", callback_preview])["result"]["previewId"],
        callback_preview
    );

    let capabilities =
        fixture.command(&["capabilities", "--workspace", workspace, "--server", "fake"]);
    assert_eq!(capabilities["result"]["protocolBaseline"], "3.17");
    assert_eq!(
        capabilities["result"]["providers"]["definition"]["state"],
        "supported"
    );

    let edited = fixture.workspace.join("edited.rs");
    fs::write(&edited, "old\n").unwrap();
    let edited_uri = url::Url::from_file_path(dunce::canonicalize(&edited).unwrap())
        .unwrap()
        .to_string();
    let executed = fixture.command(&[
        "execute-command",
        "--workspace",
        workspace,
        "--server",
        "fake",
        "--command",
        "fixture.run",
        "--arguments-json",
        &serde_json::to_string(&vec![edited_uri]).unwrap(),
        "--apply-edits",
    ]);
    assert_eq!(executed["result"]["callbackApplied"], true);
    assert_eq!(executed["applyEditLedger"][0]["applied"], true);
    assert_eq!(executed["applyEditLedger"][0]["outcome"], "applied");
    assert_eq!(fs::read_to_string(edited).unwrap(), "new\n");

    let listed = fixture.command(&["session", "list", "--workspace", workspace]);
    assert_eq!(listed["result"].as_array().unwrap().len(), 1);
    assert_eq!(listed["result"][0]["ownerGeneration"], generation);

    let stopped = fixture.stop(workspace);
    assert_eq!(stopped["result"]["ownerGeneration"], generation);
    assert_eq!(stopped["result"]["outcome"], "stopped");

    let listed = fixture.command(&["session", "list", "--workspace", workspace]);
    assert_eq!(listed["result"], json!([]));
}

#[test]
fn empty_query_result_includes_active_server_progress() {
    let fixture = Fixture::new();
    let workspace = fixture.workspace.to_str().unwrap();
    fixture.command(&[
        "raw",
        "--workspace",
        workspace,
        "--server",
        "fake",
        "--method",
        "test/start-progress",
    ]);

    let file = fixture.workspace.join("main.rs");
    fs::write(&file, "fn main() {}\n").unwrap();
    let definition = fixture.command(&[
        "definition",
        "--workspace",
        workspace,
        "--server",
        "fake",
        "--file",
        file.to_str().unwrap(),
        "--line",
        "0",
        "--column",
        "3",
    ]);

    assert_eq!(definition["result"], json!([]));
    assert_eq!(
        definition["context"]["serverProgress"],
        json!([{
            "token": "fixture-indexing",
            "kind": "work_done",
            "title": "Indexing",
            "message": "loading workspace",
            "percentage": 25,
            "cancellable": false
        }])
    );

    fixture.stop(workspace);
}

#[test]
fn graceful_stop_closes_open_documents_before_shutdown() {
    let root = TempDir::new().unwrap();
    let event_log = root.path().join("events.log");
    let event_argument = format!("--event-log={}", event_log.display());
    let fixture = Fixture::with_server_arguments(&[&event_argument]);
    let workspace = fixture.workspace.to_str().unwrap();
    let document = fixture.workspace.join("open.rs");
    fs::write(&document, "fn main() {}\n").unwrap();

    fixture.command(&[
        "definition",
        "--workspace",
        workspace,
        "--server",
        "fake",
        "--file",
        document.to_str().unwrap(),
        "--line",
        "0",
        "--column",
        "3",
    ]);
    fixture.stop(workspace);

    let events = fs::read_to_string(event_log).unwrap();
    let close = events.find("textDocument/didClose").unwrap();
    let shutdown = events.find("shutdown").unwrap();
    assert!(close < shutdown, "events were out of order:\n{events}");
}
