use std::process::{Command, Output};

use serde_json::{Value, json};

#[test]
fn version_command_and_alias_emit_the_same_machine_envelope() {
    let command_output = run_lspctl(&["version"]);
    let alias_output = run_lspctl(&["--version"]);

    assert!(command_output.status.success());
    assert!(command_output.stderr.is_empty());
    assert_eq!(command_output.stdout, alias_output.stdout);
    assert_eq!(command_output.stdout.last(), Some(&b'\n'));

    let envelope: Value = serde_json::from_slice(&command_output.stdout).unwrap();
    assert_eq!(envelope["schemaVersion"], 1);
    assert_eq!(envelope["ok"], true);
    assert_eq!(envelope["command"], json!(["version"]));
    assert_eq!(envelope["result"]["name"], "lspctl");
    assert_eq!(envelope["result"]["version"], "0.1.2");
    assert_eq!(envelope["result"]["contractVersion"], 1);
    assert_eq!(envelope["result"]["configVersion"], 1);
    assert_eq!(envelope["result"]["capabilityProfileVersion"], 1);
    assert_eq!(envelope["result"]["ownerProtocolVersion"], 1);
    assert!(envelope["result"]["target"].as_str().unwrap().contains('-'));

    let commit = envelope["result"]["commit"].as_str().unwrap();
    assert!((7..=64).contains(&commit.len()));
    assert!(commit.bytes().all(|byte| byte.is_ascii_hexdigit()));
}

#[test]
fn invalid_cli_emits_a_structured_failure_without_stderr() {
    let output = run_lspctl(&["-V"]);

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stderr.is_empty());
    assert_eq!(output.stdout.last(), Some(&b'\n'));

    let envelope: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(envelope["schemaVersion"], 1);
    assert_eq!(envelope["ok"], false);
    assert_eq!(envelope["command"], json!([]));
    assert_eq!(envelope["error"]["category"], "input");
    assert_eq!(envelope["error"]["code"], "invalid_arguments");
    assert_eq!(envelope["error"]["stage"], "parse_cli");
    assert_eq!(envelope["error"]["delivery"], "not_applicable");
    assert_eq!(envelope["error"]["retry"], "never");
    assert_eq!(
        envelope["error"]["data"]["problems"][0]["code"],
        "invalid_value"
    );
}

#[test]
fn schema_help_and_group_help_are_machine_readable() {
    let schema = run_lspctl(&["schema"]);
    assert!(schema.status.success());
    assert!(schema.stderr.is_empty());
    let schema: Value = serde_json::from_slice(&schema.stdout).unwrap();
    assert_eq!(schema["command"], json!(["schema"]));
    assert_eq!(
        schema["result"]["catalog"]["commands"]
            .as_array()
            .unwrap()
            .len(),
        41
    );

    let root_help = run_lspctl(&["--help"]);
    let named_help = run_lspctl(&["help"]);
    assert_eq!(root_help.stdout, named_help.stdout);
    let help: Value = serde_json::from_slice(&root_help.stdout).unwrap();
    assert_eq!(help["command"], json!(["help"]));

    let group_help = run_lspctl(&["trust", "--help"]);
    assert!(group_help.status.success());
    let group_help: Value = serde_json::from_slice(&group_help.stdout).unwrap();
    assert_eq!(group_help["command"], json!(["help", "trust"]));
    assert_eq!(group_help["result"]["schemas"], json!({}));
    assert_eq!(
        group_help["result"]["catalog"]["commands"]
            .as_array()
            .unwrap()
            .len(),
        5
    );
}

#[test]
fn full_and_focused_schema_results_use_the_frozen_registry() {
    let full = run_lspctl(&["schema", "--full"]);
    assert!(full.status.success());
    let full: Value = serde_json::from_slice(&full.stdout).unwrap();
    assert_eq!(full["result"]["schemas"].as_object().unwrap().len(), 243);
    assert_eq!(
        full["result"]["catalog"]["commands"]
            .as_array()
            .unwrap()
            .len(),
        41
    );

    let focused = run_lspctl(&["schema", "definition"]);
    assert!(focused.status.success());
    let focused: Value = serde_json::from_slice(&focused.stdout).unwrap();
    assert_eq!(focused["command"], json!(["schema", "definition"]));
    let schemas = focused["result"]["schemas"].as_object().unwrap();
    assert!(schemas.contains_key("lspctl://schema/v1/cli/definition"));
    assert!(schemas.contains_key("lspctl://schema/v1/command/definition"));
    assert_eq!(
        schemas["lspctl://schema/v1/output/query-context"]["properties"]["serverProgress"]["items"]
            ["$ref"],
        "lspctl://schema/v1/output/progress-record"
    );

    let invalid = run_lspctl(&["schema", "no-such-subject"]);
    assert_eq!(invalid.status.code(), Some(2));
    let invalid: Value = serde_json::from_slice(&invalid.stdout).unwrap();
    assert_eq!(invalid["error"]["code"], "invalid_arguments");
}

fn run_lspctl(arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_lspctl"))
        .args(arguments)
        .output()
        .unwrap()
}
