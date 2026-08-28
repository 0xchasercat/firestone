mod cli;
mod render;
mod store;

use std::{env, ffi::OsStr, fs, io, io::IsTerminal, path::Path, process::ExitCode};

use clap::{Parser, error::ErrorKind as ClapErrorKind};
use firestone_core::{
    Action, Catalog, Dispatcher, ErrorKind, Event, EventSink, FirestoneError, GlobalConfig, Level,
    MachineSpec, PathInputs, Paths, RealValidationHost, ValidationContext,
};

use crate::{
    cli::{Cli, Command, CreateRequest},
    render::{RenderOptions, Renderer, error_exit_code},
    store::LocalDispatcher,
};

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let arguments = env::args_os().collect::<Vec<_>>();
    let requested_json = requested_flag(&arguments, "--json");
    let requested_quiet = requested_flag(&arguments, "--quiet") || requested_flag(&arguments, "-q");
    let requested_no_color = requested_flag(&arguments, "--no-color");
    let cli = match Cli::try_parse_from(arguments) {
        Ok(cli) => cli,
        Err(error)
            if matches!(
                error.kind(),
                ClapErrorKind::DisplayHelp | ClapErrorKind::DisplayVersion
            ) =>
        {
            let _ = error.print();
            return ExitCode::SUCCESS;
        }
        Err(error) => {
            let stdout = io::stdout();
            let stderr = io::stderr();
            let options = render_options(
                requested_json,
                requested_quiet,
                requested_no_color,
                0,
                stderr.is_terminal(),
            );
            let mut renderer = Renderer::new(stdout, stderr, options);
            let error = FirestoneError::new(ErrorKind::Usage, clap_error_message(&error))
                .with_hint("run `firestone --help` for command usage");
            let exit = error_exit_code(&error);
            let _ = renderer.render_error(&error);
            return ExitCode::from(exit);
        }
    };

    let stdout = io::stdout();
    let stderr = io::stderr();
    let options = render_options(
        cli.json,
        cli.quiet,
        cli.no_color,
        cli.verbose,
        stderr.is_terminal(),
    );
    let mut renderer = Renderer::new(stdout, stderr, options);
    match run(cli, &mut renderer).await {
        Ok(()) => renderer
            .exit_override()
            .map_or(ExitCode::SUCCESS, ExitCode::from),
        Err(error) => {
            let exit = error_exit_code(&error);
            let _ = renderer.render_error(&error);
            ExitCode::from(exit)
        }
    }
}

fn render_options(
    json: bool,
    quiet: bool,
    _no_color: bool,
    verbosity: u8,
    stderr_is_terminal: bool,
) -> RenderOptions {
    if json {
        RenderOptions::json()
    } else {
        RenderOptions::human(quiet, stderr_is_terminal).with_verbosity(verbosity)
    }
}
fn requested_flag(arguments: &[std::ffi::OsString], flag: &str) -> bool {
    arguments
        .iter()
        .any(|argument| argument == OsStr::new(flag))
}

fn clap_error_message(error: &clap::Error) -> String {
    let rendered = error.to_string();
    let trimmed = rendered.trim();
    trimmed
        .strip_prefix("error: ")
        .unwrap_or(trimmed)
        .to_owned()
}

async fn run(
    cli: Cli,
    renderer: &mut Renderer<io::Stdout, io::Stderr>,
) -> Result<(), FirestoneError> {
    let Cli {
        json: _,
        quiet: _,
        verbose: _,
        no_color: _,
        yes: _,
        home,
        command,
    } = cli;
    let mut inputs = PathInputs::capture()?;
    if let Some(home) = home {
        inputs.firestone_home = Some(home);
    }
    let current_dir = inputs.current_dir.clone();
    let paths = Paths::from_inputs(&inputs)?;
    let global = load_global_config(&paths)?;
    let catalog = Catalog::load(&paths.catalog_file(), &global.images.catalog)?;
    let dispatcher = LocalDispatcher::new(paths.clone(), global.clone(), catalog.clone());

    match command {
        Command::Create(arguments) => {
            let request = arguments.into_request().map_err(|error| {
                FirestoneError::new(ErrorKind::Usage, clap_error_message(&error))
                    .with_hint("run `firestone create --help` for valid forms")
            })?;
            let edit = request.edit;
            let (name, loaded) =
                load_create_spec(request, &current_dir, &paths, &global, &catalog)?;
            for warning in &loaded.warnings {
                renderer.emit(Event::Log {
                    level: Level::Warn,
                    message: format!("{}: {}", warning.key, warning.message),
                })?;
            }
            if edit {
                dispatcher.create_with_edit(&name, loaded.spec, renderer)
            } else {
                dispatcher
                    .run(
                        Action::Create {
                            name,
                            spec: loaded.spec,
                        },
                        renderer,
                    )
                    .await
            }
        }
        Command::List => dispatcher.run(Action::List, renderer).await,
        Command::Show(arguments) => {
            if arguments.vmconfig {
                return Err(FirestoneError::new(
                    ErrorKind::Usage,
                    "--vmconfig is unavailable before the M1 VMM configuration implementation",
                )
                .with_hint("omit --vmconfig to show the machine spec and state"));
            }
            dispatcher
                .run(
                    Action::Show {
                        name: arguments.name,
                    },
                    renderer,
                )
                .await
        }
        Command::Edit(arguments) => dispatcher.edit(&arguments.name, renderer),
        Command::Doctor(arguments) => {
            dispatcher
                .run(Action::Doctor { fix: arguments.fix }, renderer)
                .await
        }
    }
}

