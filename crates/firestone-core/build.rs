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

    ["passt", "qemu-img"]
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
        })
        .collect()
}

fn write_generated(path: &Path, helpers: &[VerifiedHelper]) {
    let mut generated = String::from(
        "const BUILD_EMBEDDED_PASST: Option<EmbeddedHelper> = None;\n\
         const BUILD_EMBEDDED_QEMU_IMG: Option<EmbeddedHelper> = None;\n",
    );
    if !helpers.is_empty() {
        generated.clear();
        for helper in helpers {
            let constant = match helper.dependency.as_str() {
                "passt" => "BUILD_EMBEDDED_PASST",
                "qemu-img" => "BUILD_EMBEDDED_QEMU_IMG",
                other => panic!("unsupported embedded helper {other}"),
            };
            let kind = match helper.dependency.as_str() {
                "passt" => "InternalHelper::Passt",
                "qemu-img" => "InternalHelper::QemuImg",
                _ => unreachable!(),
            };
            generated.push_str(&format!(
                "const {constant}: Option<EmbeddedHelper> = Some(EmbeddedHelper::new({kind}, {version:?}, {install_name:?}, {sha256:?}, include_bytes!({path:?})));\n",
                version = helper.version,
                install_name = helper.install_name,
                sha256 = helper.sha256,
                path = helper.path,
            ));
        }
    }
    fs::write(path, generated)
        .unwrap_or_else(|error| panic!("cannot write {}: {error}", path.display()));
}
