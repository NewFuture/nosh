# nosh: optional suggestions for the existing Bash, never an execution hook.
if [[ -n ${BRUSH_VERSION-} ]]; then
    # nosh also reads .bashrc, but already has its own suggestion integration.
    :
elif [[ -z ${BASH_VERSION-} ]] ||
    (( BASH_VERSINFO[0] < 5 || (BASH_VERSINFO[0] == 5 && BASH_VERSINFO[1] < 1) )); then
    printf '%s\n' 'nosh: Bash integration requires Bash 5.1+ (Readline signal handling); no bindings installed' >&2
elif [[ $- != *i* || ! -t 0 || ! -t 1 ]] || ! [[ -o emacs || -o vi ]]; then
    printf '%s\n' 'nosh: Bash integration requires an interactive Readline terminal; no bindings installed' >&2
elif [[ -z ${TERM-} || $TERM == dumb || $TERM == unknown ]]; then
    printf '%s\n' 'nosh: Bash integration requires a usable TERM; no bindings installed' >&2
else

__nosh_bash_run() {
    # NOSH_COMMAND
}

__nosh_bash_suggest() {
    builtin local __nosh_line="${READLINE_LINE-}" __nosh_point="${READLINE_POINT-0}"
    builtin local __nosh_reply __nosh_status __nosh_trap __nosh_interrupted=0 __nosh_pending __nosh_length
    if [[ $__nosh_line != *[![:space:]]* ]]; then
        builtin printf '\n%s\n' 'nosh: enter a description first; input unchanged' >&2
        return 0
    fi

    # Without a temporary trap, Bash abandons the Readline draft on SIGINT.
    __nosh_trap=$(builtin trap -p INT)
    builtin trap '__nosh_interrupted=1; READLINE_LINE=$__nosh_line; READLINE_POINT=$__nosh_point' INT
    if __nosh_reply=$(__nosh_bash_run "$__nosh_line" </dev/null); then
        __nosh_status=0
    else
        __nosh_status=$?
    fi
    if (( __nosh_interrupted || __nosh_status == 130 )); then
        builtin printf '\n%s\n' 'nosh: suggestion cancelled; input unchanged' >&2
    elif (( __nosh_status != 0 )); then
        builtin printf '\n%s\n' 'nosh: no command returned; input unchanged' >&2
    elif [[ $__nosh_reply != *[![:space:]]* ]]; then
        builtin printf '\n%s\n' 'nosh: empty suggestion; input unchanged' >&2
    elif builtin read -t 0 __nosh_pending; then
        # A queued Enter must never submit a suggestion the user has not seen.
        __nosh_status=1
        builtin printf '\n%s\n' 'nosh: input received during generation; suggestion discarded' >&2
    else
        # %n counts bytes without changing the host locale. Character-based
        # versions of Bash clamp this upper bound to the end of the line.
        builtin printf -v __nosh_reply '%s%n' "$__nosh_reply" __nosh_length
        READLINE_LINE=$__nosh_reply
        READLINE_POINT=$__nosh_length
        if [[ ${READLINE_MARK+x} ]]; then
            READLINE_MARK=$READLINE_POINT
        fi
    fi
    if (( __nosh_interrupted || __nosh_status != 0 )) || [[ $__nosh_reply != *[![:space:]]* ]]; then
        READLINE_LINE=$__nosh_line
        READLINE_POINT=$__nosh_point
    fi
    if [[ -n $__nosh_trap ]]; then
        builtin eval "$__nosh_trap"
    else
        builtin trap - INT
    fi
    return 0
}

