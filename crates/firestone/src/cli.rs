use std::{
    ffi::OsString,
    net::SocketAddr,
    path::{Path, PathBuf},
};

use clap::{ArgAction, Args, Parser, Subcommand, error::ErrorKind};
use clap_complete::Shell;
use firestone_core::{
    Arch, ByteSize, CloudInitSpecPatch, Firmware, HumanDuration, ImageRef, LogSource, MacAddr,
    MachineSpecPatch, MountSpec, NetMode, NetworkSpecPatch, PortForward, SpecClear, VmmSpecPatch,
    VsockPort,
};
/// Firestone's command-line interface.
#[derive(Debug, Parser)]
#[command(name = "firestone", version)]
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

/// Commands implemented by the Firestone CLI.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create or reuse a machine, start it, and open an SSH shell.
    Run(Box<RunArgs>),

    /// Create a machine definition without booting it.
    Create(Box<CreateArgs>),

    /// Start a machine and wait for SSH readiness.
    Start(StartArgs),

    /// Stop a machine.
    Stop(StopArgs),

    /// Stop and start a machine.
    Restart(RestartArgs),

    /// Change a machine's CPU count or memory.
    Resize(ResizeArgs),

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

    /// Open SSH over the machine's private vsock transport.
    #[command(visible_alias = "ssh")]
    Shell(ShellArgs),

    /// Copy files between the host and a machine over the vsock transport.
    Cp(CpArgs),

    /// Print an OpenSSH Host block for the machine.
    #[command(name = "ssh-config")]
    SshConfig(SshConfigArgs),

    /// Attach to the machine's hvc0 console.
    Console(ConsoleArgs),

    /// Print a bounded machine log.
    Logs(LogsArgs),

    /// Print one cumulative resource sample for a running machine.
    Metrics(MetricsArgs),

    /// Print the merged built-in and user image catalog.
    Catalog,

    /// Manage the owned image store.
    Images(ImagesArgs),

    /// Check host requirements and optional safe repairs.
    Doctor(DoctorArgs),

    /// Generate a shell completion script on stdout.
    Completions(CompletionsArgs),

    /// Print Firestone, pinned dependency, and resolved path versions.
    Version,

    /// Run the stateless REST API over a private Unix socket or loopback port.
    Serve(ServeArgs),

    /// Open the Firestone web interface on a loopback port.
    Ui(UiArgs),

    /// Copy a stopped machine's spec and disk to a new machine.
    Clone(CloneArgs),
}

/// Arguments accepted by firestone clone.
#[derive(Debug, Args)]
pub struct CloneArgs {
    /// Existing stopped or created machine to copy.
    #[arg(value_name = "SRC")]
    pub source: String,

    /// New machine name.
    #[arg(value_name = "DEST")]
    pub dest: String,

    /// Give the clone an empty overlay on the same base image.
    #[arg(long)]
    pub fresh_disk: bool,
}
/// Arguments accepted by firestone completions.
#[derive(Debug, Args)]
pub struct CompletionsArgs {
    /// Shell whose completion script should be generated.
    #[arg(value_enum)]
    pub shell: Shell,
}

/// Where `firestone serve` publishes its listener.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListenAddress {
    /// A Unix socket inside Firestone's private runtime directory.
    Unix(PathBuf),
    /// A loopback TCP address. Only `127.0.0.1` and `::1` are accepted.
    Tcp(SocketAddr),
}

/// Arguments accepted by firestone serve.
#[derive(Debug, Args)]
pub struct ServeArgs {
    /// Listen at a private Unix socket, or at a loopback TCP address.
    #[arg(
        long,
        value_name = "unix:PATH|tcp:HOST:PORT",
        value_parser = parse_listen_address
    )]
    pub listen: Option<ListenAddress>,

    /// File holding the 64-hexadecimal-character session token. TCP only.
    #[arg(long, value_name = "FILE")]
    pub token: Option<PathBuf>,
}

