mod cli;
mod render;
mod store;

use std::{
    env,
    ffi::{OsStr, OsString},
    fs,
    future::Future,
    io,
    io::IsTerminal,
    path::Path,
    process::ExitCode,
    sync::Arc,
    task::{Context, Poll, Wake, Waker},
    thread,
};

use clap::{Parser, error::ErrorKind as ClapErrorKind};
use firestone_core::{
    Action, Catalog, Dispatcher, ErrorKind, Event, EventSink, FirestoneError, GlobalConfig,
    ImageRef, Level, MachineSpec, PathInputs, Paths, RealValidationHost, ValidationContext,
};

use crate::{
    cli::{Cli, Command, CreateRequest, ImageCommand},
    render::{RenderOptions, Renderer, error_exit_code},
    store::LocalDispatcher,
};

fn main() -> ExitCode {
    let arguments = env::args_os().collect::<Vec<_>>();
    if let Some(name) = hidden_shim_name(&arguments) {
        let result = Paths::from_process().and_then(|paths| firestone_core::run_shim(&paths, name));
        return match result {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("firestone shim: {}", error.message());
                if let Some(hint) = error.hint() {
                    eprintln!("hint: {hint}");
                }
                ExitCode::FAILURE
            }
        };
    }
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
            return ExitCode::from(render_terminal_error(&mut renderer, &error));
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
    let result = block_on(run(cli, &mut renderer));
    ExitCode::from(finish_command(result, &mut renderer))
}
struct ThreadWake(thread::Thread);

impl Wake for ThreadWake {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
    }
}

fn block_on<F: Future>(future: F) -> F::Output {
    let waker = Waker::from(Arc::new(ThreadWake(thread::current())));
    let mut context = Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(output) => return output,
            Poll::Pending => thread::park(),
        }
    }
}

fn hidden_shim_name(arguments: &[std::ffi::OsString]) -> Option<&str> {
    if arguments.len() != 3
        || arguments.get(1).map(OsString::as_os_str) != Some(OsStr::new("_shim"))
    {
        return None;
    }
    arguments.get(2).and_then(|name| name.to_str())
}

fn render_options(
    json: bool,
    quiet: bool,
    _no_color: bool,
    verbosity: u8,
    stderr_is_terminal: bool,
) -> RenderOptions {
    if json {
        RenderOptions::json().with_quiet(quiet)
    } else {
        RenderOptions::human(quiet, stderr_is_terminal).with_verbosity(verbosity)
    }
}

fn finish_command<Stdout, Stderr>(
    result: Result<(), FirestoneError>,
    renderer: &mut Renderer<Stdout, Stderr>,
) -> u8
where
    Stdout: io::Write,
    Stderr: io::Write,
{
    match result {
        Ok(()) => renderer.exit_override().unwrap_or(0),
        Err(error) if has_broken_pipe_source(&error) => 0,
        Err(error) => render_terminal_error(renderer, &error),
    }
}

fn render_terminal_error<Stdout, Stderr>(
    renderer: &mut Renderer<Stdout, Stderr>,
    error: &FirestoneError,
) -> u8
where
    Stdout: io::Write,
    Stderr: io::Write,
{
    let exit = error_exit_code(error);
    match renderer.render_error(error) {
        Err(output_error) if has_broken_pipe_source(&output_error) => 0,
        Ok(()) | Err(_) => exit,
    }
}

