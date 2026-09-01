#compdef firestone

autoload -U is-at-least

_firestone() {
    typeset -A opt_args
    typeset -a _arguments_options
    local ret=1

    if is-at-least 5.2; then
        _arguments_options=(-s -S -C)
    else
        _arguments_options=(-s -C)
    fi

    local context curcontext="$curcontext" state line
    _arguments "${_arguments_options[@]}" : \
'--home=[Override the Firestone home root (config, data, and runtime)]:DIR:_files' \
'--json[Print events as newline-delimited JSON and disable human output]' \
'-q[Print only errors and command results]' \
'--quiet[Print only errors and command results]' \
'*-v[Increase log detail. Pass twice for debug output]' \
'*--verbose[Increase log detail. Pass twice for debug output]' \
'--no-color[Disable colored output]' \
'-y[Assume yes when a command may prompt]' \
'--yes[Assume yes when a command may prompt]' \
'-h[Print help]' \
'--help[Print help]' \
'-V[Print version]' \
'--version[Print version]' \
":: :_firestone_commands" \
"*::: :->firestone" \
&& ret=0
    case $state in
    (firestone)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:firestone-command-$line[1]:"
        case $line[1] in
            (run)
_arguments "${_arguments_options[@]}" : \
'--name=[Name a machine created from an image reference]:NAME:_default' \
'--arch=[Set the guest architecture; it must match the host]:ARCH:_default' \
'--cpus=[Set the number of virtual CPUs]:COUNT:_default' \
'--cpus-max=[Reserve vCPU hotplug headroom for \`resize\`; must be at least --cpus]:COUNT:_default' \
'--memory=[Set guest memory, for example 2G or 2048M]:SIZE:_default' \
'--memory-max=[Reserve memory hotplug headroom for \`resize\`; must be at least --memory]:SIZE:_default' \
'--disk=[Set writable disk capacity, for example 20G]:SIZE:_default' \
'--user=[Set the guest login user created by Firestone provisioning]:USER:_default' \
'--net=[Select passt, tap, or no network]:MODE:_default' \
'*-p+[Forward a host port or range to the guest; repeat as needed]:SPEC:_default' \
'*--forward=[Forward a host port or range to the guest; repeat as needed]:SPEC:_default' \
'--tap=[Use an existing host tap interface with --net tap]:DEV:_default' \
'--network-mac=[Set a fixed guest network MAC address]:MAC:_default' \
'*--mount=[Share a host directory with the guest; repeat as needed]:HOST:GUEST[:ro]:_default' \
'--user-data=[Add a cloud-init user-data file]:FILE:_files' \
'--user-data-inline=[Set cloud-init user-data inline instead of from a file]:TEXT:_default' \
'--cloud-init-network-config=[Add a cloud-init network-config file]:FILE:_files' \
'*--ssh-key=[Add an OpenSSH public-key file; repeat as needed]:FILE:_files' \
'*--ssh-authorized-key=[Add an inline OpenSSH public key; repeat as needed]:KEY:_default' \
'--password-file=[Read the guest password for --user from a file]:FILE:_default' \
'--vmm-binary=[Use a custom cloud-hypervisor executable]:FILE:_files' \
'--vmm-firmware=[Select auto, rhf, edk2, or a firmware file]:FIRMWARE:_default' \
'*--vmm-arg=[Append one cloud-hypervisor argument; repeat as needed]:ARG:_default' \
'--vmm-config=[Merge a JSON object into the generated VMM configuration]:JSON:_default' \
'*--clear=[Clear an inherited optional field; repeat as needed]:FIELD:_default' \
'--home=[Override the Firestone home root (config, data, and runtime)]:DIR:_files' \
'--rm[Remove a machine created by this invocation after SSH exits]' \
'--ssh-pwauth[Allow SSH password authentication in the guest]' \
'--no-provisioning[Disable Firestone'\''s built-in guest provisioning]' \
'--json[Print events as newline-delimited JSON and disable human output]' \
'-q[Print only errors and command results]' \
'--quiet[Print only errors and command results]' \
'*-v[Increase log detail. Pass twice for debug output]' \
'*--verbose[Increase log detail. Pass twice for debug output]' \
'--no-color[Disable colored output]' \
'-y[Assume yes when a command may prompt]' \
'--yes[Assume yes when a command may prompt]' \
'-h[Print help]' \
'--help[Print help]' \
'::target -- Existing machine name or image reference. Defaults to ubuntu:_default' \
'*::command -- Remote command. Values are passed to OpenSSH without retokenizing:_default' \
&& ret=0
;;
(create)
_arguments "${_arguments_options[@]}" : \
'--image=[Select the image by catalog reference, HTTPS URL, or local file]:IMAGE:_default' \
'-f+[Layer an existing machine specification below command-line flags]:SPEC.toml:_files' \
'--file=[Layer an existing machine specification below command-line flags]:SPEC.toml:_files' \
'--arch=[Set the guest architecture; it must match the host]:ARCH:_default' \
'--cpus=[Set the number of virtual CPUs]:COUNT:_default' \
'--cpus-max=[Reserve vCPU hotplug headroom for \`resize\`; must be at least --cpus]:COUNT:_default' \
'--memory=[Set guest memory, for example 2G or 2048M]:SIZE:_default' \
'--memory-max=[Reserve memory hotplug headroom for \`resize\`; must be at least --memory]:SIZE:_default' \
'--disk=[Set writable disk capacity, for example 20G]:SIZE:_default' \
'--user=[Set the guest login user created by Firestone provisioning]:USER:_default' \
'--net=[Select passt, tap, or no network]:MODE:_default' \
'*-p+[Forward a host port or range to the guest; repeat as needed]:SPEC:_default' \
'*--forward=[Forward a host port or range to the guest; repeat as needed]:SPEC:_default' \
'--tap=[Use an existing host tap interface with --net tap]:DEV:_default' \
'--network-mac=[Set a fixed guest network MAC address]:MAC:_default' \
'*--mount=[Share a host directory with the guest; repeat as needed]:HOST:GUEST[:ro]:_default' \
'--user-data=[Add a cloud-init user-data file]:FILE:_files' \
'--user-data-inline=[Set cloud-init user-data inline instead of from a file]:TEXT:_default' \
'--cloud-init-network-config=[Add a cloud-init network-config file]:FILE:_files' \
'*--ssh-key=[Add an OpenSSH public-key file; repeat as needed]:FILE:_files' \
'*--ssh-authorized-key=[Add an inline OpenSSH public key; repeat as needed]:KEY:_default' \
'--password-file=[Read the guest password for --user from a file]:FILE:_default' \
'--vmm-binary=[Use a custom cloud-hypervisor executable]:FILE:_files' \
'--vmm-firmware=[Select auto, rhf, edk2, or a firmware file]:FIRMWARE:_default' \
'*--vmm-arg=[Append one cloud-hypervisor argument; repeat as needed]:ARG:_default' \
'--vmm-config=[Merge a JSON object into the generated VMM configuration]:JSON:_default' \
'*--clear=[Clear an inherited optional field; repeat as needed]:FIELD:_default' \
'--home=[Override the Firestone home root (config, data, and runtime)]:DIR:_files' \
'--edit[Open the generated specification in the configured editor]' \
'--ssh-pwauth[Allow SSH password authentication in the guest]' \
'--no-provisioning[Disable Firestone'\''s built-in guest provisioning]' \
'--json[Print events as newline-delimited JSON and disable human output]' \
'-q[Print only errors and command results]' \
'--quiet[Print only errors and command results]' \
'*-v[Increase log detail. Pass twice for debug output]' \
'*--verbose[Increase log detail. Pass twice for debug output]' \
'--no-color[Disable colored output]' \
'-y[Assume yes when a command may prompt]' \
'--yes[Assume yes when a command may prompt]' \
'-h[Print help]' \
'--help[Print help]' \
'*::positional -- Set IMAGE, or set both NAME and IMAGE:_default' \
&& ret=0
;;
(start)
_arguments "${_arguments_options[@]}" : \
'--timeout=[Override the configured start deadline]:DURATION:_default' \
'--home=[Override the Firestone home root (config, data, and runtime)]:DIR:_files' \
'--no-wait[Return immediately after the VMM reaches persisted running state]' \
'--json[Print events as newline-delimited JSON and disable human output]' \
'-q[Print only errors and command results]' \
'--quiet[Print only errors and command results]' \
'*-v[Increase log detail. Pass twice for debug output]' \
'*--verbose[Increase log detail. Pass twice for debug output]' \
'--no-color[Disable colored output]' \
'-y[Assume yes when a command may prompt]' \
'--yes[Assume yes when a command may prompt]' \
'-h[Print help]' \
'--help[Print help]' \
':name:_default' \
&& ret=0
;;
(stop)
_arguments "${_arguments_options[@]}" : \
'--timeout=[Override the configured graceful-stop deadline]:DURATION:_default' \
'--home=[Override the Firestone home root (config, data, and runtime)]:DIR:_files' \
'--force[Skip the guest power button and kill the VMM]' \
'--json[Print events as newline-delimited JSON and disable human output]' \
'-q[Print only errors and command results]' \
'--quiet[Print only errors and command results]' \
'*-v[Increase log detail. Pass twice for debug output]' \
'*--verbose[Increase log detail. Pass twice for debug output]' \
'--no-color[Disable colored output]' \
'-y[Assume yes when a command may prompt]' \
'--yes[Assume yes when a command may prompt]' \
'-h[Print help]' \
'--help[Print help]' \
':name:_default' \
&& ret=0
;;
(restart)
_arguments "${_arguments_options[@]}" : \
'--home=[Override the Firestone home root (config, data, and runtime)]:DIR:_files' \
'--json[Print events as newline-delimited JSON and disable human output]' \
'-q[Print only errors and command results]' \
'--quiet[Print only errors and command results]' \
'*-v[Increase log detail. Pass twice for debug output]' \
'*--verbose[Increase log detail. Pass twice for debug output]' \
'--no-color[Disable colored output]' \
'-y[Assume yes when a command may prompt]' \
'--yes[Assume yes when a command may prompt]' \
'-h[Print help]' \
'--help[Print help]' \
':name:_default' \
&& ret=0
;;
(resize)
_arguments "${_arguments_options[@]}" : \
'--cpus=[Set the number of virtual CPUs]:COUNT:_default' \
'--memory=[Set guest memory, for example 4G or 4096M]:SIZE:_default' \
'--home=[Override the Firestone home root (config, data, and runtime)]:DIR:_files' \
'--json[Print events as newline-delimited JSON and disable human output]' \
'-q[Print only errors and command results]' \
'--quiet[Print only errors and command results]' \
'*-v[Increase log detail. Pass twice for debug output]' \
'*--verbose[Increase log detail. Pass twice for debug output]' \
'--no-color[Disable colored output]' \
'-y[Assume yes when a command may prompt]' \
'--yes[Assume yes when a command may prompt]' \
'-h[Print help]' \
'--help[Print help]' \
':name:_default' \
&& ret=0
;;
(rm)
_arguments "${_arguments_options[@]}" : \
'--home=[Override the Firestone home root (config, data, and runtime)]:DIR:_files' \
'--force[Approve removal of running machines]' \
'--json[Print events as newline-delimited JSON and disable human output]' \
'-q[Print only errors and command results]' \
'--quiet[Print only errors and command results]' \
'*-v[Increase log detail. Pass twice for debug output]' \
'*--verbose[Increase log detail. Pass twice for debug output]' \
'--no-color[Disable colored output]' \
'-y[Assume yes when a command may prompt]' \
'--yes[Assume yes when a command may prompt]' \
'-h[Print help]' \
'--help[Print help]' \
'*::names:_default' \
&& ret=0
;;
(ls)
_arguments "${_arguments_options[@]}" : \
'--home=[Override the Firestone home root (config, data, and runtime)]:DIR:_files' \
'--json[Print events as newline-delimited JSON and disable human output]' \
'-q[Print only errors and command results]' \
'--quiet[Print only errors and command results]' \
'*-v[Increase log detail. Pass twice for debug output]' \
'*--verbose[Increase log detail. Pass twice for debug output]' \
'--no-color[Disable colored output]' \
'-y[Assume yes when a command may prompt]' \
'--yes[Assume yes when a command may prompt]' \
'-h[Print help]' \
'--help[Print help]' \
&& ret=0
;;
(list)
_arguments "${_arguments_options[@]}" : \
'--home=[Override the Firestone home root (config, data, and runtime)]:DIR:_files' \
'--json[Print events as newline-delimited JSON and disable human output]' \
'-q[Print only errors and command results]' \
'--quiet[Print only errors and command results]' \
'*-v[Increase log detail. Pass twice for debug output]' \
'*--verbose[Increase log detail. Pass twice for debug output]' \
'--no-color[Disable colored output]' \
'-y[Assume yes when a command may prompt]' \
'--yes[Assume yes when a command may prompt]' \
'-h[Print help]' \
'--help[Print help]' \
&& ret=0
;;
(show)
_arguments "${_arguments_options[@]}" : \
'--home=[Override the Firestone home root (config, data, and runtime)]:DIR:_files' \
'--vmconfig[Include the generated cloud-hypervisor configuration]' \
'--json[Print events as newline-delimited JSON and disable human output]' \
'-q[Print only errors and command results]' \
'--quiet[Print only errors and command results]' \
'*-v[Increase log detail. Pass twice for debug output]' \
'*--verbose[Increase log detail. Pass twice for debug output]' \
'--no-color[Disable colored output]' \
'-y[Assume yes when a command may prompt]' \
'--yes[Assume yes when a command may prompt]' \
'-h[Print help]' \
'--help[Print help]' \
':name:_default' \
&& ret=0
;;
(edit)
_arguments "${_arguments_options[@]}" : \
'--home=[Override the Firestone home root (config, data, and runtime)]:DIR:_files' \
'--json[Print events as newline-delimited JSON and disable human output]' \
'-q[Print only errors and command results]' \
'--quiet[Print only errors and command results]' \
'*-v[Increase log detail. Pass twice for debug output]' \
'*--verbose[Increase log detail. Pass twice for debug output]' \
'--no-color[Disable colored output]' \
'-y[Assume yes when a command may prompt]' \
'--yes[Assume yes when a command may prompt]' \
'-h[Print help]' \
'--help[Print help]' \
':name:_default' \
&& ret=0
;;
(shell)
_arguments "${_arguments_options[@]}" : \
'--user=[Select the guest login user]:USER:_default' \
'--home=[Override the Firestone home root (config, data, and runtime)]:DIR:_files' \
'--json[Print events as newline-delimited JSON and disable human output]' \
'-q[Print only errors and command results]' \
'--quiet[Print only errors and command results]' \
'*-v[Increase log detail. Pass twice for debug output]' \
'*--verbose[Increase log detail. Pass twice for debug output]' \
'--no-color[Disable colored output]' \
'-y[Assume yes when a command may prompt]' \
'--yes[Assume yes when a command may prompt]' \
'-h[Print help]' \
'--help[Print help]' \
':name:_default' \
'*::command -- Remote command. Values are passed to OpenSSH without retokenizing:_default' \
&& ret=0
;;
(ssh)
_arguments "${_arguments_options[@]}" : \
'--user=[Select the guest login user]:USER:_default' \
'--home=[Override the Firestone home root (config, data, and runtime)]:DIR:_files' \
'--json[Print events as newline-delimited JSON and disable human output]' \
'-q[Print only errors and command results]' \
'--quiet[Print only errors and command results]' \
'*-v[Increase log detail. Pass twice for debug output]' \
'*--verbose[Increase log detail. Pass twice for debug output]' \
'--no-color[Disable colored output]' \
'-y[Assume yes when a command may prompt]' \
'--yes[Assume yes when a command may prompt]' \
'-h[Print help]' \
'--help[Print help]' \
':name:_default' \
'*::command -- Remote command. Values are passed to OpenSSH without retokenizing:_default' \
&& ret=0
;;
(cp)
_arguments "${_arguments_options[@]}" : \
'--home=[Override the Firestone home root (config, data, and runtime)]:DIR:_files' \
'-r[Copy directories recursively]' \
'--recursive[Copy directories recursively]' \
'--json[Print events as newline-delimited JSON and disable human output]' \
'-q[Print only errors and command results]' \
'--quiet[Print only errors and command results]' \
'*-v[Increase log detail. Pass twice for debug output]' \
'*--verbose[Increase log detail. Pass twice for debug output]' \
'--no-color[Disable colored output]' \
'-y[Assume yes when a command may prompt]' \
'--yes[Assume yes when a command may prompt]' \
'-h[Print help]' \
'--help[Print help]' \
':source -- Source operand. Exactly one operand is remote, written `<machine>\:<path>`:_default' \
':target -- Destination operand. Exactly one operand is remote, written `<machine>\:<path>`:_default' \
&& ret=0
;;
(ssh-config)
_arguments "${_arguments_options[@]}" : \
'--home=[Override the Firestone home root (config, data, and runtime)]:DIR:_files' \
'--json[Print events as newline-delimited JSON and disable human output]' \
'-q[Print only errors and command results]' \
'--quiet[Print only errors and command results]' \
'*-v[Increase log detail. Pass twice for debug output]' \
'*--verbose[Increase log detail. Pass twice for debug output]' \
'--no-color[Disable colored output]' \
'-y[Assume yes when a command may prompt]' \
'--yes[Assume yes when a command may prompt]' \
'-h[Print help]' \
'--help[Print help]' \
':name:_default' \
&& ret=0
;;
(console)
_arguments "${_arguments_options[@]}" : \
'--home=[Override the Firestone home root (config, data, and runtime)]:DIR:_files' \
'--json[Print events as newline-delimited JSON and disable human output]' \
'-q[Print only errors and command results]' \
'--quiet[Print only errors and command results]' \
'*-v[Increase log detail. Pass twice for debug output]' \
'*--verbose[Increase log detail. Pass twice for debug output]' \
'--no-color[Disable colored output]' \
'-y[Assume yes when a command may prompt]' \
'--yes[Assume yes when a command may prompt]' \
'-h[Print help]' \
'--help[Print help]' \
':name:_default' \
&& ret=0
;;
(logs)
_arguments "${_arguments_options[@]}" : \
'--source=[Select an owned machine log]:SOURCE:_default' \
'-n+[Print the last LINES lines before following]:LINES:_default' \
'--home=[Override the Firestone home root (config, data, and runtime)]:DIR:_files' \
'-f[Continue printing appended log data until interrupted]' \
'--follow[Continue printing appended log data until interrupted]' \
'--json[Print events as newline-delimited JSON and disable human output]' \
'-q[Print only errors and command results]' \
'--quiet[Print only errors and command results]' \
'*-v[Increase log detail. Pass twice for debug output]' \
'*--verbose[Increase log detail. Pass twice for debug output]' \
'--no-color[Disable colored output]' \
'-y[Assume yes when a command may prompt]' \
'--yes[Assume yes when a command may prompt]' \
'-h[Print help]' \
'--help[Print help]' \
':name:_default' \
&& ret=0
;;
(metrics)
_arguments "${_arguments_options[@]}" : \
'--home=[Override the Firestone home root (config, data, and runtime)]:DIR:_files' \
'--json[Print events as newline-delimited JSON and disable human output]' \
'-q[Print only errors and command results]' \
'--quiet[Print only errors and command results]' \
'*-v[Increase log detail. Pass twice for debug output]' \
'*--verbose[Increase log detail. Pass twice for debug output]' \
'--no-color[Disable colored output]' \
'-y[Assume yes when a command may prompt]' \
'--yes[Assume yes when a command may prompt]' \
'-h[Print help]' \
'--help[Print help]' \
':name:_default' \
&& ret=0
;;
(catalog)
_arguments "${_arguments_options[@]}" : \
'--home=[Override the Firestone home root (config, data, and runtime)]:DIR:_files' \
'--json[Print events as newline-delimited JSON and disable human output]' \
'-q[Print only errors and command results]' \
'--quiet[Print only errors and command results]' \
'*-v[Increase log detail. Pass twice for debug output]' \
'*--verbose[Increase log detail. Pass twice for debug output]' \
'--no-color[Disable colored output]' \
'-y[Assume yes when a command may prompt]' \
'--yes[Assume yes when a command may prompt]' \
'-h[Print help]' \
'--help[Print help]' \
&& ret=0
;;
(images)
_arguments "${_arguments_options[@]}" : \
'--home=[Override the Firestone home root (config, data, and runtime)]:DIR:_files' \
'--json[Print events as newline-delimited JSON and disable human output]' \
'-q[Print only errors and command results]' \
'--quiet[Print only errors and command results]' \
'*-v[Increase log detail. Pass twice for debug output]' \
'*--verbose[Increase log detail. Pass twice for debug output]' \
'--no-color[Disable colored output]' \
'-y[Assume yes when a command may prompt]' \
'--yes[Assume yes when a command may prompt]' \
'-h[Print help]' \
'--help[Print help]' \
":: :_firestone__subcmd__images_commands" \
"*::: :->images" \
&& ret=0

    case $state in
    (images)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:firestone-images-command-$line[1]:"
        case $line[1] in
            (ls)
