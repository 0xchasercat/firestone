use std::path::{Path, PathBuf};

use clap::{ArgAction, Args, Parser, Subcommand, error::ErrorKind};
use firestone_core::{
    Arch, ByteSize, CloudInitSpecPatch, Firmware, HumanDuration, ImageRef, LogSource, MacAddr,
    MachineSpecPatch, MountSpec, NetMode, NetworkSpecPatch, PortForward, SpecClear, VmmSpecPatch,
    VsockPort,
};

/// Firestone's command-line interface.
#[derive(Debug, Parser)]
#[command(name = "firestone")]
pub struct Cli {
    /// Print events as newline-delimited JSON and disable human output.
    #[arg(long, global = true)]
    pub json: bool,

    /// Print only errors and command results.
    #[arg(short = 'q', long, global = true)]
    pub quiet: bool,

    /// Increase log detail. Pass twice for debug output.
    #[arg(
        short = 'v',
        long,
        global = true,
        action = ArgAction::Count
    )]
    pub verbose: u8,

    /// Disable colored output.
    #[arg(long, global = true)]
    pub no_color: bool,

    /// Assume yes when a command may prompt.
    #[arg(short = 'y', long, global = true)]
    pub yes: bool,

    /// Override the Firestone home root (config, data, and runtime).
    #[arg(long, global = true, value_name = "DIR")]
    pub home: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Command,
}

/// Commands implemented by the Linux M1 CLI.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create a machine definition without booting it.
    Create(Box<CreateArgs>),

    /// Start a machine and wait for the M1 running contract.
    Start(StartArgs),

    /// Stop a machine.
    Stop(StopArgs),

    /// Stop and start a machine.
    Restart(RestartArgs),

    /// Stop and remove one or more machines.
    #[command(name = "rm")]
    Remove(RemoveArgs),

    /// List machines.
    #[command(name = "ls", visible_alias = "list")]
    List,

    /// Show a machine's specification and runtime state.
    Show(ShowArgs),

    /// Edit and validate a machine's firestone.toml.
    Edit(EditArgs),

    /// Print a bounded machine log.
    Logs(LogsArgs),

    /// Manage the owned image store.
    Images(ImagesArgs),

    /// Check host requirements and optional safe repairs.
    Doctor(DoctorArgs),

    /// Relay one SSH ProxyCommand connection through the machine's vsock socket.
    #[command(name = "_vsock-proxy", hide = true)]
    VsockProxy(VsockProxyArgs),
}

/// Arguments accepted by firestone start.
#[derive(Debug, Args)]
pub struct StartArgs {
    pub name: String,

    /// Return after the M1 running contract without later readiness checks.
    #[arg(long)]
    pub no_wait: bool,

    /// Override the configured start deadline.
    #[arg(long, value_name = "DURATION")]
    pub timeout: Option<HumanDuration>,
}

/// Arguments accepted by the hidden Cloud Hypervisor vsock proxy.
#[derive(Debug, Args)]
pub struct VsockProxyArgs {
    pub name: String,
    pub port: VsockPort,
}

/// Arguments accepted by firestone stop.
#[derive(Debug, Args)]
pub struct StopArgs {
    pub name: String,

    /// Override the configured graceful-stop deadline.
    #[arg(long, value_name = "DURATION")]
    pub timeout: Option<HumanDuration>,

    /// Skip the guest power button and kill the VMM.
    #[arg(long)]
    pub force: bool,
}

/// Arguments accepted by firestone restart.
#[derive(Debug, Args)]
pub struct RestartArgs {
    pub name: String,
}

/// Arguments accepted by firestone rm.
#[derive(Debug, Args)]
pub struct RemoveArgs {
    #[arg(value_name = "NAME", num_args = 1.., required = true)]
    pub names: Vec<String>,

    /// Approve removal of running machines.
    #[arg(long)]
    pub force: bool,
}

/// Arguments accepted by firestone logs.
#[derive(Debug, Args)]
pub struct LogsArgs {
    pub name: String,

    /// Continue printing appended log data until interrupted.
    #[arg(short = 'f', long)]
    pub follow: bool,

    /// Select an owned machine log.
    #[arg(long, default_value = "console", value_name = "SOURCE")]
    pub source: LogSource,

    /// Print the last LINES lines before following.
    #[arg(
        short = 'n',
        default_value_t = 200,
        value_parser = clap::value_parser!(u32).range(0..=100_000),
        value_name = "LINES"
    )]
    pub lines: u32,
}

/// Arguments accepted by firestone images.
#[derive(Debug, Args)]
pub struct ImagesArgs {
    #[command(subcommand)]
    pub command: ImageCommand,
}

