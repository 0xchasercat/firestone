//! Turning the config document into the exact child the guest runs.
//!
//! SPEC §10.5 step 6: the child is `entrypoint ++ cmd`, with `env` as its
//! complete environment and `workdir` as its working directory. Nothing here
//! touches the operating system, so the assembly rules are unit-tested on any
//! host.

use std::fmt;

use firestone_initproto::{InitConfig, env_key};

/// A child command ready to be spawned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildPlan {
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub workdir: Option<String>,
}

/// Why a config document does not describe a runnable child.
#[derive(Debug, PartialEq, Eq)]
pub enum PlanError {
    /// `entrypoint` and `cmd` are both empty.
    NoCommand,
    /// The first argument is an empty string.
    EmptyProgram,
}

impl fmt::Display for PlanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoCommand => formatter
                .write_str("image config has an empty entrypoint and an empty cmd; nothing to run"),
            Self::EmptyProgram => {
                formatter.write_str("image config starts with an empty program name")
            }
        }
    }
}

impl std::error::Error for PlanError {}

/// Concatenates `entrypoint` and `cmd` into one argv.
pub fn child_argv(entrypoint: &[String], cmd: &[String]) -> Result<Vec<String>, PlanError> {
    let argv = entrypoint
        .iter()
        .chain(cmd.iter())
        .cloned()
        .collect::<Vec<_>>();
    match argv.first() {
        None => Err(PlanError::NoCommand),
        Some(program) if program.is_empty() => Err(PlanError::EmptyProgram),
        Some(_) => Ok(argv),
    }
}

/// Splits the config `env` array into the pairs `execve` wants.
///
/// A bare entry with no `=` becomes a variable with an empty value; a later
/// duplicate wins, which is the same rule the host-side merge applies.
#[must_use]
pub fn child_env(env: &[String]) -> Vec<(String, String)> {
    let mut pairs: Vec<(String, String)> = Vec::with_capacity(env.len());
    for entry in env {
        let key = env_key(entry).to_owned();
        let value = entry
            .split_once('=')
            .map_or_else(String::new, |(_, value)| value.to_owned());
        match pairs.iter_mut().find(|(existing, _)| *existing == key) {
            Some(slot) => slot.1 = value,
            None => pairs.push((key, value)),
        }
    }
    pairs
}

/// Builds the complete child plan from one config document.
pub fn plan_child(config: &InitConfig) -> Result<ChildPlan, PlanError> {
    let mut argv = child_argv(&config.entrypoint, &config.cmd)?;
    let program = argv.remove(0);
    Ok(ChildPlan {
        program,
        args: argv,
        env: child_env(&config.env),
        workdir: config
            .workdir
            .as_deref()
            .filter(|workdir| !workdir.is_empty())
            .map(str::to_owned),
    })
}

#[cfg(test)]
mod tests {
    use firestone_initproto::{InitConfig, InitNetwork};

    use super::{ChildPlan, PlanError, child_argv, child_env, plan_child};

    fn config(
        entrypoint: &[&str],
        cmd: &[&str],
        env: &[&str],
        workdir: Option<&str>,
    ) -> InitConfig {
        InitConfig {
            hostname: "app".to_owned(),
            entrypoint: entrypoint.iter().map(|value| (*value).to_owned()).collect(),
            cmd: cmd.iter().map(|value| (*value).to_owned()).collect(),
            env: env.iter().map(|value| (*value).to_owned()).collect(),
            workdir: workdir.map(str::to_owned),
            user: None,
            network: InitNetwork::None,
            disk_size_bytes: 4096,
        }
    }

    #[test]
    fn child_argv_appends_cmd_to_entrypoint() -> Result<(), PlanError> {
        let entrypoint = vec!["/docker-entrypoint.sh".to_owned()];
        let cmd = vec!["nginx".to_owned(), "-g".to_owned()];

        assert_eq!(
            child_argv(&entrypoint, &cmd)?,
            vec![
                "/docker-entrypoint.sh".to_owned(),
                "nginx".to_owned(),
                "-g".to_owned()
            ]
        );
        Ok(())
    }

    #[test]
    fn child_argv_cmd_only_image_runs_cmd() -> Result<(), PlanError> {
        assert_eq!(
            child_argv(&[], &["/bin/sh".to_owned()])?,
            vec!["/bin/sh".to_owned()]
        );
        Ok(())
    }

    #[test]
    fn child_argv_without_entrypoint_or_cmd_is_refused() {
        assert_eq!(child_argv(&[], &[]), Err(PlanError::NoCommand));
    }

    #[test]
    fn child_argv_empty_program_is_refused() {
        assert_eq!(
            child_argv(&[String::new()], &["x".to_owned()]),
            Err(PlanError::EmptyProgram)
        );
    }

    #[test]
    fn child_env_splits_pairs_and_keeps_bare_keys() {
        assert_eq!(
            child_env(&[
                "PATH=/bin:/usr/bin".to_owned(),
                "EMPTY=".to_owned(),
                "BARE".to_owned(),
                "WITH=an=equals".to_owned(),
            ]),
            vec![
                ("PATH".to_owned(), "/bin:/usr/bin".to_owned()),
                ("EMPTY".to_owned(), String::new()),
                ("BARE".to_owned(), String::new()),
                ("WITH".to_owned(), "an=equals".to_owned()),
            ]
        );
    }

    #[test]
    fn child_env_last_duplicate_wins_in_place() {
        assert_eq!(
            child_env(&["A=1".to_owned(), "B=1".to_owned(), "A=2".to_owned()]),
            vec![
                ("A".to_owned(), "2".to_owned()),
                ("B".to_owned(), "1".to_owned()),
            ]
        );
    }

    #[test]
    fn plan_child_splits_program_from_arguments() -> Result<(), PlanError> {
        let plan = plan_child(&config(
            &["/docker-entrypoint.sh"],
            &["nginx", "-g", "daemon off;"],
            &["PATH=/bin"],
            Some("/srv"),
        ))?;

        assert_eq!(
            plan,
            ChildPlan {
                program: "/docker-entrypoint.sh".to_owned(),
                args: vec![
                    "nginx".to_owned(),
                    "-g".to_owned(),
                    "daemon off;".to_owned()
                ],
                env: vec![("PATH".to_owned(), "/bin".to_owned())],
                workdir: Some("/srv".to_owned()),
            }
        );
        Ok(())
    }

    #[test]
    fn plan_child_empty_workdir_is_treated_as_absent() -> Result<(), PlanError> {
        let plan = plan_child(&config(&["/bin/sh"], &[], &[], Some("")))?;

        assert_eq!(plan.workdir, None);
        Ok(())
    }
}