_arguments "${_arguments_options[@]}" : \
'--home=[Override the Firestone home root (config, data, and runtime)]:DIR:_files' \
'--json[Print events as newline-delimited JSON and disable human output]' \
'-q[Print only errors and command results]' \
'--quiet[Print only errors and command results]' \
'*-v[Increase log detail. Pass twice for debug output]' \
'*--verbose[Increase log detail. Pass twice for debug output]' \
'--no-color[Disable colored output]' \
'-y[Assume yes when a command may prompt]' \
'--yes[Assume yes when a command may prompt]' \
'-h[Print help]' \
'--help[Print help]' \
&& ret=0
;;
(pull)
_arguments "${_arguments_options[@]}" : \
'--sha256=[Verify a direct HTTPS URL with this SHA-256 digest]:HEX:_default' \
'--home=[Override the Firestone home root (config, data, and runtime)]:DIR:_files' \
'--json[Print events as newline-delimited JSON and disable human output]' \
'-q[Print only errors and command results]' \
'--quiet[Print only errors and command results]' \
'*-v[Increase log detail. Pass twice for debug output]' \
'*--verbose[Increase log detail. Pass twice for debug output]' \
'--no-color[Disable colored output]' \
'-y[Assume yes when a command may prompt]' \
'--yes[Assume yes when a command may prompt]' \
'-h[Print help]' \
'--help[Print help]' \
':reference:_default' \
&& ret=0
;;
(inspect)
_arguments "${_arguments_options[@]}" : \
'--home=[Override the Firestone home root (config, data, and runtime)]:DIR:_files' \
'--json[Print events as newline-delimited JSON and disable human output]' \
'-q[Print only errors and command results]' \
'--quiet[Print only errors and command results]' \
'*-v[Increase log detail. Pass twice for debug output]' \
'*--verbose[Increase log detail. Pass twice for debug output]' \
'--no-color[Disable colored output]' \
'-y[Assume yes when a command may prompt]' \
'--yes[Assume yes when a command may prompt]' \
'-h[Print help]' \
'--help[Print help]' \
':id:_default' \
&& ret=0
;;
(rm)
_arguments "${_arguments_options[@]}" : \
'--home=[Override the Firestone home root (config, data, and runtime)]:DIR:_files' \
'--force[Approve removal while a machine still references the image]' \
'--json[Print events as newline-delimited JSON and disable human output]' \
'-q[Print only errors and command results]' \
'--quiet[Print only errors and command results]' \
'*-v[Increase log detail. Pass twice for debug output]' \
'*--verbose[Increase log detail. Pass twice for debug output]' \
'--no-color[Disable colored output]' \
'-y[Assume yes when a command may prompt]' \
'--yes[Assume yes when a command may prompt]' \
'-h[Print help]' \
'--help[Print help]' \
':id:_default' \
&& ret=0
;;
(prune)
_arguments "${_arguments_options[@]}" : \
'--home=[Override the Firestone home root (config, data, and runtime)]:DIR:_files' \
'--json[Print events as newline-delimited JSON and disable human output]' \
'-q[Print only errors and command results]' \
'--quiet[Print only errors and command results]' \
'*-v[Increase log detail. Pass twice for debug output]' \
'*--verbose[Increase log detail. Pass twice for debug output]' \
'--no-color[Disable colored output]' \
'-y[Assume yes when a command may prompt]' \
'--yes[Assume yes when a command may prompt]' \
'-h[Print help]' \
'--help[Print help]' \
&& ret=0
;;
(help)
_arguments "${_arguments_options[@]}" : \
":: :_firestone__subcmd__images__subcmd__help_commands" \
"*::: :->help" \
&& ret=0

    case $state in
    (help)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:firestone-images-help-command-$line[1]:"
        case $line[1] in
            (ls)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(pull)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(inspect)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(rm)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(prune)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(help)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
        esac
    ;;
