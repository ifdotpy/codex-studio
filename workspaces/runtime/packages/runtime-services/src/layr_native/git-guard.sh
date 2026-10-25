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
        --format|--pretty|--grep|--author|--since|--after|--until|--before|--date|-n|--max-count|--skip|-U|--unified|--stat-width|-e|-A|-B|-C|--after-context|--before-context|--context) return 0 ;;
        *) return 1 ;;
    esac
}
# This table owns both the read allowlist and the choice of backend. Layr 0.2.1
# prints state IDs in history and v1 for every porcelain version. Keep Git's
# commit formats on the managed .git until Layr has those output contracts.
read_route() {
    route=git
    translate_revisions=0
    command=$1; shift
    case "$command" in
        diff) route=layr; translate_revisions=1 ;;
        ls-files) route=layr ;;
        grep)
            route=layr
            # HEAD may be a pattern, a revision or a path. Keep Git's parser.
            for arg do case "$arg" in HEAD|HEAD[~^:@.]* ) route=git ;; esac; done ;;
        status)
            for arg do
                [ "$arg" != -- ] || break
                case "$arg" in -s|--short|--porcelain|--porcelain=1|--porcelain=v1) route=layr ;; esac
            done
            for arg do case "$arg" in --porcelain=2|--porcelain=v2|-b|--branch) route=git ;; esac; done ;;
        rev-parse)
            route=layr
            translate_revisions=1
            for arg do
                case "$arg" in
                    --show-toplevel|--show-prefix|--show-cdup|--is-inside-work-tree|--is-inside-git-dir|--is-bare-repository|--absolute-git-dir|--git-common-dir|--abbrev-ref|--abbrev-ref=*|--symbolic-full-name|--verify|-q|--quiet|--short|--short=*) ;;
                    *:*) ;;
                    *'^{tree}') ;;
                    *) route=git ;;
                esac
            done
            # A managed Git head may be detached. Preserve Git's ref names.
            for arg do case "$arg" in --abbrev-ref|--abbrev-ref=*|--symbolic-full-name|--git-common-dir) route=git ;; esac; done ;;
        log|show)
            translate_revisions=1
            # Only explicit formats without state identity fields match Git.
            for arg do
                [ "$arg" != -- ] || break
                case "$arg" in --format=*|--pretty=*)
                    case "$arg" in *%H*|*%h*|*%P*|*%p*|*%d*|*%D*) route=git ;; *%*) route=layr ;; *) route=git ;; esac ;;
                esac
            done
            if [ "$command" = show ]; then
                for arg do
                    [ "$arg" != -- ] || break
                    case "$arg" in -*) ;; *:*) route=layr ;; esac
                done
            fi ;;
        merge-base)
            translate_revisions=1
            for arg do [ "$arg" != --is-ancestor ] || route=layr; done ;;
        blame|cat-file|describe|for-each-ref|rev-list|ls-tree|show-ref|name-rev|diff-tree|diff-files|diff-index|check-ignore|check-attr|help) ;;
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
            # Layr accepts some Git flags without implementing their format.
            --raw|--summary|--full-index|--binary|--numstat|-M*|--find-renames*|--no-index|--word-diff*|--src-prefix=*|--dst-prefix=*|-w|-b|--ignore-all-space|--ignore-space-change|--minimal|--patience|--histogram|--relative*) route=git ;;
        esac
        case "$command:$arg" in diff:-z|diff:--no-prefix) route=git ;; esac
        if takes_value "$arg"; then value=1; fi
    done
}
check() {
    here=$(/bin/pwd -P) || return 1
    guarded=0
    proxy=1
    route=git
    in_line "$here" && guarded=1
    for candidate in "${GIT_DIR-}" "${GIT_WORK_TREE-}" "${GIT_COMMON_DIR-}"; do
        [ -n "$candidate" ] || continue
        proxy=0
        case "$candidate" in /*) ;; *) candidate="$here/$candidate" ;; esac
        in_line "$candidate" && guarded=1
        resolve_candidate || continue
        in_line "$candidate" && guarded=1
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
                in_line "$here" && guarded=1
                shift 2 ;;
            --git-dir|--work-tree)
                proxy=0
                [ "$#" -ge 2 ] || return 0
                candidate=$2
                case "$candidate" in /*) ;; *) candidate="$here/$candidate" ;; esac
                in_line "$candidate" && guarded=1
                resolve_candidate || candidate=$here
                in_line "$candidate" && guarded=1
                shift 2 ;;
            --git-dir=*|--work-tree=*)
                proxy=0
                candidate=${1#*=}
                case "$candidate" in /*) ;; *) candidate="$here/$candidate" ;; esac
                in_line "$candidate" && guarded=1
                resolve_candidate || candidate=$here
                in_line "$candidate" && guarded=1
                shift ;;
            -c|--config-env) proxy=0; [ "$#" -ge 2 ] || return 0; shift 2 ;;
            --no-pager|--no-optional-locks) shift ;;
            --config-env=*|--paginate|--bare|--no-replace-objects|--literal-pathspecs|--glob-pathspecs|--noglob-pathspecs|--icase-pathspecs) proxy=0; shift ;;
            --version|--help) return 0 ;;
            *) break ;;
        esac
    done
    [ "$guarded" -eq 1 ] || return 0
    [ "$#" -gt 0 ] || return 0
    read_route "$@" || return 1
    if [ "$proxy" -eq 1 ] && in_line "$here" && [ "$route" = layr ]; then
        # Strip only the global arguments accepted for the Layr route.
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
            case "$arg" in HEAD|HEAD[~^:@.]* ) arg="head${arg#HEAD}" ;; esac
            case "$arg" in *..HEAD|*..HEAD[~^:@.]* ) arg="${arg%%..HEAD*}..head${arg#*..HEAD}" ;; esac
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