fn has_broken_pipe_source(error: &FirestoneError) -> bool {
    let mut current: Option<&(dyn std::error::Error + 'static)> = Some(error);
    while let Some(cause) = current {
        let io_broken_pipe = cause
            .downcast_ref::<io::Error>()
            .is_some_and(|source| source.kind() == io::ErrorKind::BrokenPipe);
        let json_broken_pipe = cause
            .downcast_ref::<serde_json::Error>()
            .is_some_and(|source| source.io_error_kind() == Some(io::ErrorKind::BrokenPipe));
        if io_broken_pipe || json_broken_pipe {
            return true;
        }
        current = cause.source();
    }
    false
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

async fn run<Stdout, Stderr>(
    cli: Cli,
    renderer: &mut Renderer<Stdout, Stderr>,
) -> Result<(), FirestoneError>
where
    Stdout: io::Write + Send,
    Stderr: io::Write + Send,
{
    let inputs = PathInputs::capture()?;
    let interactive = io::stdin().is_terminal() && io::stderr().is_terminal();
    run_with_inputs_mode(cli, renderer, inputs, interactive).await
}

#[cfg(test)]
async fn run_with_inputs<Stdout, Stderr>(
    cli: Cli,
    renderer: &mut Renderer<Stdout, Stderr>,
    inputs: PathInputs,
) -> Result<(), FirestoneError>
where
    Stdout: io::Write + Send,
    Stderr: io::Write + Send,
{
    run_with_inputs_mode(cli, renderer, inputs, false).await
}

async fn run_with_inputs_mode<Stdout, Stderr>(
    cli: Cli,
    renderer: &mut Renderer<Stdout, Stderr>,
    mut inputs: PathInputs,
    interactive: bool,
) -> Result<(), FirestoneError>
where
    Stdout: io::Write + Send,
    Stderr: io::Write + Send,
{
    let Cli {
        json: _,
        quiet: _,
        verbose: _,
        no_color: _,
        yes,
        home,
        command,
    } = cli;
    if let Some(home) = home {
        inputs.firestone_home = Some(home);
    }
    let paths = Paths::from_inputs(&inputs)?;
    let source_base = inputs.current_dir.clone();

    match command {
        Command::Create(arguments) => {
            let (global, catalog) = load_user_configuration(&paths)?;
            let request = arguments.into_request().map_err(|error| {
                FirestoneError::new(ErrorKind::Usage, clap_error_message(&error))
                    .with_hint("run firestone create --help for valid forms")
            })?;
            let edit = request.edit;
            let (name, loaded) =
                load_create_spec(request, &inputs.current_dir, &paths, &global, &catalog)?;
            for warning in &loaded.warnings {
                renderer.emit(Event::Log {
                    level: Level::Warn,
                    message: format!("{}: {}", warning.key, warning.message),
                })?;
            }
            let dispatcher =
                LocalDispatcher::new(paths, global, catalog).with_source_base(source_base);
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
        Command::Start(arguments) => {
            let (global, catalog) = load_user_configuration(&paths)?;
            let timeout = arguments.timeout.unwrap_or(global.start.timeout).get();
            LocalDispatcher::new(paths, global, catalog)
                .with_source_base(source_base)
                .run(
                    Action::Start {
                        name: arguments.name,
                        wait: !arguments.no_wait,
                        timeout,
                    },
                    renderer,
                )
                .await
        }
        Command::Stop(arguments) => {
            let (global, catalog) = load_user_configuration(&paths)?;
            let timeout = arguments.timeout.unwrap_or(global.stop.timeout).get();
            LocalDispatcher::new(paths, global, catalog)
                .with_source_base(source_base)
                .run(
                    Action::Stop {
                        name: arguments.name,
                        timeout,
                        force: arguments.force,
                    },
                    renderer,
                )
                .await
        }
        Command::Restart(arguments) => {
            let (global, catalog) = load_user_configuration(&paths)?;
            let timeout = global.start.timeout.get();
            LocalDispatcher::new(paths, global, catalog)
                .with_source_base(source_base)
                .run(
                    Action::Restart {
                        name: arguments.name,
                        timeout,
                    },
                    renderer,
                )
                .await
        }
        Command::Remove(arguments) => {
            let (global, catalog) = load_user_configuration(&paths)?;
            let dispatcher =
                LocalDispatcher::new(paths, global, catalog).with_source_base(source_base);
            let mut force = arguments.force || yes;
            if !force && interactive {
                let running = dispatcher.remove_confirmation_names(&arguments.names)?;
                if !running.is_empty() {
                    let prompt =
                        format!("remove running machine(s) {}? [y/N] ", running.join(", "));
                    force = confirm(renderer, &prompt)?;
                    if !force {
                        return Err(FirestoneError::new(
                            ErrorKind::Generic,
                            "machine removal cancelled",
                        ));
                    }
                }
            }
            dispatcher
                .run(
                    Action::Remove {
                        names: arguments.names,
                        force,
                    },
                    renderer,
                )
                .await
        }
        Command::List => {
            let (global, catalog) = load_user_configuration(&paths)?;
            LocalDispatcher::new(paths, global, catalog)
                .with_source_base(source_base)
                .run(Action::List, renderer)
                .await
        }
        Command::Show(arguments) => {
            let (global, catalog) = load_user_configuration(&paths)?;
            LocalDispatcher::new(paths, global, catalog)
                .with_source_base(source_base)
                .run(
                    Action::Show {
                        name: arguments.name,
                        vmconfig: arguments.vmconfig,
                    },
                    renderer,
                )
                .await
        }
        Command::Edit(arguments) => {
            let (global, catalog) = load_user_configuration(&paths)?;
            LocalDispatcher::new(paths, global, catalog)
                .with_source_base(source_base)
                .edit(&arguments.name, renderer)
        }
        Command::Logs(arguments) => {
            let (global, catalog) = load_user_configuration(&paths)?;
            LocalDispatcher::new(paths, global, catalog)
                .with_source_base(source_base)
                .run(
                    Action::Logs {
                        name: arguments.name,
                        source: arguments.source,
                        lines: arguments.lines,
                        follow: arguments.follow,
                    },
                    renderer,
                )
                .await
        }
        Command::Images(arguments) => {
            let (global, catalog) = load_user_configuration(&paths)?;
            let dispatcher =
                LocalDispatcher::new(paths, global, catalog).with_source_base(source_base);
            match arguments.command {
                ImageCommand::List => dispatcher.run(Action::ImageList, renderer).await,
                ImageCommand::Pull(arguments) => {
                    dispatcher
                        .run(
                            Action::ImagePull {
                                r#ref: ImageRef::from(arguments.reference),
                                sha256: arguments.sha256,
                            },
                            renderer,
                        )
                        .await
                }
                ImageCommand::Inspect(arguments) => {
                    dispatcher
                        .run(Action::ImageInspect { id: arguments.id }, renderer)
                        .await
                }
                ImageCommand::Remove(arguments) => {
                    let mut force = arguments.force || yes;
                    if !force && interactive {
                        let references = dispatcher.image_remove_confirmation(&arguments.id)?;
                        if !references.is_empty() {
                            let prompt = format!(
                                "remove image {} referenced by machine(s) {}? [y/N] ",
                                arguments.id,
                                references.join(", ")
                            );
                            force = confirm(renderer, &prompt)?;
                            if !force {
                                return Err(FirestoneError::new(
                                    ErrorKind::Generic,
                                    "image removal cancelled",
                                ));
                            }
                        }
                    }
                    dispatcher
                        .run(
                            Action::ImageRemove {
                                id: arguments.id,
                                force,
                            },
                            renderer,
                        )
                        .await
                }
                ImageCommand::Prune => dispatcher.run(Action::ImagePrune, renderer).await,
            }
        }
        Command::Doctor(arguments) => {
            let dispatcher =
                LocalDispatcher::new(paths, GlobalConfig::default(), Catalog::built_in()?)
                    .with_source_base(source_base);
            dispatcher
                .run(Action::Doctor { fix: arguments.fix }, renderer)
                .await
        }
    }
}

fn confirm<Stdout, Stderr>(
    renderer: &mut Renderer<Stdout, Stderr>,
    prompt: &str,
) -> Result<bool, FirestoneError>
where
    Stdout: io::Write,
    Stderr: io::Write,
{
    renderer.prompt(prompt)?;
    let mut response = String::new();
    io::stdin().read_line(&mut response).map_err(|source| {
        FirestoneError::new(ErrorKind::Generic, "cannot read confirmation response")
            .with_source(source)
    })?;
    Ok(matches!(
        response.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

fn load_user_configuration(paths: &Paths) -> Result<(GlobalConfig, Catalog), FirestoneError> {
    let global = load_global_config(paths)?;
    let catalog = Catalog::load(&paths.catalog_file(), &global.images.catalog)?;
    Ok((global, catalog))
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
        &ValidationContext::new(&host, paths, &machine_dir, catalog),
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
    use std::{
        io::{self, Write},
        os::unix::fs::MetadataExt as _,
        path::Path,
    };

    use clap::Parser as _;
    use firestone_core::{
        Catalog, DoctorCheck, DoctorCheckId, DoctorReport, DoctorStatus, Event, EventSink,
        GlobalConfig, ImageRef, MachineSpecPatch, PathInputs, Paths,
    };
    use serde_json::json;

    use crate::{
        cli::Cli,
        render::{OutputMode, RenderOptions, Renderer},
    };

    use super::{
        CreateRequest, finish_command, load_create_spec, load_global_config, render_options,
        run_with_inputs,
    };

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    struct BrokenPipeWriter;

    impl Write for BrokenPipeWriter {
        fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
            Err(io::Error::from(io::ErrorKind::BrokenPipe))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn path_inputs(root: &Path) -> PathInputs {
        PathInputs {
            current_dir: root.to_path_buf(),
            home_dir: Some(root.to_path_buf()),
            firestone_home: Some(root.join("home")),
            firestone_config_dir: None,
            firestone_data_dir: None,
            firestone_runtime_dir: None,
            xdg_config_home: None,
            xdg_data_home: None,
            xdg_runtime_dir: None,
            uid: 1000,
        }
    }

    fn paths() -> Result<(tempfile::TempDir, Paths), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let root = std::fs::canonicalize(directory.path())?;
        let paths = Paths::from_inputs(&path_inputs(&root))?;
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

    #[test]
    fn render_options_with_json_quiet_retains_quiet_mode() {
        let options = render_options(true, true, false, 0, false);

        assert_eq!(options.mode, OutputMode::Json);
        assert!(options.quiet);
    }

    #[test]
    fn command_with_failed_doctor_report_returns_exit_five() -> TestResult {
        let report = DoctorReport {
            checks: vec![DoctorCheck {
                id: DoctorCheckId::Kvm,
                status: DoctorStatus::Fail,
                reason: "cannot open /dev/kvm".to_owned(),
                fix: None,
                hint: None,
            }],
        };
        let mut renderer = Renderer::new(Vec::new(), Vec::new(), RenderOptions::json());
        renderer.emit(Event::Result {
            action: "doctor".to_owned(),
            payload: serde_json::to_value(report)?,
        })?;

        let exit = finish_command(Ok(()), &mut renderer);

        assert_eq!(exit, 5);
        let (stdout, stderr) = renderer.into_writers();
        assert_eq!(stdout.iter().filter(|byte| **byte == b'\n').count(), 1);
        assert!(stderr.is_empty());
        Ok(())
    }

    #[test]
    fn command_with_json_broken_pipe_returns_success_without_secondary_error() -> TestResult {
        let mut failing_renderer =
            Renderer::new(BrokenPipeWriter, Vec::new(), RenderOptions::json());
        let output_error = match failing_renderer.emit(Event::Result {
            action: "version".to_owned(),
            payload: json!({"version": "0.1.0"}),
        }) {
            Ok(()) => return Err("broken-pipe writer unexpectedly accepted output".into()),
            Err(error) => error,
        };
        assert_eq!(output_error.message(), "failed to encode command output");

        let mut boundary_renderer =
            Renderer::new(Vec::new(), Vec::new(), RenderOptions::human(false, false));
        let exit = finish_command(Err(output_error), &mut boundary_renderer);

        assert_eq!(exit, 0);
        let (stdout, stderr) = boundary_renderer.into_writers();
        assert!(stdout.is_empty());
        assert!(stderr.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn doctor_with_missing_catalog_and_wrong_owner_storage_emits_one_report_result()
    -> TestResult {
        let directory = tempfile::tempdir()?;
        let root = std::fs::canonicalize(directory.path())?;
        let mut inputs = path_inputs(&root);
        inputs.uid = std::fs::metadata(&root)?.uid().wrapping_add(1);
        let paths = Paths::from_inputs(&inputs)?;
        let config_file = paths.config_file();
        let config_parent = config_file.parent().ok_or("config path has no parent")?;
        std::fs::create_dir_all(config_parent)?;
        let missing_catalog = root.join("missing-catalog.toml");
        let catalog_value = serde_json::to_string(&missing_catalog.to_string_lossy())?;
        std::fs::write(
            &config_file,
            format!("[images]\ncatalog = [{catalog_value}]\n"),
        )?;

        let cli = Cli::try_parse_from(["firestone", "--json", "doctor"])?;
        let mut renderer = Renderer::new(Vec::new(), Vec::new(), RenderOptions::json());
        run_with_inputs(cli, &mut renderer, inputs).await?;

        assert!(!missing_catalog.exists());
        let (stdout, stderr) = renderer.into_writers();
        let events = stdout
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(serde_json::from_slice)
            .collect::<Result<Vec<serde_json::Value>, _>>()?;
        let doctor_results = events
            .iter()
            .filter(|event| event["type"] == "Result" && event["action"] == "doctor")
            .collect::<Vec<_>>();

        assert_eq!(doctor_results.len(), 1);
        assert!(doctor_results[0]["payload"]["checks"].is_array());
        assert!(stderr.is_empty());
        Ok(())
    }
}