fn load_create_spec(
    request: CreateRequest,
    current_dir: &Path,
    paths: &Paths,
    global: &GlobalConfig,
    catalog: &Catalog,
) -> Result<(String, firestone_core::LoadedMachineSpec), FirestoneError> {
    let CreateRequest {
        name,
        image,
        mut patch,
        file,
        edit: _,
    } = request;
    patch.image = Some(image);
    let machine_dir = paths.machine_dir(&name)?;
    let source = match file {
        Some(path) => read_utf8_file(&path, "create spec")?,
        None => String::new(),
    };
    let host = RealValidationHost::new();
    let loaded = MachineSpec::load(
        &source,
        global,
        &patch,
        current_dir,
        &ValidationContext {
            host: &host,
            paths,
            machine_dir: &machine_dir,
            catalog,
            base_image_virtual_size: None,
        },
    )?;
    Ok((name, loaded))
}

fn load_global_config(paths: &Paths) -> Result<GlobalConfig, FirestoneError> {
    let path = paths.config_file();
    match fs::read_to_string(&path) {
        Ok(source) => GlobalConfig::from_toml(&source),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(GlobalConfig::default()),
        Err(source) => Err(FirestoneError::new(
            ErrorKind::InvalidSpec,
            format!("cannot read global config {}", path.display()),
        )
        .with_hint("check config.toml permissions or remove the unreadable file")
        .with_source(source)),
    }
}

fn read_utf8_file(path: &Path, label: &str) -> Result<String, FirestoneError> {
    let bytes = fs::read(path).map_err(|source| {
        FirestoneError::new(
            ErrorKind::InvalidSpec,
            format!("cannot read {label} {}", path.display()),
        )
        .with_hint("check that the file exists and is readable")
        .with_source(source)
    })?;
    String::from_utf8(bytes).map_err(|source| {
        FirestoneError::new(
            ErrorKind::InvalidSpec,
            format!("{label} {} is not UTF-8", path.display()),
        )
        .with_hint("save the file as UTF-8 TOML")
        .with_source(source)
    })
}

#[cfg(test)]
mod tests {
    use firestone_core::{Catalog, GlobalConfig, ImageRef, MachineSpecPatch, PathInputs, Paths};

    use super::{CreateRequest, load_create_spec, load_global_config};

    fn paths() -> Result<(tempfile::TempDir, Paths), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let root = std::fs::canonicalize(directory.path())?;
        let paths = Paths::from_inputs(&PathInputs {
            current_dir: root.clone(),
            home_dir: Some(root.clone()),
            firestone_home: Some(root.join("home")),
            firestone_config_dir: None,
            firestone_data_dir: None,
            firestone_runtime_dir: None,
            xdg_config_home: None,
            xdg_data_home: None,
            xdg_runtime_dir: None,
            uid: 1000,
        })?;
        Ok((directory, paths))
    }

    #[test]
    fn create_request_layers_image_and_cli_patch_without_kvm()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_directory, paths) = paths()?;
        let current_dir = paths.data_dir().parent().ok_or("data path has no parent")?;
        let global = GlobalConfig::default();
        let catalog = Catalog::built_in()?;
        let patch = MachineSpecPatch {
            cpus: Some(4),
            ..MachineSpecPatch::default()
        };

        let (name, loaded) = load_create_spec(
            CreateRequest {
                name: "ubuntu".to_owned(),
                image: ImageRef::new("ubuntu"),
                patch,
                file: None,
                edit: false,
            },
            current_dir,
            &paths,
            &global,
            &catalog,
        )?;

        assert_eq!(name, "ubuntu");
        assert_eq!(loaded.spec.image.as_str(), "ubuntu");
        assert_eq!(loaded.spec.cpus, 4);
        Ok(())
    }

    #[test]
    fn create_cli_patch_resolves_relative_paths_from_captured_directory()
    -> Result<(), Box<dyn std::error::Error>> {
        let (directory, paths) = paths()?;
        let current_dir = std::fs::canonicalize(directory.path())?;
        std::fs::write(current_dir.join("user.yaml"), b"#cloud-config\n")?;
        let patch = MachineSpecPatch {
            cloud_init: Some(firestone_core::CloudInitSpecPatch {
                user_data: Some(std::path::PathBuf::from("user.yaml")),
                ..firestone_core::CloudInitSpecPatch::default()
            }),
            ..MachineSpecPatch::default()
        };

        let (_, loaded) = load_create_spec(
            CreateRequest {
                name: "from-cli".to_owned(),
                image: ImageRef::new("ubuntu"),
                patch,
                file: None,
                edit: false,
            },
            &current_dir,
            &paths,
            &GlobalConfig::default(),
            &Catalog::built_in()?,
        )?;

        assert_eq!(
            loaded.spec.cloud_init.user_data,
            Some(current_dir.join("user.yaml"))
        );
        Ok(())
    }

    #[test]
    fn missing_global_config_uses_documented_defaults() -> Result<(), Box<dyn std::error::Error>> {
        let (_directory, paths) = paths()?;

        let global = load_global_config(&paths)?;

        assert_eq!(global, GlobalConfig::default());
        Ok(())
    }
}