/// Arguments accepted by firestone ui.
#[derive(Debug, Args)]
pub struct UiArgs {
    /// Loopback port to bind. Zero asks the kernel for any free port.
    #[arg(long, value_name = "PORT", default_value_t = 0)]
    pub port: u16,

    /// Do not launch a browser; print the URL only.
    #[arg(long)]
    pub no_open: bool,

    /// Print the URL and never launch a browser. Implies --no-open.
    #[arg(long)]
    pub print_url: bool,
}

fn parse_listen_address(value: &str) -> Result<ListenAddress, String> {
    if let Some(path) = value.strip_prefix("unix:") {
        if path.is_empty() {
            return Err("unix listener path cannot be empty".to_owned());
        }
        return Ok(ListenAddress::Unix(PathBuf::from(path)));
    }
    let Some(address) = value.strip_prefix("tcp:") else {
        return Err("listener must use the unix:PATH or tcp:HOST:PORT form".to_owned());
    };
    let address: SocketAddr = address.parse().map_err(|_| {
        "tcp listener must be an IP literal and port, such as tcp:127.0.0.1:8080".to_owned()
    })?;
    if !address.ip().is_loopback() {
        return Err(format!(
            "tcp listener address '{address}' is not a loopback address; use tcp:127.0.0.1:PORT"
        ));
    }
    Ok(ListenAddress::Tcp(address))
}

/// Arguments accepted by firestone run.
#[derive(Debug, Args)]
pub struct RunArgs {
    /// Existing machine name or image reference. Defaults to ubuntu.
    #[arg(value_name = "IMAGE|NAME")]
    pub target: Option<String>,

    /// Name a machine created from an image reference.
    #[arg(long, value_name = "NAME")]
    pub name: Option<String>,

    /// Remove a machine created by this invocation after SSH exits.
    #[arg(long = "rm")]
    pub remove: bool,

    #[command(flatten)]
    pub spec: SpecArgs,

    /// Remote command. Values are passed to OpenSSH without retokenizing.
    #[arg(last = true, value_name = "CMD")]
    pub command: Vec<OsString>,
}

/// Arguments accepted by firestone shell.
#[derive(Debug, Args)]
pub struct ShellArgs {
    pub name: String,

    /// Select the guest login user.
    #[arg(long, value_name = "USER")]
    pub user: Option<String>,

    /// Remote command. Values are passed to OpenSSH without retokenizing.
    #[arg(last = true, value_name = "CMD")]
    pub command: Vec<OsString>,
}

/// Arguments accepted by firestone cp.
#[derive(Debug, Args)]
pub struct CpArgs {
    /// Copy directories recursively.
    #[arg(short = 'r', long = "recursive")]
    pub recursive: bool,

    /// Source operand. Exactly one operand is remote, written `<machine>:<path>`.
    #[arg(value_name = "SRC")]
    pub source: String,

    /// Destination operand. Exactly one operand is remote, written `<machine>:<path>`.
    #[arg(value_name = "DST")]
    pub target: String,
}

/// Arguments accepted by firestone ssh-config.
#[derive(Debug, Args)]
pub struct SshConfigArgs {
    pub name: String,
}

/// Arguments accepted by firestone console.
#[derive(Debug, Args)]
pub struct ConsoleArgs {
    pub name: String,
}

/// Arguments accepted by firestone start.
#[derive(Debug, Args)]
pub struct StartArgs {
    pub name: String,

    /// Return immediately after the VMM reaches persisted running state.
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

#[derive(Debug, Parser)]
#[command(name = "firestone", disable_help_subcommand = true)]
struct HiddenProxyCli {
    #[arg(long, global = true, value_name = "DIR")]
    home: Option<PathBuf>,

