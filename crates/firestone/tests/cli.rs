use std::{error::Error, fs, os::unix::fs::PermissionsExt, process::Command};

type TestResult = Result<(), Box<dyn Error>>;

#[test]
fn invalid_argument_json_requested_emits_structured_usage_error() -> TestResult {
    let output = Command::new(env!("CARGO_BIN_EXE_firestone"))
        .args(["--json", "--bogus"])
        .output()?;

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stderr.is_empty());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(value["error"]["kind"], "usage");
    assert!(
        value["error"]["message"]
            .as_str()
            .is_some_and(|message| { message.contains("unexpected argument '--bogus'") })
    );
    Ok(())
}

#[test]
fn doctor_failed_checks_emit_report_and_dependency_exit() -> TestResult {
    let directory = tempfile::tempdir()?;
    let home = directory.path().join("home");
    let output = Command::new(env!("CARGO_BIN_EXE_firestone"))
        .args([
            "--json",
            "--home",
            home.to_str().ok_or("temporary home is not UTF-8")?,
            "doctor",
        ])
        .output()?;

    assert_eq!(output.status.code(), Some(5));
    assert!(output.stderr.is_empty());
    let records = String::from_utf8(output.stdout)?
        .lines()
        .map(serde_json::from_str::<serde_json::Value>)
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["type"], "Result");
    assert_eq!(records[0]["action"], "doctor");
    assert_eq!(
        records[0]["payload"]["checks"].as_array().map(Vec::len),
        Some(13)
    );
    Ok(())
}

#[test]
fn create_edit_editor_writes_stdout_emits_one_result_and_publishes_atomically() -> TestResult {
    let directory = tempfile::tempdir()?;
    let root = fs::canonicalize(directory.path())?;
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
    let home = root.join("home");
    let editor = root.join("editor.sh");
    fs::write(
        &editor,
        b"#!/bin/sh\n[ \"$1\" = \"--wait\" ] || exit 9\nprintf 'editor-noise\\n'\n",
    )?;
    let mut permissions = fs::metadata(&editor)?.permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(&editor, permissions)?;

    let output = Command::new(env!("CARGO_BIN_EXE_firestone"))
        .args([
            "--json",
            "--home",
            home.to_str().ok_or("temporary home is not UTF-8")?,
            "create",
            "demo",
            "ubuntu:24.04",
            "--edit",
        ])
        .env("VISUAL", format!("sh {} --wait", editor.display()))
        .output()?;

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stderr.contains("editor-noise"));
    let stdout = String::from_utf8(output.stdout)?;
    let records = stdout
        .lines()
        .map(serde_json::from_str::<serde_json::Value>)
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(
        records
            .iter()
            .filter(|record| { record["type"] == "Result" && record["action"] == "create" })
            .count(),
        1
    );

    let machine_dir = home.join("data/machines/demo");
    assert!(machine_dir.join("firestone.toml").is_file());
    assert!(machine_dir.join("state.json").is_file());
    assert!(!machine_dir.join(".creating").exists());
    Ok(())
}
