#!/bin/sh
# Studio native Layr Git guard.
real_git=@REAL_GIT@
store=@STORE@
commit_refusal=@COMMIT_REFUSAL@
refuse() {
    case "$1" in
        commit) printf '%s\n' "$commit_refusal" >&2; return 1 ;;
        add) next='layr add <path>' ;;
        rm|mv) next='layr add -A, then layr save -m "message"' ;;
        stash) next='layr park' ;;
        branch) next='layr line new <name>' ;;
        tag) next='layr ref set <name> <state>' ;;
        checkout|switch) next='layr line new <name> <state>' ;;
        restore|reset|revert) next='layr restore <state> -- <path>' ;;
        merge) next='layr merge <line> --expect <state>' ;;
        rebase) next='layr replay' ;;
        cherry-pick) next='layr pick <state>' ;;
        push) next='layr git push' ;;
        pull|fetch) next='layr git fetch' ;;
        worktree) next='layr line new <name>' ;;
        clean) next='layr status, then remove the selected files' ;;
        *) next='layr status and the matching layr command' ;;
    esac
    printf 'Git %s is disabled inside a native Layr line. Use %s.\n' "$1" "$next" >&2
    return 1
}
in_line() {
    case "$1" in "$store"/projects/*/lines/*) return 0 ;; *) return 1 ;; esac
}
check() {
    here=$(/bin/pwd -P) || return 1
    guarded=0
    in_line "$here" && guarded=1
    while [ "$#" -gt 0 ]; do
        case "$1" in
            -C)
                [ "$#" -ge 2 ] || return 1
                here=$(cd "$here" && cd "$2" && /bin/pwd -P) || return 1
                in_line "$here" && guarded=1
                shift 2 ;;
            --git-dir|--work-tree)
                [ "$#" -ge 2 ] || return 1
                candidate=$2
                case "$candidate" in /*) ;; *) candidate="$here/$candidate" ;; esac
                candidate=$(cd "$candidate" 2>/dev/null && /bin/pwd -P) || candidate=$here
                in_line "$candidate" && guarded=1
                shift 2 ;;
            --git-dir=*|--work-tree=*)
                candidate=${1#*=}
                case "$candidate" in /*) ;; *) candidate="$here/$candidate" ;; esac
                candidate=$(cd "$candidate" 2>/dev/null && /bin/pwd -P) || candidate=$here
                in_line "$candidate" && guarded=1
                shift ;;
            -c|--config-env) [ "$#" -ge 2 ] || return 1; shift 2 ;;
            --config-env=*|--no-pager|--paginate|--bare|--no-optional-locks|--no-replace-objects|--literal-pathspecs|--glob-pathspecs|--noglob-pathspecs|--icase-pathspecs) shift ;;
            --version|--help) return 0 ;;
            *) break ;;
        esac
    done
    [ "$guarded" -eq 1 ] || return 0
    [ "$#" -gt 0 ] || return 0
    command=$1; shift
    case "$command" in
        status|log|diff|show|rev-parse|ls-files|blame|cat-file|describe|for-each-ref|merge-base|grep|help) return 0 ;;
        config)
            case "$#:$1" in
                2:--get|2:--get-all|2:--get-regexp|1:--list) return 0 ;;
            esac ;;
        remote) [ "$#" -eq 1 ] && [ "$1" = '-v' ] && return 0 ;;
        branch|tag)
            [ "$#" -eq 0 ] && return 0
            case "$1" in
                --list)
                    shift
                    for arg do case "$arg" in -*) refuse "$command"; return 1 ;; esac; done
                    return 0 ;;
                --show-current|-a|--all|-r|--remotes|-v|-vv|--verbose)
                    [ "$command" = branch ] && [ "$#" -eq 1 ] && return 0 ;;
            esac ;;
    esac
    refuse "$command"
}
check "$@" || exit 1
exec "$real_git" "$@"