    #[command(subcommand)]
    command: HiddenProxyCommand,
}

#[derive(Debug, Subcommand)]
enum HiddenProxyCommand {
    #[command(name = "_vsock-proxy")]
    VsockProxy(VsockProxyArgs),
}

pub fn parse_hidden_vsock_proxy(
    arguments: Vec<OsString>,
) -> Result<(Option<PathBuf>, VsockProxyArgs), clap::Error> {
    let parsed = HiddenProxyCli::try_parse_from(arguments)?;
    match parsed.command {
        HiddenProxyCommand::VsockProxy(arguments) => Ok((parsed.home, arguments)),
    }
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

/// Arguments accepted by firestone resize.
#[derive(Debug, Args)]
#[command(
    after_help = "A running machine is resized live only within the cpus_max and memory_max\nheadroom it booted with. Otherwise the values are written to the spec and\napply on the next start."
)]
pub struct ResizeArgs {
    pub name: String,

    /// Set the number of virtual CPUs.
    #[arg(long, value_name = "COUNT")]
    pub cpus: Option<u8>,

    /// Set guest memory, for example 4G or 4096M.
    #[arg(long, value_name = "SIZE")]
    pub memory: Option<ByteSize>,
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

/// Arguments accepted by firestone metrics.
#[derive(Debug, Args)]
pub struct MetricsArgs {
    pub name: String,
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
    /// Apply safe repairs; AppArmor elevation needs a TTY prompt and ignores --yes.
    #[arg(long)]
    pub fix: bool,
}
/// Arguments accepted by `firestone create` before positional resolution.
#[derive(Debug, Args)]
#[command(
    after_help = "On a terminal, create prompts for the image, name, CPU, memory, disk, and network.\nPass --yes to use only arguments and configured defaults.\n\nExamples:\n  firestone create ubuntu\n  firestone create dev ubuntu:24.04 --cpus 4 --memory 8G\n  firestone create dev --image debian:12 --net none"
)]
pub struct CreateArgs {
    /// Set IMAGE, or set both NAME and IMAGE.
    #[arg(value_name = "NAME_OR_IMAGE", num_args = 0..=2)]
    pub positional: Vec<String>,

    /// Select the image by catalog reference, HTTPS URL, or local file.
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

/// Unresolved create inputs used by the interactive wizard.
#[derive(Debug)]
pub(crate) struct CreateDraft {
    pub(crate) name: Option<String>,
    pub(crate) image: Option<ImageRef>,
    pub(crate) patch: MachineSpecPatch,
    pub(crate) file: Option<PathBuf>,
    pub(crate) edit: bool,
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
        self.into_draft()?.into_request()
    }

    /// Preserves an omitted target so the terminal wizard can ask for it.
    pub(crate) fn into_draft(self) -> Result<CreateDraft, clap::Error> {
        let Self {
            positional,
            image,
            file,
            edit,
            spec,
        } = self;
        let (name, image) = resolve_create_inputs(positional, image)?;

        Ok(CreateDraft {
            name,
            image,
            patch: spec.into_patch(),
            file,
            edit,
        })
    }
}

impl CreateDraft {
    pub(crate) fn into_request(self) -> Result<CreateRequest, clap::Error> {
        let Self {
            name,
            image,
            patch,
            file,
            edit,
        } = self;
        let image = image.ok_or_else(|| {
            clap::Error::raw(
                ErrorKind::MissingRequiredArgument,
                "image is required; pass IMAGE positionally or with '--image'",
            )
        })?;
        let name = match name {
            Some(name) => name,
            None => derive_machine_name(&image)?,
        };
        Ok(CreateRequest {
            name,
            image,
            patch,
            file,
            edit,
        })
    }

