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
complete -c firestone -n "__fish_firestone_needs_command" -f -a "rm" -d 'Stop and remove one or more machines'
complete -c firestone -n "__fish_firestone_needs_command" -f -a "ls" -d 'List machines'
complete -c firestone -n "__fish_firestone_needs_command" -f -a "list" -d 'List machines'
complete -c firestone -n "__fish_firestone_needs_command" -f -a "show" -d 'Show a machine\'s specification and runtime state'
complete -c firestone -n "__fish_firestone_needs_command" -f -a "edit" -d 'Edit and validate a machine\'s firestone.toml'
complete -c firestone -n "__fish_firestone_needs_command" -f -a "shell" -d 'Open SSH over the machine\'s private vsock transport'
complete -c firestone -n "__fish_firestone_needs_command" -f -a "ssh" -d 'Open SSH over the machine\'s private vsock transport'
complete -c firestone -n "__fish_firestone_needs_command" -f -a "ssh-config" -d 'Print an OpenSSH Host block for the machine'
complete -c firestone -n "__fish_firestone_needs_command" -f -a "console" -d 'Attach to the machine\'s hvc0 console'
complete -c firestone -n "__fish_firestone_needs_command" -f -a "logs" -d 'Print a bounded machine log'
complete -c firestone -n "__fish_firestone_needs_command" -f -a "images" -d 'Manage the owned image store'
complete -c firestone -n "__fish_firestone_needs_command" -f -a "doctor" -d 'Check host requirements and optional safe repairs'
complete -c firestone -n "__fish_firestone_needs_command" -f -a "completions" -d 'Generate a shell completion script on stdout'
complete -c firestone -n "__fish_firestone_needs_command" -f -a "version" -d 'Print Firestone, pinned dependency, and resolved path versions'
complete -c firestone -n "__fish_firestone_needs_command" -f -a "serve" -d 'Run the stateless REST API over a private Unix socket'
complete -c firestone -n "__fish_firestone_needs_command" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c firestone -n "__fish_firestone_using_subcommand run" -l name -d 'Name a machine created from an image reference' -r
complete -c firestone -n "__fish_firestone_using_subcommand run" -l arch -r
complete -c firestone -n "__fish_firestone_using_subcommand run" -l cpus -r
complete -c firestone -n "__fish_firestone_using_subcommand run" -l memory -r
complete -c firestone -n "__fish_firestone_using_subcommand run" -l disk -r
complete -c firestone -n "__fish_firestone_using_subcommand run" -l user -r
complete -c firestone -n "__fish_firestone_using_subcommand run" -l net -r
complete -c firestone -n "__fish_firestone_using_subcommand run" -s p -l forward -r
complete -c firestone -n "__fish_firestone_using_subcommand run" -l tap -r
complete -c firestone -n "__fish_firestone_using_subcommand run" -l network-mac -r
complete -c firestone -n "__fish_firestone_using_subcommand run" -l mount -r
complete -c firestone -n "__fish_firestone_using_subcommand run" -l user-data -r -F
complete -c firestone -n "__fish_firestone_using_subcommand run" -l cloud-init-network-config -r -F
complete -c firestone -n "__fish_firestone_using_subcommand run" -l ssh-key -r -F
complete -c firestone -n "__fish_firestone_using_subcommand run" -l vmm-binary -r -F
complete -c firestone -n "__fish_firestone_using_subcommand run" -l vmm-firmware -r
complete -c firestone -n "__fish_firestone_using_subcommand run" -l vmm-arg -r
complete -c firestone -n "__fish_firestone_using_subcommand run" -l vmm-config -r
complete -c firestone -n "__fish_firestone_using_subcommand run" -l clear -r
complete -c firestone -n "__fish_firestone_using_subcommand run" -l home -d 'Override the Firestone home root (config, data, and runtime)' -r -F
complete -c firestone -n "__fish_firestone_using_subcommand run" -l rm -d 'Remove a machine created by this invocation after SSH exits'
complete -c firestone -n "__fish_firestone_using_subcommand run" -l no-provisioning
complete -c firestone -n "__fish_firestone_using_subcommand run" -l json -d 'Print events as newline-delimited JSON and disable human output'
complete -c firestone -n "__fish_firestone_using_subcommand run" -s q -l quiet -d 'Print only errors and command results'
complete -c firestone -n "__fish_firestone_using_subcommand run" -s v -l verbose -d 'Increase log detail. Pass twice for debug output'
complete -c firestone -n "__fish_firestone_using_subcommand run" -l no-color -d 'Disable colored output'
complete -c firestone -n "__fish_firestone_using_subcommand run" -s y -l yes -d 'Assume yes when a command may prompt'
complete -c firestone -n "__fish_firestone_using_subcommand run" -s h -l help -d 'Print help'
complete -c firestone -n "__fish_firestone_using_subcommand create" -l image -d 'Supply IMAGE as a flag. A sole positional value is then NAME' -r
complete -c firestone -n "__fish_firestone_using_subcommand create" -s f -l file -d 'Layer an existing machine specification below command-line flags' -r -F
complete -c firestone -n "__fish_firestone_using_subcommand create" -l arch -r
complete -c firestone -n "__fish_firestone_using_subcommand create" -l cpus -r
complete -c firestone -n "__fish_firestone_using_subcommand create" -l memory -r
complete -c firestone -n "__fish_firestone_using_subcommand create" -l disk -r
complete -c firestone -n "__fish_firestone_using_subcommand create" -l user -r
complete -c firestone -n "__fish_firestone_using_subcommand create" -l net -r
complete -c firestone -n "__fish_firestone_using_subcommand create" -s p -l forward -r
complete -c firestone -n "__fish_firestone_using_subcommand create" -l tap -r
complete -c firestone -n "__fish_firestone_using_subcommand create" -l network-mac -r
complete -c firestone -n "__fish_firestone_using_subcommand create" -l mount -r
complete -c firestone -n "__fish_firestone_using_subcommand create" -l user-data -r -F
complete -c firestone -n "__fish_firestone_using_subcommand create" -l cloud-init-network-config -r -F
complete -c firestone -n "__fish_firestone_using_subcommand create" -l ssh-key -r -F
complete -c firestone -n "__fish_firestone_using_subcommand create" -l vmm-binary -r -F
complete -c firestone -n "__fish_firestone_using_subcommand create" -l vmm-firmware -r
complete -c firestone -n "__fish_firestone_using_subcommand create" -l vmm-arg -r
complete -c firestone -n "__fish_firestone_using_subcommand create" -l vmm-config -r
complete -c firestone -n "__fish_firestone_using_subcommand create" -l clear -r
complete -c firestone -n "__fish_firestone_using_subcommand create" -l home -d 'Override the Firestone home root (config, data, and runtime)' -r -F
complete -c firestone -n "__fish_firestone_using_subcommand create" -l edit -d 'Open the generated specification in the configured editor'
complete -c firestone -n "__fish_firestone_using_subcommand create" -l no-provisioning
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
complete -c firestone -n "__fish_firestone_using_subcommand doctor" -l fix -d 'Perform only the safe unprivileged repairs'
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
complete -c firestone -n "__fish_firestone_using_subcommand serve" -l listen -d 'Listen at a Unix socket inside Firestone\'s private runtime directory' -r -F
complete -c firestone -n "__fish_firestone_using_subcommand serve" -l home -d 'Override the Firestone home root (config, data, and runtime)' -r -F
complete -c firestone -n "__fish_firestone_using_subcommand serve" -l json -d 'Print events as newline-delimited JSON and disable human output'
complete -c firestone -n "__fish_firestone_using_subcommand serve" -s q -l quiet -d 'Print only errors and command results'
complete -c firestone -n "__fish_firestone_using_subcommand serve" -s v -l verbose -d 'Increase log detail. Pass twice for debug output'
complete -c firestone -n "__fish_firestone_using_subcommand serve" -l no-color -d 'Disable colored output'
complete -c firestone -n "__fish_firestone_using_subcommand serve" -s y -l yes -d 'Assume yes when a command may prompt'
complete -c firestone -n "__fish_firestone_using_subcommand serve" -s h -l help -d 'Print help'
complete -c firestone -n "__fish_firestone_using_subcommand help; and not __fish_seen_subcommand_from run create start stop restart rm ls show edit shell ssh-config console logs images doctor completions version serve help" -f -a "run" -d 'Create or reuse a machine, start it, and open an SSH shell'
complete -c firestone -n "__fish_firestone_using_subcommand help; and not __fish_seen_subcommand_from run create start stop restart rm ls show edit shell ssh-config console logs images doctor completions version serve help" -f -a "create" -d 'Create a machine definition without booting it'
complete -c firestone -n "__fish_firestone_using_subcommand help; and not __fish_seen_subcommand_from run create start stop restart rm ls show edit shell ssh-config console logs images doctor completions version serve help" -f -a "start" -d 'Start a machine and wait for SSH readiness'
complete -c firestone -n "__fish_firestone_using_subcommand help; and not __fish_seen_subcommand_from run create start stop restart rm ls show edit shell ssh-config console logs images doctor completions version serve help" -f -a "stop" -d 'Stop a machine'
complete -c firestone -n "__fish_firestone_using_subcommand help; and not __fish_seen_subcommand_from run create start stop restart rm ls show edit shell ssh-config console logs images doctor completions version serve help" -f -a "restart" -d 'Stop and start a machine'
complete -c firestone -n "__fish_firestone_using_subcommand help; and not __fish_seen_subcommand_from run create start stop restart rm ls show edit shell ssh-config console logs images doctor completions version serve help" -f -a "rm" -d 'Stop and remove one or more machines'
complete -c firestone -n "__fish_firestone_using_subcommand help; and not __fish_seen_subcommand_from run create start stop restart rm ls show edit shell ssh-config console logs images doctor completions version serve help" -f -a "ls" -d 'List machines'
complete -c firestone -n "__fish_firestone_using_subcommand help; and not __fish_seen_subcommand_from run create start stop restart rm ls show edit shell ssh-config console logs images doctor completions version serve help" -f -a "show" -d 'Show a machine\'s specification and runtime state'
complete -c firestone -n "__fish_firestone_using_subcommand help; and not __fish_seen_subcommand_from run create start stop restart rm ls show edit shell ssh-config console logs images doctor completions version serve help" -f -a "edit" -d 'Edit and validate a machine\'s firestone.toml'
complete -c firestone -n "__fish_firestone_using_subcommand help; and not __fish_seen_subcommand_from run create start stop restart rm ls show edit shell ssh-config console logs images doctor completions version serve help" -f -a "shell" -d 'Open SSH over the machine\'s private vsock transport'
complete -c firestone -n "__fish_firestone_using_subcommand help; and not __fish_seen_subcommand_from run create start stop restart rm ls show edit shell ssh-config console logs images doctor completions version serve help" -f -a "ssh-config" -d 'Print an OpenSSH Host block for the machine'
complete -c firestone -n "__fish_firestone_using_subcommand help; and not __fish_seen_subcommand_from run create start stop restart rm ls show edit shell ssh-config console logs images doctor completions version serve help" -f -a "console" -d 'Attach to the machine\'s hvc0 console'
complete -c firestone -n "__fish_firestone_using_subcommand help; and not __fish_seen_subcommand_from run create start stop restart rm ls show edit shell ssh-config console logs images doctor completions version serve help" -f -a "logs" -d 'Print a bounded machine log'
complete -c firestone -n "__fish_firestone_using_subcommand help; and not __fish_seen_subcommand_from run create start stop restart rm ls show edit shell ssh-config console logs images doctor completions version serve help" -f -a "images" -d 'Manage the owned image store'
complete -c firestone -n "__fish_firestone_using_subcommand help; and not __fish_seen_subcommand_from run create start stop restart rm ls show edit shell ssh-config console logs images doctor completions version serve help" -f -a "doctor" -d 'Check host requirements and optional safe repairs'
complete -c firestone -n "__fish_firestone_using_subcommand help; and not __fish_seen_subcommand_from run create start stop restart rm ls show edit shell ssh-config console logs images doctor completions version serve help" -f -a "completions" -d 'Generate a shell completion script on stdout'
complete -c firestone -n "__fish_firestone_using_subcommand help; and not __fish_seen_subcommand_from run create start stop restart rm ls show edit shell ssh-config console logs images doctor completions version serve help" -f -a "version" -d 'Print Firestone, pinned dependency, and resolved path versions'
complete -c firestone -n "__fish_firestone_using_subcommand help; and not __fish_seen_subcommand_from run create start stop restart rm ls show edit shell ssh-config console logs images doctor completions version serve help" -f -a "serve" -d 'Run the stateless REST API over a private Unix socket'
complete -c firestone -n "__fish_firestone_using_subcommand help; and not __fish_seen_subcommand_from run create start stop restart rm ls show edit shell ssh-config console logs images doctor completions version serve help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c firestone -n "__fish_firestone_using_subcommand help; and __fish_seen_subcommand_from images" -f -a "ls" -d 'List stored images'
complete -c firestone -n "__fish_firestone_using_subcommand help; and __fish_seen_subcommand_from images" -f -a "pull" -d 'Pull and verify one image'
complete -c firestone -n "__fish_firestone_using_subcommand help; and __fish_seen_subcommand_from images" -f -a "inspect" -d 'Verify and inspect one stored image'
complete -c firestone -n "__fish_firestone_using_subcommand help; and __fish_seen_subcommand_from images" -f -a "rm" -d 'Remove one stored image'
complete -c firestone -n "__fish_firestone_using_subcommand help; and __fish_seen_subcommand_from images" -f -a "prune" -d 'Remove all unreferenced images'
