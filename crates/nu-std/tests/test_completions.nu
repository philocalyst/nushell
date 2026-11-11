use std/assert
use std/testing *
use std/completions *

# A directory holding an ssh `config` and `known_hosts`; nothing here reaches the network.
def ssh-fixture []: nothing -> string {
    let dir = mktemp --directory

    "# a comment
Host alpha beta
    HostName 10.0.0.1
    HostKeyAlias keyed
Host *.example.com
Host gamma # a trailing comment
Host=delta
host EPSILON
Host !excluded wild?
Match host nowhere
Host \"quoted\"
" | save ($dir | path join config)

    "# a comment
|1|c2FsdA==|aGFzaA== ssh-ed25519 AAAA
one.example.com,192.168.0.9 ssh-rsa AAAA
[bracketed.example.com]:2222 ssh-ed25519 AAAA
@cert-authority *.corp ssh-rsa AAAA
@revoked revoked.example.com ssh-rsa AAAA
*.wild.example.com ssh-rsa AAAA
alpha ssh-ed25519 AAAA
" | save ($dir | path join known_hosts)

    $dir
}

@test
def "ssh-hosts reads the Host names of a config and skips patterns" [] {
    let dir = ssh-fixture
    let hosts = ssh-hosts --config ($dir | path join config) --known-hosts ($dir | path join missing)
    rm --recursive $dir

    assert equal $hosts [alpha beta gamma delta EPSILON quoted]
}

@test
def "ssh-hosts reads known_hosts, without hashed entries and wildcards" [] {
    let dir = ssh-fixture
    let hosts = ssh-hosts --config ($dir | path join missing) --known-hosts ($dir | path join known_hosts)
    rm --recursive $dir

    assert equal $hosts [one.example.com 192.168.0.9 bracketed.example.com revoked.example.com alpha]
}

@test
def "ssh-hosts lists each host once, config first" [] {
    let dir = ssh-fixture
    let hosts = ssh-hosts --config ($dir | path join config) --known-hosts ($dir | path join known_hosts)
    rm --recursive $dir

    assert equal $hosts [
        alpha beta gamma delta EPSILON quoted
        one.example.com 192.168.0.9 bracketed.example.com revoked.example.com
    ]
}

@test
def "ssh-hosts of files that are not there is empty" [] {
    let dir = mktemp --directory
    let hosts = ssh-hosts --config ($dir | path join nothing) --known-hosts $dir
    rm --recursive $dir

    assert equal $hosts []
}

@test
def "delegate builds the record a completer returns" [] {
    assert equal (delegate 5) {delegate: {from: 5}}

    let record = delegate 5 --paths {|dir| [] } --commands {|| [] }
    assert equal ($record.delegate | columns) [from paths commands]
    assert equal ($record.delegate.from) 5
    assert equal ($record.delegate.paths | describe) closure
    assert equal ($record.delegate.commands | describe) closure
}
