# Completion helpers.
#
# `ssh-hosts` lists the hosts ssh knows about, and an `ssh` extern uses it for its destination
# and completes the remote command on the remote host. `delegate` builds the record a
# completer returns to have part of the line completed as a command line of its own, which is
# how a command that runs other commands gets its completions. `tldr examples` turns a tldr page
# into the `@example`s of an `extern`, which completion previews, `help` and the LSP then show.
#
#   use std/completions *

# Hosts ssh can connect to: the `Host` names in an ssh config, and the hosts in `known_hosts`.
#
# Wildcard and negated patterns in the config are left out, as are the hashed entries of
# `known_hosts`, which do not name their host. A file that is missing or unreadable adds nothing.
@example "the hosts of a config and a known_hosts file" {
    ssh-hosts --config ~/.ssh/config --known-hosts ~/.ssh/known_hosts
}
export def ssh-hosts [
    --config: path       # The ssh client config to read (default: ~/.ssh/config)
    --known-hosts: path  # The known_hosts file to read (default: ~/.ssh/known_hosts)
]: nothing -> list<string> {
    let config = $config | default ($nu.home-dir | path join '.ssh' 'config')
    let known_hosts = $known_hosts | default ($nu.home-dir | path join '.ssh' 'known_hosts')

    (config-hosts $config) ++ (known-hosts $known_hosts) | uniq
}

# Build the record a completer returns to complete part of the line as a command line of its
# own: the line from byte offset `from` of the completer's `buffer`.
#
# The delegated line completes like any line typed alone: its first word names a program, and
# its arguments go to the external completer, or to files. `--paths` and `--commands` swap the
# local filesystem and `PATH` for another machine's.
@example "complete from byte 4 of the line as a command line of its own, with these commands" {
    delegate 4 --commands {|| [ls cat grep] }
}
export def delegate [
    from: int            # Byte offset into the completer's `buffer` where the command line starts
    --paths: closure     # {|dir| ...}: the names in `dir`, directories ending in `/` (`.` if none is typed)
    --commands: closure  # {|| ...}: the names of the commands to complete the first word from
    --inside: string     # The quote of the string `from` is inside of: names are escaped for it
    --path               # `from` starts one path (`host:dir/f`), not a command line
]: nothing -> record {
    mut delegation = {from: $from}
    if $inside != null {
        $delegation.inside = $inside
    }
    if $path {
        $delegation.path = true
    }
    if $paths != null {
        $delegation.paths = $paths
    }
    if $commands != null {
        $delegation.commands = $commands
    }
    {delegate: $delegation}
}

# The remote command of ssh completes on the remote host, and the destination from ssh-hosts.
# Flags are listed as ssh(1) has them so the line parses around them.
@complete ssh-remote
export extern ssh [
    destination?: string@ssh-host   # [user@]hostname or ssh:// URI
    ...command: string              # The command to run on the remote host
    -4                              # Use IPv4 addresses only
    -6                              # Use IPv6 addresses only
    -A                              # Forward the authentication agent connection
    -a                              # Do not forward the authentication agent connection
    -B: string                      # Bind to the address of this interface
    -b: string                      # Source address on the local machine
    -C                              # Compress data
    -c: string                      # Cipher specification
    -D: string                      # Dynamic application-level port forwarding
    -E: path                        # Append debug logs to this file
    -e: string                      # Escape character
    -F: path                        # Use this per-user configuration file
    -f                              # Go to the background before running the command
    -G                              # Print the configuration and exit
    -g                              # Allow remote hosts to connect to forwarded ports
    -I: string                      # PKCS#11 shared library
    -i: path                        # Identity file
    -J: string                      # Connect through this jump host
    -K                              # Enable GSSAPI authentication
    -k                              # Disable GSSAPI credential forwarding
    -L: string                      # Local port forwarding
    -l: string                      # Login name
    -M                              # Place the client in master mode
    -m: string                      # MAC algorithms
    -N                              # Do not run a remote command
    -n                              # Redirect stdin from /dev/null
    -O: string                      # Control a running connection master
    -o: string                      # Option in the format of the configuration file
    -P: string                      # Tag for the session
    -p: int                         # Port to connect to
    -Q: string                      # Query supported algorithms
    -q                              # Quiet mode
    -R: string                      # Remote port forwarding
    -S: path                        # Control socket
    -s                              # Request a subsystem
    -T                              # Disable pseudo-terminal allocation
    -t                              # Force pseudo-terminal allocation
    -V                              # Show the version
    -v                              # Verbose mode
    -W: string                      # Forward stdin and stdout to this host and port
    -w: string                      # Tunnel device forwarding
    -X                              # Enable X11 forwarding
    -x                              # Disable X11 forwarding
    -Y                              # Enable trusted X11 forwarding
    -y                              # Send logs to syslog
    -Z                              # See ssh(1)
]

