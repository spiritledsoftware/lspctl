use std::{fs, process::Command};

use serde_json::{Value, json};

#[test]
fn local_install_is_managed_idempotent_and_replace_requires_consent() {
    let workspace = tempfile::tempdir().unwrap();
    let installed = run(workspace.path(), &[]);
    assert!(installed.status.success());
    assert!(installed.stderr.is_empty());
    let installed: Value = serde_json::from_slice(&installed.stdout).unwrap();
    assert_eq!(installed["command"], json!(["skill", "install"]));
    assert_eq!(installed["result"]["scope"], "local");
    assert_eq!(installed["result"]["outcome"], "installed");
    assert_eq!(installed["result"]["skillVersion"], "0.1.2");
    assert_eq!(installed["result"]["previousDigest"], Value::Null);
    let destination = workspace.path().join(".agent/skills/lspctl");
    let marker: Value =
        serde_json::from_slice(&fs::read(destination.join(".lspctl-managed.json")).unwrap())
            .unwrap();
    assert_eq!(marker["formatVersion"], 1);
    assert_eq!(marker["manager"], "lspctl");
    assert_eq!(marker["digest"], installed["result"]["digest"]);

    let unchanged: Value = serde_json::from_slice(&run(workspace.path(), &[]).stdout).unwrap();
    assert_eq!(unchanged["result"]["outcome"], "unchanged");

    fs::write(destination.join("SKILL.md"), "human edit").unwrap();
    let refused = run(workspace.path(), &[]);
    assert_eq!(refused.status.code(), Some(3));
    let refused: Value = serde_json::from_slice(&refused.stdout).unwrap();
    assert_eq!(refused["error"]["code"], "skill_install_conflict");
    assert_eq!(refused["error"]["data"]["replaceRequired"], true);
    assert_eq!(
        fs::read_to_string(destination.join("SKILL.md")).unwrap(),
        "human edit"
    );

    let replaced: Value =
        serde_json::from_slice(&run(workspace.path(), &["--replace"]).stdout).unwrap();
    assert_eq!(replaced["result"]["outcome"], "replaced");
    assert_ne!(
        fs::read_to_string(destination.join("SKILL.md")).unwrap(),
        "human edit"
    );
}

#[test]
fn local_recovery_preserves_unmanaged_content_without_replace() {
    for after_install in [false, true] {
        let workspace = tempfile::tempdir().unwrap();
        let installed = run(workspace.path(), &[]);
        assert!(installed.status.success());
        let installed: Value = serde_json::from_slice(&installed.stdout).unwrap();
        let destination =
            std::path::PathBuf::from(installed["result"]["resolvedPath"].as_str().unwrap());
        let parent = destination.parent().unwrap();
        let id = "0123456789abcdef0123456789abcdef";
        let stage = parent.join(format!(".lspctl-stage-{id}"));
        let backup = parent.join(format!(".lspctl-backup-.lspctl-stage-{id}"));
        let path = parent.join(format!(".lspctl-journal-{id}.json"));
        let unmanaged = if after_install {
            &backup
        } else {
            fs::rename(&destination, &stage).unwrap();
            &destination
        };
        fs::create_dir(unmanaged).unwrap();
        fs::write(unmanaged.join("custom"), b"human content").unwrap();
        let journal = serde_json::to_vec(&json!({
            "formatVersion": 1,
            "destination": destination,
            "stage": stage,
            "backup": backup,
            "digest": installed["result"]["digest"],
            "outcome": "replaced",
            "previousSkillVersion": null,
            "previousDigest": null
        }))
        .unwrap();
        fs::write(&path, &journal).unwrap();

        let refused = run(workspace.path(), &[]);
        assert_eq!(refused.status.code(), Some(3));
        assert!(refused.stderr.is_empty());
        let refused: Value = serde_json::from_slice(&refused.stdout).unwrap();
        assert_eq!(refused["ok"], false);
        assert_eq!(refused["error"]["code"], "skill_install_conflict");
        assert_eq!(refused["error"]["data"]["replaceRequired"], true);
        assert_eq!(
            fs::read(unmanaged.join("custom")).unwrap(),
            b"human content"
        );
        assert_eq!(fs::read(&path).unwrap(), journal);
        assert_eq!(stage.is_dir(), !after_install);

        let replaced = run(workspace.path(), &["--replace"]);
        assert!(replaced.status.success());
        assert!(replaced.stderr.is_empty());
        let replaced: Value = serde_json::from_slice(&replaced.stdout).unwrap();
        assert_eq!(replaced["result"]["outcome"], "replaced");
        assert!(destination.join("SKILL.md").is_file());
        assert!(!destination.join("custom").exists());
        assert!(!stage.exists());
        assert!(!backup.exists());
        assert!(!path.exists());
    }
}

#[cfg(unix)]
#[test]
fn global_install_uses_the_home_agent_directory() {
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_lspctl"))
        .args(["skill", "install", "--global"])
        .current_dir(workspace.path())
        .env("HOME", home.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    let envelope: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(envelope["result"]["scope"], "global");
    assert_eq!(
        envelope["result"]["resolvedPath"],
        dunce::canonicalize(home.path())
            .unwrap()
            .join(".agent/skills/lspctl")
            .to_str()
            .unwrap()
    );
    assert!(home.path().join(".agent/skills/lspctl/SKILL.md").is_file());
}

fn run(workspace: &std::path::Path, extra: &[&str]) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_lspctl"));
    command
        .args(["skill", "install"])
        .args(extra)
        .current_dir(workspace);
    command.output().unwrap()
}