esac
;;
        esac
    ;;
esac
;;
(doctor)
_arguments "${_arguments_options[@]}" : \
'--home=[Override the Firestone home root (config, data, and runtime)]:DIR:_files' \
'--fix[Apply safe repairs; AppArmor elevation needs a TTY prompt and ignores --yes]' \
'--json[Print events as newline-delimited JSON and disable human output]' \
'-q[Print only errors and command results]' \
'--quiet[Print only errors and command results]' \
'*-v[Increase log detail. Pass twice for debug output]' \
'*--verbose[Increase log detail. Pass twice for debug output]' \
'--no-color[Disable colored output]' \
'-y[Assume yes when a command may prompt]' \
'--yes[Assume yes when a command may prompt]' \
'-h[Print help]' \
'--help[Print help]' \
&& ret=0
;;
(completions)
_arguments "${_arguments_options[@]}" : \
'--home=[Override the Firestone home root (config, data, and runtime)]:DIR:_files' \
'--json[Print events as newline-delimited JSON and disable human output]' \
'-q[Print only errors and command results]' \
'--quiet[Print only errors and command results]' \
'*-v[Increase log detail. Pass twice for debug output]' \
'*--verbose[Increase log detail. Pass twice for debug output]' \
'--no-color[Disable colored output]' \
'-y[Assume yes when a command may prompt]' \
'--yes[Assume yes when a command may prompt]' \
'-h[Print help]' \
'--help[Print help]' \
':shell -- Shell whose completion script should be generated:(bash elvish fish powershell zsh)' \
&& ret=0
;;
(version)
_arguments "${_arguments_options[@]}" : \
'--home=[Override the Firestone home root (config, data, and runtime)]:DIR:_files' \
'--json[Print events as newline-delimited JSON and disable human output]' \
'-q[Print only errors and command results]' \
'--quiet[Print only errors and command results]' \
'*-v[Increase log detail. Pass twice for debug output]' \
'*--verbose[Increase log detail. Pass twice for debug output]' \
'--no-color[Disable colored output]' \
'-y[Assume yes when a command may prompt]' \
'--yes[Assume yes when a command may prompt]' \
'-h[Print help]' \
'--help[Print help]' \
&& ret=0
;;
(serve)
_arguments "${_arguments_options[@]}" : \
'--listen=[Listen at a private Unix socket, or at a loopback TCP address]:unix:PATH|tcp:HOST:PORT:_default' \
'--token=[File holding the 64-hexadecimal-character session token. TCP only]:FILE:_files' \
'--home=[Override the Firestone home root (config, data, and runtime)]:DIR:_files' \
'--json[Print events as newline-delimited JSON and disable human output]' \
'-q[Print only errors and command results]' \
'--quiet[Print only errors and command results]' \
'*-v[Increase log detail. Pass twice for debug output]' \
'*--verbose[Increase log detail. Pass twice for debug output]' \
'--no-color[Disable colored output]' \
'-y[Assume yes when a command may prompt]' \
'--yes[Assume yes when a command may prompt]' \
'-h[Print help]' \
'--help[Print help]' \
&& ret=0
;;
(ui)
_arguments "${_arguments_options[@]}" : \
'--port=[Loopback port to bind. Zero asks the kernel for any free port]:PORT:_default' \
'--home=[Override the Firestone home root (config, data, and runtime)]:DIR:_files' \
'--no-open[Do not launch a browser; print the URL only]' \
'--print-url[Print the URL and never launch a browser. Implies --no-open]' \
'--json[Print events as newline-delimited JSON and disable human output]' \
'-q[Print only errors and command results]' \
'--quiet[Print only errors and command results]' \
'*-v[Increase log detail. Pass twice for debug output]' \
'*--verbose[Increase log detail. Pass twice for debug output]' \
'--no-color[Disable colored output]' \
'-y[Assume yes when a command may prompt]' \
'--yes[Assume yes when a command may prompt]' \
'-h[Print help]' \
'--help[Print help]' \
&& ret=0
;;
(clone)
_arguments "${_arguments_options[@]}" : \
'--home=[Override the Firestone home root (config, data, and runtime)]:DIR:_files' \
'--fresh-disk[Give the clone an empty overlay on the same base image]' \
'--json[Print events as newline-delimited JSON and disable human output]' \
'-q[Print only errors and command results]' \
'--quiet[Print only errors and command results]' \
'*-v[Increase log detail. Pass twice for debug output]' \
'*--verbose[Increase log detail. Pass twice for debug output]' \
'--no-color[Disable colored output]' \
'-y[Assume yes when a command may prompt]' \
'--yes[Assume yes when a command may prompt]' \
'-h[Print help]' \
'--help[Print help]' \
':source -- Existing stopped or created machine to copy:_default' \
':dest -- New machine name:_default' \
&& ret=0
;;
(snapshot)
_arguments "${_arguments_options[@]}" : \
'--home=[Override the Firestone home root (config, data, and runtime)]:DIR:_files' \
'--json[Print events as newline-delimited JSON and disable human output]' \
'-q[Print only errors and command results]' \
'--quiet[Print only errors and command results]' \
'*-v[Increase log detail. Pass twice for debug output]' \
'*--verbose[Increase log detail. Pass twice for debug output]' \
'--no-color[Disable colored output]' \
'-y[Assume yes when a command may prompt]' \
'--yes[Assume yes when a command may prompt]' \
'-h[Print help]' \
'--help[Print help]' \
":: :_firestone__subcmd__snapshot_commands" \
"*::: :->snapshot" \
&& ret=0

    case $state in
    (snapshot)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:firestone-snapshot-command-$line[1]:"
        case $line[1] in
            (create)