/// Image-store commands.
#[derive(Debug, Subcommand)]
pub enum ImageCommand {
    /// List stored images.
    #[command(name = "ls")]
    List,

    /// Pull and verify one image.
    Pull(ImagePullArgs),

    /// Verify and inspect one stored image.
    Inspect(ImageInspectArgs),

    /// Remove one stored image.
    #[command(name = "rm")]
    Remove(ImageRemoveArgs),

    /// Remove all unreferenced images.
    Prune,
}

/// Arguments accepted by firestone images pull.
#[derive(Debug, Args)]
pub struct ImagePullArgs {
    #[arg(value_name = "REF")]
    pub reference: String,

    /// Verify a direct HTTPS URL with this SHA-256 digest.
    #[arg(long, value_name = "HEX")]
    pub sha256: Option<String>,
}

/// Arguments accepted by firestone images inspect.
#[derive(Debug, Args)]
pub struct ImageInspectArgs {
    pub id: String,
}

/// Arguments accepted by firestone images rm.
#[derive(Debug, Args)]
pub struct ImageRemoveArgs {
    pub id: String,

    /// Approve removal while a machine still references the image.
    #[arg(long)]
    pub force: bool,
}

/// Arguments accepted by firestone show.
#[derive(Debug, Args)]
pub struct ShowArgs {
    pub name: String,

    /// Include the generated cloud-hypervisor configuration.
    #[arg(long)]
    pub vmconfig: bool,
}

/// Arguments accepted by firestone edit.
#[derive(Debug, Args)]
pub struct EditArgs {
    pub name: String,
}

/// Arguments accepted by firestone doctor.
#[derive(Debug, Args)]
pub struct DoctorArgs {
    /// Perform only the safe unprivileged repairs.
    #[arg(long)]
    pub fix: bool,
}
/// Arguments accepted by `firestone create` before positional resolution.
#[derive(Debug, Args)]
pub struct CreateArgs {
    /// One value is IMAGE; two values are NAME followed by IMAGE.
    #[arg(value_name = "NAME_OR_IMAGE", num_args = 0..=2)]
    pub positional: Vec<String>,

    /// Supply IMAGE as a flag. A sole positional value is then NAME.
    #[arg(long, value_name = "IMAGE")]
    pub image: Option<String>,

    /// Layer an existing machine specification below command-line flags.
    #[arg(short = 'f', long, value_name = "SPEC.toml")]
    pub file: Option<PathBuf>,

    /// Open the generated specification in the configured editor.
    #[arg(long)]
    pub edit: bool,

    #[command(flatten)]
    pub spec: SpecArgs,
}

/// A resolved create invocation for the command runner.
///
/// `patch.image` is deliberately unset. `image` is the sole owned image
/// reference; the caller moves it into the patch immediately before layering.
#[derive(Debug)]
pub struct CreateRequest {
    pub name: String,
    pub image: ImageRef,
    pub patch: MachineSpecPatch,
    pub file: Option<PathBuf>,
    pub edit: bool,
}

impl CreateArgs {
    /// Resolves create's context-sensitive positionals without cloning inputs.
    pub fn into_request(self) -> Result<CreateRequest, clap::Error> {
        let Self {
            positional,
            image,
            file,
            edit,
            spec,
        } = self;
        let (name, image) = resolve_create_target(positional, image)?;

        Ok(CreateRequest {
            name,
            image,
            patch: spec.into_patch(),
            file,
            edit,
        })
    }
}

/// Clap's projection of every CLI-settable `MachineSpecPatch` leaf except image.
///
/// Image has context-sensitive positional behavior and therefore lives on
/// [`CreateArgs`].
#[derive(Debug, Default, Args)]
pub struct SpecArgs {
    #[arg(long, value_name = "ARCH", value_parser = parse_arch)]
    pub arch: Option<Arch>,

    #[arg(long, value_name = "COUNT")]
    pub cpus: Option<u8>,

    #[arg(long, value_name = "SIZE")]
    pub memory: Option<ByteSize>,

    #[arg(long, value_name = "SIZE")]
    pub disk: Option<ByteSize>,

    #[arg(long, value_name = "USER")]
    pub user: Option<String>,

    #[arg(long, value_name = "MODE", value_parser = parse_net_mode)]
    pub net: Option<NetMode>,

    #[arg(short = 'p', long, value_name = "SPEC")]
    pub forward: Vec<PortForward>,

    #[arg(long, value_name = "DEV")]
    pub tap: Option<String>,