# The destination completer: hosts, keeping a `user@` the token already has.
def ssh-host [token: record]: nothing -> list<string> {
    let at = $token.text | str index-of '@' --end
    let user = if $at >= 0 { $token.text | str substring 0..$at } else { '' }
    ssh-hosts | each {|host| $user + $host }
}

# The `ssh` options that take a value, per ssh(1).
const SSH_VALUED = 'BbcDEeFIiJLlmOoPpQRSWw'

# The ones that choose how to connect to a host, which a listing on that host repeats.
const SSH_CONNECTION = 'piJoFl'

# The remote command completer: once ssh's destination is typed, the rest of the line is a
# command line on that host, so it is completed as one, listing paths and commands over ssh.
# Anything else (an option, the value of an option) is left to the usual completion.
def ssh-remote [buffer: string] {
    let words = command-words $buffer
    let scan = ssh-scan ($words | skip 1)
    if $scan == null {
        return null
    }

    let connection = {host: $scan.host, options: $scan.options}
    delegate ($words | get ($scan.command + 1) | get start) --paths {|dir|
        ssh-run $connection $"ls -1Ap -- (remote-path $dir)"
    } --commands {||
        ssh-run $connection "bash -c 'compgen -c' 2>/dev/null || sh -c 'IFS=:; for d in $PATH; do ls -1 \"$d\"; done' 2>/dev/null"
    }
}

# Find where ssh's remote command starts among the words after `ssh`, the last being the one
# the cursor is in. ssh reads options as getopt does, but also after the destination, up to
# the first word that is not one. `null` unless the cursor is in the remote command.
def ssh-scan [words: list<record<text: string, start: int>>]: nothing -> any {
    mut host = ''
    mut has_host = false
    mut options = []
    mut pending = ''
    mut literal = false
    let last = ($words | length) - 1

    for index in 0..<($words | length) {
        let text = $words | get $index | get text
        let at_cursor = $index == $last

        if $pending != '' {
            if ($SSH_CONNECTION | str contains $pending) {
                $options = $options ++ [$"-($pending)" $text]
            }
            $pending = ''
        } else if not $literal and $text == '--' {
            $literal = true
        } else if not $literal and ($text | str starts-with '-') and ($text | str length) > 1 {
            # A cluster of options: the first one that takes a value ends it, taking the rest
            # of the word, or else the next word.
            let flags = $text | split chars | skip 1
            for position in 0..<($flags | length) {
                let flag = $flags | get $position
                if ($SSH_VALUED | str contains $flag) {
                    let value = $flags | skip ($position + 1) | str join
                    if $value == '' {
                        $pending = $flag
                    } else if ($SSH_CONNECTION | str contains $flag) {
                        $options = $options ++ [$"-($flag)" $value]
                    }
                    break
                }
            }
        } else if not $has_host {
            $host = $text
            $has_host = true
        } else {
            return {command: $index, host: $host, options: $options}
        }

        if $at_cursor {
            return null
        }
    }
    null
}

# The words of the last command on `buffer`, each with the byte offset it starts at, plus an
# empty one where the cursor is in a slot not typed yet. Empty unless that command is `ssh`.
def command-words [buffer: string]: nothing -> list<record<text: string, start: int>> {
    let end = $buffer | str length
    mut words = []
    mut position = 0

    loop {
        let found = $buffer
        | str substring $position..
        | parse --regex '^(?<gap>\s*)(?<text>(?:[^\s\x27"|;]|\x27[^\x27]*\x27?|"(?:[^"\\]|\\.)*"?)+|[|;])'
        if ($found | is-empty) {
            break
        }

        let gap = $found.0.gap | str length
        let text = $found.0.text
        if $text in ['|' ';'] {
            # A new command starts.
            $words = []
        } else {
            $words = $words ++ [{text: $text, start: ($position + $gap)}]
        }
        $position = $position + $gap + ($text | str length)
    }

    if $position < $end or ($words | is-empty) {
        $words = $words ++ [{text: '', start: $end}]
    }

    if (($words | first).text | str replace --regex '^\^' '') == 'ssh' {
        $words
    } else {
        []
    }
}

