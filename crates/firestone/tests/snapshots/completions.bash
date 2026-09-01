_firestone() {
    local i cur prev opts cmd
    COMPREPLY=()
    if [[ "${BASH_VERSINFO[0]}" -ge 4 ]]; then
        cur="$2"
    else
        cur="${COMP_WORDS[COMP_CWORD]}"
    fi
    prev="$3"
    cmd=""
    opts=""

    for i in "${COMP_WORDS[@]:0:COMP_CWORD}"
    do
        case "${cmd},${i}" in
            ",$1")
                cmd="firestone"
                ;;
            firestone,catalog)
                cmd="firestone__subcmd__catalog"
                ;;
            firestone,clone)
                cmd="firestone__subcmd__clone"
                ;;
            firestone,completions)
                cmd="firestone__subcmd__completions"
                ;;
            firestone,console)
                cmd="firestone__subcmd__console"
                ;;
            firestone,cp)
                cmd="firestone__subcmd__cp"
                ;;
            firestone,create)
                cmd="firestone__subcmd__create"
                ;;
            firestone,doctor)
                cmd="firestone__subcmd__doctor"
                ;;
            firestone,edit)
                cmd="firestone__subcmd__edit"
                ;;
            firestone,help)
                cmd="firestone__subcmd__help"
                ;;
            firestone,images)
                cmd="firestone__subcmd__images"
                ;;
            firestone,list)
                cmd="firestone__subcmd__ls"
                ;;
            firestone,logs)
                cmd="firestone__subcmd__logs"
                ;;
            firestone,ls)
                cmd="firestone__subcmd__ls"
                ;;
            firestone,metrics)
                cmd="firestone__subcmd__metrics"
                ;;
            firestone,resize)
                cmd="firestone__subcmd__resize"
                ;;
            firestone,restart)
                cmd="firestone__subcmd__restart"
                ;;
            firestone,rm)
                cmd="firestone__subcmd__rm"
                ;;
            firestone,run)
                cmd="firestone__subcmd__run"
                ;;
            firestone,serve)
                cmd="firestone__subcmd__serve"
                ;;
            firestone,shell)
                cmd="firestone__subcmd__shell"
                ;;
            firestone,show)
                cmd="firestone__subcmd__show"
                ;;
            firestone,ssh)
                cmd="firestone__subcmd__shell"
                ;;
            firestone,ssh-config)
                cmd="firestone__subcmd__ssh__subcmd__config"
                ;;
            firestone,start)
                cmd="firestone__subcmd__start"
                ;;
            firestone,stop)
                cmd="firestone__subcmd__stop"
                ;;
            firestone,ui)
                cmd="firestone__subcmd__ui"
                ;;
            firestone,version)
                cmd="firestone__subcmd__version"
                ;;
            firestone__subcmd__help,catalog)
                cmd="firestone__subcmd__help__subcmd__catalog"
                ;;
            firestone__subcmd__help,clone)
                cmd="firestone__subcmd__help__subcmd__clone"
                ;;
            firestone__subcmd__help,completions)
                cmd="firestone__subcmd__help__subcmd__completions"
                ;;
            firestone__subcmd__help,console)
                cmd="firestone__subcmd__help__subcmd__console"
                ;;
            firestone__subcmd__help,cp)
                cmd="firestone__subcmd__help__subcmd__cp"
                ;;
            firestone__subcmd__help,create)
                cmd="firestone__subcmd__help__subcmd__create"
                ;;
            firestone__subcmd__help,doctor)
                cmd="firestone__subcmd__help__subcmd__doctor"
                ;;
            firestone__subcmd__help,edit)
                cmd="firestone__subcmd__help__subcmd__edit"
                ;;
            firestone__subcmd__help,help)
                cmd="firestone__subcmd__help__subcmd__help"
                ;;
            firestone__subcmd__help,images)
                cmd="firestone__subcmd__help__subcmd__images"
                ;;
            firestone__subcmd__help,logs)
                cmd="firestone__subcmd__help__subcmd__logs"
                ;;
            firestone__subcmd__help,ls)
                cmd="firestone__subcmd__help__subcmd__ls"
                ;;
            firestone__subcmd__help,metrics)
                cmd="firestone__subcmd__help__subcmd__metrics"
                ;;
            firestone__subcmd__help,resize)
                cmd="firestone__subcmd__help__subcmd__resize"
                ;;
            firestone__subcmd__help,restart)
                cmd="firestone__subcmd__help__subcmd__restart"
                ;;
            firestone__subcmd__help,rm)
                cmd="firestone__subcmd__help__subcmd__rm"
                ;;
            firestone__subcmd__help,run)
                cmd="firestone__subcmd__help__subcmd__run"
                ;;
            firestone__subcmd__help,serve)
                cmd="firestone__subcmd__help__subcmd__serve"
                ;;
            firestone__subcmd__help,shell)
                cmd="firestone__subcmd__help__subcmd__shell"
                ;;
            firestone__subcmd__help,show)
                cmd="firestone__subcmd__help__subcmd__show"
                ;;
            firestone__subcmd__help,ssh-config)
                cmd="firestone__subcmd__help__subcmd__ssh__subcmd__config"
                ;;
            firestone__subcmd__help,start)
                cmd="firestone__subcmd__help__subcmd__start"
                ;;
            firestone__subcmd__help,stop)
                cmd="firestone__subcmd__help__subcmd__stop"
                ;;
            firestone__subcmd__help,ui)
                cmd="firestone__subcmd__help__subcmd__ui"
                ;;
            firestone__subcmd__help,version)
                cmd="firestone__subcmd__help__subcmd__version"
                ;;
            firestone__subcmd__help__subcmd__images,inspect)
                cmd="firestone__subcmd__help__subcmd__images__subcmd__inspect"
                ;;
            firestone__subcmd__help__subcmd__images,ls)
                cmd="firestone__subcmd__help__subcmd__images__subcmd__ls"
                ;;
            firestone__subcmd__help__subcmd__images,prune)
                cmd="firestone__subcmd__help__subcmd__images__subcmd__prune"
                ;;
            firestone__subcmd__help__subcmd__images,pull)
                cmd="firestone__subcmd__help__subcmd__images__subcmd__pull"
                ;;
            firestone__subcmd__help__subcmd__images,rm)
                cmd="firestone__subcmd__help__subcmd__images__subcmd__rm"
                ;;
            firestone__subcmd__images,help)
                cmd="firestone__subcmd__images__subcmd__help"
                ;;
            firestone__subcmd__images,inspect)
                cmd="firestone__subcmd__images__subcmd__inspect"
                ;;
            firestone__subcmd__images,ls)
                cmd="firestone__subcmd__images__subcmd__ls"
                ;;
            firestone__subcmd__images,prune)
                cmd="firestone__subcmd__images__subcmd__prune"
                ;;
            firestone__subcmd__images,pull)
                cmd="firestone__subcmd__images__subcmd__pull"
                ;;
            firestone__subcmd__images,rm)
                cmd="firestone__subcmd__images__subcmd__rm"
                ;;
            firestone__subcmd__images__subcmd__help,help)
                cmd="firestone__subcmd__images__subcmd__help__subcmd__help"
                ;;
            firestone__subcmd__images__subcmd__help,inspect)
                cmd="firestone__subcmd__images__subcmd__help__subcmd__inspect"
                ;;
            firestone__subcmd__images__subcmd__help,ls)
                cmd="firestone__subcmd__images__subcmd__help__subcmd__ls"
                ;;
            firestone__subcmd__images__subcmd__help,prune)
                cmd="firestone__subcmd__images__subcmd__help__subcmd__prune"
                ;;
            firestone__subcmd__images__subcmd__help,pull)
                cmd="firestone__subcmd__images__subcmd__help__subcmd__pull"
                ;;
            firestone__subcmd__images__subcmd__help,rm)
                cmd="firestone__subcmd__images__subcmd__help__subcmd__rm"
                ;;
            *)
                ;;
        esac
    done

    case "${cmd}" in
        firestone)
            opts="-q -v -y -h -V --json --quiet --verbose --no-color --yes --home --help --version run create start stop restart resize rm ls list show edit shell ssh cp ssh-config console logs metrics catalog images doctor completions version serve ui clone help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 1 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --home)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        firestone__subcmd__catalog)
            opts="-q -v -y -h --json --quiet --verbose --no-color --yes --home --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --home)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        firestone__subcmd__clone)
            opts="-q -v -y -h --fresh-disk --json --quiet --verbose --no-color --yes --home --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --home)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        firestone__subcmd__completions)
            opts="-q -v -y -h --json --quiet --verbose --no-color --yes --home --help bash elvish fish powershell zsh"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --home)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        firestone__subcmd__console)
            opts="-q -v -y -h --json --quiet --verbose --no-color --yes --home --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --home)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        firestone__subcmd__cp)
            opts="-r -q -v -y -h --recursive --json --quiet --verbose --no-color --yes --home --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --home)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        firestone__subcmd__create)
            opts="-f -p -q -v -y -h --image --file --edit --arch --cpus --cpus-max --memory --memory-max --disk --user --net --forward --tap --network-mac --mount --user-data --cloud-init-network-config --ssh-key --no-provisioning --vmm-binary --vmm-firmware --vmm-arg --vmm-config --clear --json --quiet --verbose --no-color --yes --home --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --image)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --file)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -f)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --arch)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --cpus)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --cpus-max)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --memory)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --memory-max)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --disk)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --user)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --net)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --forward)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -p)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --tap)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --network-mac)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --mount)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --user-data)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --cloud-init-network-config)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --ssh-key)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --vmm-binary)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --vmm-firmware)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --vmm-arg)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --vmm-config)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --clear)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --home)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        firestone__subcmd__doctor)
            opts="-q -v -y -h --fix --json --quiet --verbose --no-color --yes --home --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --home)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        firestone__subcmd__edit)
            opts="-q -v -y -h --json --quiet --verbose --no-color --yes --home --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --home)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        firestone__subcmd__help)
            opts="run create start stop restart resize rm ls show edit shell cp ssh-config console logs metrics catalog images doctor completions version serve ui clone help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        firestone__subcmd__help__subcmd__catalog)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        firestone__subcmd__help__subcmd__clone)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        firestone__subcmd__help__subcmd__completions)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        firestone__subcmd__help__subcmd__console)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        firestone__subcmd__help__subcmd__cp)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        firestone__subcmd__help__subcmd__create)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        firestone__subcmd__help__subcmd__doctor)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        firestone__subcmd__help__subcmd__edit)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        firestone__subcmd__help__subcmd__help)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        firestone__subcmd__help__subcmd__images)
            opts="ls pull inspect rm prune"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        firestone__subcmd__help__subcmd__images__subcmd__inspect)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        firestone__subcmd__help__subcmd__images__subcmd__ls)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        firestone__subcmd__help__subcmd__images__subcmd__prune)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        firestone__subcmd__help__subcmd__images__subcmd__pull)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        firestone__subcmd__help__subcmd__images__subcmd__rm)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        firestone__subcmd__help__subcmd__logs)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        firestone__subcmd__help__subcmd__ls)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        firestone__subcmd__help__subcmd__metrics)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        firestone__subcmd__help__subcmd__resize)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        firestone__subcmd__help__subcmd__restart)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        firestone__subcmd__help__subcmd__rm)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        firestone__subcmd__help__subcmd__run)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        firestone__subcmd__help__subcmd__serve)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        firestone__subcmd__help__subcmd__shell)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        firestone__subcmd__help__subcmd__show)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        firestone__subcmd__help__subcmd__ssh__subcmd__config)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        firestone__subcmd__help__subcmd__start)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        firestone__subcmd__help__subcmd__stop)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        firestone__subcmd__help__subcmd__ui)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        firestone__subcmd__help__subcmd__version)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        firestone__subcmd__images)
            opts="-q -v -y -h --json --quiet --verbose --no-color --yes --home --help ls pull inspect rm prune help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --home)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        firestone__subcmd__images__subcmd__help)
            opts="ls pull inspect rm prune help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        firestone__subcmd__images__subcmd__help__subcmd__help)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        firestone__subcmd__images__subcmd__help__subcmd__inspect)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        firestone__subcmd__images__subcmd__help__subcmd__ls)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        firestone__subcmd__images__subcmd__help__subcmd__prune)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        firestone__subcmd__images__subcmd__help__subcmd__pull)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        firestone__subcmd__images__subcmd__help__subcmd__rm)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        firestone__subcmd__images__subcmd__inspect)
            opts="-q -v -y -h --json --quiet --verbose --no-color --yes --home --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --home)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        firestone__subcmd__images__subcmd__ls)
            opts="-q -v -y -h --json --quiet --verbose --no-color --yes --home --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --home)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        firestone__subcmd__images__subcmd__prune)
            opts="-q -v -y -h --json --quiet --verbose --no-color --yes --home --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --home)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        firestone__subcmd__images__subcmd__pull)
            opts="-q -v -y -h --sha256 --json --quiet --verbose --no-color --yes --home --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --sha256)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --home)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        firestone__subcmd__images__subcmd__rm)
            opts="-q -v -y -h --force --json --quiet --verbose --no-color --yes --home --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --home)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        firestone__subcmd__logs)
            opts="-f -n -q -v -y -h --follow --source --json --quiet --verbose --no-color --yes --home --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --source)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -n)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --home)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        firestone__subcmd__ls)
            opts="-q -v -y -h --json --quiet --verbose --no-color --yes --home --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --home)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        firestone__subcmd__metrics)
            opts="-q -v -y -h --json --quiet --verbose --no-color --yes --home --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --home)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        firestone__subcmd__resize)
            opts="-q -v -y -h --cpus --memory --json --quiet --verbose --no-color --yes --home --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --cpus)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --memory)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --home)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        firestone__subcmd__restart)
            opts="-q -v -y -h --json --quiet --verbose --no-color --yes --home --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --home)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        firestone__subcmd__rm)
            opts="-q -v -y -h --force --json --quiet --verbose --no-color --yes --home --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --home)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        firestone__subcmd__run)
            opts="-p -q -v -y -h --name --rm --arch --cpus --cpus-max --memory --memory-max --disk --user --net --forward --tap --network-mac --mount --user-data --cloud-init-network-config --ssh-key --no-provisioning --vmm-binary --vmm-firmware --vmm-arg --vmm-config --clear --json --quiet --verbose --no-color --yes --home --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --name)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --arch)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --cpus)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --cpus-max)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --memory)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --memory-max)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --disk)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --user)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --net)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --forward)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -p)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --tap)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --network-mac)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --mount)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --user-data)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --cloud-init-network-config)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --ssh-key)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --vmm-binary)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --vmm-firmware)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --vmm-arg)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --vmm-config)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --clear)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --home)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        firestone__subcmd__serve)
            opts="-q -v -y -h --listen --token --json --quiet --verbose --no-color --yes --home --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --listen)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --token)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --home)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        firestone__subcmd__shell)
            opts="-q -v -y -h --user --json --quiet --verbose --no-color --yes --home --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --user)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --home)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        firestone__subcmd__show)
            opts="-q -v -y -h --vmconfig --json --quiet --verbose --no-color --yes --home --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --home)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        firestone__subcmd__ssh__subcmd__config)
            opts="-q -v -y -h --json --quiet --verbose --no-color --yes --home --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --home)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        firestone__subcmd__start)
            opts="-q -v -y -h --no-wait --timeout --json --quiet --verbose --no-color --yes --home --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --timeout)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --home)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        firestone__subcmd__stop)
            opts="-q -v -y -h --timeout --force --json --quiet --verbose --no-color --yes --home --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --timeout)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --home)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        firestone__subcmd__ui)
            opts="-q -v -y -h --port --no-open --print-url --json --quiet --verbose --no-color --yes --home --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --port)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --home)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        firestone__subcmd__version)
            opts="-q -v -y -h --json --quiet --verbose --no-color --yes --home --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --home)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
    esac
}

if [[ "${BASH_VERSINFO[0]}" -eq 4 && "${BASH_VERSINFO[1]}" -ge 4 || "${BASH_VERSINFO[0]}" -gt 4 ]]; then
    complete -F _firestone -o nosort -o bashdefault -o default firestone
else
    complete -F _firestone -o bashdefault -o default firestone
fi
