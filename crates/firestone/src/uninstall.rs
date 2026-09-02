//! `firestone uninstall`: remove the executable, and with `--purge` the data.
//!
//! Uninstall is a CLI-only command (SPEC §5.2, §15.1). It removes files that
//! belong to this installation and nothing else: a binary a package manager
//! owns is refused rather than deleted behind the package manager's back, and
//! the default run keeps every machine and image where it is.

use std::path::{Path, PathBuf};

use firestone_core::{ErrorKind, FirestoneError, Paths};
use serde::{Deserialize, Serialize};

/// Prefixes a package manager owns. Firestone never writes here.
const SYSTEM_PREFIXES: [&str; 2] = ["/usr", "/opt"];

/// What one uninstall would remove and what it would keep.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UninstallPlan {
    /// The executable running this command.
    pub executable: PathBuf,
    /// Directories `--purge` deletes, in removal order. Empty without it.
    pub directories: Vec<UninstallDirectory>,
    /// Directories a default run keeps, with what they hold.
    pub kept: Vec<UninstallDirectory>,
}

/// One Firestone-owned directory named in a plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UninstallDirectory {
    pub path: PathBuf,
    /// What a reader loses by deleting it, in plain words.
    pub holds: &'static str,
}

/// What one uninstall actually did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UninstallResult {
    pub removed: Vec<String>,
    pub kept: Vec<UninstallKept>,
}

/// One path an uninstall left alone, and what it holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UninstallKept {
    pub path: String,
    pub holds: String,
}

/// Plans an uninstall from this process's own executable path.
///
/// # Errors
///
/// Returns `usage` when the executable lives under a system prefix or in a
/// directory this user cannot write, because either one means the file belongs
/// to something other than this installation.
pub fn plan(paths: &Paths, purge: bool) -> Result<UninstallPlan, FirestoneError> {
    let executable = std::env::current_exe().map_err(|source| {
        FirestoneError::new(
            ErrorKind::Generic,
            "cannot resolve the firestone executable path",
        )
        .with_hint("remove the binary by hand")
        .with_source(source)
    })?;
    refuse_system_prefix(&executable)?;
    refuse_unwritable(&executable)?;

    let directories = firestone_directories(paths);
    Ok(if purge {
        UninstallPlan {
            executable,
            directories,
            kept: Vec::new(),
        }
    } else {
        UninstallPlan {
            executable,
            directories: Vec::new(),
            kept: directories,
        }
    })
}

/// The Firestone-owned directories, deduplicated and existing only.
fn firestone_directories(paths: &Paths) -> Vec<UninstallDirectory> {
    let candidates = [
        (paths.data_dir(), "machines, images, and helper binaries"),
        (paths.config_dir(), "settings and the user catalog"),
        (paths.runtime_dir(), "sockets and logs of running machines"),
    ];
    let mut directories: Vec<UninstallDirectory> = Vec::new();
    for (path, holds) in candidates {
        if !path.exists() {
            continue;
        }
        if directories
            .iter()
            .any(|directory| directory.path.as_path() == path)
        {
            continue;
        }
        directories.push(UninstallDirectory {
            path: path.to_path_buf(),
            holds,
        });
    }
    directories
}

fn refuse_system_prefix(executable: &Path) -> Result<(), FirestoneError> {
    let Some(prefix) = SYSTEM_PREFIXES
        .into_iter()
        .find(|prefix| executable.starts_with(prefix))
    else {
        return Ok(());
    };
    Err(FirestoneError::new(
        ErrorKind::Usage,
        format!(
            "{} is under {prefix}, so a package manager installed it",
            executable.display()
        ),
    )
    .with_hint("remove it with the package manager that installed it"))
}

fn refuse_unwritable(executable: &Path) -> Result<(), FirestoneError> {
    use nix::unistd::{AccessFlags, access};

    let Some(parent) = executable.parent() else {
        return Err(FirestoneError::new(
            ErrorKind::Usage,
            format!("{} has no parent directory", executable.display()),
        )
        .with_hint("remove the binary by hand"));
    };
    if access(parent, AccessFlags::W_OK).is_ok() {
        return Ok(());
    }
    Err(FirestoneError::new(
        ErrorKind::Usage,
        format!(
            "cannot write {}, so the file cannot be removed",
            parent.display()
        ),
    )
    .with_hint("remove the binary as its owner, or with the tool that installed it"))
}

