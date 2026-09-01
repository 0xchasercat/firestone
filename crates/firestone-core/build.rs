use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
};

use serde::Deserialize;
use sha2::{Digest as _, Sha256};

const STRICT_ENV: &str = "FIRESTONE_REQUIRE_EMBEDDED_HELPERS";
const INPUT_DIR_ENV: &str = "FIRESTONE_EMBEDDED_HELPERS_DIR";
const STANDALONE_TARGET: &str = "x86_64-unknown-linux-musl";

#[derive(Deserialize)]
struct Manifest {
    dependency: BTreeMap<String, Dependency>,
}

#[derive(Deserialize)]
struct Dependency {
    version: String,
    x86_64: Option<Artifact>,
}

#[derive(Deserialize)]
struct Artifact {
    asset: String,
    install_name: String,
    sha256: String,
}

struct VerifiedHelper {
    dependency: String,
    version: String,
    install_name: String,
    sha256: String,
    path: PathBuf,
}

fn main() {
    println!("cargo:rerun-if-env-changed={STRICT_ENV}");
    println!("cargo:rerun-if-env-changed={INPUT_DIR_ENV}");

    let manifest_dir = required_env("CARGO_MANIFEST_DIR");
    let target = required_env("TARGET");
    let output_dir = PathBuf::from(required_env("OUT_DIR"));
    let manifest_path = Path::new(&manifest_dir).join("../../deps.toml");
    println!("cargo:rerun-if-changed={}", manifest_path.display());

    let strict = env::var_os(STRICT_ENV).is_some_and(|value| value == "1");
    let input_dir = env::var_os(INPUT_DIR_ENV).map(PathBuf::from);
    if strict && target != STANDALONE_TARGET {
        panic!("{STRICT_ENV}=1 is supported only for {STANDALONE_TARGET}, not {target}");
    }
    if strict && input_dir.is_none() {
        panic!("{STRICT_ENV}=1 requires {INPUT_DIR_ENV}");
    }

    let helpers = match input_dir {
        Some(directory) => {
            if target != STANDALONE_TARGET {
                panic!(
                    "embedded helper inputs are valid only for {STANDALONE_TARGET}, not {target}"
                );
            }
            verify_helpers(&manifest_path, &directory)
        }
        None => Vec::new(),
    };
    write_generated(&output_dir.join("embedded_helpers.rs"), &helpers);
}

/// Helpers that a standalone release must always carry.
const REQUIRED_HELPERS: [&str; 3] = ["cloud-hypervisor", "passt", "qemu-img"];

/// Helpers that a standalone release carries only once they exist.
///
/// SPEC §10.5/§17.2: `firestone-init` is a Firestone-owned build artifact, not
/// a third-party download, and its standalone release is not pinned yet. The
/// seam is here so that the day `deps.toml` gains a `[dependency.firestone-init]`
/// entry and the build directory holds the asset, the payload is hash-verified
/// and embedded exactly like the other three; until then the build stays green
/// and `firestone_init_payload()` reports the missing dependency at runtime.
const OPTIONAL_HELPERS: [&str; 1] = ["firestone-init"];

fn required_env(name: &str) -> String {
    env::var(name).unwrap_or_else(|_| panic!("Cargo did not set {name}"))
}

