pub mod api;
mod cli;
mod render;
mod serve;
mod store;

use std::{
    env,
    ffi::{OsStr, OsString},
    fs, io,
    io::IsTerminal,
    os::unix::process::ExitStatusExt,
    path::Path,
    process::ExitCode,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::Duration,
};

use clap::{Parser, error::ErrorKind as ClapErrorKind};
use firestone_core::{
    Action, Catalog, Dispatcher, ErrorKind, Event, EventSink, FirestoneError, GlobalConfig,
    ImageRef, MachineSpec, MachineSpecPatch, MachineStatus, PathInputs, Paths, ProcessSignal,
    RawTerminal, RealValidationHost, RunResult, ShellResult, SshConfigResult, ValidationContext,
    block_on, console_plan, relay_console, shell_ssh_plan, ssh_config_plan,
};

use crate::{
    cli::{Cli, Command, CreateRequest, ImageCommand, RunArgs, ShellArgs, derive_machine_name},
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
    if matches!(&cli.command, Command::VsockProxy(_)) {
        return run_hidden_vsock_proxy(cli);
    }

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
fn hidden_shim_name(arguments: &[std::ffi::OsString]) -> Option<&str> {
    if arguments.len() != 3
        || arguments.get(1).map(OsString::as_os_str) != Some(OsStr::new("_shim"))
    {
        return None;
    }
    arguments.get(2).and_then(|name| name.to_str())
}

fn run_hidden_vsock_proxy(cli: Cli) -> ExitCode {
    let (home, arguments) = match cli.command {
        Command::VsockProxy(arguments) => (cli.home, arguments),
        _ => return ExitCode::FAILURE,
    };
    let result = (|| {
        let mut inputs = PathInputs::capture()?;
        if let Some(home) = home {
            inputs.firestone_home = Some(home);
        }
        let paths = Paths::from_inputs(&inputs)?;
        firestone_core::run_vsock_proxy(
            &paths,
            &arguments.name,
            arguments.port,
            io::stdin(),
            io::stdout(),
        )
    })();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            use io::Write as _;
            let mut stderr = io::stderr().lock();
            let _ = writeln!(
                stderr,
                "firestone _vsock-proxy: {}: {}",
                error.kind(),
                error.message()
            );
            if let Some(hint) = error.hint() {
                let _ = writeln!(stderr, "hint: {hint}");
            }
            ExitCode::from(error_exit_code(&error))
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
#[derive(Debug, Clone, Copy, Default)]
struct TerminalMode {
    stdin: bool,
    stdout: bool,
    stderr: bool,
}

impl TerminalMode {
    const fn interactive(self) -> bool {
        self.stdin && self.stderr
    }

    const fn console(self) -> bool {
        self.stdin && self.stdout && self.stderr
    }
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
    let terminal = TerminalMode {
        stdin: io::stdin().is_terminal(),
        stdout: io::stdout().is_terminal(),
        stderr: io::stderr().is_terminal(),
    };
    run_with_inputs_mode(cli, renderer, inputs, terminal).await
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
    run_with_inputs_mode(cli, renderer, inputs, TerminalMode::default()).await
}

async fn run_with_inputs_mode<Stdout, Stderr>(
    cli: Cli,
    renderer: &mut Renderer<Stdout, Stderr>,
    mut inputs: PathInputs,
    terminal: TerminalMode,
) -> Result<(), FirestoneError>
where
    Stdout: io::Write + Send,
    Stderr: io::Write + Send,
{
    let Cli {
        json,
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
        Command::Run(arguments) => {
            run_machine_command(*arguments, paths, source_base, json, terminal, renderer).await
        }
        Command::Create(arguments) => {
            let (global, catalog) = load_user_configuration(&paths)?;
            let request = arguments.into_request().map_err(|error| {
                FirestoneError::new(ErrorKind::Usage, clap_error_message(&error))
                    .with_hint("run firestone create --help for valid forms")
            })?;
            let edit = request.edit;
            let (name, loaded) =
                load_create_spec(request, &inputs.current_dir, &paths, &global, &catalog)?;
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
            let (cancellation, _signals) = StartSignals::register()?;
            let (global, catalog) = load_user_configuration(&paths)?;
            let automatic_timeout = arguments.timeout.is_none();
            let timeout = arguments.timeout.unwrap_or(global.start.timeout).get();
            LocalDispatcher::new(paths, global, catalog)
                .with_source_base(source_base)
                .with_automatic_start_timeout(automatic_timeout)
                .with_start_cancellation(cancellation)
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
            let (cancellation, _signals) = StartSignals::register()?;
            let (global, catalog) = load_user_configuration(&paths)?;
            let timeout = global.start.timeout.get();
            LocalDispatcher::new(paths, global, catalog)
                .with_source_base(source_base)
                .with_automatic_start_timeout(true)
                .with_start_cancellation(cancellation)
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
            if !force && terminal.interactive() {
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
        Command::Shell(arguments) => {
            shell_command(arguments, paths, source_base, json, terminal, renderer).await
        }
        Command::SshConfig(arguments) => {
            ssh_config_command(arguments.name, paths, source_base, renderer)
        }
        Command::Console(arguments) => {
            console_command(arguments.name, paths, source_base, json, terminal)
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
                    if !force && terminal.interactive() {
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
        Command::Serve(arguments) => {
            if yes {
                return Err(FirestoneError::new(
                    ErrorKind::Usage,
                    "--yes is not valid with firestone serve",
                )
                .with_hint("remove --yes; serve never prompts"));
            }
            let (global, catalog) = load_user_configuration(&paths)?;
            let dispatcher = LocalDispatcher::new(paths.clone(), global.clone(), catalog)
                .with_source_base(source_base);
            let socket = match arguments.listen {
                Some(path) if path.is_absolute() => path,
                Some(path) => paths.runtime_dir().join(path),
                None => paths.serve_socket(),
            };
            serve::run(&paths, &socket, Arc::new(dispatcher), &global)
        }
        Command::VsockProxy(_) => Err(FirestoneError::new(
            ErrorKind::Generic,
            "hidden vsock proxy reached event dispatch",
        )),
    }
}

struct WithoutResults<'a> {
    inner: &'a mut dyn EventSink,
}

impl EventSink for WithoutResults<'_> {
    fn emit(&mut self, event: Event) -> Result<(), FirestoneError> {
        if matches!(event, Event::Result { .. }) {
            Ok(())
        } else {
            self.inner.emit(event)
        }
    }
}

async fn dispatch_without_result(
    dispatcher: &LocalDispatcher,
    action: Action,
    events: &mut dyn EventSink,
) -> Result<(), FirestoneError> {
    let mut filtered = WithoutResults { inner: events };
    dispatcher.run(action, &mut filtered).await
}

async fn run_machine_command<Stdout, Stderr>(
    arguments: RunArgs,
    paths: Paths,
    source_base: std::path::PathBuf,
    json: bool,
    terminal: TerminalMode,
    renderer: &mut Renderer<Stdout, Stderr>,
) -> Result<(), FirestoneError>
where
    Stdout: io::Write + Send,
    Stderr: io::Write + Send,
{
    if json {
        return Err(FirestoneError::new(
            ErrorKind::Usage,
            "run cannot mix an interactive SSH byte stream with NDJSON output",
        )
        .with_hint("remove --json or use create and start as separate actions"));
    }

    let (start_cancellation, start_signals) = StartSignals::register()?;
    let (global, catalog) = load_user_configuration(&paths)?;
    let dispatcher = LocalDispatcher::new(paths.clone(), global.clone(), catalog.clone())
        .with_source_base(source_base.clone())
        .with_automatic_start_timeout(true)
        .with_start_cancellation(start_cancellation);
    let RunArgs {
        target,
        name: requested_name,
        remove,
        spec,
        command,
    } = arguments;
    let target = target.unwrap_or_else(|| "ubuntu".to_owned());
    let patch = spec.into_patch();
    let user_override = patch.user.clone();
    let mut forbidden_patch = patch.clone();
    forbidden_patch.user = None;
    let has_spec_flags = forbidden_patch != MachineSpecPatch::default();

    let exact_machine = dispatcher.find_terminal_machine(&target)?;
    let (name, machine, create_spec) = if let Some(machine) = exact_machine {
        if requested_name.is_some() || has_spec_flags {
            return Err(FirestoneError::new(
                ErrorKind::Usage,
                format!(
                    "run target {target} is an existing machine; specification flags do not apply"
                ),
            )
            .with_hint(format!(
                "use firestone edit {target} to change its specification"
            )));
        }
        (target, Some(machine), None)
    } else {
        let image = ImageRef::new(target.clone());
        let name = match requested_name {
            Some(name) => name,
            None => derive_machine_name(&image).map_err(|error| {
                FirestoneError::new(ErrorKind::Usage, clap_error_message(&error))
                    .with_hint("pass --name with a valid machine name")
            })?,
        };
        let (_, loaded) = load_create_spec(
            CreateRequest {
                name: name.clone(),
                image,
                patch,
                file: None,
                edit: false,
            },
            &source_base,
            &paths,
            &global,
            &catalog,
        )?;
        match dispatcher.find_terminal_machine(&name)? {
            Some(machine) => {
                if machine.spec.image != loaded.spec.image {
                    return Err(FirestoneError::new(
                        ErrorKind::Conflict,
                        format!(
                            "machine name {name} is already used for image {}",
                            machine.spec.image
                        ),
                    )
                    .with_hint("pass --name to choose a different machine name"));
                }
                if has_spec_flags {
                    return Err(FirestoneError::new(
                        ErrorKind::Usage,
                        format!("run would reuse machine {name}; specification flags do not apply"),
                    )
                    .with_hint(format!(
                        "use firestone edit {name} to change its specification"
                    )));
                }
                (name, Some(machine), None)
            }
            None => (name, None, Some(loaded.spec)),
        }
    };

    let created = create_spec.is_some();
    let mut machine = match (machine, create_spec) {
        (Some(machine), None) => machine,
        (None, Some(spec)) => {
            dispatch_without_result(
                &dispatcher,
                Action::Create {
                    name: name.clone(),
                    spec,
                },
                renderer,
            )
            .await?;
            dispatcher.terminal_machine(&name)?
        }
        _ => {
            return Err(FirestoneError::new(
                ErrorKind::Generic,
                "run target resolution produced an inconsistent machine plan",
            ));
        }
    };

    if matches!(
        machine.state.status,
        MachineStatus::Created | MachineStatus::Stopped | MachineStatus::Failed
    ) {
        let timeout = if created || machine.state.instance_id.is_none() {
            global.start.timeout_first_boot.get()
        } else {
            global.start.timeout.get()
        };
        if let Err(error) = dispatch_without_result(
            &dispatcher,
            Action::Start {
                name: name.clone(),
                wait: true,
                timeout,
            },
            renderer,
        )
        .await
        {
            let running = dispatcher
                .find_terminal_machine(&name)
                .ok()
                .flatten()
                .is_some_and(|machine| machine.state.status == MachineStatus::Running);
            if created && remove && !running {
                cleanup_created_machine(&dispatcher, &name, renderer).await?;
            }
            return Err(error);
        }
        machine = dispatcher.terminal_machine(&name)?;
    } else if machine.state.status != MachineStatus::Running {
        return Err(FirestoneError::new(
            ErrorKind::Busy,
            format!(
                "machine {name} is {}; wait for its current lifecycle operation",
                machine_status_word(machine.state.status)
            ),
        ));
    }

    drop(start_signals);
    let user = user_override.unwrap_or_else(|| machine.spec.user.clone());
    let executable = env::current_exe().map_err(|source| {
        FirestoneError::new(
            ErrorKind::Dependency,
            "cannot locate the current firestone executable for SSH ProxyCommand",
        )
        .with_source(source)
    })?;
    let plan = match shell_ssh_plan(&paths, &executable, &name, &user, terminal.stdin, command) {
        Ok(plan) => plan,
        Err(error) => {
            if created && remove {
                cleanup_created_machine(&dispatcher, &name, renderer).await?;
            }
            return Err(error);
        }
    };

    if !(created && remove) {
        return exec_ssh(plan);
    }

    let received_signal = Arc::new(AtomicUsize::new(0));
    let signals = match RunSignals::register(Arc::clone(&received_signal)) {
        Ok(signals) => signals,
        Err(error) => {
            cleanup_created_machine(&dispatcher, &name, renderer).await?;
            return Err(error);
        }
    };
    let mut child = match plan
        .command()
        .stdin_inherit()
        .spawn_interactive_process_group()
    {
        Ok(child) => child,
        Err(error) => {
            cleanup_created_machine(&dispatcher, &name, renderer).await?;
            return Err(error);
        }
    };
    let mut forwarded_signal = 0_usize;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) => {}
            Err(error) => break Err(error),
        }
        let signal = received_signal.load(Ordering::SeqCst);
        if signal != 0 && signal != forwarded_signal {
            match run_process_signal(signal).and_then(|signal| child.signal_group(signal)) {
                Ok(()) => forwarded_signal = signal,
                Err(error) => break Err(error),
            }
        }
        thread::sleep(Duration::from_millis(20));
    };
    if status.is_err() {
        let _ = child.signal_group(ProcessSignal::Kill);
        let _ = child.wait();
    }
    let cleanup = cleanup_created_machine(&dispatcher, &name, renderer).await;
    cleanup?;
    let status = status?;
    let received = received_signal.load(Ordering::SeqCst);
    let termination_signal = status
        .signal()
        .or_else(|| i32::try_from(received).ok().filter(|signal| *signal != 0));
    let result = RunResult {
        name: name.clone(),
        created: true,
        removed: true,
        shell: ShellResult {
            name,
            user,
            exit_code: status.code(),
            signal: termination_signal,
        },
    };
    drop(signals);
    if let Some(signal) = termination_signal {
        return match signal_hook::low_level::emulate_default_handler(signal) {
            Ok(()) => Err(FirestoneError::new(
                ErrorKind::Generic,
                format!("default handler for signal {signal} returned unexpectedly"),
            )),
            Err(source) => Err(FirestoneError::new(
                ErrorKind::Generic,
                format!("cannot preserve SSH terminating signal {signal}"),
            )
            .with_source(source)),
        };
    }
    renderer.set_exit_override(run_exit_code(&result));
    Ok(())
}

async fn cleanup_created_machine(
    dispatcher: &LocalDispatcher,
    name: &str,
    events: &mut dyn EventSink,
) -> Result<(), FirestoneError> {
    dispatch_without_result(
        dispatcher,
        Action::Remove {
            names: vec![name.to_owned()],
            force: true,
        },
        events,
    )
    .await
    .map_err(|error| {
        FirestoneError::new(
            error.kind(),
            format!("cannot clean up run machine {name}: {}", error.message()),
        )
        .with_hint(format!("remove it with firestone rm --force {name}"))
    })?;
    Ok(())
}

async fn shell_command<Stdout, Stderr>(
    arguments: ShellArgs,
    paths: Paths,
    source_base: std::path::PathBuf,
    json: bool,
    terminal: TerminalMode,
    renderer: &mut Renderer<Stdout, Stderr>,
) -> Result<(), FirestoneError>
where
    Stdout: io::Write + Send,
    Stderr: io::Write + Send,
{
    if json {
        return Err(FirestoneError::new(
            ErrorKind::Usage,
            "shell cannot mix an SSH byte stream with NDJSON output",
        )
        .with_hint("remove --json and retry"));
    }
    let (start_cancellation, start_signals) = StartSignals::register()?;
    let (global, catalog) = load_user_configuration(&paths)?;
    let dispatcher = LocalDispatcher::new(paths.clone(), global.clone(), catalog)
        .with_source_base(source_base)
        .with_automatic_start_timeout(true)
        .with_start_cancellation(start_cancellation);
    let mut machine = dispatcher.terminal_machine(&arguments.name)?;
    if machine.state.status != MachineStatus::Running {
        if !terminal.stdin {
            return Err(not_running_terminal_error(&arguments.name));
        }
        if !matches!(
            machine.state.status,
            MachineStatus::Created | MachineStatus::Stopped | MachineStatus::Failed
        ) {
            return Err(not_running_terminal_error(&arguments.name));
        }
        let timeout = if machine.state.instance_id.is_none() {
            global.start.timeout_first_boot.get()
        } else {
            global.start.timeout.get()
        };
        dispatch_without_result(
            &dispatcher,
            Action::Start {
                name: arguments.name.clone(),
                wait: true,
                timeout,
            },
            renderer,
        )
        .await?;
        machine = dispatcher.terminal_machine(&arguments.name)?;
    }
    if machine.state.status != MachineStatus::Running {
        return Err(not_running_terminal_error(&arguments.name));
    }
    drop(start_signals);
    let user = arguments.user.unwrap_or(machine.spec.user);
    let executable = env::current_exe().map_err(|source| {
        FirestoneError::new(
            ErrorKind::Dependency,
            "cannot locate the current firestone executable for SSH ProxyCommand",
        )
        .with_source(source)
    })?;
    let plan = shell_ssh_plan(
        &paths,
        &executable,
        &arguments.name,
        &user,
        terminal.stdin,
        arguments.command,
    )?;
    exec_ssh(plan)
}

fn exec_ssh(plan: firestone_core::SshCommandPlan) -> Result<(), FirestoneError> {
    match plan.command().stdin_inherit().exec() {
        Ok(never) => match never {},
        Err(error) => Err(error),
    }
}

fn ssh_config_command<Stdout, Stderr>(
    name: String,
    paths: Paths,
    source_base: std::path::PathBuf,
    renderer: &mut Renderer<Stdout, Stderr>,
) -> Result<(), FirestoneError>
where
    Stdout: io::Write + Send,
    Stderr: io::Write + Send,
{
    let (global, catalog) = load_user_configuration(&paths)?;
    let dispatcher =
        LocalDispatcher::new(paths.clone(), global, catalog).with_source_base(source_base);
    let machine = dispatcher.terminal_machine(&name)?;
    let executable = env::current_exe().map_err(|source| {
        FirestoneError::new(
            ErrorKind::Dependency,
            "cannot locate the current firestone executable for SSH ProxyCommand",
        )
        .with_source(source)
    })?;
    let plan = ssh_config_plan(&paths, &executable, &name, &machine.spec.user)?;
    let payload = SshConfigResult {
        name,
        host: plan.host().to_owned(),
        config: plan.block().to_owned(),
    };
    let payload = serde_json::to_value(payload).map_err(|source| {
        FirestoneError::new(ErrorKind::Generic, "cannot serialize ssh-config result")
            .with_source(source)
    })?;
    renderer.emit(Event::Result {
        action: "ssh-config".to_owned(),
        payload,
    })
}

fn console_command(
    name: String,
    paths: Paths,
    source_base: std::path::PathBuf,
    json: bool,
    terminal: TerminalMode,
) -> Result<(), FirestoneError> {
    if json {
        return Err(FirestoneError::new(
            ErrorKind::Usage,
            "console cannot mix a binary terminal stream with NDJSON output",
        )
        .with_hint("remove --json and retry from a terminal"));
    }
    if !terminal.console() {
        return Err(FirestoneError::new(
            ErrorKind::Usage,
            "console requires terminal stdin, stdout, and stderr",
        )
        .with_hint("run firestone console from an interactive terminal"));
    }
    let (global, catalog) = load_user_configuration(&paths)?;
    let dispatcher =
        LocalDispatcher::new(paths.clone(), global, catalog).with_source_base(source_base);
    let machine = dispatcher.terminal_machine(&name)?;
    if machine.state.status != MachineStatus::Running {
        return Err(not_running_terminal_error(&name));
    }
    let plan = console_plan(&paths, &name)?;
    let mut stream = plan.connect(Duration::from_secs(5))?;
    let cancelled = Arc::new(AtomicBool::new(false));
    let _signals = TerminalSignals::register(Arc::clone(&cancelled))?;
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut raw = RawTerminal::enter(&stdin)?;
    {
        use io::Write as _;
        let mut stderr = io::stderr().lock();
        writeln!(stderr, "connected to {name} console · escape: Ctrl-]")
            .and_then(|()| stderr.flush())
            .map_err(|source| {
                FirestoneError::new(ErrorKind::Generic, "cannot print console connection status")
                    .with_source(source)
            })?;
    }
    let mut input = stdin.lock();
    let mut output = stdout.lock();
    let relay = relay_console(&name, &mut stream, &mut input, &mut output, &cancelled);
    let restore = raw.restore();
    match (relay, restore) {
        (Ok(_), Ok(())) => Ok(()),
        (_, Err(error)) => Err(error),
        (Err(error), Ok(())) => Err(error),
    }
}

struct StartSignals {
    id: Option<signal_hook::SigId>,
}

impl StartSignals {
    fn register() -> Result<(Arc<AtomicBool>, Self), FirestoneError> {
        let cancelled = Arc::new(AtomicBool::new(false));
        let id = signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&cancelled))
            .map_err(|source| {
            FirestoneError::new(
                ErrorKind::Generic,
                "cannot install start cancellation handler",
            )
            .with_source(source)
        })?;
        Ok((cancelled, Self { id: Some(id) }))
    }
}

