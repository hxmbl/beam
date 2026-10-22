//! Server-side shell hooks: run beam-server-handler when a command is missing.
//!
//! The hook only acts inside a Beam session (BEAM_SESSION_ID and
//! BEAM_LISTENER_PORT set, which needs `AcceptEnv` in sshd_config). Anywhere
//! else it falls back to the handler that was installed before it, or to the
//! shell's usual "command not found" message, and returns 127.

/// Marker written into rc files so `install` does not add the hook twice.
pub const MARKER: &str = "# >>> beam command-not-found hook >>>";

pub fn script(shell: &str) -> Option<&'static str> {
    match shell {
        "bash" => Some(BASH),
        "zsh" => Some(ZSH),
        "fish" => Some(FISH),
        _ => None,
    }
}

/// rc file `install` appends to.
pub fn rc_path(shell: &str, system: bool) -> Option<&'static str> {
    Some(match (shell, system) {
        ("bash", true) => "/etc/bash.bashrc",
        ("zsh", true) => "/etc/zsh/zshrc",
        ("fish", true) => "/etc/fish/conf.d/beam.fish",
        ("bash", false) => "~/.bashrc",
        ("zsh", false) => "~/.zshrc",
        ("fish", false) => "~/.config/fish/conf.d/beam.fish",
        _ => return None,
    })
}

const BASH: &str = r#"# Beam command-not-found hook (bash).
# Load it with:  eval "$(beam-server-handler shell-init --shell bash)"
# Keep any handler that was already installed, and call it outside Beam sessions.
if declare -F command_not_found_handle >/dev/null 2>&1 \
    && ! declare -f command_not_found_handle | grep -q beam-server-handler; then
    eval "__beam_prev_cnf() $(declare -f command_not_found_handle | tail -n +2)"
fi
command_not_found_handle() {
    if [ -n "${BEAM_SESSION_ID:-}" ] && [ -n "${BEAM_LISTENER_PORT:-}" ] \
        && command -v beam-server-handler >/dev/null 2>&1; then
        beam-server-handler handle -- "$@"
        return $?
    fi
    if declare -F __beam_prev_cnf >/dev/null 2>&1; then
        __beam_prev_cnf "$@"
        return $?
    fi
    printf 'bash: %s: command not found\n' "$1" >&2
    return 127
}
"#;

const ZSH: &str = r#"# Beam command-not-found hook (zsh).
# Load it with:  eval "$(beam-server-handler shell-init --shell zsh)"
# Keep any handler that was already installed, and call it outside Beam sessions.
if (( ${+functions[command_not_found_handler]} )) \
    && [[ ${functions[command_not_found_handler]} != *beam-server-handler* ]]; then
    functions[__beam_prev_cnf]=${functions[command_not_found_handler]}
fi
command_not_found_handler() {
    if [[ -n ${BEAM_SESSION_ID:-} && -n ${BEAM_LISTENER_PORT:-} ]] \
        && (( ${+commands[beam-server-handler]} )); then
        beam-server-handler handle -- "$@"
        return $?
    fi
    if (( ${+functions[__beam_prev_cnf]} )); then
        __beam_prev_cnf "$@"
        return $?
    fi
    print -u2 "zsh: command not found: $1"
    return 127
}
"#;

const FISH: &str = r#"# Beam command-not-found hook (fish).
# Load it with:  beam-server-handler shell-init --shell fish | source
# Note: fish sets $status to 127 after this handler whatever it returns, so
# the beamed program's output arrives but its exit code is not preserved.
if functions -q fish_command_not_found; and not functions -q __beam_prev_cnf
    functions -c fish_command_not_found __beam_prev_cnf
end
function fish_command_not_found
    if set -q BEAM_SESSION_ID; and set -q BEAM_LISTENER_PORT; and command -q beam-server-handler
        beam-server-handler handle -- $argv
        return $status
    end
    if functions -q __beam_prev_cnf
        __beam_prev_cnf $argv
    else
        printf 'fish: Unknown command: %s\n' $argv[1] >&2
    end
end
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_shell_has_a_hook_and_an_rc_path() {
        for sh in ["bash", "zsh", "fish"] {
            let s = script(sh).unwrap();
            assert!(s.contains("beam-server-handler handle --"));
            assert!(s.contains("BEAM_SESSION_ID"));
            assert!(rc_path(sh, false).is_some());
            assert!(rc_path(sh, true).is_some());
        }
        assert!(script("csh").is_none());
    }
}