_arguments "${_arguments_options[@]}" : \
'--home=[Override the Firestone home root (config, data, and runtime)]:DIR:_files' \
'--json[Print events as newline-delimited JSON and disable human output]' \
'-q[Print only errors and command results]' \
'--quiet[Print only errors and command results]' \
'*-v[Increase log detail. Pass twice for debug output]' \
'*--verbose[Increase log detail. Pass twice for debug output]' \
'--no-color[Disable colored output]' \
'-y[Assume yes when a command may prompt]' \
'--yes[Assume yes when a command may prompt]' \
'-h[Print help]' \
'--help[Print help]' \
':name:_default' \
'::snapshot -- Snapshot name. Defaults to snap-<yyyymmdd>-<hhmmss>:_default' \
&& ret=0
;;
(list)
_arguments "${_arguments_options[@]}" : \
'--home=[Override the Firestone home root (config, data, and runtime)]:DIR:_files' \
'--json[Print events as newline-delimited JSON and disable human output]' \
'-q[Print only errors and command results]' \
'--quiet[Print only errors and command results]' \
'*-v[Increase log detail. Pass twice for debug output]' \
'*--verbose[Increase log detail. Pass twice for debug output]' \
'--no-color[Disable colored output]' \
'-y[Assume yes when a command may prompt]' \
'--yes[Assume yes when a command may prompt]' \
'-h[Print help]' \
'--help[Print help]' \
':name:_default' \
&& ret=0
;;
(ls)
_arguments "${_arguments_options[@]}" : \
'--home=[Override the Firestone home root (config, data, and runtime)]:DIR:_files' \
'--json[Print events as newline-delimited JSON and disable human output]' \
'-q[Print only errors and command results]' \
'--quiet[Print only errors and command results]' \
'*-v[Increase log detail. Pass twice for debug output]' \
'*--verbose[Increase log detail. Pass twice for debug output]' \
'--no-color[Disable colored output]' \
'-y[Assume yes when a command may prompt]' \
'--yes[Assume yes when a command may prompt]' \
'-h[Print help]' \
'--help[Print help]' \
':name:_default' \
&& ret=0
;;
(restore)
_arguments "${_arguments_options[@]}" : \
'--home=[Override the Firestone home root (config, data, and runtime)]:DIR:_files' \
'--force[Stop the machine first instead of refusing a running machine]' \
'--start[Start the machine after restoring a cold snapshot]' \
'--json[Print events as newline-delimited JSON and disable human output]' \
'-q[Print only errors and command results]' \
'--quiet[Print only errors and command results]' \
'*-v[Increase log detail. Pass twice for debug output]' \
'*--verbose[Increase log detail. Pass twice for debug output]' \
'--no-color[Disable colored output]' \
'-y[Assume yes when a command may prompt]' \
'--yes[Assume yes when a command may prompt]' \
'-h[Print help]' \
'--help[Print help]' \
':name:_default' \
':snapshot:_default' \
&& ret=0
;;
(rm)
_arguments "${_arguments_options[@]}" : \
'--home=[Override the Firestone home root (config, data, and runtime)]:DIR:_files' \
'--json[Print events as newline-delimited JSON and disable human output]' \
'-q[Print only errors and command results]' \
'--quiet[Print only errors and command results]' \
'*-v[Increase log detail. Pass twice for debug output]' \
'*--verbose[Increase log detail. Pass twice for debug output]' \
'--no-color[Disable colored output]' \
'-y[Assume yes when a command may prompt]' \
'--yes[Assume yes when a command may prompt]' \
'-h[Print help]' \
'--help[Print help]' \
':name:_default' \
':snapshot:_default' \
&& ret=0
;;
(help)
_arguments "${_arguments_options[@]}" : \
":: :_firestone__subcmd__snapshot__subcmd__help_commands" \
"*::: :->help" \
&& ret=0

    case $state in
    (help)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:firestone-snapshot-help-command-$line[1]:"
        case $line[1] in
            (create)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(list)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(restore)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(rm)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(help)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
        esac
    ;;