impl Drop for StartSignals {
    fn drop(&mut self) {
        if let Some(id) = self.id.take() {
            signal_hook::low_level::unregister(id);
        }
    }
}

struct RunSignals {
    ids: Vec<signal_hook::SigId>,
}

impl RunSignals {
    fn register(signal_flag: Arc<AtomicUsize>) -> Result<Self, FirestoneError> {
        let mut ids = Vec::with_capacity(4);
        for signal in [
            signal_hook::consts::SIGHUP,
            signal_hook::consts::SIGINT,
            signal_hook::consts::SIGQUIT,
            signal_hook::consts::SIGTERM,
        ] {
            match signal_hook::flag::register_usize(
                signal,
                Arc::clone(&signal_flag),
                usize::try_from(signal).unwrap_or_default(),
            ) {
                Ok(id) => ids.push(id),
                Err(source) => {
                    for id in ids {
                        signal_hook::low_level::unregister(id);
                    }
                    return Err(FirestoneError::new(
                        ErrorKind::Generic,
                        "cannot install run signal handlers",
                    )
                    .with_source(source));
                }
            }
        }
        Ok(Self { ids })
    }
}

impl Drop for RunSignals {
    fn drop(&mut self) {
        for id in self.ids.drain(..) {
            signal_hook::low_level::unregister(id);
        }
    }
}