fn verify_helpers(manifest_path: &Path, input_dir: &Path) -> Vec<VerifiedHelper> {
    let manifest_bytes = fs::read(manifest_path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", manifest_path.display()));
    let manifest_text = std::str::from_utf8(&manifest_bytes)
        .unwrap_or_else(|error| panic!("{} is not UTF-8: {error}", manifest_path.display()));
    let manifest = toml::from_str::<Manifest>(manifest_text)
        .unwrap_or_else(|error| panic!("cannot parse {}: {error}", manifest_path.display()));

    let mut helpers = REQUIRED_HELPERS
        .into_iter()
        .map(|dependency| {
            let entry = manifest
                .dependency
                .get(dependency)
                .unwrap_or_else(|| panic!("deps.toml has no dependency.{dependency} entry"));
            let artifact = entry
                .x86_64
                .as_ref()
                .unwrap_or_else(|| panic!("deps.toml has no dependency.{dependency}.x86_64 entry"));
            verify_one(dependency, entry, artifact, input_dir)
        })
        .collect::<Vec<_>>();

    for dependency in OPTIONAL_HELPERS {
        // Both halves must be present: a `deps.toml` pin and the built asset.
        // A pin with no asset, or an asset with no pin, is a mistake rather
        // than a payload, so each is reported instead of silently ignored.
        let entry = manifest.dependency.get(dependency);
        let artifact = entry.and_then(|entry| entry.x86_64.as_ref());
        let path = artifact.map(|artifact| input_dir.join(&artifact.asset));
        if let Some(path) = &path {
            println!("cargo:rerun-if-changed={}", path.display());
        }
        match (entry, artifact, path) {
            (Some(entry), Some(artifact), Some(path)) => {
                if !path.exists() {
                    panic!(
                        "deps.toml pins dependency.{dependency} but {} is missing",
                        path.display()
                    );
                }
                helpers.push(verify_one(dependency, entry, artifact, input_dir));
            }
            (Some(_), None, _) | (Some(_), Some(_), None) => {
                panic!("deps.toml has no dependency.{dependency}.x86_64 entry");
            }
            (None, _, _) => {}
        }
    }
    helpers
}

fn verify_one(
    dependency: &str,
    entry: &Dependency,
    artifact: &Artifact,
    input_dir: &Path,
) -> VerifiedHelper {
    let path = input_dir.join(&artifact.asset);
    println!("cargo:rerun-if-changed={}", path.display());
    let bytes = fs::read(&path).unwrap_or_else(|error| {
        panic!(
            "cannot read embedded {dependency} at {}: {error}",
            path.display()
        )
    });
    let actual = format!("{:x}", Sha256::digest(&bytes));
    if actual != artifact.sha256 {
        panic!(
            "embedded {dependency} checksum mismatch for {}: expected {}, got {actual}",
            path.display(),
            artifact.sha256
        );
    }
    VerifiedHelper {
        dependency: dependency.to_owned(),
        version: entry.version.clone(),
        install_name: artifact.install_name.clone(),
        sha256: artifact.sha256.clone(),
        path,
    }
}

fn write_generated(path: &Path, helpers: &[VerifiedHelper]) {
    // Every constant is emitted on every build: an absent payload is `None`,
    // never a missing symbol, so `embedded_helper` stays total.
    let mut generated = String::new();
    for dependency in REQUIRED_HELPERS.into_iter().chain(OPTIONAL_HELPERS) {
        let constant = constant_name(dependency);
        match helpers.iter().find(|helper| helper.dependency == dependency) {
            Some(helper) => generated.push_str(&format!(
                "const {constant}: Option<EmbeddedHelper> = Some(EmbeddedHelper::new({kind}, {version:?}, {install_name:?}, {sha256:?}, include_bytes!({path:?})));\n",
                kind = helper_kind(dependency),
                version = helper.version,
                install_name = helper.install_name,
                sha256 = helper.sha256,
                path = helper.path,
            )),
            None => generated.push_str(&format!(
                "const {constant}: Option<EmbeddedHelper> = None;\n"
            )),
        }
    }
    fs::write(path, generated)
        .unwrap_or_else(|error| panic!("cannot write {}: {error}", path.display()));
}

fn constant_name(dependency: &str) -> &'static str {
    match dependency {
        "cloud-hypervisor" => "BUILD_EMBEDDED_CLOUD_HYPERVISOR",
        "passt" => "BUILD_EMBEDDED_PASST",
        "qemu-img" => "BUILD_EMBEDDED_QEMU_IMG",
        "firestone-init" => "BUILD_EMBEDDED_FIRESTONE_INIT",
        other => panic!("unsupported embedded helper {other}"),
    }
}

fn helper_kind(dependency: &str) -> &'static str {
    match dependency {
        "cloud-hypervisor" => "InternalHelper::CloudHypervisor",
        "passt" => "InternalHelper::Passt",
        "qemu-img" => "InternalHelper::QemuImg",
        "firestone-init" => "InternalHelper::FirestoneInit",
        other => panic!("unsupported embedded helper {other}"),
    }
}