esac
;;
        esac
    ;;
esac
;;
(help)
_arguments "${_arguments_options[@]}" : \
":: :_firestone__subcmd__help_commands" \
"*::: :->help" \
&& ret=0

    case $state in
    (help)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:firestone-help-command-$line[1]:"
        case $line[1] in
            (run)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(create)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(start)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(stop)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(restart)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(resize)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(rm)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(ls)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(show)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(edit)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(shell)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(cp)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(ssh-config)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(console)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(logs)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(metrics)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(catalog)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(images)
_arguments "${_arguments_options[@]}" : \
":: :_firestone__subcmd__help__subcmd__images_commands" \
"*::: :->images" \
&& ret=0

    case $state in
    (images)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:firestone-help-images-command-$line[1]:"
        case $line[1] in
            (ls)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(pull)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(inspect)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(rm)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(prune)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
        esac
    ;;
esac
;;
(doctor)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(completions)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(version)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(serve)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(ui)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(clone)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(snapshot)
_arguments "${_arguments_options[@]}" : \
":: :_firestone__subcmd__help__subcmd__snapshot_commands" \
"*::: :->snapshot" \
&& ret=0

    case $state in
    (snapshot)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:firestone-help-snapshot-command-$line[1]:"
        case $line[1] in
            (create)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(list)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(restore)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(rm)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
        esac
    ;;
esac
;;
(help)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
        esac
    ;;
esac
;;
        esac
    ;;
esac
}