    #[arg(long = "network-mac", value_name = "MAC")]
    pub network_mac: Option<MacAddr>,

    #[arg(long, value_name = "HOST:GUEST[:ro]", value_parser = parse_mount)]
    pub mount: Vec<MountSpec>,

    #[arg(long = "user-data", value_name = "FILE")]
    pub user_data: Option<PathBuf>,

    #[arg(long = "cloud-init-network-config", value_name = "FILE")]
    pub cloud_init_network_config: Option<PathBuf>,

    #[arg(long = "ssh-key", value_name = "FILE")]
    pub ssh_key: Vec<PathBuf>,

    #[arg(long = "no-provisioning")]
    pub no_provisioning: bool,

    #[arg(long = "vmm-binary", value_name = "FILE")]
    pub vmm_binary: Option<PathBuf>,

    #[arg(long = "vmm-firmware", value_name = "FIRMWARE")]
    pub vmm_firmware: Option<Firmware>,

    #[arg(long = "vmm-arg", value_name = "ARG", allow_hyphen_values = true)]
    pub vmm_arg: Vec<String>,

    #[arg(long = "vmm-config", value_name = "JSON", value_parser = parse_vmm_config)]
    pub vmm_config: Option<serde_json::Value>,

    #[arg(long, value_name = "FIELD")]
    pub clear: Vec<SpecClear>,
}

impl SpecArgs {
    /// Converts only flags the user supplied into a sparse core patch.
    #[must_use]
    pub fn into_patch(self) -> MachineSpecPatch {
        let Self {
            arch,
            cpus,
            memory,
            disk,
            user,
            net,
            forward,
            tap,
            network_mac,
            mount,
            user_data,
            cloud_init_network_config,
            ssh_key,
            no_provisioning,
            vmm_binary,
            vmm_firmware,
            vmm_arg,
            vmm_config,
            clear,
        } = self;

        let forward = non_empty(forward);
        let network =
            if net.is_none() && forward.is_none() && tap.is_none() && network_mac.is_none() {
                None
            } else {
                Some(NetworkSpecPatch {
                    mode: net,
                    forward,
                    tap,
                    mac: network_mac,
                })
            };

        let mounts = non_empty(mount);
        let ssh_keys = non_empty(ssh_key);
        let provisioning = no_provisioning.then_some(false);
        let cloud_init = if user_data.is_none()
            && cloud_init_network_config.is_none()
            && ssh_keys.is_none()
            && provisioning.is_none()
        {
            None
        } else {
            Some(CloudInitSpecPatch {
                user_data,
                network_config: cloud_init_network_config,
                ssh_keys,
                provisioning,
            })
        };

        let extra_args = non_empty(vmm_arg);
        let vmm = if vmm_binary.is_none()
            && vmm_firmware.is_none()
            && extra_args.is_none()
            && vmm_config.is_none()
        {
            None
        } else {
            Some(VmmSpecPatch {
                binary: vmm_binary,
                firmware: vmm_firmware,
                extra_args,
                config_overlay: vmm_config,
            })
        };

        MachineSpecPatch {
            clear,
            image: None,
            arch,
            cpus,
            memory,
            disk,
            user,
            network,
            mounts,
            cloud_init,
            vmm,
        }
    }
}

impl From<SpecArgs> for MachineSpecPatch {
    fn from(arguments: SpecArgs) -> Self {
        arguments.into_patch()
    }
}

fn non_empty<T>(values: Vec<T>) -> Option<Vec<T>> {
    if values.is_empty() {
        None
    } else {
        Some(values)
    }
}

fn resolve_create_target(
    positional: Vec<String>,
    image_flag: Option<String>,
) -> Result<(String, ImageRef), clap::Error> {
    if positional.len() > 2 {
        return Err(clap::Error::raw(
            ErrorKind::TooManyValues,
            "create accepts at most NAME and IMAGE as positional values",
        ));
    }

    let mut positional = positional.into_iter();
    let first = positional.next();
    let second = positional.next();

    let (explicit_name, image) = match (first, second, image_flag) {
        (None, None, None) => {
            return Err(clap::Error::raw(
                ErrorKind::MissingRequiredArgument,
                "image is required; pass IMAGE positionally or with '--image'",
            ));
        }
        (None, None, Some(image)) | (Some(image), None, None) => (None, image),
        (Some(name), None, Some(image)) | (Some(name), Some(image), None) => (Some(name), image),
        (Some(_), Some(_), Some(_)) => {
            return Err(clap::Error::raw(
                ErrorKind::ArgumentConflict,
                "image cannot be supplied both positionally and with '--image'",
            ));
        }
        (None, Some(_), _) => {
            return Err(clap::Error::raw(
                ErrorKind::InvalidValue,
                "create positional arguments are not contiguous",
            ));
        }
    };

    let image = ImageRef::from(image);
    let name = match explicit_name {
        Some(name) => name,
        None => derive_machine_name(&image)?,
    };
    Ok((name, image))
}

