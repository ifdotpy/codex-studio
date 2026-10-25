#!/bin/sh
# Studio native Layr Git guard.
real_git=@REAL_GIT@
layr=@LAYR@
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
resolve_candidate() {
    suffix=''
    while [ ! -d "$candidate" ]; do
        suffix="/${candidate##*/}$suffix"
        parent=${candidate%/*}
        [ -n "$parent" ] && [ "$parent" != "$candidate" ] || return 1
        candidate=$parent
    done
    candidate=$(cd "$candidate" 2>/dev/null && /bin/pwd -P) || return 1
    candidate="$candidate$suffix"
}
takes_value() {
    case "$1" in
        --grep|--author|--since|--after|--until|--before|--date|-n|--max-count|--skip|-U|--unified|--stat-width|-e|-A|-B|-C|--after-context|--before-context|--context) return 0 ;;
        *) return 1 ;;
    esac
}
read_hint() {
    printf 'Git %s is unavailable inside a native Layr line. Use %s.\n' "$command" "$1" >&2
    return 1
}
# This table owns the read allowlist, exact Layr forms and revision policy.
# Other allowed reads use Git's parser and the exported head's .git index.
read_route() {
    route=git
    translate_revisions=0
    command=$1; shift
    case "$command" in
        diff)
            for arg do
                [ "$arg" != -- ] || break
                case "$arg" in --cached|--staged) read_hint 'layr diff --cached'; return 1 ;; esac
            done ;;
        status)
            nul=0; porcelain=0; compatible=1
            for arg do
                case "$arg" in
                    --porcelain=2|--porcelain=v2) read_hint 'layr status --porcelain'; return 1 ;;
                    -z) nul=1 ;;
                    --porcelain|--porcelain=1|--porcelain=v1) porcelain=1 ;;
                    --untracked-files=no|--untracked-files=all|--untracked-files=normal) ;;
                    --) compatible=0; break ;;
                    *) compatible=0 ;;
                esac
            done
            if [ "$compatible:$nul:$porcelain" = 1:1:1 ]; then route=layr; fi ;;
        ls-files)
            nul=0; compatible=1
            for arg do
                case "$arg" in -z) nul=1 ;; -s|--stage|-c|--cached|--full-name) ;; *) compatible=0 ;; esac
            done
            if [ "$compatible:$nul" = 1:1 ]; then route=layr; fi ;;
        rev-parse)
            if [ "$#" -eq 1 ]; then
                case "$1" in
                    --show-toplevel|--show-prefix|--show-cdup|--is-inside-work-tree) route=layr ;;
                    -*) ;;
                    *:*|*'^{tree}') route=layr; translate_revisions=1 ;;
                esac
            fi ;;
        log)
            if [ "$#" -eq 2 ] && [ "$1" = -1 ] && [ "$2" = --format=%ct ]; then route=layr; fi ;;
        show)
            if [ "$#" -eq 1 ]; then
                case "$1" in HEAD:?*)
                    if [ -f "$here/${1#*:}" ]; then route=layr; translate_revisions=1; fi ;;
                esac
            fi ;;
        merge-base)
            if [ "$#" -eq 3 ] && [ "$1" = --is-ancestor ]; then
                case "$2:$3" in -*|*:-*) ;; *) route=layr; translate_revisions=1 ;; esac
            fi ;;
        grep|blame|cat-file|describe|for-each-ref|rev-list|ls-tree|show-ref|name-rev|diff-tree|diff-files|diff-index|check-ignore|check-attr|help) ;;
        config)
            case "$#:$1" in 2:--get|2:--get-all|2:--get-regexp|1:--list) return 0 ;; esac
            refuse "$command"; return 1 ;;
        remote) [ "$#" -eq 1 ] && [ "$1" = '-v' ] && return 0; refuse "$command"; return 1 ;;
        branch|tag)
            [ "$#" -eq 0 ] && return 0
            case "$1" in
                --list)
                    shift
                    for arg do case "$arg" in -*) refuse "$command"; return 1 ;; esac; done
                    return 0 ;;
                --show-current|-a|--all|-r|--remotes|-v|-vv|--verbose)
                    [ "$command" = branch ] && [ "$#" -eq 1 ] && return 0 ;;
            esac
            refuse "$command"; return 1 ;;
        *) refuse "$command"; return 1 ;;
    esac
    value=0
    for arg do
        if [ "$value" -eq 1 ]; then value=0; continue; fi
        [ "$arg" != -- ] || break
        case "$arg" in
            --o|--o=*|--ou|--ou=*|--out|--out=*|--outp|--outp=*|--outpu|--outpu=*|--output|--output=*) refuse "$command"; return 1 ;;
        esac
        # Existing paths need Git's revision/path disambiguation, even HEAD.
        if [ "$translate_revisions" -eq 1 ] && { [ -e "$here/$arg" ] || [ -L "$here/$arg" ]; }; then route=git; fi
        if takes_value "$arg"; then value=1; fi
    done
}
check() {
    here=$(/bin/pwd -P) || return 1
    guarded=0
    env_guarded=0
    proxy=1
    route=git
    git_dir_target=''; work_tree_target=''
    # Known limit: relative environment selectors precede any later -C.
    for candidate in "${GIT_DIR-}" "${GIT_WORK_TREE-}" "${GIT_COMMON_DIR-}"; do
        [ -n "$candidate" ] || continue
        proxy=0
        case "$candidate" in /*) ;; *) candidate="$here/$candidate" ;; esac
        resolve_candidate || continue
        in_line "$candidate" && env_guarded=1
    done
    while [ "$#" -gt 0 ]; do
        case "$1" in
            -C)
                [ "$#" -ge 2 ] || return 0
                case "$2" in
                    '') ;;
                    /*) here=$(cd "$2" 2>/dev/null && /bin/pwd -P) || return 0 ;;
                    *) here=$(cd "$here/$2" 2>/dev/null && /bin/pwd -P) || return 0 ;;
                esac
                shift 2 ;;
            --git-dir|--work-tree)
                proxy=0
                [ "$#" -ge 2 ] || return 0
                case "$1" in --git-dir) git_dir_target=$2 ;; *) work_tree_target=$2 ;; esac
                shift 2 ;;
            --git-dir=*) proxy=0; git_dir_target=${1#*=}; shift ;;
            --work-tree=*) proxy=0; work_tree_target=${1#*=}; shift ;;
            -c|--config-env|--namespace|--super-prefix)
                proxy=0; [ "$#" -ge 2 ] || return 0; shift 2 ;;
            --no-pager|-P|--no-optional-locks) shift ;;
            --version|--help) return 0 ;;
            --) shift; break ;;
            # Unknown flags cannot hide a later repository selector. Disable
            # proxying; real Git retains option validation and outside behavior.
            -*) proxy=0; shift ;;
            *) break ;;
        esac
    done
    guarded=$env_guarded
    in_line "$here" && guarded=1
    for candidate in "$git_dir_target" "$work_tree_target"; do
        [ -n "$candidate" ] || continue
        case "$candidate" in /*) ;; *) candidate="$here/$candidate" ;; esac
        resolve_candidate || continue
        in_line "$candidate" && guarded=1
    done
    [ "$guarded" -eq 1 ] || return 0
    [ "$#" -gt 0 ] || return 0
    read_route "$@" || return 1
    if [ "$proxy" -eq 1 ] && in_line "$here" && [ "$route" = layr ]; then
        proxy_read "$@"
        exit $?
    fi
}

proxy_read() {
    cd "$here" || return 1
    paths=0
    value=0
    # Rotate the argument list once, preserving bytes, spaces and empty values.
    for arg do
        shift
        if [ "$paths" -eq 0 ] && [ "$value" -eq 0 ] && [ "$translate_revisions" -eq 1 ]; then
            case "$arg" in HEAD|HEAD[~^:@]*|HEAD..* ) arg="head${arg#HEAD}" ;; esac
            case "$arg" in *..HEAD|*..HEAD[~^:@]* ) arg="${arg%%..HEAD*}..head${arg#*..HEAD}" ;; esac
        fi
        if [ "$value" -eq 1 ]; then value=0
        else
            [ "$arg" != -- ] || paths=1
            if takes_value "$arg"; then value=1; fi
        fi
        set -- "$@" "$arg"
    done
    exec "$layr" "$@"
}
check "$@" || exit 1
# Prevent optional index refreshes in the read fallback inside native lines.
if [ "$guarded" -eq 1 ]; then GIT_OPTIONAL_LOCKS=0; export GIT_OPTIONAL_LOCKS; fi
exec "$real_git" "$@"