# Use Readline's printable spelling so bind -p/-s/-X can be compared literally.
__nosh_bash_key() {
    builtin local __nosh_bytes="$1" __nosh_i __nosh_n __nosh_char
    for ((__nosh_i=0; __nosh_i<${#__nosh_bytes}; __nosh_i++)); do
        builtin printf -v __nosh_n '%d' "'${__nosh_bytes:__nosh_i:1}"
        case $__nosh_n in
            27) builtin printf '%s' '\e' ;;
            28) builtin printf '%s' '\C-\\' ;;
            34) builtin printf '%s' '\"' ;;
            92) builtin printf '%s' '\\' ;;
            127) builtin printf '%s' '\C-?' ;;
            *)
                if (( __nosh_n < 32 )); then
                    builtin printf -v __nosh_char '\\%03o' "$((__nosh_n < 27 ? __nosh_n + 96 : __nosh_n + 64))"
                    builtin printf -v __nosh_char '%b' "$__nosh_char"
                    builtin printf '%s%s' '\C-' "$__nosh_char"
                else
                    builtin printf '%s' "${__nosh_bytes:__nosh_i:1}"
                fi ;;
        esac
    done
}

__nosh_bash_restore() {
    builtin local __nosh_id __nosh_entries __nosh_entry __nosh_map __nosh_key __nosh_previous __nosh_owned
    for __nosh_id in "${!__nosh_bash_saved[@]}"; do
        __nosh_map=${__nosh_id%%:*}
        __nosh_key=${__nosh_id#*:}
        __nosh_owned=0
        __nosh_entries=$(builtin bind -m "$__nosh_map" -X) || return
        while IFS= builtin read -r __nosh_entry; do
            if [[ $__nosh_entry == "\"$__nosh_key\" \"__nosh_bash_suggest\"" ||
                  $__nosh_entry == "\"$__nosh_key\": \"__nosh_bash_suggest\"" ]]; then
                __nosh_owned=1
                break
            fi
        done <<< "$__nosh_entries"
        # Older Bash retains -X entries after a function or macro replaces them.
        __nosh_entries=$(builtin bind -m "$__nosh_map" -p && builtin bind -m "$__nosh_map" -s) || return
        while IFS= builtin read -r __nosh_entry; do
            if [[ $__nosh_entry == "\"$__nosh_key"* ]]; then
                __nosh_owned=0
                break
            fi
        done <<< "$__nosh_entries"
        if (( __nosh_owned )); then
            builtin bind -m "$__nosh_map" -r "$__nosh_key" || return
            __nosh_previous=${__nosh_bash_saved[$__nosh_id]}
            if [[ -n $__nosh_previous ]]; then
                if [[ $__nosh_previous == X* ]]; then
                    builtin bind -m "$__nosh_map" -x "${__nosh_previous:1}" || return
                else
                    builtin bind -m "$__nosh_map" "${__nosh_previous:1}" || return
                fi
            fi
        fi
    done
    __nosh_bash_saved=()
}

# Bash 5.1/5.2 dump shell commands as escaped Readline macros, but bind -x
# does not decode those escapes. Decode only reversible values, never eval.
__nosh_bash_shell_binding() {
    builtin local __nosh_key="$1" __nosh_dump="$2" __nosh_value __nosh_check
    if [[ $__nosh_dump == "\"$__nosh_key\" "* ]]; then
        builtin printf '%s' "$__nosh_dump"
        return
    fi
    __nosh_value=${__nosh_dump#"\"$__nosh_key\": \""}
    __nosh_value=${__nosh_value%\"}
    __nosh_check=${__nosh_value//\\\\/}
    __nosh_check=${__nosh_check//\\\"/}
    [[ $__nosh_check != *\\* ]] || return 1
    __nosh_value=${__nosh_value//\\\"/\"}
    __nosh_value=${__nosh_value//\\\\/\\}
    # The older bind -x parser treats a leading quote as a value delimiter.
    [[ $__nosh_value != [[:space:]\"\']* ]] || return 1
    builtin printf '"%s":%s' "$__nosh_key" "$__nosh_value"
}

__nosh_bash_bind() {
    builtin local __nosh_map="$1" __nosh_bytes="$3" __nosh_policy="$4" __nosh_key
    builtin local __nosh_entries __nosh_entry __nosh_kind __nosh_original= __nosh_original_kind=
    builtin local __nosh_i __nosh_prefix __nosh_restorable
    if [[ $2 == terminfo ]]; then
        if ! __nosh_bytes=$(builtin command tput "$3") || [[ -z $__nosh_bytes ]]; then
            builtin printf 'nosh: %s: terminal key %s unavailable; binding skipped\n' "$__nosh_map" "$3" >&2
            return 0
        fi
    fi
    if [[ $__nosh_bytes == $'\003' || ( $__nosh_map != emacs-standard && $__nosh_bytes == $'\e' ) ]]; then
        builtin printf 'nosh: %s: terminal key overlaps a protected cancellation key; binding skipped\n' "$__nosh_map" >&2
        return 0
    fi
    __nosh_key=$(__nosh_bash_key "$__nosh_bytes")
    [[ ${__nosh_bash_saved["$__nosh_map:$__nosh_key"]+installed} ]] && return 0
    for __nosh_kind in p s X; do
        __nosh_entries=$(builtin bind -m "$__nosh_map" "-$__nosh_kind") || return
        while IFS= builtin read -r __nosh_entry; do
            if [[ $__nosh_kind == X ]] &&
                [[ $__nosh_entry == "\"$__nosh_key\" \"__nosh_bash_suggest\"" ||
                   $__nosh_entry == "\"$__nosh_key\": \"__nosh_bash_suggest\"" ]]; then
                continue
            fi
            if [[ $__nosh_entry == "\"$__nosh_key\":"* || $__nosh_entry == "\"$__nosh_key\" "* ]]; then
                __nosh_original=$__nosh_entry
                __nosh_original_kind=$__nosh_kind
                continue
            fi
            if [[ $__nosh_entry == "\"$__nosh_key"* ]]; then
                builtin printf 'nosh: %s: suggestion key overlaps %s; binding skipped\n' "$__nosh_map" "$__nosh_entry" >&2
                return 0
            fi
            for ((__nosh_i=1; __nosh_i<${#__nosh_key}; __nosh_i++)); do
                __nosh_prefix=${__nosh_key:0:__nosh_i}
                # Readline retains Vi's Escape action as the prefix fallback.
                [[ $__nosh_entry == '"\e": vi-movement-mode' ]] && continue
                # Readline 8.2 can print a prefix fallback with a \000 suffix.
                if [[ $__nosh_entry == "\"$__nosh_prefix\":"* ||
                      $__nosh_entry == "\"$__nosh_prefix\" "* ||
                      $__nosh_entry == "\"$__nosh_prefix\\000\":"* ]]; then
                    builtin printf 'nosh: %s: suggestion key overlaps %s; binding skipped\n' "$__nosh_map" "$__nosh_entry" >&2
                    return 0
                fi
            done
        done <<< "$__nosh_entries"
    done
    if [[ -n $__nosh_original ]]; then
        if [[ $__nosh_policy == default ]]; then
            builtin printf 'nosh: %s: keeping existing binding %s; set shell.keybindings.ai_suggest to override\n' "$__nosh_map" "$__nosh_original" >&2
            return 0
        fi
        if [[ $__nosh_original_kind == X ]]; then
            if ! __nosh_restorable=$(__nosh_bash_shell_binding "$__nosh_key" "$__nosh_original"); then
                builtin printf 'nosh: %s: existing shell binding cannot be restored by this Bash version; binding skipped\n' "$__nosh_map" >&2
                return 0
            fi
        fi
        builtin printf 'nosh: %s: ai_suggest replaces %s\n' "$__nosh_map" "$__nosh_original" >&2
        if [[ $__nosh_original_kind == X ]]; then
            __nosh_original=$__nosh_restorable
        fi
    fi
    builtin bind -m "$__nosh_map" -x "\"$__nosh_key\":__nosh_bash_suggest" || return
    # Keep the original dump and its bind option together, indexed by keymap:key.
    __nosh_bash_saved["$__nosh_map:$__nosh_key"]=$__nosh_original_kind$__nosh_original
}

__nosh_bash_install() {
    builtin declare -gA __nosh_bash_saved
    __nosh_bash_restore || return
    # NOSH_BINDINGS
}

if ! __nosh_bash_install; then
    builtin printf '%s\n' 'nosh: could not install Bash integration' >&2
fi
fi