fn derive_machine_name(image: &ImageRef) -> Result<String, clap::Error> {
    let value = image.as_str();
    let candidate = if value.contains('/') {
        let suffix = match value.find(['?', '#']) {
            Some(index) => index,
            None => value.len(),
        };
        let without_suffix = value[..suffix].trim_end_matches('/');
        Path::new(without_suffix)
            .file_stem()
            .and_then(|name| name.to_str())
    } else {
        Some(value.split_once(':').map_or(value, |(name, _)| name))
    };

    match candidate {
        Some(name) if !name.is_empty() && name != "." && name != ".." => Ok(name.to_owned()),
        _ => Err(clap::Error::raw(
            ErrorKind::InvalidValue,
            format!("cannot derive a machine name from image '{value}'; pass NAME before IMAGE"),
        )),
    }
}

fn parse_arch(value: &str) -> Result<Arch, String> {
    match value {
        "x86_64" => Ok(Arch::X86_64),
        "aarch64" => Ok(Arch::Aarch64),
        _ => Err("architecture must be 'x86_64' or 'aarch64'".to_owned()),
    }
}

fn parse_net_mode(value: &str) -> Result<NetMode, String> {
    match value {
        "passt" => Ok(NetMode::Passt),
        "tap" => Ok(NetMode::Tap),
        "none" => Ok(NetMode::None),
        _ => Err("network mode must be 'passt', 'tap', or 'none'".to_owned()),
    }
}

fn parse_mount(value: &str) -> Result<MountSpec, String> {
    let mut components = value.split(':');
    let (Some(host), Some(guest)) = (components.next(), components.next()) else {
        return Err("mount must have the form HOST:GUEST[:ro]".to_owned());
    };
    let access = components.next();

    if host.is_empty() || guest.is_empty() || components.next().is_some() {
        return Err("mount must have the form HOST:GUEST[:ro]".to_owned());
    }
    let readonly = match access {
        None => false,
        Some("ro") => true,
        Some(_) => return Err("mount access mode must be 'ro' when present".to_owned()),
    };

    Ok(MountSpec {
        host: PathBuf::from(host),
        guest: PathBuf::from(guest),
        readonly,
        tag: None,
    })
}