# A directory as the remote shell should read it: quoted, except for a leading `~`.
def remote-path [dir: string]: nothing -> string {
    if $dir == '~' {
        '~'
    } else if ($dir | str starts-with '~/') {
        '~/' + (shell-quote ($dir | str substring 2..))
    } else {
        shell-quote $dir
    }
}

# Quote text for a POSIX shell.
def shell-quote [text: string]: nothing -> string {
    "'" + ($text | str replace --all "'" "'\\''") + "'"
}

# Run `remote` on the host without ever asking anything. Any failure (no ssh, no route, no
# key, a nonzero exit) gives no names.
def ssh-run [connection: record, remote: string]: nothing -> list<string> {
    let result = try {
        ^ssh ...(ssh-arguments $connection $remote) | complete
    } catch {
        return []
    }
    if $result.exit_code != 0 {
        return []
    }
    $result.stdout | lines | where $it != '' | uniq
}

# The arguments of an ssh that runs `remote` on the host and nothing else: no prompts, a short
# timeout, and the connection options the line chose.
def ssh-arguments [connection: record, remote: string]: nothing -> list<string> {
    [-n -o BatchMode=yes -o ConnectTimeout=3] ++ $connection.options ++ [$connection.host $remote]
}

# The `Host` names of an ssh client config.
def config-hosts [path: path]: nothing -> list<string> {
    read-lines $path
    | where {|line| $line =~ '(?i)^host(\s+|\s*=\s*)\S' }
    | each {|line|
        $line
        | str replace --regex '(?i)^host\s*=?\s*' ''
        | str replace --regex '\s+#.*$' ''
        | split row --regex '\s+'
    }
    | flatten
    | each {|name| $name | str trim --char '"' }
    | where {|name| $name != '' and not ($name =~ '[*?!]') }
}

# The hosts of a known_hosts file: the first field of each line, which lists hosts separated
# by commas, after an optional `@cert-authority` or `@revoked` marker.
def known-hosts [path: path]: nothing -> list<string> {
    read-lines $path
    | where {|line| not ($line | str starts-with '#') }
    | each {|line|
        let fields = $line | split row --regex '\s+'
        let hosts = if ($fields.0 | str starts-with '@') { $fields | get 1? } else { $fields.0 }
        # A hashed entry (`|1|salt|hash`) does not name its host.
        if $hosts == null or ($hosts | str starts-with '|') { [] } else { $hosts | split row ',' }
    }
    | flatten
    | each {|host| $host | str replace --regex '^\[(.+)\]:\d+$' '$1' }
    | where {|host| $host != '' and not ($host =~ '[*?!|]') }
}

# The trimmed, non-empty lines of a file; none if it cannot be read.
def read-lines [path: path]: nothing -> list<string> {
    if not ($path | path exists) {
        return []
    }
    try {
        open --raw $path | into string | lines | str trim | where $it != ''
    } catch {
        []
    }
}

# The examples of the tldr page of `command` as an `extern` for it with those `@example`s, to save
# and `use`: completion previews, `help` and the LSP then show them. `{{placeholders}}` become
# `<placeholders>`, and the first of tldr's `{{[-f|--force]}}` forms is written in its place.
@example "the examples of git's page, as a module to `use git-examples.nu *`" {
    tldr examples git | save git-examples.nu
}
export def "tldr examples" [
    command: string        # The command whose page to read, as tldr names it (`git checkout`)
    --pages: path          # The directory of tldr's pages (default: the tldr client's cache)
]: nothing -> string {
    let cache = $env.TLDR_CACHE_DIR? | default ($nu.home-dir | path join .cache tldr)
    let pages = $pages | default ($cache | path join pages)
    let name = $command | str replace --all ' ' '-'
    let page = [common linux osx windows]
        | each {|platform| $pages | path join $platform $'($name).md' }
        | where {|file| $file | path exists }
        | first
    let lines = open --raw $page | lines
    let examples = $lines | enumerate | where {|line| $line.item starts-with '- ' } | each {|line|
        let code = $lines | skip ($line.index + 1) | where {|next| $next starts-with '`' } | first
        let code = $code | str trim --char '`' | str replace --all --regex '\{\{\[([^|\]]*)[^\]]*\]\}\}' '$1'
            | str replace --all --regex '\{\{([^}]*)\}\}' {|inner| $'<($inner | str replace --all --regex '[^A-Za-z0-9_-]+' '_' | str trim --char '_')>' }
        $'@example ($line.item | str substring 2.. | str trim --right --char ':' | to nuon) ($code | to nuon)'
    }
    $examples | append $'export extern "($command)" []' | str join "\n"
}