/// Applies one plan: directories first, then the executable.
///
/// The executable goes last so that a failed directory removal leaves a
/// working `firestone` to retry with.
///
/// # Errors
///
/// Returns the underlying filesystem error, naming the path it could not
/// remove.
pub fn apply(plan: &UninstallPlan) -> Result<UninstallResult, FirestoneError> {
    let mut removed = Vec::new();
    for directory in &plan.directories {
        std::fs::remove_dir_all(&directory.path).map_err(|source| {
            FirestoneError::new(
                ErrorKind::Generic,
                format!("cannot remove {}", directory.path.display()),
            )
            .with_hint("check the directory permissions and remove it by hand")
            .with_source(source)
        })?;
        removed.push(directory.path.display().to_string());
    }
    std::fs::remove_file(&plan.executable).map_err(|source| {
        FirestoneError::new(
            ErrorKind::Generic,
            format!("cannot remove {}", plan.executable.display()),
        )
        .with_hint("check the directory permissions and remove the file by hand")
        .with_source(source)
    })?;
    removed.push(plan.executable.display().to_string());
    Ok(UninstallResult {
        removed,
        kept: plan
            .kept
            .iter()
            .map(|directory| UninstallKept {
                path: directory.path.display().to_string(),
                holds: directory.holds.to_owned(),
            })
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use std::{fs, os::unix::fs::PermissionsExt as _};

    use firestone_core::{ErrorKind, PathInputs, Paths};

    use super::{UninstallDirectory, UninstallPlan, apply, firestone_directories};

    fn test_paths(root: &std::path::Path) -> Result<Paths, Box<dyn std::error::Error>> {
        Ok(Paths::from_inputs(&PathInputs {
            current_dir: root.to_path_buf(),
            home_dir: Some(root.to_path_buf()),
            firestone_home: Some(root.join("home")),
            firestone_config_dir: None,
            firestone_data_dir: None,
            firestone_runtime_dir: None,
            xdg_config_home: None,
            xdg_data_home: None,
            xdg_runtime_dir: None,
            uid: nix::unistd::getuid().as_raw(),
        })?)
    }

    #[test]
    fn plan_refuses_an_executable_under_a_system_prefix() -> Result<(), Box<dyn std::error::Error>>
    {
        for prefix in ["/usr/local/bin/firestone", "/opt/firestone/bin/firestone"] {
            let error = super::refuse_system_prefix(std::path::Path::new(prefix))
                .err()
                .ok_or("a system prefix must be refused")?;
            assert_eq!(error.kind(), ErrorKind::Usage);
            assert!(
                error.hint().unwrap_or_default().contains("package manager"),
                "{error}"
            );
        }
        Ok(())
    }

    #[test]
    fn plan_accepts_a_user_owned_executable_path() -> Result<(), Box<dyn std::error::Error>> {
        super::refuse_system_prefix(std::path::Path::new("/home/reader/.local/bin/firestone"))?;
        Ok(())
    }

    #[test]
    fn plan_refuses_an_executable_in_an_unwritable_directory()
    -> Result<(), Box<dyn std::error::Error>> {
        if nix::unistd::getuid().is_root() {
            return Ok(());
        }
        let directory = tempfile::tempdir()?;
        let bin = directory.path().join("bin");
        fs::create_dir(&bin)?;
        let executable = bin.join("firestone");
        fs::write(&executable, b"#!/bin/sh\n")?;
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o500))?;

        let error = super::refuse_unwritable(&executable)
            .err()
            .ok_or("an unwritable directory must be refused")?;

        assert_eq!(error.kind(), ErrorKind::Usage);
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o700))?;
        Ok(())
    }

    #[test]
    fn directories_skip_what_does_not_exist_and_never_repeat_a_path()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let paths = test_paths(directory.path())?;
        assert!(firestone_directories(&paths).is_empty());

        fs::create_dir_all(paths.data_dir())?;
        fs::create_dir_all(paths.config_dir())?;
        let listed = firestone_directories(&paths);
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].path, paths.data_dir());
        Ok(())
    }

    #[test]
    fn apply_removes_the_purged_directories_and_then_the_executable()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let paths = test_paths(directory.path())?;
        fs::create_dir_all(paths.data_dir())?;
        fs::write(paths.data_dir().join("marker"), b"machine")?;
        let executable = directory.path().join("firestone");
        fs::write(&executable, b"#!/bin/sh\n")?;

        let result = apply(&UninstallPlan {
            executable: executable.clone(),
            directories: vec![UninstallDirectory {
                path: paths.data_dir().to_path_buf(),
                holds: "machines, images, and helper binaries",
            }],
            kept: Vec::new(),
        })?;

        assert_eq!(
            result.removed,
            vec![
                paths.data_dir().display().to_string(),
                executable.display().to_string(),
            ]
        );
        assert!(!paths.data_dir().exists());
        assert!(!executable.exists());
        Ok(())
    }

    #[test]
    fn apply_reports_the_directories_a_default_run_keeps() -> Result<(), Box<dyn std::error::Error>>
    {
        let directory = tempfile::tempdir()?;
        let paths = test_paths(directory.path())?;
        fs::create_dir_all(paths.data_dir())?;
        let executable = directory.path().join("firestone");
        fs::write(&executable, b"#!/bin/sh\n")?;

        let result = apply(&UninstallPlan {
            executable: executable.clone(),
            directories: Vec::new(),
            kept: vec![UninstallDirectory {
                path: paths.data_dir().to_path_buf(),
                holds: "machines, images, and helper binaries",
            }],
        })?;

        assert_eq!(result.removed, vec![executable.display().to_string()]);
        assert_eq!(result.kept.len(), 1);
        assert_eq!(result.kept[0].path, paths.data_dir().display().to_string());
        assert!(paths.data_dir().exists());
        Ok(())
    }
}