fn parse_vmm_config(value: &str) -> Result<serde_json::Value, String> {
    let parsed: serde_json::Value = serde_json::from_str(value)
        .map_err(|error| format!("invalid JSON object for --vmm-config: {error}"))?;
    if !parsed.is_object() {
        return Err("--vmm-config must be a JSON object".to_owned());
    }
    Ok(parsed)
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use std::{collections::BTreeSet, path::PathBuf};

    use clap::{Args as _, CommandFactory as _, Parser as _, error::ErrorKind};
    use firestone_core::{Arch, ByteSize, Firmware, NetMode, SPEC_FIELD_METADATA, SpecClear};
    use serde_json::json;

    use super::{Cli, Command, CreateArgs, ImageCommand};

    fn create_request(arguments: &[&str]) -> Result<super::CreateRequest, clap::Error> {
        let cli = Cli::try_parse_from(arguments)?;
        match cli.command {
            Command::Create(arguments) => arguments.into_request(),
            _ => Err(clap::Error::raw(
                ErrorKind::InvalidSubcommand,
                "test expected create command",
            )),
        }
    }

    #[test]
    fn globals_are_accepted_after_a_subcommand_and_verbose_counts() -> Result<(), clap::Error> {
        let cli = Cli::try_parse_from([
            "firestone",
            "ls",
            "--json",
            "-q",
            "-vv",
            "--no-color",
            "-y",
            "--home",
            "/tmp/firestone-home",
        ])?;

        assert!(cli.json);
        assert!(cli.quiet);
        assert_eq!(cli.verbose, 2);
        assert!(cli.no_color);
        assert!(cli.yes);
        assert_eq!(cli.home, Some(PathBuf::from("/tmp/firestone-home")));
        assert!(matches!(cli.command, Command::List));
        Ok(())
    }

    #[test]
    fn list_alias_selects_the_list_command() -> Result<(), clap::Error> {
        let cli = Cli::try_parse_from(["firestone", "list"])?;
        assert!(matches!(cli.command, Command::List));
        Ok(())
    }

    #[test]
    fn show_and_edit_capture_parent_facing_arguments() -> Result<(), clap::Error> {
        let show = Cli::try_parse_from(["firestone", "show", "dev", "--vmconfig"])?;
        match show.command {
            Command::Show(arguments) => {
                assert_eq!(arguments.name, "dev");
                assert!(arguments.vmconfig);
            }
            _ => panic!("expected show command"),
        }

        let edit = Cli::try_parse_from(["firestone", "edit", "dev"])?;
        match edit.command {
            Command::Edit(arguments) => assert_eq!(arguments.name, "dev"),
            _ => panic!("expected edit command"),
        }
        Ok(())
    }

    #[test]
    fn doctor_fix_sets_safe_repair_mode() -> Result<(), clap::Error> {
        let cli = Cli::try_parse_from(["firestone", "doctor", "--fix"])?;
        match cli.command {
            Command::Doctor(arguments) => assert!(arguments.fix),
            _ => panic!("expected doctor command"),
        }
        Ok(())
    }

    #[test]
    fn lifecycle_commands_capture_exact_flags_and_defaults() -> Result<(), clap::Error> {
        let start =
            Cli::try_parse_from(["firestone", "start", "dev", "--no-wait", "--timeout", "17s"])?;
        match start.command {
            Command::Start(arguments) => {
                assert_eq!(arguments.name, "dev");
                assert!(arguments.no_wait);
                assert_eq!(
                    arguments.timeout.map(|timeout| timeout.get()),
                    Some(std::time::Duration::from_secs(17))
                );
            }
            _ => panic!("expected start command"),
        }

        let stop = Cli::try_parse_from(["firestone", "stop", "dev", "--timeout", "9s", "--force"])?;
        match stop.command {
            Command::Stop(arguments) => {
                assert_eq!(arguments.name, "dev");
                assert_eq!(
                    arguments.timeout.map(|timeout| timeout.get()),
                    Some(std::time::Duration::from_secs(9))
                );
                assert!(arguments.force);
            }
            _ => panic!("expected stop command"),
        }

        let restart = Cli::try_parse_from(["firestone", "restart", "dev"])?;
        assert!(matches!(
            restart.command,
            Command::Restart(arguments) if arguments.name == "dev"
        ));

        let remove = Cli::try_parse_from(["firestone", "rm", "one", "two", "--force"])?;
        match remove.command {
            Command::Remove(arguments) => {
                assert_eq!(arguments.names, ["one", "two"]);
                assert!(arguments.force);
            }
            _ => panic!("expected rm command"),
        }

        let defaults = Cli::try_parse_from(["firestone", "start", "dev"])?;
        match defaults.command {
            Command::Start(arguments) => {
                assert!(!arguments.no_wait);
                assert!(arguments.timeout.is_none());
            }
            _ => panic!("expected start command"),
        }
        Ok(())
    }

    #[test]
    fn logs_accepts_follow_source_and_line_boundaries() -> Result<(), clap::Error> {
        let defaults = Cli::try_parse_from(["firestone", "logs", "dev"])?;
        match defaults.command {
            Command::Logs(arguments) => {
                assert_eq!(arguments.name, "dev");
                assert_eq!(arguments.source, firestone_core::LogSource::Console);
                assert_eq!(arguments.lines, 200);
                assert!(!arguments.follow);
            }
            _ => panic!("expected logs command"),
        }

        let selected = Cli::try_parse_from([
            "firestone",
            "logs",
            "dev",
            "-f",
            "--source",
            "virtiofsd-3",
            "-n",
            "0",
        ])?;
        match selected.command {
            Command::Logs(arguments) => {
                assert!(arguments.follow);
                assert_eq!(arguments.source, firestone_core::LogSource::Virtiofsd(3));
                assert_eq!(arguments.lines, 0);
            }
            _ => panic!("expected logs command"),
        }
        Ok(())
    }

    #[test]
    fn images_commands_capture_every_operation() -> Result<(), clap::Error> {
        let list = Cli::try_parse_from(["firestone", "images", "ls"])?;
        assert!(matches!(
            list.command,
            Command::Images(arguments) if matches!(arguments.command, ImageCommand::List)
        ));

        let pull = Cli::try_parse_from([
            "firestone",
            "images",
            "pull",
            "https://images.example/base.qcow2",
            "--sha256",
            "abcd",
        ])?;
        match pull.command {
            Command::Images(arguments) => match arguments.command {
                ImageCommand::Pull(arguments) => {
                    assert_eq!(arguments.reference, "https://images.example/base.qcow2");
                    assert_eq!(arguments.sha256.as_deref(), Some("abcd"));
                }
                _ => panic!("expected images pull command"),
            },
            _ => panic!("expected images command"),
        }

        let inspect = Cli::try_parse_from(["firestone", "images", "inspect", "image-id"])?;
        assert!(matches!(
            inspect.command,
            Command::Images(arguments)
                if matches!(&arguments.command, ImageCommand::Inspect(arguments) if arguments.id == "image-id")
        ));

        let remove = Cli::try_parse_from(["firestone", "images", "rm", "image-id", "--force"])?;
        assert!(matches!(
            remove.command,
            Command::Images(arguments)
                if matches!(&arguments.command, ImageCommand::Remove(arguments) if arguments.id == "image-id" && arguments.force)
        ));

        let prune = Cli::try_parse_from(["firestone", "images", "prune"])?;
        assert!(matches!(
            prune.command,
            Command::Images(arguments) if matches!(arguments.command, ImageCommand::Prune)
        ));
        Ok(())
    }

    #[test]
    fn lifecycle_parser_rejects_missing_invalid_and_out_of_scope_flags() {
        let missing =
            Cli::try_parse_from(["firestone", "rm"]).expect_err("rm without a name must fail");
        assert_eq!(missing.kind(), ErrorKind::MissingRequiredArgument);

        let source = Cli::try_parse_from(["firestone", "logs", "dev", "--source", "../console"])
            .expect_err("unsafe source must fail");
        assert_eq!(source.kind(), ErrorKind::ValueValidation);

        let lines = Cli::try_parse_from(["firestone", "logs", "dev", "-n", "100001"])
            .expect_err("unbounded line count must fail");
        assert_eq!(lines.kind(), ErrorKind::ValueValidation);

        let restart_timeout =
            Cli::try_parse_from(["firestone", "restart", "dev", "--timeout", "5s"])
                .expect_err("restart has no timeout flag");
        assert_eq!(restart_timeout.kind(), ErrorKind::UnknownArgument);

        let missing_pull = Cli::try_parse_from(["firestone", "images", "pull"])
            .expect_err("images pull requires a reference");
        assert_eq!(missing_pull.kind(), ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn m1_short_and_long_help_contracts_do_not_drift() {
        let command = Cli::command();
        let names = command
            .get_subcommands()
            .map(clap::Command::get_name)
            .collect::<BTreeSet<_>>();
        for name in ["start", "stop", "restart", "rm", "logs", "images", "show"] {
            assert!(names.contains(name), "missing {name} command");
        }

        let logs = command
            .get_subcommands()
            .find(|command| command.get_name() == "logs")
            .expect("logs command");
        assert_eq!(
            logs.get_arguments()
                .find(|argument| argument.get_long() == Some("follow"))
                .and_then(clap::Arg::get_short),
            Some('f')
        );
        let lines = logs
            .get_arguments()
            .find(|argument| argument.get_short() == Some('n'))
            .expect("-n argument");
        assert_eq!(lines.get_long(), None);
    }
    #[test]
    fn create_resolves_all_supported_image_forms() -> Result<(), clap::Error> {
        let derived = create_request(&["firestone", "create", "ubuntu:24.04"])?;
        assert_eq!(derived.name, "ubuntu");
        assert_eq!(derived.image.as_str(), "ubuntu:24.04");

        let named = create_request(&["firestone", "create", "dev", "debian:12"])?;
        assert_eq!(named.name, "dev");
        assert_eq!(named.image.as_str(), "debian:12");

        let flagged = create_request(&["firestone", "create", "--image", "fedora:latest"])?;
        assert_eq!(flagged.name, "fedora");
        assert_eq!(flagged.image.as_str(), "fedora:latest");

        let named_flagged =
            create_request(&["firestone", "create", "sandbox", "--image", "ubuntu:22.04"])?;
        assert_eq!(named_flagged.name, "sandbox");
        assert_eq!(named_flagged.image.as_str(), "ubuntu:22.04");
        let local = create_request(&["firestone", "create", "/var/lib/firestone/dev.qcow2"])?;
        assert_eq!(local.name, "dev");

        let url = create_request(&[
            "firestone",
            "create",
            "--image",
            "https://images.example/noble.qcow2?download=1",
        ])?;
        assert_eq!(url.name, "noble");
        Ok(())
    }

    #[test]
    fn create_rejects_missing_and_duplicate_images_with_stable_messages() {
        let missing = create_request(&["firestone", "create"]).expect_err("missing image");
        assert_eq!(missing.kind(), ErrorKind::MissingRequiredArgument);
        assert_eq!(
            missing.to_string(),
            "error: image is required; pass IMAGE positionally or with '--image'"
        );

        let duplicate =
            create_request(&["firestone", "create", "dev", "ubuntu", "--image", "debian"])
                .expect_err("duplicate image");
        assert_eq!(duplicate.kind(), ErrorKind::ArgumentConflict);
        assert_eq!(
            duplicate.to_string(),
            "error: image cannot be supplied both positionally and with '--image'"
        );
    }

    #[test]
    fn create_converts_every_spec_value_to_a_sparse_patch() -> Result<(), clap::Error> {
        let request = create_request(&[
            "firestone",
            "create",
            "dev",
            "ubuntu:24.04",
            "--arch",
            "x86_64",
            "--cpus",
            "4",
            "--memory",
            "4G",
            "--disk",
            "40G",
            "--user",
            "alice",
            "--net",
            "tap",
            "-p",
            "8080:80",
            "--tap",
            "tap0",
            "--network-mac",
            "52:54:00:9a:1f:c3",
            "--mount",
            "/srv/project:/work:ro",
            "--user-data",
            "user-data.yaml",
            "--cloud-init-network-config",
            "network.yaml",
            "--ssh-key",
            "id_one.pub",
            "--ssh-key",
            "id_two.pub",
            "--no-provisioning",
            "--vmm-binary",
            "/opt/cloud-hypervisor",
            "--vmm-firmware",
            "edk2",
            "--vmm-arg=--serial",
            "--vmm-arg",
            "tty",
            "--vmm-config",
            r#"{"cpus":{"boot_vcpus":4},"payload":null}"#,
            "-f",
            "base.toml",
            "--edit",
        ])?;

        assert_eq!(request.name, "dev");
        assert_eq!(request.image.as_str(), "ubuntu:24.04");
        assert_eq!(request.file, Some(PathBuf::from("base.toml")));
        assert!(request.edit);
        assert!(request.patch.image.is_none());
        assert_eq!(request.patch.arch, Some(Arch::X86_64));
        assert_eq!(request.patch.cpus, Some(4));
        assert_eq!(
            request.patch.memory,
            Some("4G".parse::<ByteSize>().expect("size"))
        );
        assert_eq!(
            request.patch.disk,
            Some("40G".parse::<ByteSize>().expect("size"))
        );
        assert_eq!(request.patch.user.as_deref(), Some("alice"));

        let network = request.patch.network.expect("network patch");
        assert_eq!(network.mode, Some(NetMode::Tap));
        assert_eq!(
            network
                .forward
                .expect("forwards")
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            ["8080:80"]
        );
        assert_eq!(network.tap.as_deref(), Some("tap0"));
        assert_eq!(
            network.mac.map(|address| address.to_string()).as_deref(),
            Some("52:54:00:9a:1f:c3")
        );

        let mounts = request.patch.mounts.expect("mount patch");
        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0].host, PathBuf::from("/srv/project"));
        assert_eq!(mounts[0].guest, PathBuf::from("/work"));
        assert!(mounts[0].readonly);
        assert!(mounts[0].tag.is_none());

        let cloud_init = request.patch.cloud_init.expect("cloud-init patch");
        assert_eq!(cloud_init.user_data, Some(PathBuf::from("user-data.yaml")));
        assert_eq!(
            cloud_init.network_config,
            Some(PathBuf::from("network.yaml"))
        );
        assert_eq!(
            cloud_init.ssh_keys,
            Some(vec![
                PathBuf::from("id_one.pub"),
                PathBuf::from("id_two.pub")
            ])
        );
        assert_eq!(cloud_init.provisioning, Some(false));

        let vmm = request.patch.vmm.expect("VMM patch");
        assert_eq!(vmm.binary, Some(PathBuf::from("/opt/cloud-hypervisor")));
        assert_eq!(vmm.firmware, Some(Firmware::EDK2));
        assert_eq!(
            vmm.extra_args,
            Some(vec!["--serial".to_owned(), "tty".to_owned()])
        );
        assert_eq!(
            vmm.config_overlay,
            Some(json!({"cpus": {"boot_vcpus": 4}, "payload": null}))
        );
        Ok(())
    }

    #[test]
    fn repeatable_clear_is_typed_and_preserves_order() -> Result<(), clap::Error> {
        let request = create_request(&[
            "firestone",
            "create",
            "ubuntu",
            "--clear",
            "arch",
            "--clear",
            "network.forward",
        ])?;
        assert_eq!(
            request.patch.clear,
            [SpecClear::Arch, SpecClear::NetworkForward]
        );

        let invalid = Cli::try_parse_from([
            "firestone",
            "create",
            "ubuntu",
            "--clear",
            "network.unknown",
        ])
        .expect_err("unknown clear path");
        assert_eq!(invalid.kind(), ErrorKind::ValueValidation);
        assert!(
            invalid
                .to_string()
                .contains("unknown clear field 'network.unknown'")
        );
        Ok(())
    }

    #[test]
    fn every_architecture_and_network_mode_is_accepted() -> Result<(), clap::Error> {
        for (value, expected) in [("x86_64", Arch::X86_64), ("aarch64", Arch::Aarch64)] {
            let request = create_request(&["firestone", "create", "ubuntu", "--arch", value])?;
            assert_eq!(request.patch.arch, Some(expected));
        }

        for (value, expected) in [
            ("passt", NetMode::Passt),
            ("tap", NetMode::Tap),
            ("none", NetMode::None),
        ] {
            let request = create_request(&["firestone", "create", "ubuntu", "--net", value])?;
            assert_eq!(
                request.patch.network.and_then(|network| network.mode),
                Some(expected)
            );
        }
        Ok(())
    }

    #[test]
    fn architecture_and_network_mode_are_closed_values() {
        let invalid_arch =
            Cli::try_parse_from(["firestone", "create", "ubuntu", "--arch", "riscv64"])
                .expect_err("unsupported architecture");
        assert_eq!(invalid_arch.kind(), ErrorKind::ValueValidation);
        assert!(
            invalid_arch
                .to_string()
                .contains("architecture must be 'x86_64' or 'aarch64'")
        );

        let invalid_net = Cli::try_parse_from(["firestone", "create", "ubuntu", "--net", "bridge"])
            .expect_err("unsupported network mode");
        assert_eq!(invalid_net.kind(), ErrorKind::ValueValidation);
        assert!(
            invalid_net
                .to_string()
                .contains("network mode must be 'passt', 'tap', or 'none'")
        );
    }

    #[test]
    fn mount_requires_two_paths_and_optional_ro() {
        for value in ["host", "host:guest:rw", ":guest", "host:", "a:b:ro:extra"] {
            let error = Cli::try_parse_from(["firestone", "create", "ubuntu", "--mount", value])
                .expect_err("invalid mount");
            assert_eq!(error.kind(), ErrorKind::ValueValidation, "accepted {value}");
        }
    }

    #[test]
    fn mount_without_ro_is_writable() -> Result<(), clap::Error> {
        let request = create_request(&["firestone", "create", "ubuntu", "--mount", "host:guest"])?;
        let mounts = request.patch.mounts.expect("mount patch");
        assert_eq!(mounts.len(), 1);
        assert!(!mounts[0].readonly);
        Ok(())
    }

    #[test]
    fn vmm_config_accepts_only_json_objects() -> Result<(), clap::Error> {
        let empty = create_request(&["firestone", "create", "ubuntu", "--vmm-config", "{}"])?;
        assert_eq!(
            empty.patch.vmm.and_then(|vmm| vmm.config_overlay),
            Some(json!({}))
        );

        for value in ["[]", "null", "true", "\"text\""] {
            let error =
                Cli::try_parse_from(["firestone", "create", "ubuntu", "--vmm-config", value])
                    .expect_err("non-object JSON");
            assert_eq!(error.kind(), ErrorKind::ValueValidation, "accepted {value}");
            assert!(
                error
                    .to_string()
                    .contains("--vmm-config must be a JSON object")
            );
        }
        Ok(())
    }

    #[test]
    fn create_spec_flags_match_core_metadata() {
        let command = CreateArgs::augment_args(clap::Command::new("create"));
        let controls = BTreeSet::from(["edit", "file", "help"]);
        let actual = command
            .get_arguments()
            .filter_map(|argument| argument.get_long())
            .filter(|long| !controls.contains(long))
            .collect::<BTreeSet<_>>();

        let mut expected = SPEC_FIELD_METADATA
            .iter()
            .map(|field| field.long)
            .collect::<BTreeSet<_>>();
        assert!(expected.insert("clear"));
        assert_eq!(actual, expected);

        for field in SPEC_FIELD_METADATA {
            let actual_short = command
                .get_arguments()
                .find(|argument| argument.get_long() == Some(field.long))
                .and_then(clap::Arg::get_short);
            assert_eq!(
                actual_short, field.short,
                "short flag drift for {}",
                field.key
            );
        }
    }

    #[test]
    fn clap_definition_is_internally_consistent() {
        Cli::command().debug_assert();
    }
}
