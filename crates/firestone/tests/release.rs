use std::{
    env,
    error::Error,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use firestone_core::Event;
use serde_json::json;

type TestResult = Result<(), Box<dyn Error>>;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn command() -> Command {
    Command::new(env!("CARGO_BIN_EXE_firestone"))
}

fn protected_temp_root() -> Result<(tempfile::TempDir, PathBuf), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let root = fs::canonicalize(directory.path())?;
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
    Ok((directory, root))
}

fn architecture() -> &'static str {
    std::env::consts::ARCH
}

fn dependency_pins() -> Vec<(&'static str, &'static str, &'static str)> {
    match architecture() {
        "x86_64" => vec![
            (
                "cloud-hypervisor",
                "v53.0",
                "448af3d4e59b22c2987f7df94c213ad40fb53a10d437e42b5ee6c4fce7c29ecc",
            ),
            (
                "cloud-hypervisor-edk2",
                "ch-1e1b96f126",
                "9fb511fc0dd423d90a79615a90a8ace9b9e078b4a115ea2c459e0ac2f4e60218",
            ),
            (
                "passt",
                "2025_02_17.a1e48a0",
                "40e59201765c60a0a5bbd0f2caae1aae3fd8f9a9a0628a835159fb2f17ff7025",
            ),
            (
                "qemu-img",
                "8.2.2",
                "30bff329fe1001635cafcfebddc68a1c824d25110c66f968b428c4cf4785d75d",
            ),
            (
                "rust-hypervisor-firmware",
                "0.5.0",
                "4a0a1e977368f6b15d2198a216bdedf9a350bf5e5ae07e29e695373ec16ad958",
            ),
            (
                "virtiofsd",
                "v1.14.0",
                "9ad3e33c45dd816b24ad483b60ca469974ba54c3b37ef93be3da2a623986646f",
            ),
        ],
        "aarch64" => vec![
            (
                "cloud-hypervisor",
                "v53.0",
                "f192b510eea1c710cbc439d716bb0573c223fc463dbe3e6523788a2b7ef62850",
            ),
            (
                "cloud-hypervisor-edk2",
                "ch-1e1b96f126",
                "460cefa75c72461745ac2f8e828ac8646475f93823101980dfc3f5967175c1ef",
            ),
            (
                "rust-hypervisor-firmware",
                "0.5.0",
                "2a22aed888572ae319e231b85a7b4de951c7eca8857730300653512d064c8102",
            ),
            (
                "virtiofsd",
                "v1.14.0",
                "e45bd62e346eca87857279d5680782e80148379fbca524a648089f642ac001d2",
            ),
        ],
        other => panic!("unsupported test architecture {other}"),
    }
}

fn expected_human_version(home: &Path) -> String {
    let git_commit = option_env!("FIRESTONE_GIT_COMMIT").unwrap_or("not embedded");
    let mut output = format!(
        "firestone {VERSION}\nrelease: v{VERSION}\ngit commit: {git_commit}\narchitecture: {}\ndependencies:\n",
        architecture()
    );
    for (name, version, sha256) in dependency_pins() {
        output.push_str(&format!("  {name}: {version} (sha256 {sha256})\n"));
    }
    output.push_str(&format!(
        "paths:\n  config: {}\n  data: {}\n  runtime: {}\n",
        home.join("config").display(),
        home.join("data").display(),
        home.join("run").display()
    ));
    output
}

fn expected_json_version(home: &Path) -> Result<Vec<u8>, serde_json::Error> {
    let mut dependencies = serde_json::Map::new();
    for (name, version, sha256) in dependency_pins() {
        dependencies.insert(
            name.to_owned(),
            json!({"version": version, "sha256": sha256}),
        );
    }
    let payload = json!({
        "version": VERSION,
        "identity": {
            "release": format!("v{VERSION}"),
            "git_commit": option_env!("FIRESTONE_GIT_COMMIT"),
        },
        "architecture": architecture(),
        "dependencies": dependencies,
        "paths": {
            "config": home.join("config").display().to_string(),
            "data": home.join("data").display().to_string(),
            "runtime": home.join("run").display().to_string(),
        },
    });
    let event = Event::Result {
        action: "version".to_owned(),
        payload,
    };
    let mut bytes = serde_json::to_vec(&event)?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "command failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn version_short_and_detailed_human_output_are_exact() -> TestResult {
    let (_directory, root) = protected_temp_root()?;
    let home = root.join("firestone-home");

    let short = command().arg("--version").output()?;
    assert_success(&short);
    assert_eq!(short.stdout, format!("firestone {VERSION}\n").as_bytes());

    let detailed = command()
        .args([
            "--home",
            home.to_str().ok_or("home path is not UTF-8")?,
            "version",
        ])
        .output()?;
    assert_success(&detailed);
    let expected = expected_human_version(&home);
    assert_eq!(detailed.stdout, expected.as_bytes());

    for output_flag in ["--quiet", "--verbose"] {
        let output = command()
            .args([
                output_flag,
                "--home",
                home.to_str().ok_or("home path is not UTF-8")?,
                "version",
            ])
            .output()?;
        assert_success(&output);
        assert_eq!(
            output.stdout,
            expected.as_bytes(),
            "{output_flag} changed version output"
        );
    }
    Ok(())
}

