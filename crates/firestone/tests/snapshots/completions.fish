# Print an optspec for argparse to handle cmd's options that are independent of any subcommand.
function __fish_firestone_global_optspecs
    string join \n json q/quiet v/verbose no-color y/yes home= h/help V/version
end

function __fish_firestone_needs_command
    # Figure out if the current invocation already has a command.
    set -l cmd (commandline -opc)
    set -e cmd[1]
    argparse -s (__fish_firestone_global_optspecs) -- $cmd 2>/dev/null
    or return
    if set -q argv[1]
        # Also print the command, so this can be used to figure out what it is.
        echo $argv[1]
        return 1
    end
    return 0
end

function __fish_firestone_using_subcommand
    set -l cmd (__fish_firestone_needs_command)
    test -z "$cmd"
    and return 1
    contains -- $cmd[1] $argv
end

complete -c firestone -n "__fish_firestone_needs_command" -l home -d 'Override the Firestone home root (config, data, and runtime)' -r -F
complete -c firestone -n "__fish_firestone_needs_command" -l json -d 'Print events as newline-delimited JSON and disable human output'
complete -c firestone -n "__fish_firestone_needs_command" -s q -l quiet -d 'Print only errors and command results'
complete -c firestone -n "__fish_firestone_needs_command" -s v -l verbose -d 'Increase log detail. Pass twice for debug output'
complete -c firestone -n "__fish_firestone_needs_command" -l no-color -d 'Disable colored output'
complete -c firestone -n "__fish_firestone_needs_command" -s y -l yes -d 'Assume yes when a command may prompt'
complete -c firestone -n "__fish_firestone_needs_command" -s h -l help -d 'Print help'
complete -c firestone -n "__fish_firestone_needs_command" -s V -l version -d 'Print version'
complete -c firestone -n "__fish_firestone_needs_command" -f -a "run" -d 'Create or reuse a machine, start it, and open an SSH shell'
complete -c firestone -n "__fish_firestone_needs_command" -f -a "create" -d 'Create a machine definition without booting it'
complete -c firestone -n "__fish_firestone_needs_command" -f -a "start" -d 'Start a machine and wait for SSH readiness'
complete -c firestone -n "__fish_firestone_needs_command" -f -a "stop" -d 'Stop a machine'
complete -c firestone -n "__fish_firestone_needs_command" -f -a "restart" -d 'Stop and start a machine'
complete -c firestone -n "__fish_firestone_needs_command" -f -a "resize" -d 'Change a machine\'s CPU count or memory'
complete -c firestone -n "__fish_firestone_needs_command" -f -a "rm" -d 'Stop and remove one or more machines'
complete -c firestone -n "__fish_firestone_needs_command" -f -a "ls" -d 'List machines'
complete -c firestone -n "__fish_firestone_needs_command" -f -a "list" -d 'List machines'
complete -c firestone -n "__fish_firestone_needs_command" -f -a "show" -d 'Show a machine\'s specification and runtime state'
complete -c firestone -n "__fish_firestone_needs_command" -f -a "edit" -d 'Edit and validate a machine\'s firestone.toml'
complete -c firestone -n "__fish_firestone_needs_command" -f -a "shell" -d 'Open SSH over the machine\'s private vsock transport'
complete -c firestone -n "__fish_firestone_needs_command" -f -a "ssh" -d 'Open SSH over the machine\'s private vsock transport'
complete -c firestone -n "__fish_firestone_needs_command" -f -a "cp" -d 'Copy files between the host and a machine over the vsock transport'
complete -c firestone -n "__fish_firestone_needs_command" -f -a "ssh-config" -d 'Print an OpenSSH Host block for the machine'
complete -c firestone -n "__fish_firestone_needs_command" -f -a "console" -d 'Attach to the machine\'s hvc0 console'
complete -c firestone -n "__fish_firestone_needs_command" -f -a "logs" -d 'Print a bounded machine log'
complete -c firestone -n "__fish_firestone_needs_command" -f -a "metrics" -d 'Print one cumulative resource sample for a running machine'
complete -c firestone -n "__fish_firestone_needs_command" -f -a "catalog" -d 'Print the merged built-in and user image catalog'
complete -c firestone -n "__fish_firestone_needs_command" -f -a "images" -d 'Manage the owned image store'
complete -c firestone -n "__fish_firestone_needs_command" -f -a "doctor" -d 'Check host requirements and optional safe repairs'
complete -c firestone -n "__fish_firestone_needs_command" -f -a "completions" -d 'Generate a shell completion script on stdout'
complete -c firestone -n "__fish_firestone_needs_command" -f -a "version" -d 'Print Firestone, pinned dependency, and resolved path versions'
complete -c firestone -n "__fish_firestone_needs_command" -f -a "serve" -d 'Run the stateless REST API over a private Unix socket or loopback port'
complete -c firestone -n "__fish_firestone_needs_command" -f -a "ui" -d 'Open the Firestone web interface on a loopback port'
complete -c firestone -n "__fish_firestone_needs_command" -f -a "clone" -d 'Copy a stopped machine\'s spec and disk to a new machine'
complete -c firestone -n "__fish_firestone_needs_command" -f -a "snapshot" -d 'Capture, list, restore, and remove machine snapshots'
complete -c firestone -n "__fish_firestone_needs_command" -f -a "system" -d 'Inspect and reclaim host-wide Firestone storage'
complete -c firestone -n "__fish_firestone_needs_command" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c firestone -n "__fish_firestone_using_subcommand run" -l name -d 'Name a machine created from an image reference' -r
complete -c firestone -n "__fish_firestone_using_subcommand run" -l arch -d 'Set the guest architecture; it must match the host' -r
complete -c firestone -n "__fish_firestone_using_subcommand run" -l cpus -d 'Set the number of virtual CPUs' -r
complete -c firestone -n "__fish_firestone_using_subcommand run" -l cpus-max -d 'Reserve vCPU hotplug headroom for `resize`; must be at least --cpus' -r
complete -c firestone -n "__fish_firestone_using_subcommand run" -l memory -d 'Set guest memory, for example 2G or 2048M' -r
complete -c firestone -n "__fish_firestone_using_subcommand run" -l memory-max -d 'Reserve memory hotplug headroom for `resize`; must be at least --memory' -r
complete -c firestone -n "__fish_firestone_using_subcommand run" -l disk -d 'Set writable disk capacity, for example 20G' -r
complete -c firestone -n "__fish_firestone_using_subcommand run" -l user -d 'Set the guest login user created by Firestone provisioning' -r
complete -c firestone -n "__fish_firestone_using_subcommand run" -l net -d 'Select passt, tap, or no network' -r
complete -c firestone -n "__fish_firestone_using_subcommand run" -s p -l forward -d 'Forward a host port or range to the guest; repeat as needed' -r
complete -c firestone -n "__fish_firestone_using_subcommand run" -l tap -d 'Use an existing host tap interface with --net tap' -r
complete -c firestone -n "__fish_firestone_using_subcommand run" -l network-mac -d 'Set a fixed guest network MAC address' -r
complete -c firestone -n "__fish_firestone_using_subcommand run" -l mount -d 'Share a host directory with the guest; repeat as needed' -r
complete -c firestone -n "__fish_firestone_using_subcommand run" -l user-data -d 'Add a cloud-init user-data file' -r -F
complete -c firestone -n "__fish_firestone_using_subcommand run" -l user-data-inline -d 'Set cloud-init user-data inline instead of from a file' -r
complete -c firestone -n "__fish_firestone_using_subcommand run" -l cloud-init-network-config -d 'Add a cloud-init network-config file' -r -F
complete -c firestone -n "__fish_firestone_using_subcommand run" -l ssh-key -d 'Add an OpenSSH public-key file; repeat as needed' -r -F
complete -c firestone -n "__fish_firestone_using_subcommand run" -l ssh-authorized-key -d 'Add an inline OpenSSH public key; repeat as needed' -r
complete -c firestone -n "__fish_firestone_using_subcommand run" -l password-file -d 'Read the guest password for --user from a file' -r
complete -c firestone -n "__fish_firestone_using_subcommand run" -l vmm-binary -d 'Use a custom cloud-hypervisor executable' -r -F
complete -c firestone -n "__fish_firestone_using_subcommand run" -l vmm-firmware -d 'Select auto, rhf, edk2, or a firmware file' -r
complete -c firestone -n "__fish_firestone_using_subcommand run" -l vmm-arg -d 'Append one cloud-hypervisor argument; repeat as needed' -r
complete -c firestone -n "__fish_firestone_using_subcommand run" -l vmm-config -d 'Merge a JSON object into the generated VMM configuration' -r
complete -c firestone -n "__fish_firestone_using_subcommand run" -l clear -d 'Clear an inherited optional field; repeat as needed' -r
complete -c firestone -n "__fish_firestone_using_subcommand run" -l home -d 'Override the Firestone home root (config, data, and runtime)' -r -F
complete -c firestone -n "__fish_firestone_using_subcommand run" -l rm -d 'Remove a machine created by this invocation after SSH exits'
complete -c firestone -n "__fish_firestone_using_subcommand run" -l ssh-pwauth -d 'Allow SSH password authentication in the guest'
complete -c firestone -n "__fish_firestone_using_subcommand run" -l no-provisioning -d 'Disable Firestone\'s built-in guest provisioning'
complete -c firestone -n "__fish_firestone_using_subcommand run" -l json -d 'Print events as newline-delimited JSON and disable human output'
complete -c firestone -n "__fish_firestone_using_subcommand run" -s q -l quiet -d 'Print only errors and command results'
complete -c firestone -n "__fish_firestone_using_subcommand run" -s v -l verbose -d 'Increase log detail. Pass twice for debug output'
complete -c firestone -n "__fish_firestone_using_subcommand run" -l no-color -d 'Disable colored output'
complete -c firestone -n "__fish_firestone_using_subcommand run" -s y -l yes -d 'Assume yes when a command may prompt'
complete -c firestone -n "__fish_firestone_using_subcommand run" -s h -l help -d 'Print help'
complete -c firestone -n "__fish_firestone_using_subcommand create" -l image -d 'Select the image by catalog reference, HTTPS URL, or local file' -r
complete -c firestone -n "__fish_firestone_using_subcommand create" -s f -l file -d 'Layer an existing machine specification below command-line flags' -r -F
complete -c firestone -n "__fish_firestone_using_subcommand create" -l arch -d 'Set the guest architecture; it must match the host' -r
complete -c firestone -n "__fish_firestone_using_subcommand create" -l cpus -d 'Set the number of virtual CPUs' -r
complete -c firestone -n "__fish_firestone_using_subcommand create" -l cpus-max -d 'Reserve vCPU hotplug headroom for `resize`; must be at least --cpus' -r
complete -c firestone -n "__fish_firestone_using_subcommand create" -l memory -d 'Set guest memory, for example 2G or 2048M' -r
complete -c firestone -n "__fish_firestone_using_subcommand create" -l memory-max -d 'Reserve memory hotplug headroom for `resize`; must be at least --memory' -r
complete -c firestone -n "__fish_firestone_using_subcommand create" -l disk -d 'Set writable disk capacity, for example 20G' -r
complete -c firestone -n "__fish_firestone_using_subcommand create" -l user -d 'Set the guest login user created by Firestone provisioning' -r
complete -c firestone -n "__fish_firestone_using_subcommand create" -l net -d 'Select passt, tap, or no network' -r
complete -c firestone -n "__fish_firestone_using_subcommand create" -s p -l forward -d 'Forward a host port or range to the guest; repeat as needed' -r
complete -c firestone -n "__fish_firestone_using_subcommand create" -l tap -d 'Use an existing host tap interface with --net tap' -r
complete -c firestone -n "__fish_firestone_using_subcommand create" -l network-mac -d 'Set a fixed guest network MAC address' -r
complete -c firestone -n "__fish_firestone_using_subcommand create" -l mount -d 'Share a host directory with the guest; repeat as needed' -r
complete -c firestone -n "__fish_firestone_using_subcommand create" -l user-data -d 'Add a cloud-init user-data file' -r -F
complete -c firestone -n "__fish_firestone_using_subcommand create" -l user-data-inline -d 'Set cloud-init user-data inline instead of from a file' -r
complete -c firestone -n "__fish_firestone_using_subcommand create" -l cloud-init-network-config -d 'Add a cloud-init network-config file' -r -F
complete -c firestone -n "__fish_firestone_using_subcommand create" -l ssh-key -d 'Add an OpenSSH public-key file; repeat as needed' -r -F
complete -c firestone -n "__fish_firestone_using_subcommand create" -l ssh-authorized-key -d 'Add an inline OpenSSH public key; repeat as needed' -r
complete -c firestone -n "__fish_firestone_using_subcommand create" -l password-file -d 'Read the guest password for --user from a file' -r
complete -c firestone -n "__fish_firestone_using_subcommand create" -l vmm-binary -d 'Use a custom cloud-hypervisor executable' -r -F
complete -c firestone -n "__fish_firestone_using_subcommand create" -l vmm-firmware -d 'Select auto, rhf, edk2, or a firmware file' -r
complete -c firestone -n "__fish_firestone_using_subcommand create" -l vmm-arg -d 'Append one cloud-hypervisor argument; repeat as needed' -r
complete -c firestone -n "__fish_firestone_using_subcommand create" -l vmm-config -d 'Merge a JSON object into the generated VMM configuration' -r
complete -c firestone -n "__fish_firestone_using_subcommand create" -l clear -d 'Clear an inherited optional field; repeat as needed' -r
complete -c firestone -n "__fish_firestone_using_subcommand create" -l home -d 'Override the Firestone home root (config, data, and runtime)' -r -F
complete -c firestone -n "__fish_firestone_using_subcommand create" -l edit -d 'Open the generated specification in the configured editor'
complete -c firestone -n "__fish_firestone_using_subcommand create" -l ssh-pwauth -d 'Allow SSH password authentication in the guest'
complete -c firestone -n "__fish_firestone_using_subcommand create" -l no-provisioning -d 'Disable Firestone\'s built-in guest provisioning'
complete -c firestone -n "__fish_firestone_using_subcommand create" -l json -d 'Print events as newline-delimited JSON and disable human output'
complete -c firestone -n "__fish_firestone_using_subcommand create" -s q -l quiet -d 'Print only errors and command results'
complete -c firestone -n "__fish_firestone_using_subcommand create" -s v -l verbose -d 'Increase log detail. Pass twice for debug output'
complete -c firestone -n "__fish_firestone_using_subcommand create" -l no-color -d 'Disable colored output'
complete -c firestone -n "__fish_firestone_using_subcommand create" -s y -l yes -d 'Assume yes when a command may prompt'
complete -c firestone -n "__fish_firestone_using_subcommand create" -s h -l help -d 'Print help'
complete -c firestone -n "__fish_firestone_using_subcommand start" -l timeout -d 'Override the configured start deadline' -r
complete -c firestone -n "__fish_firestone_using_subcommand start" -l home -d 'Override the Firestone home root (config, data, and runtime)' -r -F
complete -c firestone -n "__fish_firestone_using_subcommand start" -l no-wait -d 'Return immediately after the VMM reaches persisted running state'
complete -c firestone -n "__fish_firestone_using_subcommand start" -l json -d 'Print events as newline-delimited JSON and disable human output'
complete -c firestone -n "__fish_firestone_using_subcommand start" -s q -l quiet -d 'Print only errors and command results'
complete -c firestone -n "__fish_firestone_using_subcommand start" -s v -l verbose -d 'Increase log detail. Pass twice for debug output'
complete -c firestone -n "__fish_firestone_using_subcommand start" -l no-color -d 'Disable colored output'
complete -c firestone -n "__fish_firestone_using_subcommand start" -s y -l yes -d 'Assume yes when a command may prompt'
complete -c firestone -n "__fish_firestone_using_subcommand start" -s h -l help -d 'Print help'
complete -c firestone -n "__fish_firestone_using_subcommand stop" -l timeout -d 'Override the configured graceful-stop deadline' -r
complete -c firestone -n "__fish_firestone_using_subcommand stop" -l home -d 'Override the Firestone home root (config, data, and runtime)' -r -F
complete -c firestone -n "__fish_firestone_using_subcommand stop" -l force -d 'Skip the guest power button and kill the VMM'
complete -c firestone -n "__fish_firestone_using_subcommand stop" -l json -d 'Print events as newline-delimited JSON and disable human output'
complete -c firestone -n "__fish_firestone_using_subcommand stop" -s q -l quiet -d 'Print only errors and command results'
complete -c firestone -n "__fish_firestone_using_subcommand stop" -s v -l verbose -d 'Increase log detail. Pass twice for debug output'
complete -c firestone -n "__fish_firestone_using_subcommand stop" -l no-color -d 'Disable colored output'
complete -c firestone -n "__fish_firestone_using_subcommand stop" -s y -l yes -d 'Assume yes when a command may prompt'
complete -c firestone -n "__fish_firestone_using_subcommand stop" -s h -l help -d 'Print help'
complete -c firestone -n "__fish_firestone_using_subcommand restart" -l home -d 'Override the Firestone home root (config, data, and runtime)' -r -F
complete -c firestone -n "__fish_firestone_using_subcommand restart" -l json -d 'Print events as newline-delimited JSON and disable human output'
complete -c firestone -n "__fish_firestone_using_subcommand restart" -s q -l quiet -d 'Print only errors and command results'
complete -c firestone -n "__fish_firestone_using_subcommand restart" -s v -l verbose -d 'Increase log detail. Pass twice for debug output'
complete -c firestone -n "__fish_firestone_using_subcommand restart" -l no-color -d 'Disable colored output'
complete -c firestone -n "__fish_firestone_using_subcommand restart" -s y -l yes -d 'Assume yes when a command may prompt'
complete -c firestone -n "__fish_firestone_using_subcommand restart" -s h -l help -d 'Print help'
complete -c firestone -n "__fish_firestone_using_subcommand resize" -l cpus -d 'Set the number of virtual CPUs' -r
complete -c firestone -n "__fish_firestone_using_subcommand resize" -l memory -d 'Set guest memory, for example 4G or 4096M' -r
complete -c firestone -n "__fish_firestone_using_subcommand resize" -l home -d 'Override the Firestone home root (config, data, and runtime)' -r -F
complete -c firestone -n "__fish_firestone_using_subcommand resize" -l json -d 'Print events as newline-delimited JSON and disable human output'
complete -c firestone -n "__fish_firestone_using_subcommand resize" -s q -l quiet -d 'Print only errors and command results'
complete -c firestone -n "__fish_firestone_using_subcommand resize" -s v -l verbose -d 'Increase log detail. Pass twice for debug output'
complete -c firestone -n "__fish_firestone_using_subcommand resize" -l no-color -d 'Disable colored output'
complete -c firestone -n "__fish_firestone_using_subcommand resize" -s y -l yes -d 'Assume yes when a command may prompt'
complete -c firestone -n "__fish_firestone_using_subcommand resize" -s h -l help -d 'Print help'
complete -c firestone -n "__fish_firestone_using_subcommand rm" -l home -d 'Override the Firestone home root (config, data, and runtime)' -r -F
complete -c firestone -n "__fish_firestone_using_subcommand rm" -l force -d 'Approve removal of running machines'
complete -c firestone -n "__fish_firestone_using_subcommand rm" -l json -d 'Print events as newline-delimited JSON and disable human output'
complete -c firestone -n "__fish_firestone_using_subcommand rm" -s q -l quiet -d 'Print only errors and command results'
complete -c firestone -n "__fish_firestone_using_subcommand rm" -s v -l verbose -d 'Increase log detail. Pass twice for debug output'
complete -c firestone -n "__fish_firestone_using_subcommand rm" -l no-color -d 'Disable colored output'
complete -c firestone -n "__fish_firestone_using_subcommand rm" -s y -l yes -d 'Assume yes when a command may prompt'
complete -c firestone -n "__fish_firestone_using_subcommand rm" -s h -l help -d 'Print help'
complete -c firestone -n "__fish_firestone_using_subcommand ls" -l home -d 'Override the Firestone home root (config, data, and runtime)' -r -F
complete -c firestone -n "__fish_firestone_using_subcommand ls" -l json -d 'Print events as newline-delimited JSON and disable human output'
complete -c firestone -n "__fish_firestone_using_subcommand ls" -s q -l quiet -d 'Print only errors and command results'
complete -c firestone -n "__fish_firestone_using_subcommand ls" -s v -l verbose -d 'Increase log detail. Pass twice for debug output'
complete -c firestone -n "__fish_firestone_using_subcommand ls" -l no-color -d 'Disable colored output'
complete -c firestone -n "__fish_firestone_using_subcommand ls" -s y -l yes -d 'Assume yes when a command may prompt'
complete -c firestone -n "__fish_firestone_using_subcommand ls" -s h -l help -d 'Print help'
complete -c firestone -n "__fish_firestone_using_subcommand list" -l home -d 'Override the Firestone home root (config, data, and runtime)' -r -F
complete -c firestone -n "__fish_firestone_using_subcommand list" -l json -d 'Print events as newline-delimited JSON and disable human output'
complete -c firestone -n "__fish_firestone_using_subcommand list" -s q -l quiet -d 'Print only errors and command results'
complete -c firestone -n "__fish_firestone_using_subcommand list" -s v -l verbose -d 'Increase log detail. Pass twice for debug output'
complete -c firestone -n "__fish_firestone_using_subcommand list" -l no-color -d 'Disable colored output'
complete -c firestone -n "__fish_firestone_using_subcommand list" -s y -l yes -d 'Assume yes when a command may prompt'
complete -c firestone -n "__fish_firestone_using_subcommand list" -s h -l help -d 'Print help'
complete -c firestone -n "__fish_firestone_using_subcommand show" -l home -d 'Override the Firestone home root (config, data, and runtime)' -r -F
complete -c firestone -n "__fish_firestone_using_subcommand show" -l vmconfig -d 'Include the generated cloud-hypervisor configuration'
complete -c firestone -n "__fish_firestone_using_subcommand show" -l json -d 'Print events as newline-delimited JSON and disable human output'
complete -c firestone -n "__fish_firestone_using_subcommand show" -s q -l quiet -d 'Print only errors and command results'
complete -c firestone -n "__fish_firestone_using_subcommand show" -s v -l verbose -d 'Increase log detail. Pass twice for debug output'
complete -c firestone -n "__fish_firestone_using_subcommand show" -l no-color -d 'Disable colored output'
complete -c firestone -n "__fish_firestone_using_subcommand show" -s y -l yes -d 'Assume yes when a command may prompt'
complete -c firestone -n "__fish_firestone_using_subcommand show" -s h -l help -d 'Print help'
complete -c firestone -n "__fish_firestone_using_subcommand edit" -l home -d 'Override the Firestone home root (config, data, and runtime)' -r -F
complete -c firestone -n "__fish_firestone_using_subcommand edit" -l json -d 'Print events as newline-delimited JSON and disable human output'
complete -c firestone -n "__fish_firestone_using_subcommand edit" -s q -l quiet -d 'Print only errors and command results'
complete -c firestone -n "__fish_firestone_using_subcommand edit" -s v -l verbose -d 'Increase log detail. Pass twice for debug output'
complete -c firestone -n "__fish_firestone_using_subcommand edit" -l no-color -d 'Disable colored output'
complete -c firestone -n "__fish_firestone_using_subcommand edit" -s y -l yes -d 'Assume yes when a command may prompt'
complete -c firestone -n "__fish_firestone_using_subcommand edit" -s h -l help -d 'Print help'
complete -c firestone -n "__fish_firestone_using_subcommand shell" -l user -d 'Select the guest login user' -r
complete -c firestone -n "__fish_firestone_using_subcommand shell" -l home -d 'Override the Firestone home root (config, data, and runtime)' -r -F
complete -c firestone -n "__fish_firestone_using_subcommand shell" -l json -d 'Print events as newline-delimited JSON and disable human output'
complete -c firestone -n "__fish_firestone_using_subcommand shell" -s q -l quiet -d 'Print only errors and command results'
complete -c firestone -n "__fish_firestone_using_subcommand shell" -s v -l verbose -d 'Increase log detail. Pass twice for debug output'
complete -c firestone -n "__fish_firestone_using_subcommand shell" -l no-color -d 'Disable colored output'
complete -c firestone -n "__fish_firestone_using_subcommand shell" -s y -l yes -d 'Assume yes when a command may prompt'
complete -c firestone -n "__fish_firestone_using_subcommand shell" -s h -l help -d 'Print help'
complete -c firestone -n "__fish_firestone_using_subcommand ssh" -l user -d 'Select the guest login user' -r
complete -c firestone -n "__fish_firestone_using_subcommand ssh" -l home -d 'Override the Firestone home root (config, data, and runtime)' -r -F
complete -c firestone -n "__fish_firestone_using_subcommand ssh" -l json -d 'Print events as newline-delimited JSON and disable human output'
complete -c firestone -n "__fish_firestone_using_subcommand ssh" -s q -l quiet -d 'Print only errors and command results'
complete -c firestone -n "__fish_firestone_using_subcommand ssh" -s v -l verbose -d 'Increase log detail. Pass twice for debug output'
complete -c firestone -n "__fish_firestone_using_subcommand ssh" -l no-color -d 'Disable colored output'
complete -c firestone -n "__fish_firestone_using_subcommand ssh" -s y -l yes -d 'Assume yes when a command may prompt'
complete -c firestone -n "__fish_firestone_using_subcommand ssh" -s h -l help -d 'Print help'
complete -c firestone -n "__fish_firestone_using_subcommand cp" -l home -d 'Override the Firestone home root (config, data, and runtime)' -r -F
complete -c firestone -n "__fish_firestone_using_subcommand cp" -s r -l recursive -d 'Copy directories recursively'
complete -c firestone -n "__fish_firestone_using_subcommand cp" -l json -d 'Print events as newline-delimited JSON and disable human output'
complete -c firestone -n "__fish_firestone_using_subcommand cp" -s q -l quiet -d 'Print only errors and command results'
complete -c firestone -n "__fish_firestone_using_subcommand cp" -s v -l verbose -d 'Increase log detail. Pass twice for debug output'
complete -c firestone -n "__fish_firestone_using_subcommand cp" -l no-color -d 'Disable colored output'
complete -c firestone -n "__fish_firestone_using_subcommand cp" -s y -l yes -d 'Assume yes when a command may prompt'
complete -c firestone -n "__fish_firestone_using_subcommand cp" -s h -l help -d 'Print help'
complete -c firestone -n "__fish_firestone_using_subcommand ssh-config" -l home -d 'Override the Firestone home root (config, data, and runtime)' -r -F
complete -c firestone -n "__fish_firestone_using_subcommand ssh-config" -l json -d 'Print events as newline-delimited JSON and disable human output'
complete -c firestone -n "__fish_firestone_using_subcommand ssh-config" -s q -l quiet -d 'Print only errors and command results'
complete -c firestone -n "__fish_firestone_using_subcommand ssh-config" -s v -l verbose -d 'Increase log detail. Pass twice for debug output'
complete -c firestone -n "__fish_firestone_using_subcommand ssh-config" -l no-color -d 'Disable colored output'
complete -c firestone -n "__fish_firestone_using_subcommand ssh-config" -s y -l yes -d 'Assume yes when a command may prompt'
complete -c firestone -n "__fish_firestone_using_subcommand ssh-config" -s h -l help -d 'Print help'
complete -c firestone -n "__fish_firestone_using_subcommand console" -l home -d 'Override the Firestone home root (config, data, and runtime)' -r -F
complete -c firestone -n "__fish_firestone_using_subcommand console" -l json -d 'Print events as newline-delimited JSON and disable human output'
complete -c firestone -n "__fish_firestone_using_subcommand console" -s q -l quiet -d 'Print only errors and command results'
complete -c firestone -n "__fish_firestone_using_subcommand console" -s v -l verbose -d 'Increase log detail. Pass twice for debug output'
complete -c firestone -n "__fish_firestone_using_subcommand console" -l no-color -d 'Disable colored output'
complete -c firestone -n "__fish_firestone_using_subcommand console" -s y -l yes -d 'Assume yes when a command may prompt'
complete -c firestone -n "__fish_firestone_using_subcommand console" -s h -l help -d 'Print help'
complete -c firestone -n "__fish_firestone_using_subcommand logs" -l source -d 'Select an owned machine log' -r
complete -c firestone -n "__fish_firestone_using_subcommand logs" -s n -d 'Print the last LINES lines before following' -r
complete -c firestone -n "__fish_firestone_using_subcommand logs" -l home -d 'Override the Firestone home root (config, data, and runtime)' -r -F
complete -c firestone -n "__fish_firestone_using_subcommand logs" -s f -l follow -d 'Continue printing appended log data until interrupted'
complete -c firestone -n "__fish_firestone_using_subcommand logs" -l json -d 'Print events as newline-delimited JSON and disable human output'
complete -c firestone -n "__fish_firestone_using_subcommand logs" -s q -l quiet -d 'Print only errors and command results'
complete -c firestone -n "__fish_firestone_using_subcommand logs" -s v -l verbose -d 'Increase log detail. Pass twice for debug output'
complete -c firestone -n "__fish_firestone_using_subcommand logs" -l no-color -d 'Disable colored output'
complete -c firestone -n "__fish_firestone_using_subcommand logs" -s y -l yes -d 'Assume yes when a command may prompt'
complete -c firestone -n "__fish_firestone_using_subcommand logs" -s h -l help -d 'Print help'
complete -c firestone -n "__fish_firestone_using_subcommand metrics" -l home -d 'Override the Firestone home root (config, data, and runtime)' -r -F
complete -c firestone -n "__fish_firestone_using_subcommand metrics" -l json -d 'Print events as newline-delimited JSON and disable human output'
complete -c firestone -n "__fish_firestone_using_subcommand metrics" -s q -l quiet -d 'Print only errors and command results'
complete -c firestone -n "__fish_firestone_using_subcommand metrics" -s v -l verbose -d 'Increase log detail. Pass twice for debug output'
complete -c firestone -n "__fish_firestone_using_subcommand metrics" -l no-color -d 'Disable colored output'
complete -c firestone -n "__fish_firestone_using_subcommand metrics" -s y -l yes -d 'Assume yes when a command may prompt'
complete -c firestone -n "__fish_firestone_using_subcommand metrics" -s h -l help -d 'Print help'
complete -c firestone -n "__fish_firestone_using_subcommand catalog" -l home -d 'Override the Firestone home root (config, data, and runtime)' -r -F
complete -c firestone -n "__fish_firestone_using_subcommand catalog" -l json -d 'Print events as newline-delimited JSON and disable human output'
complete -c firestone -n "__fish_firestone_using_subcommand catalog" -s q -l quiet -d 'Print only errors and command results'
complete -c firestone -n "__fish_firestone_using_subcommand catalog" -s v -l verbose -d 'Increase log detail. Pass twice for debug output'
complete -c firestone -n "__fish_firestone_using_subcommand catalog" -l no-color -d 'Disable colored output'
complete -c firestone -n "__fish_firestone_using_subcommand catalog" -s y -l yes -d 'Assume yes when a command may prompt'
complete -c firestone -n "__fish_firestone_using_subcommand catalog" -s h -l help -d 'Print help'
complete -c firestone -n "__fish_firestone_using_subcommand images; and not __fish_seen_subcommand_from ls pull inspect rm prune help" -l home -d 'Override the Firestone home root (config, data, and runtime)' -r -F
complete -c firestone -n "__fish_firestone_using_subcommand images; and not __fish_seen_subcommand_from ls pull inspect rm prune help" -l json -d 'Print events as newline-delimited JSON and disable human output'
complete -c firestone -n "__fish_firestone_using_subcommand images; and not __fish_seen_subcommand_from ls pull inspect rm prune help" -s q -l quiet -d 'Print only errors and command results'
complete -c firestone -n "__fish_firestone_using_subcommand images; and not __fish_seen_subcommand_from ls pull inspect rm prune help" -s v -l verbose -d 'Increase log detail. Pass twice for debug output'
complete -c firestone -n "__fish_firestone_using_subcommand images; and not __fish_seen_subcommand_from ls pull inspect rm prune help" -l no-color -d 'Disable colored output'
complete -c firestone -n "__fish_firestone_using_subcommand images; and not __fish_seen_subcommand_from ls pull inspect rm prune help" -s y -l yes -d 'Assume yes when a command may prompt'
complete -c firestone -n "__fish_firestone_using_subcommand images; and not __fish_seen_subcommand_from ls pull inspect rm prune help" -s h -l help -d 'Print help'
complete -c firestone -n "__fish_firestone_using_subcommand images; and not __fish_seen_subcommand_from ls pull inspect rm prune help" -f -a "ls" -d 'List stored images'
complete -c firestone -n "__fish_firestone_using_subcommand images; and not __fish_seen_subcommand_from ls pull inspect rm prune help" -f -a "pull" -d 'Pull and verify one image'
complete -c firestone -n "__fish_firestone_using_subcommand images; and not __fish_seen_subcommand_from ls pull inspect rm prune help" -f -a "inspect" -d 'Verify and inspect one stored image'
complete -c firestone -n "__fish_firestone_using_subcommand images; and not __fish_seen_subcommand_from ls pull inspect rm prune help" -f -a "rm" -d 'Remove one stored image'
complete -c firestone -n "__fish_firestone_using_subcommand images; and not __fish_seen_subcommand_from ls pull inspect rm prune help" -f -a "prune" -d 'Remove all unreferenced images'
complete -c firestone -n "__fish_firestone_using_subcommand images; and not __fish_seen_subcommand_from ls pull inspect rm prune help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c firestone -n "__fish_firestone_using_subcommand images; and __fish_seen_subcommand_from ls" -l home -d 'Override the Firestone home root (config, data, and runtime)' -r -F
complete -c firestone -n "__fish_firestone_using_subcommand images; and __fish_seen_subcommand_from ls" -l json -d 'Print events as newline-delimited JSON and disable human output'
complete -c firestone -n "__fish_firestone_using_subcommand images; and __fish_seen_subcommand_from ls" -s q -l quiet -d 'Print only errors and command results'
complete -c firestone -n "__fish_firestone_using_subcommand images; and __fish_seen_subcommand_from ls" -s v -l verbose -d 'Increase log detail. Pass twice for debug output'
complete -c firestone -n "__fish_firestone_using_subcommand images; and __fish_seen_subcommand_from ls" -l no-color -d 'Disable colored output'
complete -c firestone -n "__fish_firestone_using_subcommand images; and __fish_seen_subcommand_from ls" -s y -l yes -d 'Assume yes when a command may prompt'
complete -c firestone -n "__fish_firestone_using_subcommand images; and __fish_seen_subcommand_from ls" -s h -l help -d 'Print help'
complete -c firestone -n "__fish_firestone_using_subcommand images; and __fish_seen_subcommand_from pull" -l sha256 -d 'Verify a direct HTTPS URL with this SHA-256 digest' -r
complete -c firestone -n "__fish_firestone_using_subcommand images; and __fish_seen_subcommand_from pull" -l home -d 'Override the Firestone home root (config, data, and runtime)' -r -F
complete -c firestone -n "__fish_firestone_using_subcommand images; and __fish_seen_subcommand_from pull" -l json -d 'Print events as newline-delimited JSON and disable human output'
complete -c firestone -n "__fish_firestone_using_subcommand images; and __fish_seen_subcommand_from pull" -s q -l quiet -d 'Print only errors and command results'
complete -c firestone -n "__fish_firestone_using_subcommand images; and __fish_seen_subcommand_from pull" -s v -l verbose -d 'Increase log detail. Pass twice for debug output'
complete -c firestone -n "__fish_firestone_using_subcommand images; and __fish_seen_subcommand_from pull" -l no-color -d 'Disable colored output'
complete -c firestone -n "__fish_firestone_using_subcommand images; and __fish_seen_subcommand_from pull" -s y -l yes -d 'Assume yes when a command may prompt'
complete -c firestone -n "__fish_firestone_using_subcommand images; and __fish_seen_subcommand_from pull" -s h -l help -d 'Print help'
complete -c firestone -n "__fish_firestone_using_subcommand images; and __fish_seen_subcommand_from inspect" -l home -d 'Override the Firestone home root (config, data, and runtime)' -r -F
complete -c firestone -n "__fish_firestone_using_subcommand images; and __fish_seen_subcommand_from inspect" -l json -d 'Print events as newline-delimited JSON and disable human output'
complete -c firestone -n "__fish_firestone_using_subcommand images; and __fish_seen_subcommand_from inspect" -s q -l quiet -d 'Print only errors and command results'
complete -c firestone -n "__fish_firestone_using_subcommand images; and __fish_seen_subcommand_from inspect" -s v -l verbose -d 'Increase log detail. Pass twice for debug output'
complete -c firestone -n "__fish_firestone_using_subcommand images; and __fish_seen_subcommand_from inspect" -l no-color -d 'Disable colored output'
complete -c firestone -n "__fish_firestone_using_subcommand images; and __fish_seen_subcommand_from inspect" -s y -l yes -d 'Assume yes when a command may prompt'
complete -c firestone -n "__fish_firestone_using_subcommand images; and __fish_seen_subcommand_from inspect" -s h -l help -d 'Print help'
complete -c firestone -n "__fish_firestone_using_subcommand images; and __fish_seen_subcommand_from rm" -l home -d 'Override the Firestone home root (config, data, and runtime)' -r -F
complete -c firestone -n "__fish_firestone_using_subcommand images; and __fish_seen_subcommand_from rm" -l force -d 'Approve removal while a machine still references the image'
complete -c firestone -n "__fish_firestone_using_subcommand images; and __fish_seen_subcommand_from rm" -l json -d 'Print events as newline-delimited JSON and disable human output'
complete -c firestone -n "__fish_firestone_using_subcommand images; and __fish_seen_subcommand_from rm" -s q -l quiet -d 'Print only errors and command results'
complete -c firestone -n "__fish_firestone_using_subcommand images; and __fish_seen_subcommand_from rm" -s v -l verbose -d 'Increase log detail. Pass twice for debug output'
complete -c firestone -n "__fish_firestone_using_subcommand images; and __fish_seen_subcommand_from rm" -l no-color -d 'Disable colored output'
complete -c firestone -n "__fish_firestone_using_subcommand images; and __fish_seen_subcommand_from rm" -s y -l yes -d 'Assume yes when a command may prompt'
complete -c firestone -n "__fish_firestone_using_subcommand images; and __fish_seen_subcommand_from rm" -s h -l help -d 'Print help'
complete -c firestone -n "__fish_firestone_using_subcommand images; and __fish_seen_subcommand_from prune" -l home -d 'Override the Firestone home root (config, data, and runtime)' -r -F
complete -c firestone -n "__fish_firestone_using_subcommand images; and __fish_seen_subcommand_from prune" -l json -d 'Print events as newline-delimited JSON and disable human output'
complete -c firestone -n "__fish_firestone_using_subcommand images; and __fish_seen_subcommand_from prune" -s q -l quiet -d 'Print only errors and command results'
complete -c firestone -n "__fish_firestone_using_subcommand images; and __fish_seen_subcommand_from prune" -s v -l verbose -d 'Increase log detail. Pass twice for debug output'
complete -c firestone -n "__fish_firestone_using_subcommand images; and __fish_seen_subcommand_from prune" -l no-color -d 'Disable colored output'
complete -c firestone -n "__fish_firestone_using_subcommand images; and __fish_seen_subcommand_from prune" -s y -l yes -d 'Assume yes when a command may prompt'
complete -c firestone -n "__fish_firestone_using_subcommand images; and __fish_seen_subcommand_from prune" -s h -l help -d 'Print help'
complete -c firestone -n "__fish_firestone_using_subcommand images; and __fish_seen_subcommand_from help" -f -a "ls" -d 'List stored images'
complete -c firestone -n "__fish_firestone_using_subcommand images; and __fish_seen_subcommand_from help" -f -a "pull" -d 'Pull and verify one image'
complete -c firestone -n "__fish_firestone_using_subcommand images; and __fish_seen_subcommand_from help" -f -a "inspect" -d 'Verify and inspect one stored image'
complete -c firestone -n "__fish_firestone_using_subcommand images; and __fish_seen_subcommand_from help" -f -a "rm" -d 'Remove one stored image'
complete -c firestone -n "__fish_firestone_using_subcommand images; and __fish_seen_subcommand_from help" -f -a "prune" -d 'Remove all unreferenced images'
complete -c firestone -n "__fish_firestone_using_subcommand images; and __fish_seen_subcommand_from help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c firestone -n "__fish_firestone_using_subcommand doctor" -l home -d 'Override the Firestone home root (config, data, and runtime)' -r -F
complete -c firestone -n "__fish_firestone_using_subcommand doctor" -l fix -d 'Apply safe repairs; AppArmor elevation needs a TTY prompt and ignores --yes'
complete -c firestone -n "__fish_firestone_using_subcommand doctor" -l json -d 'Print events as newline-delimited JSON and disable human output'
complete -c firestone -n "__fish_firestone_using_subcommand doctor" -s q -l quiet -d 'Print only errors and command results'
complete -c firestone -n "__fish_firestone_using_subcommand doctor" -s v -l verbose -d 'Increase log detail. Pass twice for debug output'
complete -c firestone -n "__fish_firestone_using_subcommand doctor" -l no-color -d 'Disable colored output'
complete -c firestone -n "__fish_firestone_using_subcommand doctor" -s y -l yes -d 'Assume yes when a command may prompt'
complete -c firestone -n "__fish_firestone_using_subcommand doctor" -s h -l help -d 'Print help'
complete -c firestone -n "__fish_firestone_using_subcommand completions" -l home -d 'Override the Firestone home root (config, data, and runtime)' -r -F
complete -c firestone -n "__fish_firestone_using_subcommand completions" -l json -d 'Print events as newline-delimited JSON and disable human output'
complete -c firestone -n "__fish_firestone_using_subcommand completions" -s q -l quiet -d 'Print only errors and command results'
complete -c firestone -n "__fish_firestone_using_subcommand completions" -s v -l verbose -d 'Increase log detail. Pass twice for debug output'
complete -c firestone -n "__fish_firestone_using_subcommand completions" -l no-color -d 'Disable colored output'
complete -c firestone -n "__fish_firestone_using_subcommand completions" -s y -l yes -d 'Assume yes when a command may prompt'
complete -c firestone -n "__fish_firestone_using_subcommand completions" -s h -l help -d 'Print help'
complete -c firestone -n "__fish_firestone_using_subcommand version" -l home -d 'Override the Firestone home root (config, data, and runtime)' -r -F
complete -c firestone -n "__fish_firestone_using_subcommand version" -l json -d 'Print events as newline-delimited JSON and disable human output'
complete -c firestone -n "__fish_firestone_using_subcommand version" -s q -l quiet -d 'Print only errors and command results'
complete -c firestone -n "__fish_firestone_using_subcommand version" -s v -l verbose -d 'Increase log detail. Pass twice for debug output'
complete -c firestone -n "__fish_firestone_using_subcommand version" -l no-color -d 'Disable colored output'
complete -c firestone -n "__fish_firestone_using_subcommand version" -s y -l yes -d 'Assume yes when a command may prompt'
complete -c firestone -n "__fish_firestone_using_subcommand version" -s h -l help -d 'Print help'
complete -c firestone -n "__fish_firestone_using_subcommand serve" -l listen -d 'Listen at a private Unix socket, or at a loopback TCP address' -r
complete -c firestone -n "__fish_firestone_using_subcommand serve" -l token -d 'File holding the 64-hexadecimal-character session token. TCP only' -r -F
complete -c firestone -n "__fish_firestone_using_subcommand serve" -l home -d 'Override the Firestone home root (config, data, and runtime)' -r -F
complete -c firestone -n "__fish_firestone_using_subcommand serve" -l json -d 'Print events as newline-delimited JSON and disable human output'
complete -c firestone -n "__fish_firestone_using_subcommand serve" -s q -l quiet -d 'Print only errors and command results'
complete -c firestone -n "__fish_firestone_using_subcommand serve" -s v -l verbose -d 'Increase log detail. Pass twice for debug output'
complete -c firestone -n "__fish_firestone_using_subcommand serve" -l no-color -d 'Disable colored output'
complete -c firestone -n "__fish_firestone_using_subcommand serve" -s y -l yes -d 'Assume yes when a command may prompt'
complete -c firestone -n "__fish_firestone_using_subcommand serve" -s h -l help -d 'Print help'
complete -c firestone -n "__fish_firestone_using_subcommand ui" -l port -d 'Loopback port to bind. Zero asks the kernel for any free port' -r
complete -c firestone -n "__fish_firestone_using_subcommand ui" -l home -d 'Override the Firestone home root (config, data, and runtime)' -r -F
complete -c firestone -n "__fish_firestone_using_subcommand ui" -l no-open -d 'Do not launch a browser; print the URL only'
complete -c firestone -n "__fish_firestone_using_subcommand ui" -l print-url -d 'Print the URL and never launch a browser. Implies --no-open'
complete -c firestone -n "__fish_firestone_using_subcommand ui" -l json -d 'Print events as newline-delimited JSON and disable human output'
complete -c firestone -n "__fish_firestone_using_subcommand ui" -s q -l quiet -d 'Print only errors and command results'
complete -c firestone -n "__fish_firestone_using_subcommand ui" -s v -l verbose -d 'Increase log detail. Pass twice for debug output'
complete -c firestone -n "__fish_firestone_using_subcommand ui" -l no-color -d 'Disable colored output'
complete -c firestone -n "__fish_firestone_using_subcommand ui" -s y -l yes -d 'Assume yes when a command may prompt'
complete -c firestone -n "__fish_firestone_using_subcommand ui" -s h -l help -d 'Print help'
complete -c firestone -n "__fish_firestone_using_subcommand clone" -l home -d 'Override the Firestone home root (config, data, and runtime)' -r -F
complete -c firestone -n "__fish_firestone_using_subcommand clone" -l fresh-disk -d 'Give the clone an empty overlay on the same base image'
complete -c firestone -n "__fish_firestone_using_subcommand clone" -l json -d 'Print events as newline-delimited JSON and disable human output'
complete -c firestone -n "__fish_firestone_using_subcommand clone" -s q -l quiet -d 'Print only errors and command results'
complete -c firestone -n "__fish_firestone_using_subcommand clone" -s v -l verbose -d 'Increase log detail. Pass twice for debug output'
complete -c firestone -n "__fish_firestone_using_subcommand clone" -l no-color -d 'Disable colored output'
complete -c firestone -n "__fish_firestone_using_subcommand clone" -s y -l yes -d 'Assume yes when a command may prompt'
complete -c firestone -n "__fish_firestone_using_subcommand clone" -s h -l help -d 'Print help'
complete -c firestone -n "__fish_firestone_using_subcommand snapshot; and not __fish_seen_subcommand_from create list ls restore rm help" -l home -d 'Override the Firestone home root (config, data, and runtime)' -r -F
complete -c firestone -n "__fish_firestone_using_subcommand snapshot; and not __fish_seen_subcommand_from create list ls restore rm help" -l json -d 'Print events as newline-delimited JSON and disable human output'
complete -c firestone -n "__fish_firestone_using_subcommand snapshot; and not __fish_seen_subcommand_from create list ls restore rm help" -s q -l quiet -d 'Print only errors and command results'
complete -c firestone -n "__fish_firestone_using_subcommand snapshot; and not __fish_seen_subcommand_from create list ls restore rm help" -s v -l verbose -d 'Increase log detail. Pass twice for debug output'
complete -c firestone -n "__fish_firestone_using_subcommand snapshot; and not __fish_seen_subcommand_from create list ls restore rm help" -l no-color -d 'Disable colored output'
complete -c firestone -n "__fish_firestone_using_subcommand snapshot; and not __fish_seen_subcommand_from create list ls restore rm help" -s y -l yes -d 'Assume yes when a command may prompt'
complete -c firestone -n "__fish_firestone_using_subcommand snapshot; and not __fish_seen_subcommand_from create list ls restore rm help" -s h -l help -d 'Print help'
complete -c firestone -n "__fish_firestone_using_subcommand snapshot; and not __fish_seen_subcommand_from create list ls restore rm help" -f -a "create" -d 'Capture one immutable snapshot of a machine'
complete -c firestone -n "__fish_firestone_using_subcommand snapshot; and not __fish_seen_subcommand_from create list ls restore rm help" -f -a "list" -d 'List a machine\'s snapshots'
complete -c firestone -n "__fish_firestone_using_subcommand snapshot; and not __fish_seen_subcommand_from create list ls restore rm help" -f -a "ls" -d 'List a machine\'s snapshots'
complete -c firestone -n "__fish_firestone_using_subcommand snapshot; and not __fish_seen_subcommand_from create list ls restore rm help" -f -a "restore" -d 'Roll a machine back to one of its snapshots'
complete -c firestone -n "__fish_firestone_using_subcommand snapshot; and not __fish_seen_subcommand_from create list ls restore rm help" -f -a "rm" -d 'Remove one snapshot'
complete -c firestone -n "__fish_firestone_using_subcommand snapshot; and not __fish_seen_subcommand_from create list ls restore rm help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c firestone -n "__fish_firestone_using_subcommand snapshot; and __fish_seen_subcommand_from create" -l home -d 'Override the Firestone home root (config, data, and runtime)' -r -F
complete -c firestone -n "__fish_firestone_using_subcommand snapshot; and __fish_seen_subcommand_from create" -l json -d 'Print events as newline-delimited JSON and disable human output'
complete -c firestone -n "__fish_firestone_using_subcommand snapshot; and __fish_seen_subcommand_from create" -s q -l quiet -d 'Print only errors and command results'
complete -c firestone -n "__fish_firestone_using_subcommand snapshot; and __fish_seen_subcommand_from create" -s v -l verbose -d 'Increase log detail. Pass twice for debug output'
complete -c firestone -n "__fish_firestone_using_subcommand snapshot; and __fish_seen_subcommand_from create" -l no-color -d 'Disable colored output'
complete -c firestone -n "__fish_firestone_using_subcommand snapshot; and __fish_seen_subcommand_from create" -s y -l yes -d 'Assume yes when a command may prompt'
complete -c firestone -n "__fish_firestone_using_subcommand snapshot; and __fish_seen_subcommand_from create" -s h -l help -d 'Print help'
complete -c firestone -n "__fish_firestone_using_subcommand snapshot; and __fish_seen_subcommand_from list" -l home -d 'Override the Firestone home root (config, data, and runtime)' -r -F
complete -c firestone -n "__fish_firestone_using_subcommand snapshot; and __fish_seen_subcommand_from list" -l json -d 'Print events as newline-delimited JSON and disable human output'
complete -c firestone -n "__fish_firestone_using_subcommand snapshot; and __fish_seen_subcommand_from list" -s q -l quiet -d 'Print only errors and command results'
complete -c firestone -n "__fish_firestone_using_subcommand snapshot; and __fish_seen_subcommand_from list" -s v -l verbose -d 'Increase log detail. Pass twice for debug output'
complete -c firestone -n "__fish_firestone_using_subcommand snapshot; and __fish_seen_subcommand_from list" -l no-color -d 'Disable colored output'
complete -c firestone -n "__fish_firestone_using_subcommand snapshot; and __fish_seen_subcommand_from list" -s y -l yes -d 'Assume yes when a command may prompt'
complete -c firestone -n "__fish_firestone_using_subcommand snapshot; and __fish_seen_subcommand_from list" -s h -l help -d 'Print help'
complete -c firestone -n "__fish_firestone_using_subcommand snapshot; and __fish_seen_subcommand_from ls" -l home -d 'Override the Firestone home root (config, data, and runtime)' -r -F
complete -c firestone -n "__fish_firestone_using_subcommand snapshot; and __fish_seen_subcommand_from ls" -l json -d 'Print events as newline-delimited JSON and disable human output'
complete -c firestone -n "__fish_firestone_using_subcommand snapshot; and __fish_seen_subcommand_from ls" -s q -l quiet -d 'Print only errors and command results'
complete -c firestone -n "__fish_firestone_using_subcommand snapshot; and __fish_seen_subcommand_from ls" -s v -l verbose -d 'Increase log detail. Pass twice for debug output'
complete -c firestone -n "__fish_firestone_using_subcommand snapshot; and __fish_seen_subcommand_from ls" -l no-color -d 'Disable colored output'
complete -c firestone -n "__fish_firestone_using_subcommand snapshot; and __fish_seen_subcommand_from ls" -s y -l yes -d 'Assume yes when a command may prompt'
complete -c firestone -n "__fish_firestone_using_subcommand snapshot; and __fish_seen_subcommand_from ls" -s h -l help -d 'Print help'
complete -c firestone -n "__fish_firestone_using_subcommand snapshot; and __fish_seen_subcommand_from restore" -l home -d 'Override the Firestone home root (config, data, and runtime)' -r -F
complete -c firestone -n "__fish_firestone_using_subcommand snapshot; and __fish_seen_subcommand_from restore" -l force -d 'Stop the machine first instead of refusing a running machine'
complete -c firestone -n "__fish_firestone_using_subcommand snapshot; and __fish_seen_subcommand_from restore" -l start -d 'Start the machine after restoring a cold snapshot'
complete -c firestone -n "__fish_firestone_using_subcommand snapshot; and __fish_seen_subcommand_from restore" -l json -d 'Print events as newline-delimited JSON and disable human output'
complete -c firestone -n "__fish_firestone_using_subcommand snapshot; and __fish_seen_subcommand_from restore" -s q -l quiet -d 'Print only errors and command results'
complete -c firestone -n "__fish_firestone_using_subcommand snapshot; and __fish_seen_subcommand_from restore" -s v -l verbose -d 'Increase log detail. Pass twice for debug output'
complete -c firestone -n "__fish_firestone_using_subcommand snapshot; and __fish_seen_subcommand_from restore" -l no-color -d 'Disable colored output'
complete -c firestone -n "__fish_firestone_using_subcommand snapshot; and __fish_seen_subcommand_from restore" -s y -l yes -d 'Assume yes when a command may prompt'
complete -c firestone -n "__fish_firestone_using_subcommand snapshot; and __fish_seen_subcommand_from restore" -s h -l help -d 'Print help'
complete -c firestone -n "__fish_firestone_using_subcommand snapshot; and __fish_seen_subcommand_from rm" -l home -d 'Override the Firestone home root (config, data, and runtime)' -r -F
complete -c firestone -n "__fish_firestone_using_subcommand snapshot; and __fish_seen_subcommand_from rm" -l json -d 'Print events as newline-delimited JSON and disable human output'
complete -c firestone -n "__fish_firestone_using_subcommand snapshot; and __fish_seen_subcommand_from rm" -s q -l quiet -d 'Print only errors and command results'
complete -c firestone -n "__fish_firestone_using_subcommand snapshot; and __fish_seen_subcommand_from rm" -s v -l verbose -d 'Increase log detail. Pass twice for debug output'
complete -c firestone -n "__fish_firestone_using_subcommand snapshot; and __fish_seen_subcommand_from rm" -l no-color -d 'Disable colored output'
complete -c firestone -n "__fish_firestone_using_subcommand snapshot; and __fish_seen_subcommand_from rm" -s y -l yes -d 'Assume yes when a command may prompt'
complete -c firestone -n "__fish_firestone_using_subcommand snapshot; and __fish_seen_subcommand_from rm" -s h -l help -d 'Print help'
complete -c firestone -n "__fish_firestone_using_subcommand snapshot; and __fish_seen_subcommand_from help" -f -a "create" -d 'Capture one immutable snapshot of a machine'
complete -c firestone -n "__fish_firestone_using_subcommand snapshot; and __fish_seen_subcommand_from help" -f -a "list" -d 'List a machine\'s snapshots'
complete -c firestone -n "__fish_firestone_using_subcommand snapshot; and __fish_seen_subcommand_from help" -f -a "restore" -d 'Roll a machine back to one of its snapshots'
complete -c firestone -n "__fish_firestone_using_subcommand snapshot; and __fish_seen_subcommand_from help" -f -a "rm" -d 'Remove one snapshot'
complete -c firestone -n "__fish_firestone_using_subcommand snapshot; and __fish_seen_subcommand_from help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c firestone -n "__fish_firestone_using_subcommand system; and not __fish_seen_subcommand_from prune help" -l home -d 'Override the Firestone home root (config, data, and runtime)' -r -F
complete -c firestone -n "__fish_firestone_using_subcommand system; and not __fish_seen_subcommand_from prune help" -l json -d 'Print events as newline-delimited JSON and disable human output'
complete -c firestone -n "__fish_firestone_using_subcommand system; and not __fish_seen_subcommand_from prune help" -s q -l quiet -d 'Print only errors and command results'
complete -c firestone -n "__fish_firestone_using_subcommand system; and not __fish_seen_subcommand_from prune help" -s v -l verbose -d 'Increase log detail. Pass twice for debug output'
complete -c firestone -n "__fish_firestone_using_subcommand system; and not __fish_seen_subcommand_from prune help" -l no-color -d 'Disable colored output'
complete -c firestone -n "__fish_firestone_using_subcommand system; and not __fish_seen_subcommand_from prune help" -s y -l yes -d 'Assume yes when a command may prompt'
complete -c firestone -n "__fish_firestone_using_subcommand system; and not __fish_seen_subcommand_from prune help" -s h -l help -d 'Print help'
complete -c firestone -n "__fish_firestone_using_subcommand system; and not __fish_seen_subcommand_from prune help" -f -a "prune" -d 'Reclaim disk space held by Firestone\'s own artifacts'
complete -c firestone -n "__fish_firestone_using_subcommand system; and not __fish_seen_subcommand_from prune help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c firestone -n "__fish_firestone_using_subcommand system; and __fish_seen_subcommand_from prune" -l home -d 'Override the Firestone home root (config, data, and runtime)' -r -F
complete -c firestone -n "__fish_firestone_using_subcommand system; and __fish_seen_subcommand_from prune" -l machines -d 'Also remove machines that are stopped, created, or failed'
complete -c firestone -n "__fish_firestone_using_subcommand system; and __fish_seen_subcommand_from prune" -l images -d 'Also remove base images nothing references'
complete -c firestone -n "__fish_firestone_using_subcommand system; and __fish_seen_subcommand_from prune" -l all -d 'Shorthand for --machines --images'
complete -c firestone -n "__fish_firestone_using_subcommand system; and __fish_seen_subcommand_from prune" -l force -d 'Approve the destructive machine tier without a prompt'
complete -c firestone -n "__fish_firestone_using_subcommand system; and __fish_seen_subcommand_from prune" -l dry-run -d 'Report what would be removed without removing anything'
complete -c firestone -n "__fish_firestone_using_subcommand system; and __fish_seen_subcommand_from prune" -l json -d 'Print events as newline-delimited JSON and disable human output'
complete -c firestone -n "__fish_firestone_using_subcommand system; and __fish_seen_subcommand_from prune" -s q -l quiet -d 'Print only errors and command results'
complete -c firestone -n "__fish_firestone_using_subcommand system; and __fish_seen_subcommand_from prune" -s v -l verbose -d 'Increase log detail. Pass twice for debug output'
complete -c firestone -n "__fish_firestone_using_subcommand system; and __fish_seen_subcommand_from prune" -l no-color -d 'Disable colored output'
complete -c firestone -n "__fish_firestone_using_subcommand system; and __fish_seen_subcommand_from prune" -s y -l yes -d 'Assume yes when a command may prompt'
complete -c firestone -n "__fish_firestone_using_subcommand system; and __fish_seen_subcommand_from prune" -s h -l help -d 'Print help'
complete -c firestone -n "__fish_firestone_using_subcommand system; and __fish_seen_subcommand_from help" -f -a "prune" -d 'Reclaim disk space held by Firestone\'s own artifacts'
complete -c firestone -n "__fish_firestone_using_subcommand system; and __fish_seen_subcommand_from help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c firestone -n "__fish_firestone_using_subcommand help; and not __fish_seen_subcommand_from run create start stop restart resize rm ls show edit shell cp ssh-config console logs metrics catalog images doctor completions version serve ui clone snapshot system help" -f -a "run" -d 'Create or reuse a machine, start it, and open an SSH shell'
complete -c firestone -n "__fish_firestone_using_subcommand help; and not __fish_seen_subcommand_from run create start stop restart resize rm ls show edit shell cp ssh-config console logs metrics catalog images doctor completions version serve ui clone snapshot system help" -f -a "create" -d 'Create a machine definition without booting it'
complete -c firestone -n "__fish_firestone_using_subcommand help; and not __fish_seen_subcommand_from run create start stop restart resize rm ls show edit shell cp ssh-config console logs metrics catalog images doctor completions version serve ui clone snapshot system help" -f -a "start" -d 'Start a machine and wait for SSH readiness'
complete -c firestone -n "__fish_firestone_using_subcommand help; and not __fish_seen_subcommand_from run create start stop restart resize rm ls show edit shell cp ssh-config console logs metrics catalog images doctor completions version serve ui clone snapshot system help" -f -a "stop" -d 'Stop a machine'
complete -c firestone -n "__fish_firestone_using_subcommand help; and not __fish_seen_subcommand_from run create start stop restart resize rm ls show edit shell cp ssh-config console logs metrics catalog images doctor completions version serve ui clone snapshot system help" -f -a "restart" -d 'Stop and start a machine'
complete -c firestone -n "__fish_firestone_using_subcommand help; and not __fish_seen_subcommand_from run create start stop restart resize rm ls show edit shell cp ssh-config console logs metrics catalog images doctor completions version serve ui clone snapshot system help" -f -a "resize" -d 'Change a machine\'s CPU count or memory'
complete -c firestone -n "__fish_firestone_using_subcommand help; and not __fish_seen_subcommand_from run create start stop restart resize rm ls show edit shell cp ssh-config console logs metrics catalog images doctor completions version serve ui clone snapshot system help" -f -a "rm" -d 'Stop and remove one or more machines'
complete -c firestone -n "__fish_firestone_using_subcommand help; and not __fish_seen_subcommand_from run create start stop restart resize rm ls show edit shell cp ssh-config console logs metrics catalog images doctor completions version serve ui clone snapshot system help" -f -a "ls" -d 'List machines'
complete -c firestone -n "__fish_firestone_using_subcommand help; and not __fish_seen_subcommand_from run create start stop restart resize rm ls show edit shell cp ssh-config console logs metrics catalog images doctor completions version serve ui clone snapshot system help" -f -a "show" -d 'Show a machine\'s specification and runtime state'
complete -c firestone -n "__fish_firestone_using_subcommand help; and not __fish_seen_subcommand_from run create start stop restart resize rm ls show edit shell cp ssh-config console logs metrics catalog images doctor completions version serve ui clone snapshot system help" -f -a "edit" -d 'Edit and validate a machine\'s firestone.toml'
complete -c firestone -n "__fish_firestone_using_subcommand help; and not __fish_seen_subcommand_from run create start stop restart resize rm ls show edit shell cp ssh-config console logs metrics catalog images doctor completions version serve ui clone snapshot system help" -f -a "shell" -d 'Open SSH over the machine\'s private vsock transport'
complete -c firestone -n "__fish_firestone_using_subcommand help; and not __fish_seen_subcommand_from run create start stop restart resize rm ls show edit shell cp ssh-config console logs metrics catalog images doctor completions version serve ui clone snapshot system help" -f -a "cp" -d 'Copy files between the host and a machine over the vsock transport'
complete -c firestone -n "__fish_firestone_using_subcommand help; and not __fish_seen_subcommand_from run create start stop restart resize rm ls show edit shell cp ssh-config console logs metrics catalog images doctor completions version serve ui clone snapshot system help" -f -a "ssh-config" -d 'Print an OpenSSH Host block for the machine'
complete -c firestone -n "__fish_firestone_using_subcommand help; and not __fish_seen_subcommand_from run create start stop restart resize rm ls show edit shell cp ssh-config console logs metrics catalog images doctor completions version serve ui clone snapshot system help" -f -a "console" -d 'Attach to the machine\'s hvc0 console'
complete -c firestone -n "__fish_firestone_using_subcommand help; and not __fish_seen_subcommand_from run create start stop restart resize rm ls show edit shell cp ssh-config console logs metrics catalog images doctor completions version serve ui clone snapshot system help" -f -a "logs" -d 'Print a bounded machine log'
complete -c firestone -n "__fish_firestone_using_subcommand help; and not __fish_seen_subcommand_from run create start stop restart resize rm ls show edit shell cp ssh-config console logs metrics catalog images doctor completions version serve ui clone snapshot system help" -f -a "metrics" -d 'Print one cumulative resource sample for a running machine'
complete -c firestone -n "__fish_firestone_using_subcommand help; and not __fish_seen_subcommand_from run create start stop restart resize rm ls show edit shell cp ssh-config console logs metrics catalog images doctor completions version serve ui clone snapshot system help" -f -a "catalog" -d 'Print the merged built-in and user image catalog'
complete -c firestone -n "__fish_firestone_using_subcommand help; and not __fish_seen_subcommand_from run create start stop restart resize rm ls show edit shell cp ssh-config console logs metrics catalog images doctor completions version serve ui clone snapshot system help" -f -a "images" -d 'Manage the owned image store'
complete -c firestone -n "__fish_firestone_using_subcommand help; and not __fish_seen_subcommand_from run create start stop restart resize rm ls show edit shell cp ssh-config console logs metrics catalog images doctor completions version serve ui clone snapshot system help" -f -a "doctor" -d 'Check host requirements and optional safe repairs'
complete -c firestone -n "__fish_firestone_using_subcommand help; and not __fish_seen_subcommand_from run create start stop restart resize rm ls show edit shell cp ssh-config console logs metrics catalog images doctor completions version serve ui clone snapshot system help" -f -a "completions" -d 'Generate a shell completion script on stdout'
complete -c firestone -n "__fish_firestone_using_subcommand help; and not __fish_seen_subcommand_from run create start stop restart resize rm ls show edit shell cp ssh-config console logs metrics catalog images doctor completions version serve ui clone snapshot system help" -f -a "version" -d 'Print Firestone, pinned dependency, and resolved path versions'
complete -c firestone -n "__fish_firestone_using_subcommand help; and not __fish_seen_subcommand_from run create start stop restart resize rm ls show edit shell cp ssh-config console logs metrics catalog images doctor completions version serve ui clone snapshot system help" -f -a "serve" -d 'Run the stateless REST API over a private Unix socket or loopback port'
complete -c firestone -n "__fish_firestone_using_subcommand help; and not __fish_seen_subcommand_from run create start stop restart resize rm ls show edit shell cp ssh-config console logs metrics catalog images doctor completions version serve ui clone snapshot system help" -f -a "ui" -d 'Open the Firestone web interface on a loopback port'
complete -c firestone -n "__fish_firestone_using_subcommand help; and not __fish_seen_subcommand_from run create start stop restart resize rm ls show edit shell cp ssh-config console logs metrics catalog images doctor completions version serve ui clone snapshot system help" -f -a "clone" -d 'Copy a stopped machine\'s spec and disk to a new machine'
complete -c firestone -n "__fish_firestone_using_subcommand help; and not __fish_seen_subcommand_from run create start stop restart resize rm ls show edit shell cp ssh-config console logs metrics catalog images doctor completions version serve ui clone snapshot system help" -f -a "snapshot" -d 'Capture, list, restore, and remove machine snapshots'
complete -c firestone -n "__fish_firestone_using_subcommand help; and not __fish_seen_subcommand_from run create start stop restart resize rm ls show edit shell cp ssh-config console logs metrics catalog images doctor completions version serve ui clone snapshot system help" -f -a "system" -d 'Inspect and reclaim host-wide Firestone storage'
complete -c firestone -n "__fish_firestone_using_subcommand help; and not __fish_seen_subcommand_from run create start stop restart resize rm ls show edit shell cp ssh-config console logs metrics catalog images doctor completions version serve ui clone snapshot system help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c firestone -n "__fish_firestone_using_subcommand help; and __fish_seen_subcommand_from images" -f -a "ls" -d 'List stored images'
complete -c firestone -n "__fish_firestone_using_subcommand help; and __fish_seen_subcommand_from images" -f -a "pull" -d 'Pull and verify one image'
complete -c firestone -n "__fish_firestone_using_subcommand help; and __fish_seen_subcommand_from images" -f -a "inspect" -d 'Verify and inspect one stored image'
complete -c firestone -n "__fish_firestone_using_subcommand help; and __fish_seen_subcommand_from images" -f -a "rm" -d 'Remove one stored image'
complete -c firestone -n "__fish_firestone_using_subcommand help; and __fish_seen_subcommand_from images" -f -a "prune" -d 'Remove all unreferenced images'
complete -c firestone -n "__fish_firestone_using_subcommand help; and __fish_seen_subcommand_from snapshot" -f -a "create" -d 'Capture one immutable snapshot of a machine'
complete -c firestone -n "__fish_firestone_using_subcommand help; and __fish_seen_subcommand_from snapshot" -f -a "list" -d 'List a machine\'s snapshots'
complete -c firestone -n "__fish_firestone_using_subcommand help; and __fish_seen_subcommand_from snapshot" -f -a "restore" -d 'Roll a machine back to one of its snapshots'
complete -c firestone -n "__fish_firestone_using_subcommand help; and __fish_seen_subcommand_from snapshot" -f -a "rm" -d 'Remove one snapshot'
complete -c firestone -n "__fish_firestone_using_subcommand help; and __fish_seen_subcommand_from system" -f -a "prune" -d 'Reclaim disk space held by Firestone\'s own artifacts'