    pub(crate) fn with_target(self, name: String, image: ImageRef) -> CreateRequest {
        CreateRequest {
            name,
            image,
            patch: self.patch,
            file: self.file,
            edit: self.edit,
        }
    }
}
/// Clap's projection of every CLI-settable `MachineSpecPatch` leaf except image.
///
/// Image has context-sensitive positional behavior and therefore lives on
/// [`CreateArgs`].
#[derive(Debug, Default, Args)]
pub struct SpecArgs {
    /// Set the guest architecture; it must match the host.
    #[arg(long, value_name = "ARCH", value_parser = parse_arch)]
    pub arch: Option<Arch>,

    /// Set the number of virtual CPUs.
    #[arg(long, value_name = "COUNT")]
    pub cpus: Option<u8>,

    /// Reserve vCPU hotplug headroom for `resize`; must be at least --cpus.
    #[arg(long = "cpus-max", value_name = "COUNT")]
    pub cpus_max: Option<u8>,

    /// Set guest memory, for example 2G or 2048M.
    #[arg(long, value_name = "SIZE")]
    pub memory: Option<ByteSize>,

    /// Reserve memory hotplug headroom for `resize`; must be at least --memory.
    #[arg(long = "memory-max", value_name = "SIZE")]
    pub memory_max: Option<ByteSize>,

    /// Set writable disk capacity, for example 20G.
    #[arg(long, value_name = "SIZE")]
    pub disk: Option<ByteSize>,

    /// Set the guest login user created by Firestone provisioning.
    #[arg(long, value_name = "USER")]
    pub user: Option<String>,

    /// Select passt, tap, or no network.
    #[arg(long, value_name = "MODE", value_parser = parse_net_mode)]
    pub net: Option<NetMode>,

    /// Forward a host port or range to the guest; repeat as needed.
    #[arg(short = 'p', long, value_name = "SPEC")]
    pub forward: Vec<PortForward>,

    /// Use an existing host tap interface with --net tap.
    #[arg(long, value_name = "DEV")]
    pub tap: Option<String>,

    /// Set a fixed guest network MAC address.
    #[arg(long = "network-mac", value_name = "MAC")]
    pub network_mac: Option<MacAddr>,

    /// Share a host directory with the guest; repeat as needed.
    #[arg(long, value_name = "HOST:GUEST[:ro]", value_parser = parse_mount)]
    pub mount: Vec<MountSpec>,

    /// Add a cloud-init user-data file.
    #[arg(long = "user-data", value_name = "FILE")]
    pub user_data: Option<PathBuf>,

    /// Add a cloud-init network-config file.
    #[arg(long = "cloud-init-network-config", value_name = "FILE")]
    pub cloud_init_network_config: Option<PathBuf>,

    /// Add an OpenSSH public-key file; repeat as needed.
    #[arg(long = "ssh-key", value_name = "FILE")]
    pub ssh_key: Vec<PathBuf>,

    /// Disable Firestone's built-in guest provisioning.
    #[arg(long = "no-provisioning")]
    pub no_provisioning: bool,

    /// Use a custom cloud-hypervisor executable.
    #[arg(long = "vmm-binary", value_name = "FILE")]
    pub vmm_binary: Option<PathBuf>,

    /// Select auto, rhf, edk2, or a firmware file.
    #[arg(long = "vmm-firmware", value_name = "FIRMWARE")]
    pub vmm_firmware: Option<Firmware>,

    /// Append one cloud-hypervisor argument; repeat as needed.
    #[arg(long = "vmm-arg", value_name = "ARG", allow_hyphen_values = true)]
    pub vmm_arg: Vec<String>,

    /// Merge a JSON object into the generated VMM configuration.
    #[arg(long = "vmm-config", value_name = "JSON", value_parser = parse_vmm_config)]
    pub vmm_config: Option<serde_json::Value>,