#[test]
fn version_json_result_is_exact_and_ignores_nonreproducible_environment() -> TestResult {
    let (_directory, root) = protected_temp_root()?;
    let home = root.join("firestone-home");
    let other_cwd = root.join("other");
    fs::create_dir(&other_cwd)?;
    let home_text = home.to_str().ok_or("home path is not UTF-8")?;

    let first = command()
        .args(["--json", "--home", home_text, "version"])
        .env("HOSTNAME", "first-builder")
        .env("SOURCE_DATE_EPOCH", "1")
        .env("TZ", "Pacific/Honolulu")
        .current_dir(&root)
        .output()?;
    assert_success(&first);
    assert_eq!(first.stdout, expected_json_version(&home)?);

    let second = command()
        .args(["--json", "--home", home_text, "version"])
        .env("HOSTNAME", "second-builder")
        .env("SOURCE_DATE_EPOCH", "9999999999")
        .env("TZ", "UTC")
        .current_dir(other_cwd)
        .output()?;
    assert_success(&second);
    assert_eq!(second.stdout, first.stdout);
    Ok(())
}

#[test]
fn completions_stdout_matches_all_shell_snapshots() -> TestResult {
    let snapshots: [(&str, &[u8]); 5] = [
        ("bash", include_bytes!("snapshots/completions.bash")),
        ("elvish", include_bytes!("snapshots/completions.elvish")),
        ("fish", include_bytes!("snapshots/completions.fish")),
        (
            "powershell",
            include_bytes!("snapshots/completions.powershell"),
        ),
        ("zsh", include_bytes!("snapshots/completions.zsh")),
    ];

    for (shell, expected) in snapshots {
        let first = command().args(["completions", shell]).output()?;
        assert_success(&first);
        assert_eq!(
            first.stdout, expected,
            "{shell} completion snapshot changed"
        );
        assert!(
            !first
                .stdout
                .windows(b"_shim".len())
                .any(|bytes| bytes == b"_shim")
        );
        assert!(
            !first
                .stdout
                .windows(b"_vsock-proxy".len())
                .any(|bytes| bytes == b"_vsock-proxy")
        );

        let second = command().args(["completions", shell]).output()?;
        assert_success(&second);
        assert_eq!(
            second.stdout, first.stdout,
            "{shell} completion was not deterministic"
        );
    }
    Ok(())
}

#[test]
fn completions_reject_incompatible_output_controls() -> TestResult {
    let json_output = command().args(["--json", "completions", "bash"]).output()?;
    assert_eq!(json_output.status.code(), Some(2));
    assert!(json_output.stderr.is_empty());
    assert_eq!(
        json_output.stdout,
        b"{\"error\":{\"kind\":\"usage\",\"message\":\"--json is not valid with firestone completions\",\"hint\":\"remove output-control flags; completions writes only the shell script to stdout\"}}\n"
    );

    for (output_flag, canonical_flag) in [
        ("--quiet", "--quiet"),
        ("-q", "--quiet"),
        ("--verbose", "--verbose"),
        ("-vv", "--verbose"),
    ] {
        let output = command()
            .args([output_flag, "completions", "bash"])
            .output()?;
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
        let stderr = String::from_utf8(output.stderr)?;
        assert!(stderr.contains(&format!(
            "{canonical_flag} is not valid with firestone completions"
        )));
        assert!(stderr.contains(
            "hint:  remove output-control flags; completions writes only the shell script to stdout"
        ));
    }
    Ok(())
}