fn run_process_signal(signal: usize) -> Result<ProcessSignal, FirestoneError> {
    match i32::try_from(signal).ok() {
        Some(signal_hook::consts::SIGHUP) => Ok(ProcessSignal::Hangup),
        Some(signal_hook::consts::SIGINT) => Ok(ProcessSignal::Interrupt),
        Some(signal_hook::consts::SIGQUIT) => Ok(ProcessSignal::Quit),
        Some(signal_hook::consts::SIGTERM) => Ok(ProcessSignal::Terminate),
        _ => Err(FirestoneError::new(
            ErrorKind::Generic,
            format!("cannot forward unsupported signal {signal}"),
        )),
    }
}

struct TerminalSignals {
    ids: Vec<signal_hook::SigId>,
}

impl TerminalSignals {
    fn register(flag: Arc<AtomicBool>) -> Result<Self, FirestoneError> {
        let mut ids = Vec::with_capacity(4);
        for signal in [
            signal_hook::consts::SIGINT,
            signal_hook::consts::SIGTERM,
            signal_hook::consts::SIGHUP,
            signal_hook::consts::SIGQUIT,
        ] {
            match signal_hook::flag::register(signal, Arc::clone(&flag)) {
                Ok(id) => ids.push(id),
                Err(source) => {
                    for id in ids {
                        signal_hook::low_level::unregister(id);
                    }
                    return Err(FirestoneError::new(
                        ErrorKind::Generic,
                        "cannot install console terminal signal handlers",
                    )
                    .with_source(source));
                }
            }
        }
        Ok(Self { ids })
    }
}

impl Drop for TerminalSignals {
    fn drop(&mut self) {
        for id in self.ids.drain(..) {
            signal_hook::low_level::unregister(id);
        }
    }
}

fn not_running_terminal_error(name: &str) -> FirestoneError {
    FirestoneError::new(
        ErrorKind::NotRunning,
        format!("machine {name} is not running"),
    )
    .with_hint(format!("start it with firestone start {name}"))
}

fn machine_status_word(status: MachineStatus) -> &'static str {
    match status {
        MachineStatus::Created => "created",
        MachineStatus::Starting => "starting",
        MachineStatus::Running => "running",
        MachineStatus::Stopping => "stopping",
        MachineStatus::Stopped => "stopped",
        MachineStatus::Failed => "failed",
    }
}

fn run_exit_code(result: &RunResult) -> u8 {
    match (result.shell.exit_code, result.shell.signal) {
        (Some(code), _) => u8::try_from(code).unwrap_or(1),
        (None, Some(signal)) => u8::try_from(128_i32.saturating_add(signal)).unwrap_or(1),
        (None, None) => 1,
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