    /// Clear an inherited optional field; repeat as needed.
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
            cpus_max,
            memory,
            memory_max,
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
            cpus_max,
            memory,
            memory_max,
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

fn resolve_create_inputs(
    positional: Vec<String>,
    image_flag: Option<String>,
) -> Result<(Option<String>, Option<ImageRef>), clap::Error> {
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
        (None, None, None) => (None, None),
        (None, None, Some(image)) | (Some(image), None, None) => (None, Some(image)),
        (Some(name), None, Some(image)) | (Some(name), Some(image), None) => {
            (Some(name), Some(image))
        }
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

    Ok((explicit_name, image.map(ImageRef::from)))
}

pub(crate) fn derive_machine_name(image: &ImageRef) -> Result<String, clap::Error> {
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
    use std::{collections::BTreeSet, ffi::OsString, path::PathBuf};

    use clap::{Args as _, CommandFactory as _, Parser as _, ValueEnum as _, error::ErrorKind};
    use clap_complete::Shell;
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
    fn serve_grammar_accepts_unix_and_loopback_tcp_listener_addresses() -> Result<(), clap::Error> {
        let default = Cli::try_parse_from(["firestone", "serve"])?;
        assert!(matches!(
            default.command,
            Command::Serve(arguments) if arguments.listen.is_none() && arguments.token.is_none()
        ));

        let selected =
            Cli::try_parse_from(["firestone", "serve", "--listen", "unix:private.sock"])?;
        assert!(matches!(
            selected.command,
            Command::Serve(arguments)
                if arguments.listen
                    == Some(crate::cli::ListenAddress::Unix(PathBuf::from("private.sock")))
        ));

        let tcp = Cli::try_parse_from([
            "firestone",
            "serve",
            "--listen",
            "tcp:127.0.0.1:8080",
            "--token",
            "/run/firestone.token",
        ])?;
        assert!(matches!(
            tcp.command,
            Command::Serve(arguments)
                if arguments.listen
                    == Some(crate::cli::ListenAddress::Tcp(
                        "127.0.0.1:8080".parse().expect("loopback address")
                    ))
                    && arguments.token == Some(PathBuf::from("/run/firestone.token"))
        ));

        let ipv6 = Cli::try_parse_from([
            "firestone",
            "serve",
            "--listen",
            "tcp:[::1]:8080",
            "--token",
            "/run/firestone.token",
        ])?;
        assert!(matches!(
            ipv6.command,
            Command::Serve(arguments)
                if arguments.listen
                    == Some(crate::cli::ListenAddress::Tcp(
                        "[::1]:8080".parse().expect("loopback address")
                    ))
        ));
        Ok(())
    }

    #[test]
    fn serve_grammar_rejects_routable_and_wildcard_tcp_listener_addresses() {
        for address in [
            "tcp:0.0.0.0:8080",
            "tcp:[::]:8080",
            "tcp:192.168.1.10:8080",
            "tcp:localhost:8080",
            "tcp:127.0.0.1",
            "http:127.0.0.1:8080",
        ] {
            let error = Cli::try_parse_from([
                "firestone",
                "serve",
                "--listen",
                address,
                "--token",
                "/run/firestone.token",
            ])
            .expect_err("only loopback IP literals may be bound");
            assert_eq!(error.kind(), ErrorKind::ValueValidation, "{address}");
        }
    }

    #[test]
    fn serve_grammar_parses_token_flag_for_later_listener_validation() -> Result<(), clap::Error> {
        // The token/listener pairing is a semantic rule, not a clap rule, so
        // both mismatched forms must parse and be refused with a usage error
        // by the command wiring instead.
        let unix_with_token = Cli::try_parse_from([
            "firestone",
            "serve",
            "--listen",
            "unix:private.sock",
            "--token",
            "/run/firestone.token",
        ])?;
        assert!(matches!(
            unix_with_token.command,
            Command::Serve(arguments) if arguments.token.is_some()
        ));

        let tcp_without_token =
            Cli::try_parse_from(["firestone", "serve", "--listen", "tcp:127.0.0.1:8080"])?;
        assert!(matches!(
            tcp_without_token.command,
            Command::Serve(arguments) if arguments.token.is_none()
        ));
        Ok(())
    }

    #[test]
    fn ui_grammar_defaults_to_an_ephemeral_port_and_opens_a_browser() -> Result<(), clap::Error> {
        let default = Cli::try_parse_from(["firestone", "ui"])?;
        assert!(matches!(
            default.command,
            Command::Ui(arguments)
                if arguments.port == 0 && !arguments.no_open && !arguments.print_url
        ));

        let selected = Cli::try_parse_from(["firestone", "ui", "--port", "8080", "--no-open"])?;
        assert!(matches!(
            selected.command,
            Command::Ui(arguments) if arguments.port == 8080 && arguments.no_open
        ));

        let printed = Cli::try_parse_from(["firestone", "ui", "--print-url"])?;
        assert!(matches!(
            printed.command,
            Command::Ui(arguments) if arguments.print_url
        ));

        let overflow = Cli::try_parse_from(["firestone", "ui", "--port", "70000"])
            .expect_err("a port must fit in sixteen bits");
        assert_eq!(overflow.kind(), ErrorKind::ValueValidation);
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
    fn run_grammar_preserves_target_flags_and_post_separator_argv() -> Result<(), clap::Error> {
        let cli = Cli::try_parse_from([
            "firestone",
            "run",
            "ubuntu:24.04",
            "--name",
            "dev",
            "--rm",
            "--cpus",
            "4",
            "--user",
            "builder",
            "--",
            "printf one argument",
            "--remote-flag",
        ])?;
        match cli.command {
            Command::Run(arguments) => {
                assert_eq!(arguments.target.as_deref(), Some("ubuntu:24.04"));
                assert_eq!(arguments.name.as_deref(), Some("dev"));
                assert!(arguments.remove);
                assert_eq!(arguments.spec.cpus, Some(4));
                assert_eq!(arguments.spec.user.as_deref(), Some("builder"));
                assert_eq!(
                    arguments.command,
                    vec![
                        OsString::from("printf one argument"),
                        OsString::from("--remote-flag"),
                    ]
                );
            }
            _ => panic!("expected run command"),
        }
        Ok(())
    }

    #[test]
    fn shell_alias_and_separator_preserve_remote_argv() -> Result<(), clap::Error> {
        let cli = Cli::try_parse_from([
            "firestone",
            "ssh",
            "dev",
            "--user",
            "root",
            "--",
            "echo two words",
            "-n",
        ])?;
        match cli.command {
            Command::Shell(arguments) => {
                assert_eq!(arguments.name, "dev");
                assert_eq!(arguments.user.as_deref(), Some("root"));
                assert_eq!(
                    arguments.command,
                    vec![OsString::from("echo two words"), OsString::from("-n")]
                );
            }
            _ => panic!("expected shell command"),
        }
        let missing_separator = Cli::try_parse_from(["firestone", "shell", "dev", "echo"])
            .expect_err("remote command without -- must fail");
        assert!(matches!(
            missing_separator.kind(),
            ErrorKind::UnknownArgument | ErrorKind::TooManyValues
        ));
        Ok(())
    }

    #[test]
    fn cp_grammar_accepts_recursive_and_two_positional_operands() -> Result<(), clap::Error> {
        let cli = Cli::try_parse_from(["firestone", "cp", "-r", "./notes", "dev:/srv/notes"])?;
        match cli.command {
            Command::Cp(arguments) => {
                assert!(arguments.recursive);
                assert_eq!(arguments.source, "./notes");
                assert_eq!(arguments.target, "dev:/srv/notes");
            }
            _ => panic!("expected cp command"),
        }

        let long = Cli::try_parse_from(["firestone", "cp", "--recursive", "dev:/a", "./b"])?;
        match long.command {
            Command::Cp(arguments) => {
                assert!(arguments.recursive);
                assert_eq!(arguments.source, "dev:/a");
                assert_eq!(arguments.target, "./b");
            }
            _ => panic!("expected cp command"),
        }

        let plain = Cli::try_parse_from(["firestone", "cp", "dev:/a", "./b"])?;
        match plain.command {
            Command::Cp(arguments) => assert!(!arguments.recursive),
            _ => panic!("expected cp command"),
        }

        let missing = Cli::try_parse_from(["firestone", "cp", "dev:/a"])
            .expect_err("cp requires two operands");
        assert_eq!(missing.kind(), ErrorKind::MissingRequiredArgument);
        let extra = Cli::try_parse_from(["firestone", "cp", "dev:/a", "./b", "./c"])
            .expect_err("cp accepts exactly two operands");
        assert_eq!(extra.kind(), ErrorKind::UnknownArgument);
        Ok(())
    }

    #[test]
    fn command_help_contracts_include_serve_and_ui() {
        let command = Cli::command();
        let names = command
            .get_subcommands()
            .map(clap::Command::get_name)
            .collect::<BTreeSet<_>>();
        for name in [
            "run",
            "create",
            "start",
            "stop",
            "restart",
            "rm",
            "ls",
            "show",
            "edit",
            "shell",
            "ssh-config",
            "console",
            "logs",
            "images",
            "doctor",
            "serve",
            "ui",
        ] {
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
    fn completions_parser_accepts_every_supported_shell() -> Result<(), clap::Error> {
        for shell in Shell::value_variants() {
            let value = shell.to_string();
            let cli = Cli::try_parse_from(["firestone", "completions", value.as_str()])?;
            assert!(matches!(
                cli.command,
                Command::Completions(arguments) if arguments.shell == *shell
            ));
        }
        Ok(())
    }

    #[test]
    fn completion_scripts_cover_public_grammar_and_exclude_internals() {
        fn assert_visible_grammar(command: &clap::Command, script: &str, shell: Shell) {
            for argument in command
                .get_arguments()
                .filter(|argument| !argument.is_hide_set())
            {
                if let Some(long) = argument.get_long() {
                    assert!(script.contains(long), "{shell} completion omitted --{long}");
                }
                if let Some(aliases) = argument.get_visible_aliases() {
                    for alias in aliases {
                        assert!(
                            script.contains(alias),
                            "{shell} completion omitted --{alias}"
                        );
                    }
                }
                if let Some(short) = argument.get_short() {
                    assert!(
                        script.contains(short),
                        "{shell} completion omitted -{short}"
                    );
                }
                if let Some(aliases) = argument.get_visible_short_aliases() {
                    for alias in aliases {
                        assert!(
                            script.contains(alias),
                            "{shell} completion omitted -{alias}"
                        );
                    }
                }
            }
            for subcommand in command
                .get_subcommands()
                .filter(|subcommand| !subcommand.is_hide_set())
            {
                assert!(
                    script.contains(subcommand.get_name()),
                    "{shell} completion omitted {}",
                    subcommand.get_name()
                );
                for alias in subcommand.get_visible_aliases() {
                    assert!(
                        script.contains(alias),
                        "{shell} completion omitted alias {alias}"
                    );
                }
                assert_visible_grammar(subcommand, script, shell);
            }
        }

        for shell in Shell::value_variants() {
            let mut command = Cli::command();
            let mut first = Vec::new();
            clap_complete::generate(*shell, &mut command, "firestone", &mut first);
            let mut command = Cli::command();
            let mut second = Vec::new();
            clap_complete::generate(*shell, &mut command, "firestone", &mut second);
            assert_eq!(first, second, "{shell} completion is not deterministic");
            let script = String::from_utf8(first).expect("completion output must be UTF-8");
            assert!(!script.contains("_shim"));
            assert!(!script.contains("_vsock-proxy"));
            let command = Cli::command();
            assert_visible_grammar(&command, &script, *shell);
        }
    }

    #[test]
    fn clap_definition_is_internally_consistent() {
        Cli::command().debug_assert();
    }
}
