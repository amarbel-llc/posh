//! Embedded shell completion scripts (ported from zmx's completions.zig,
//! adapted to posh's command set including the remote commands).

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shell {
    Bash,
    Zsh,
    Fish,
}

impl Shell {
    pub fn from_str(s: &str) -> Option<Shell> {
        match s {
            "bash" => Some(Shell::Bash),
            "zsh" => Some(Shell::Zsh),
            "fish" => Some(Shell::Fish),
            _ => None,
        }
    }

    pub fn script(self) -> &'static str {
        match self {
            Shell::Bash => BASH_COMPLETIONS,
            Shell::Zsh => ZSH_COMPLETIONS,
            Shell::Fish => FISH_COMPLETIONS,
        }
    }
}

const BASH_COMPLETIONS: &str = r#"_posh_remote_sessions() {
  # Remote session names for host:<Tab>, via a short-TTL cache so repeated
  # tabs are instant and a dead host stalls at most once per window. The
  # 2s connect timeout and ~30s TTL are tuning levers (FDR 0001).
  local host=$1
  local cache_dir=${XDG_CACHE_HOME:-$HOME/.cache}/posh
  local cache=$cache_dir/sessions-$host
  if [ -z "$(find "$cache" -newermt '-30 seconds' 2>/dev/null)" ]; then
    mkdir -p "$cache_dir"
    ssh -o BatchMode=yes -o ConnectTimeout=2 "$host" posh list --short \
      >"$cache.new" 2>/dev/null && mv "$cache.new" "$cache"
  fi
  cat "$cache" 2>/dev/null
}