(( $+functions[_firestone_commands] )) ||
_firestone_commands() {
    local commands; commands=(
'run:Create or reuse a machine, start it, and open an SSH shell' \
'create:Create a machine definition without booting it' \
'start:Start a machine and wait for SSH readiness' \
'stop:Stop a machine' \
'restart:Stop and start a machine' \
'resize:Change a machine'\''s CPU count or memory' \
'rm:Stop and remove one or more machines' \
'ls:List machines' \
'list:List machines' \
'show:Show a machine'\''s specification and runtime state' \
'edit:Edit and validate a machine'\''s firestone.toml' \
'shell:Open SSH over the machine'\''s private vsock transport' \
'ssh:Open SSH over the machine'\''s private vsock transport' \
'cp:Copy files between the host and a machine over the vsock transport' \
'ssh-config:Print an OpenSSH Host block for the machine' \
'console:Attach to the machine'\''s hvc0 console' \
'logs:Print a bounded machine log' \
'metrics:Print one cumulative resource sample for a running machine' \
'catalog:Print the merged built-in and user image catalog' \
'images:Manage the owned image store' \
'doctor:Check host requirements and optional safe repairs' \
'completions:Generate a shell completion script on stdout' \
'version:Print Firestone, pinned dependency, and resolved path versions' \
'serve:Run the stateless REST API over a private Unix socket or loopback port' \
'ui:Open the Firestone web interface on a loopback port' \
'clone:Copy a stopped machine'\''s spec and disk to a new machine' \
'snapshot:Capture, list, restore, and remove machine snapshots' \
'help:Print this message or the help of the given subcommand(s)' \
    )
    _describe -t commands 'firestone commands' commands "$@"
}
(( $+functions[_firestone__subcmd__catalog_commands] )) ||
_firestone__subcmd__catalog_commands() {
    local commands; commands=()
    _describe -t commands 'firestone catalog commands' commands "$@"
}
(( $+functions[_firestone__subcmd__clone_commands] )) ||
_firestone__subcmd__clone_commands() {
    local commands; commands=()
    _describe -t commands 'firestone clone commands' commands "$@"
}
(( $+functions[_firestone__subcmd__completions_commands] )) ||
_firestone__subcmd__completions_commands() {
    local commands; commands=()
    _describe -t commands 'firestone completions commands' commands "$@"
}
(( $+functions[_firestone__subcmd__console_commands] )) ||
_firestone__subcmd__console_commands() {
    local commands; commands=()
    _describe -t commands 'firestone console commands' commands "$@"
}
(( $+functions[_firestone__subcmd__cp_commands] )) ||
_firestone__subcmd__cp_commands() {
    local commands; commands=()
    _describe -t commands 'firestone cp commands' commands "$@"
}
(( $+functions[_firestone__subcmd__create_commands] )) ||
_firestone__subcmd__create_commands() {
    local commands; commands=()
    _describe -t commands 'firestone create commands' commands "$@"
}
(( $+functions[_firestone__subcmd__doctor_commands] )) ||
_firestone__subcmd__doctor_commands() {
    local commands; commands=()
    _describe -t commands 'firestone doctor commands' commands "$@"
}
(( $+functions[_firestone__subcmd__edit_commands] )) ||
_firestone__subcmd__edit_commands() {
    local commands; commands=()
    _describe -t commands 'firestone edit commands' commands "$@"
}
(( $+functions[_firestone__subcmd__help_commands] )) ||
_firestone__subcmd__help_commands() {
    local commands; commands=(
'run:Create or reuse a machine, start it, and open an SSH shell' \
'create:Create a machine definition without booting it' \
'start:Start a machine and wait for SSH readiness' \
'stop:Stop a machine' \
'restart:Stop and start a machine' \
'resize:Change a machine'\''s CPU count or memory' \
'rm:Stop and remove one or more machines' \
'ls:List machines' \
'show:Show a machine'\''s specification and runtime state' \
'edit:Edit and validate a machine'\''s firestone.toml' \
'shell:Open SSH over the machine'\''s private vsock transport' \
'cp:Copy files between the host and a machine over the vsock transport' \
'ssh-config:Print an OpenSSH Host block for the machine' \
'console:Attach to the machine'\''s hvc0 console' \
'logs:Print a bounded machine log' \
'metrics:Print one cumulative resource sample for a running machine' \
'catalog:Print the merged built-in and user image catalog' \
'images:Manage the owned image store' \
'doctor:Check host requirements and optional safe repairs' \
'completions:Generate a shell completion script on stdout' \
'version:Print Firestone, pinned dependency, and resolved path versions' \
'serve:Run the stateless REST API over a private Unix socket or loopback port' \
'ui:Open the Firestone web interface on a loopback port' \
'clone:Copy a stopped machine'\''s spec and disk to a new machine' \
'snapshot:Capture, list, restore, and remove machine snapshots' \
'help:Print this message or the help of the given subcommand(s)' \
    )
    _describe -t commands 'firestone help commands' commands "$@"
}
(( $+functions[_firestone__subcmd__help__subcmd__catalog_commands] )) ||
_firestone__subcmd__help__subcmd__catalog_commands() {
    local commands; commands=()
    _describe -t commands 'firestone help catalog commands' commands "$@"
}
(( $+functions[_firestone__subcmd__help__subcmd__clone_commands] )) ||
_firestone__subcmd__help__subcmd__clone_commands() {
    local commands; commands=()
    _describe -t commands 'firestone help clone commands' commands "$@"
}
(( $+functions[_firestone__subcmd__help__subcmd__completions_commands] )) ||
_firestone__subcmd__help__subcmd__completions_commands() {
    local commands; commands=()
    _describe -t commands 'firestone help completions commands' commands "$@"
}
(( $+functions[_firestone__subcmd__help__subcmd__console_commands] )) ||
_firestone__subcmd__help__subcmd__console_commands() {
    local commands; commands=()
    _describe -t commands 'firestone help console commands' commands "$@"
}
(( $+functions[_firestone__subcmd__help__subcmd__cp_commands] )) ||
_firestone__subcmd__help__subcmd__cp_commands() {
    local commands; commands=()
    _describe -t commands 'firestone help cp commands' commands "$@"
}
(( $+functions[_firestone__subcmd__help__subcmd__create_commands] )) ||
_firestone__subcmd__help__subcmd__create_commands() {
    local commands; commands=()
    _describe -t commands 'firestone help create commands' commands "$@"
}
(( $+functions[_firestone__subcmd__help__subcmd__doctor_commands] )) ||
_firestone__subcmd__help__subcmd__doctor_commands() {
    local commands; commands=()
    _describe -t commands 'firestone help doctor commands' commands "$@"
}
(( $+functions[_firestone__subcmd__help__subcmd__edit_commands] )) ||
_firestone__subcmd__help__subcmd__edit_commands() {
    local commands; commands=()
    _describe -t commands 'firestone help edit commands' commands "$@"
}
(( $+functions[_firestone__subcmd__help__subcmd__help_commands] )) ||
_firestone__subcmd__help__subcmd__help_commands() {
    local commands; commands=()
    _describe -t commands 'firestone help help commands' commands "$@"
}
(( $+functions[_firestone__subcmd__help__subcmd__images_commands] )) ||
_firestone__subcmd__help__subcmd__images_commands() {
    local commands; commands=(
'ls:List stored images' \
'pull:Pull and verify one image' \
'inspect:Verify and inspect one stored image' \
'rm:Remove one stored image' \
'prune:Remove all unreferenced images' \
    )
    _describe -t commands 'firestone help images commands' commands "$@"
}
(( $+functions[_firestone__subcmd__help__subcmd__images__subcmd__inspect_commands] )) ||
_firestone__subcmd__help__subcmd__images__subcmd__inspect_commands() {
    local commands; commands=()
    _describe -t commands 'firestone help images inspect commands' commands "$@"
}
(( $+functions[_firestone__subcmd__help__subcmd__images__subcmd__ls_commands] )) ||
_firestone__subcmd__help__subcmd__images__subcmd__ls_commands() {
    local commands; commands=()
    _describe -t commands 'firestone help images ls commands' commands "$@"
}
(( $+functions[_firestone__subcmd__help__subcmd__images__subcmd__prune_commands] )) ||
_firestone__subcmd__help__subcmd__images__subcmd__prune_commands() {
    local commands; commands=()
    _describe -t commands 'firestone help images prune commands' commands "$@"
}
(( $+functions[_firestone__subcmd__help__subcmd__images__subcmd__pull_commands] )) ||
_firestone__subcmd__help__subcmd__images__subcmd__pull_commands() {
    local commands; commands=()
    _describe -t commands 'firestone help images pull commands' commands "$@"
}
(( $+functions[_firestone__subcmd__help__subcmd__images__subcmd__rm_commands] )) ||
_firestone__subcmd__help__subcmd__images__subcmd__rm_commands() {
    local commands; commands=()
    _describe -t commands 'firestone help images rm commands' commands "$@"
}
(( $+functions[_firestone__subcmd__help__subcmd__logs_commands] )) ||
_firestone__subcmd__help__subcmd__logs_commands() {
    local commands; commands=()
    _describe -t commands 'firestone help logs commands' commands "$@"
}
(( $+functions[_firestone__subcmd__help__subcmd__ls_commands] )) ||
_firestone__subcmd__help__subcmd__ls_commands() {
    local commands; commands=()
    _describe -t commands 'firestone help ls commands' commands "$@"
}
(( $+functions[_firestone__subcmd__help__subcmd__metrics_commands] )) ||
_firestone__subcmd__help__subcmd__metrics_commands() {
    local commands; commands=()
    _describe -t commands 'firestone help metrics commands' commands "$@"
}
(( $+functions[_firestone__subcmd__help__subcmd__resize_commands] )) ||
_firestone__subcmd__help__subcmd__resize_commands() {
    local commands; commands=()
    _describe -t commands 'firestone help resize commands' commands "$@"
}
(( $+functions[_firestone__subcmd__help__subcmd__restart_commands] )) ||
_firestone__subcmd__help__subcmd__restart_commands() {
    local commands; commands=()
    _describe -t commands 'firestone help restart commands' commands "$@"
}
(( $+functions[_firestone__subcmd__help__subcmd__rm_commands] )) ||
_firestone__subcmd__help__subcmd__rm_commands() {
    local commands; commands=()
    _describe -t commands 'firestone help rm commands' commands "$@"
}
(( $+functions[_firestone__subcmd__help__subcmd__run_commands] )) ||
_firestone__subcmd__help__subcmd__run_commands() {
    local commands; commands=()
    _describe -t commands 'firestone help run commands' commands "$@"
}
(( $+functions[_firestone__subcmd__help__subcmd__serve_commands] )) ||
_firestone__subcmd__help__subcmd__serve_commands() {
    local commands; commands=()
    _describe -t commands 'firestone help serve commands' commands "$@"
}
(( $+functions[_firestone__subcmd__help__subcmd__shell_commands] )) ||
_firestone__subcmd__help__subcmd__shell_commands() {
    local commands; commands=()
    _describe -t commands 'firestone help shell commands' commands "$@"
}
(( $+functions[_firestone__subcmd__help__subcmd__show_commands] )) ||
_firestone__subcmd__help__subcmd__show_commands() {
    local commands; commands=()
    _describe -t commands 'firestone help show commands' commands "$@"
}
(( $+functions[_firestone__subcmd__help__subcmd__snapshot_commands] )) ||
_firestone__subcmd__help__subcmd__snapshot_commands() {
    local commands; commands=(
'create:Capture one immutable snapshot of a machine' \
'list:List a machine'\''s snapshots' \
'restore:Roll a machine back to one of its snapshots' \
'rm:Remove one snapshot' \
    )
    _describe -t commands 'firestone help snapshot commands' commands "$@"
}
(( $+functions[_firestone__subcmd__help__subcmd__snapshot__subcmd__create_commands] )) ||
_firestone__subcmd__help__subcmd__snapshot__subcmd__create_commands() {
    local commands; commands=()
    _describe -t commands 'firestone help snapshot create commands' commands "$@"
}
(( $+functions[_firestone__subcmd__help__subcmd__snapshot__subcmd__list_commands] )) ||
_firestone__subcmd__help__subcmd__snapshot__subcmd__list_commands() {
    local commands; commands=()
    _describe -t commands 'firestone help snapshot list commands' commands "$@"
}
(( $+functions[_firestone__subcmd__help__subcmd__snapshot__subcmd__restore_commands] )) ||
_firestone__subcmd__help__subcmd__snapshot__subcmd__restore_commands() {
    local commands; commands=()
    _describe -t commands 'firestone help snapshot restore commands' commands "$@"
}
(( $+functions[_firestone__subcmd__help__subcmd__snapshot__subcmd__rm_commands] )) ||
_firestone__subcmd__help__subcmd__snapshot__subcmd__rm_commands() {
    local commands; commands=()
    _describe -t commands 'firestone help snapshot rm commands' commands "$@"
}
(( $+functions[_firestone__subcmd__help__subcmd__ssh-config_commands] )) ||
_firestone__subcmd__help__subcmd__ssh-config_commands() {
    local commands; commands=()
    _describe -t commands 'firestone help ssh-config commands' commands "$@"
}
(( $+functions[_firestone__subcmd__help__subcmd__start_commands] )) ||
_firestone__subcmd__help__subcmd__start_commands() {
    local commands; commands=()
    _describe -t commands 'firestone help start commands' commands "$@"
}
(( $+functions[_firestone__subcmd__help__subcmd__stop_commands] )) ||
_firestone__subcmd__help__subcmd__stop_commands() {
    local commands; commands=()
    _describe -t commands 'firestone help stop commands' commands "$@"
}
(( $+functions[_firestone__subcmd__help__subcmd__ui_commands] )) ||
_firestone__subcmd__help__subcmd__ui_commands() {
    local commands; commands=()
    _describe -t commands 'firestone help ui commands' commands "$@"
}
(( $+functions[_firestone__subcmd__help__subcmd__version_commands] )) ||
_firestone__subcmd__help__subcmd__version_commands() {
    local commands; commands=()
    _describe -t commands 'firestone help version commands' commands "$@"
}
(( $+functions[_firestone__subcmd__images_commands] )) ||
_firestone__subcmd__images_commands() {
    local commands; commands=(
'ls:List stored images' \
'pull:Pull and verify one image' \
'inspect:Verify and inspect one stored image' \
'rm:Remove one stored image' \
'prune:Remove all unreferenced images' \
'help:Print this message or the help of the given subcommand(s)' \
    )
    _describe -t commands 'firestone images commands' commands "$@"
}
(( $+functions[_firestone__subcmd__images__subcmd__help_commands] )) ||
_firestone__subcmd__images__subcmd__help_commands() {
    local commands; commands=(
'ls:List stored images' \
'pull:Pull and verify one image' \
'inspect:Verify and inspect one stored image' \
'rm:Remove one stored image' \
'prune:Remove all unreferenced images' \
'help:Print this message or the help of the given subcommand(s)' \
    )
    _describe -t commands 'firestone images help commands' commands "$@"
}
(( $+functions[_firestone__subcmd__images__subcmd__help__subcmd__help_commands] )) ||
_firestone__subcmd__images__subcmd__help__subcmd__help_commands() {
    local commands; commands=()
    _describe -t commands 'firestone images help help commands' commands "$@"
}
(( $+functions[_firestone__subcmd__images__subcmd__help__subcmd__inspect_commands] )) ||
_firestone__subcmd__images__subcmd__help__subcmd__inspect_commands() {
    local commands; commands=()
    _describe -t commands 'firestone images help inspect commands' commands "$@"
}
(( $+functions[_firestone__subcmd__images__subcmd__help__subcmd__ls_commands] )) ||
_firestone__subcmd__images__subcmd__help__subcmd__ls_commands() {
    local commands; commands=()
    _describe -t commands 'firestone images help ls commands' commands "$@"
}
(( $+functions[_firestone__subcmd__images__subcmd__help__subcmd__prune_commands] )) ||
_firestone__subcmd__images__subcmd__help__subcmd__prune_commands() {
    local commands; commands=()
    _describe -t commands 'firestone images help prune commands' commands "$@"
}
(( $+functions[_firestone__subcmd__images__subcmd__help__subcmd__pull_commands] )) ||
_firestone__subcmd__images__subcmd__help__subcmd__pull_commands() {
    local commands; commands=()
    _describe -t commands 'firestone images help pull commands' commands "$@"
}
(( $+functions[_firestone__subcmd__images__subcmd__help__subcmd__rm_commands] )) ||
_firestone__subcmd__images__subcmd__help__subcmd__rm_commands() {
    local commands; commands=()
    _describe -t commands 'firestone images help rm commands' commands "$@"
}
(( $+functions[_firestone__subcmd__images__subcmd__inspect_commands] )) ||
_firestone__subcmd__images__subcmd__inspect_commands() {
    local commands; commands=()
    _describe -t commands 'firestone images inspect commands' commands "$@"
}
(( $+functions[_firestone__subcmd__images__subcmd__ls_commands] )) ||
_firestone__subcmd__images__subcmd__ls_commands() {
    local commands; commands=()
    _describe -t commands 'firestone images ls commands' commands "$@"
}
(( $+functions[_firestone__subcmd__images__subcmd__prune_commands] )) ||
_firestone__subcmd__images__subcmd__prune_commands() {
    local commands; commands=()
    _describe -t commands 'firestone images prune commands' commands "$@"
}
(( $+functions[_firestone__subcmd__images__subcmd__pull_commands] )) ||
_firestone__subcmd__images__subcmd__pull_commands() {
    local commands; commands=()
    _describe -t commands 'firestone images pull commands' commands "$@"
}
(( $+functions[_firestone__subcmd__images__subcmd__rm_commands] )) ||
_firestone__subcmd__images__subcmd__rm_commands() {
    local commands; commands=()
    _describe -t commands 'firestone images rm commands' commands "$@"
}
(( $+functions[_firestone__subcmd__logs_commands] )) ||
_firestone__subcmd__logs_commands() {
    local commands; commands=()
    _describe -t commands 'firestone logs commands' commands "$@"
}
(( $+functions[_firestone__subcmd__ls_commands] )) ||
_firestone__subcmd__ls_commands() {
    local commands; commands=()
    _describe -t commands 'firestone ls commands' commands "$@"
}
(( $+functions[_firestone__subcmd__metrics_commands] )) ||
_firestone__subcmd__metrics_commands() {
    local commands; commands=()
    _describe -t commands 'firestone metrics commands' commands "$@"
}
(( $+functions[_firestone__subcmd__resize_commands] )) ||
_firestone__subcmd__resize_commands() {
    local commands; commands=()
    _describe -t commands 'firestone resize commands' commands "$@"
}
(( $+functions[_firestone__subcmd__restart_commands] )) ||
_firestone__subcmd__restart_commands() {
    local commands; commands=()
    _describe -t commands 'firestone restart commands' commands "$@"
}
(( $+functions[_firestone__subcmd__rm_commands] )) ||
_firestone__subcmd__rm_commands() {
    local commands; commands=()
    _describe -t commands 'firestone rm commands' commands "$@"
}
(( $+functions[_firestone__subcmd__run_commands] )) ||
_firestone__subcmd__run_commands() {
    local commands; commands=()
    _describe -t commands 'firestone run commands' commands "$@"
}
(( $+functions[_firestone__subcmd__serve_commands] )) ||
_firestone__subcmd__serve_commands() {
    local commands; commands=()
    _describe -t commands 'firestone serve commands' commands "$@"
}
(( $+functions[_firestone__subcmd__shell_commands] )) ||
_firestone__subcmd__shell_commands() {
    local commands; commands=()
    _describe -t commands 'firestone shell commands' commands "$@"
}
(( $+functions[_firestone__subcmd__show_commands] )) ||
_firestone__subcmd__show_commands() {
    local commands; commands=()
    _describe -t commands 'firestone show commands' commands "$@"
}
(( $+functions[_firestone__subcmd__snapshot_commands] )) ||
_firestone__subcmd__snapshot_commands() {
    local commands; commands=(
'create:Capture one immutable snapshot of a machine' \
'list:List a machine'\''s snapshots' \
'ls:List a machine'\''s snapshots' \
'restore:Roll a machine back to one of its snapshots' \
'rm:Remove one snapshot' \
'help:Print this message or the help of the given subcommand(s)' \
    )
    _describe -t commands 'firestone snapshot commands' commands "$@"
}
(( $+functions[_firestone__subcmd__snapshot__subcmd__create_commands] )) ||
_firestone__subcmd__snapshot__subcmd__create_commands() {
    local commands; commands=()
    _describe -t commands 'firestone snapshot create commands' commands "$@"
}
(( $+functions[_firestone__subcmd__snapshot__subcmd__help_commands] )) ||
_firestone__subcmd__snapshot__subcmd__help_commands() {
    local commands; commands=(
'create:Capture one immutable snapshot of a machine' \
'list:List a machine'\''s snapshots' \
'restore:Roll a machine back to one of its snapshots' \
'rm:Remove one snapshot' \
'help:Print this message or the help of the given subcommand(s)' \
    )
    _describe -t commands 'firestone snapshot help commands' commands "$@"
}
(( $+functions[_firestone__subcmd__snapshot__subcmd__help__subcmd__create_commands] )) ||
_firestone__subcmd__snapshot__subcmd__help__subcmd__create_commands() {
    local commands; commands=()
    _describe -t commands 'firestone snapshot help create commands' commands "$@"
}
(( $+functions[_firestone__subcmd__snapshot__subcmd__help__subcmd__help_commands] )) ||
_firestone__subcmd__snapshot__subcmd__help__subcmd__help_commands() {
    local commands; commands=()
    _describe -t commands 'firestone snapshot help help commands' commands "$@"
}
(( $+functions[_firestone__subcmd__snapshot__subcmd__help__subcmd__list_commands] )) ||
_firestone__subcmd__snapshot__subcmd__help__subcmd__list_commands() {
    local commands; commands=()
    _describe -t commands 'firestone snapshot help list commands' commands "$@"
}
(( $+functions[_firestone__subcmd__snapshot__subcmd__help__subcmd__restore_commands] )) ||
_firestone__subcmd__snapshot__subcmd__help__subcmd__restore_commands() {
    local commands; commands=()
    _describe -t commands 'firestone snapshot help restore commands' commands "$@"
}
(( $+functions[_firestone__subcmd__snapshot__subcmd__help__subcmd__rm_commands] )) ||
_firestone__subcmd__snapshot__subcmd__help__subcmd__rm_commands() {
    local commands; commands=()
    _describe -t commands 'firestone snapshot help rm commands' commands "$@"
}
(( $+functions[_firestone__subcmd__snapshot__subcmd__list_commands] )) ||
_firestone__subcmd__snapshot__subcmd__list_commands() {
    local commands; commands=()
    _describe -t commands 'firestone snapshot list commands' commands "$@"
}
(( $+functions[_firestone__subcmd__snapshot__subcmd__restore_commands] )) ||
_firestone__subcmd__snapshot__subcmd__restore_commands() {
    local commands; commands=()
    _describe -t commands 'firestone snapshot restore commands' commands "$@"
}
(( $+functions[_firestone__subcmd__snapshot__subcmd__rm_commands] )) ||
_firestone__subcmd__snapshot__subcmd__rm_commands() {
    local commands; commands=()
    _describe -t commands 'firestone snapshot rm commands' commands "$@"
}
(( $+functions[_firestone__subcmd__ssh-config_commands] )) ||
_firestone__subcmd__ssh-config_commands() {
    local commands; commands=()
    _describe -t commands 'firestone ssh-config commands' commands "$@"
}
(( $+functions[_firestone__subcmd__start_commands] )) ||
_firestone__subcmd__start_commands() {
    local commands; commands=()
    _describe -t commands 'firestone start commands' commands "$@"
}
(( $+functions[_firestone__subcmd__stop_commands] )) ||
_firestone__subcmd__stop_commands() {
    local commands; commands=()
    _describe -t commands 'firestone stop commands' commands "$@"
}
(( $+functions[_firestone__subcmd__ui_commands] )) ||
_firestone__subcmd__ui_commands() {
    local commands; commands=()
    _describe -t commands 'firestone ui commands' commands "$@"
}
(( $+functions[_firestone__subcmd__version_commands] )) ||
_firestone__subcmd__version_commands() {
    local commands; commands=()
    _describe -t commands 'firestone version commands' commands "$@"
}

if [ "$funcstack[1]" = "_firestone" ]; then
    _firestone "$@"
else
    compdef _firestone firestone
fi