_posh_ssh_hosts() {
  # ssh config Host aliases (wildcard patterns dropped). Reads the user
  # config plus the common config.d/conf.d include layouts.
  command sed -n 's/^[[:space:]]*[Hh]ost[[:space:]]\{1,\}//p' \
    ~/.ssh/config ~/.ssh/config.d/* ~/.ssh/conf.d/* 2>/dev/null \
    | tr ' \t' '\n\n' | command grep -v '[*?!]' | sort -u
}

_posh_completions() {
  local cur prev words cword
  COMPREPLY=()
  cur="${COMP_WORDS[COMP_CWORD]}"
  prev="${COMP_WORDS[COMP_CWORD-1]}"

  local commands="attach run detach detach-all fork groups list completions kill history server client ssh version help"

  # Handle -g/--group flag
  if [[ "$prev" == "-g" || "$prev" == "--group" ]]; then
    local groups=$(posh groups 2>/dev/null | tr '\n' ' ')
    COMPREPLY=($(compgen -W "$groups" -- "$cur"))
    return 0
  fi

  # Find the subcommand (skip -g <group>)
  local subcmd=""
  local i=1
  while [[ $i -lt $COMP_CWORD ]]; do
    local word="${COMP_WORDS[$i]}"
    if [[ "$word" == "-g" || "$word" == "--group" ]]; then
      ((i+=2))
      continue
    fi
    if [[ "$word" != -* ]]; then
      subcmd="$word"
      break
    fi
    ((i++))
  done

  if [[ "$cur" == -* ]]; then
    local flags="-g --group"
    case "$subcmd" in
      attach|a) flags="$flags --detach" ;;
      list|ls|l) flags="--short --json -j" ;;
      history|hi) flags="--vt" ;;
      server) flags="-p -4 -6" ;;
      client) flags="-4 -6" ;;
      ssh) flags="-p -4 -6" ;;
    esac
    COMPREPLY=($(compgen -W "$flags" -- "$cur"))
    return 0
  fi

  if [[ -z "$subcmd" ]]; then
    # host:<Tab> — complete the host's session names (RFC 0001 namespace).
    if [[ "$cur" == ?*:* && "$cur" != \[* ]]; then
      local rhost="${cur%%:*}"
      local rsessions=$(_posh_remote_sessions "$rhost" | sed "s|^|$rhost:|" | tr '\n' ' ')
      COMPREPLY=($(compgen -W "$rsessions" -- "$cur"))
      return 0
    fi
    # The bare first argument is also the attach shorthand (session name)
    # and the mosh-style remote form (ssh config alias).
    local sessions=$(posh list --short 2>/dev/null | tr '\n' ' ')
    local hosts=$(_posh_ssh_hosts | tr '\n' ' ')
    COMPREPLY=($(compgen -W "$commands $sessions $hosts" -- "$cur"))
    return 0
  fi

  case "$subcmd" in
    attach|a|run|r|detach|d|kill|k|history|hi)
      local sessions=$(posh list --short 2>/dev/null | tr '\n' ' ')
      COMPREPLY=($(compgen -W "$sessions" -- "$cur"))
      ;;
    completions|c)
      COMPREPLY=($(compgen -W "bash zsh fish" -- "$cur"))
      ;;
    list|ls|l)
      COMPREPLY=($(compgen -W "--short --json -j" -- "$cur"))
      ;;
    ssh)
      COMPREPLY=($(compgen -W "$(_posh_ssh_hosts | tr '\n' ' ')" -- "$cur"))
      ;;
    *)
      ;;
  esac
}

complete -o bashdefault -o default -F _posh_completions posh
"#;

const ZSH_COMPLETIONS: &str = r#"_posh() {
  local context state state_descr line
  typeset -A opt_args

  _arguments -C \
    '(-g --group)'{-g,--group}'[Session group]:group:_posh_groups' \
    '1: :->commands' \
    '2: :->args' \
    '*: :->trailing' \
    && return 0

  case $state in
    commands)
      # The bare first argument is also the attach shorthand (session
      # name) and the mosh-style remote form (ssh config alias).
      _posh_sessions
      _posh_ssh_hosts
      local -a commands
      commands=(
        'attach:Attach to session, creating if needed'
        'run:Send command without attaching'
        'detach:Detach all clients from current or named session'
        'detach-all:Detach all clients from all sessions in the group'
        'fork:Fork current session with same command'
        'groups:List active session groups'
        'list:List active sessions in group'
        'completions:Shell completion scripts'
        'kill:Kill a session'
        'history:Output session scrollback'
        'server:Start a roaming remote server'
        'client:Connect to a posh server over UDP'
        'ssh:Start and connect to a remote server over ssh'
        'version:Show version'
        'help:Show help message'
      )
      _describe 'command' commands
      ;;
    args)
      case $words[2] in
        attach|a)
          if [[ $words[CURRENT] == -* ]]; then
            _values 'options' '--detach[Create session without attaching]'
          else
            _posh_sessions
          fi
          ;;
        detach|d|kill|k|run|r|history|hi)
          _posh_sessions
          ;;
        completions|c)
          _values 'shell' 'bash' 'zsh' 'fish'
          ;;
        list|ls|l)
          _values 'options' '--short' '--json' '-j'
          ;;
        ssh)
          _posh_ssh_hosts
          ;;
      esac
      ;;
    trailing)
      # Additional args for commands like 'attach' or 'run'
      ;;
  esac
}

_posh_groups() {
  local -a groups
  local output=$(posh groups 2>/dev/null)
  if [[ -n "$output" ]]; then
    groups+=(${(f)output})
  fi
  _describe 'group' groups
}

_posh_sessions() {
  local -a sessions

  local local_sessions=$(posh list --short 2>/dev/null)
  if [[ -n "$local_sessions" ]]; then
    sessions+=(${(f)local_sessions})
  fi

  _describe 'local session' sessions
}

_posh_ssh_hosts() {
  # ssh config Host aliases (wildcard patterns dropped). Reads the user
  # config plus the common config.d/conf.d include layouts.
  local -a hosts
  hosts=(${(f)"$(command sed -n 's/^[[:space:]]*[Hh]ost[[:space:]]\{1,\}//p' \
    ~/.ssh/config ~/.ssh/config.d/*(N) ~/.ssh/conf.d/*(N) 2>/dev/null \
    | tr ' \t' '\n\n' | command grep -v '[*?!]' | sort -u)"})
  _describe 'ssh host' hosts
}

compdef _posh posh
"#;

const FISH_COMPLETIONS: &str = r#"function __posh_subcommand
    # Print the active subcommand (first non-switch arg after `posh`,
    # skipping the global `-g <group>` / `--group <group>` pair).
    # Returns 1 if no subcommand has been typed yet.
    set -l tokens (commandline -opc)
    set -l i 2
    set -l n (count $tokens)
    while test $i -le $n
        switch $tokens[$i]
            case -g --group
                set i (math $i + 2)
            case '-*'
                set i (math $i + 1)
            case '*'
                echo $tokens[$i]
                return 0
        end
    end
    return 1
end

function __posh_subcommand_is
    contains -- (__posh_subcommand) $argv
end

function __posh_ssh_config_hosts
    # ssh config Host aliases (wildcard patterns dropped). Reads the user
    # config plus the common config.d/conf.d include layouts.
    for file in ~/.ssh/config ~/.ssh/config.d/* ~/.ssh/conf.d/*
        test -r $file; or continue
        string replace -rf '^\s*[Hh]ost\s+' '' <$file | string split ' ' | string match -rv '[*?!]'
    end | sort -u
end

function __posh_remote_sessions
    # Remote session names for host:<Tab>, via a short-TTL cache so
    # repeated tabs are instant and a dead host stalls at most once per
    # window. The 2s connect timeout and ~30s TTL are tuning levers
    # (FDR 0001).
    set -l host $argv[1]
    set -l cache_dir $HOME/.cache/posh
    set -q XDG_CACHE_HOME; and set cache_dir $XDG_CACHE_HOME/posh
    set -l cache $cache_dir/sessions-$host
    if test -z "$(find $cache -newermt '-30 seconds' 2>/dev/null)"
        mkdir -p $cache_dir
        ssh -o BatchMode=yes -o ConnectTimeout=2 $host posh list --short >$cache.new 2>/dev/null
        and mv $cache.new $cache
    end
    cat $cache 2>/dev/null
end

function __posh_complete_remote_target
    # host:<Tab> -> host:session candidates (RFC 0001 namespace).
    set -l m (string match -r '^([^:\[]+):' -- (commandline -ct)); or return
    __posh_remote_sessions $m[2] | string replace -r '^' "$m[2]:"
end

complete -c posh -f

set -l subcommands attach run detach detach-all fork groups list completions kill history server client ssh version help
set -l no_subcmd "not __fish_seen_subcommand_from $subcommands"

complete -c posh -n $no_subcmd -s g -l group -d 'Session group' -r -a '(posh groups 2>/dev/null)'

complete -c posh -n $no_subcmd -a attach -d 'Attach to session, creating if needed'
complete -c posh -n $no_subcmd -a run -d 'Send command without attaching'
complete -c posh -n $no_subcmd -a detach -d 'Detach all clients from current or named session'
complete -c posh -n $no_subcmd -a detach-all -d 'Detach all clients from all sessions in the group'
complete -c posh -n $no_subcmd -a fork -d 'Fork current session with same command'
complete -c posh -n $no_subcmd -a groups -d 'List active session groups'
complete -c posh -n $no_subcmd -a list -d 'List active sessions in group'
complete -c posh -n $no_subcmd -a completions -d 'Shell completion scripts'
complete -c posh -n $no_subcmd -a kill -d 'Kill a session'
complete -c posh -n $no_subcmd -a history -d 'Output session scrollback'
complete -c posh -n $no_subcmd -a server -d 'Start a roaming remote server'
complete -c posh -n $no_subcmd -a client -d 'Connect to a posh server over UDP'
complete -c posh -n $no_subcmd -a ssh -d 'Start and connect to a remote server over ssh'
complete -c posh -n $no_subcmd -a version -d 'Show version'
complete -c posh -n $no_subcmd -a help -d 'Show help message'

# The bare first argument is also the attach shorthand (session name) and
# the mosh-style remote form (ssh config alias); host:<Tab> completes the
# host's session names (RFC 0001 namespace).
complete -c posh -n $no_subcmd -a '(posh list --short 2>/dev/null)' -d 'Session'
complete -c posh -n $no_subcmd -a '(__posh_ssh_config_hosts)' -d 'ssh host'
complete -c posh -n $no_subcmd -a '(__posh_complete_remote_target)' -d 'Remote session'

complete -c posh -n "__fish_seen_subcommand_from attach run detach kill history" -a '(posh list --short 2>/dev/null)' -d 'Session name'

complete -c posh -n "__posh_subcommand_is attach a" -l detach -d 'Create session without attaching'

complete -c posh -n "__fish_seen_subcommand_from completions" -a 'bash zsh fish' -d 'Shell'

complete -c posh -n "__fish_seen_subcommand_from list" -l short -d 'Short output'
complete -c posh -n "__fish_seen_subcommand_from list" -l json -s j -d 'JSON output'
complete -c posh -n "__fish_seen_subcommand_from history" -l vt -d 'VT escape stream output'
complete -c posh -n "__fish_seen_subcommand_from ssh" -a '(__posh_ssh_config_hosts)' -d 'Host'
"#;

#[cfg(test)]
mod tests {
    use super::*;

    const COMMANDS: &[&str] = &[
        "attach",
        "run",
        "detach",
        "detach-all",
        "fork",
        "groups",
        "list",
        "completions",
        "kill",
        "history",
        "server",
        "client",
        "ssh",
        "version",
        "help",
    ];

    #[test]
    fn shell_parsing() {
        assert_eq!(Shell::from_str("bash"), Some(Shell::Bash));
        assert_eq!(Shell::from_str("zsh"), Some(Shell::Zsh));
        assert_eq!(Shell::from_str("fish"), Some(Shell::Fish));
        assert_eq!(Shell::from_str("powershell"), None);
        assert_eq!(Shell::from_str(""), None);
    }

    #[test]
    fn bash_script_covers_all_subcommands() {
        let script = Shell::Bash.script();
        for cmd in COMMANDS {
            assert!(script.contains(cmd), "bash completions missing {cmd}");
        }
        assert!(script.contains("complete -o bashdefault -o default -F _posh_completions posh"));
        assert!(!script.contains("zmx"));
    }

    #[test]
    fn zsh_script_covers_all_subcommands() {
        let script = Shell::Zsh.script();
        for cmd in COMMANDS {
            assert!(
                script.contains(&format!("{cmd}:")) || script.contains(cmd),
                "zsh completions missing {cmd}"
            );
        }
        assert!(script.contains("compdef _posh posh"));
        assert!(!script.contains("zmx"));
    }

    #[test]
    fn fish_script_covers_all_subcommands() {
        let script = Shell::Fish.script();
        for cmd in COMMANDS {
            assert!(
                script.contains(&format!("-a {cmd} ")),
                "fish completions missing {cmd}"
            );
        }
        assert!(script.contains("complete -c posh"));
        assert!(!script.contains("zmx"));
    }

    #[test]
    fn scripts_reference_dynamic_session_and_group_lookup() {
        for shell in [Shell::Bash, Shell::Zsh, Shell::Fish] {
            let script = shell.script();
            assert!(
                script.contains("posh list --short"),
                "{shell:?} should complete session names"
            );
            assert!(
                script.contains("posh groups"),
                "{shell:?} should complete group names"
            );
        }
    }

    #[test]
    fn bash_script_parses() {
        // bash -n syntax-checks without executing. Skip quietly where
        // bash is unavailable (it exists in the devShell and sandbox).
        use std::io::Write;
        use std::process::{Command, Stdio};
        let Ok(mut child) = Command::new("bash")
            .arg("-n")
            .stdin(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        else {
            return;
        };
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(Shell::Bash.script().as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "bash -n rejected the completion script: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn scripts_complete_ssh_config_aliases() {
        // github #37: the remote forms (bare `posh <host>`, `posh ssh`)
        // complete from ~/.ssh/config Host aliases, wildcards dropped.
        for shell in [Shell::Bash, Shell::Zsh, Shell::Fish] {
            let script = shell.script();
            assert!(
                script.contains(".ssh/config"),
                "{shell:?} should read ssh config Host aliases"
            );
        }
    }

    #[test]
    fn remote_session_completion_is_cached_and_batchmode() {
        // RFC 0001 namespace: host:<Tab> queries the host's sessions over
        // ssh — BatchMode so a Tab can never hang on auth, a bounded
        // connect timeout, and a short-TTL cache (FDR 0001 tuning levers).
        for shell in [Shell::Bash, Shell::Fish] {
            let script = shell.script();
            for needle in ["BatchMode=yes", "ConnectTimeout=2", "/posh", "sessions-"] {
                assert!(
                    script.contains(needle),
                    "{shell:?} remote completion missing {needle}"
                );
            }
        }
    }

    #[test]
    fn bare_position_completes_sessions_and_hosts() {
        // github #37: the first argument is also the attach shorthand and
        // the mosh-style host form, so both complete alongside commands.
        let bash = Shell::Bash.script();
        assert!(
            bash.contains(r#"compgen -W "$commands $sessions $hosts""#),
            "bash bare position must offer commands + sessions + hosts"
        );
        let fish = Shell::Fish.script();
        assert!(
            fish.contains("$no_subcmd -a '(posh list --short 2>/dev/null)'"),
            "fish bare position must offer sessions"
        );
        assert!(
            fish.contains("$no_subcmd -a '(__posh_ssh_config_hosts)'"),
            "fish bare position must offer ssh hosts"
        );
    }
}
