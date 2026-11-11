# Completion cases for the differential check against an earlier build, and the regression
# corpus of the completion suite: each is a line and a cursor (a byte offset into it).
# `source` says where the case comes from: the showcase guide, the curated list of what the
# completion rethink covers, or a sweep of every cursor position of the tricky lines (`probe`).
const curated = [
    {line: "ls | each ", at: 10, source: showcase}
    {line: "let x ", at: 6, source: showcase}
    {line: "ls | where size > 1", at: 19, source: showcase}
    {line: "ls | where modified > ", at: 22, source: showcase}
    {line: "ls | where size > 1kb ", at: 22, source: showcase}
    {line: "ls | where $in.name ", at: 20, source: showcase}
    {line: "ls | where name = foo and size > 1b ", at: 36, source: showcase}
    {line: "ls | where nosuch > 1 and name == ", at: 34, source: showcase}
    {line: "ls | ", at: 5, source: showcase}
    {line: "ls | each { ", at: 12, source: showcase}
    {line: "ls | where size > 1b and type == dir", at: 36, source: showcase}
    {line: "str replace ", at: 12, source: showcase}
    {line: "ls | select name size modified | ", at: 33, source: showcase}
    {line: "crgo build", at: 10, source: showcase}
    {line: "git checkout feature/pa", at: 23, source: showcase}
    {line: "ls | where size > 1kb | first 3", at: 31, source: showcase}
    {line: "ls | select name type ", at: 22, source: showcase}
    {line: "ls -a -", at: 7, source: showcase}
    {line: "[{os: linux, arch: x86}, {", at: 26, source: showcase}
    {line: "ls | each { {name: ", at: 19, source: showcase}
    {line: "[[name size]; [a 1]] | merge {", at: 30, source: showcase}
    {line: "def f [r: record<depth: int, name: string>] {}; f {", at: 51, source: showcase}
    {line: "ls | get ", at: 9, source: showcase}
    {line: "[[commit subject author]; ['0123456789abcdef0123456789abcdef01234567' 'a fairly long subject' 'me']] | first", at: 108, source: showcase}
    {line: "git ", at: 4, source: showcase}
    {line: "git checkout gone-branch  (Enter), then git checkout", at: 52, source: showcase}
    {line: "echohello", at: 9, source: showcase}
    {line: "ls my", at: 5, source: showcase}
    {line: "cd ", at: 3, source: curated}
    {line: "ls ", at: 3, source: curated}
    {line: "open ", at: 5, source: curated}
    {line: "ls -", at: 4, source: curated}
    {line: "ls --a", at: 6, source: curated}
    {line: "^ec", at: 3, source: curated}
    {line: "%ls", at: 3, source: curated}
    {line: "str tr", at: 6, source: curated}
    {line: "str ", at: 4, source: curated}
    {line: "sort-by ", at: 8, source: curated}
    {line: "ls | where ", at: 11, source: curated}
    {line: "ls | where name ", at: 16, source: curated}
    {line: "ls | where name == ", at: 19, source: curated}
    {line: "ls | where type == ", at: 19, source: curated}
    {line: "ls | where $in.type == ", at: 23, source: curated}
    {line: "ls | where $in.", at: 15, source: curated}
    {line: "ls | each {|r| $r.", at: 18, source: curated}
    {line: "ls | select ", at: 12, source: curated}
    {line: "ls | select name ", at: 17, source: curated}
    {line: "ls | sort-by ", at: 13, source: curated}
    {line: "$env.", at: 5, source: curated}
    {line: "$env.config.completions.", at: 24, source: curated}
    {line: "$nu.", at: 4, source: curated}
    {line: "let x = {a: 1, b: {c: 2}}; $x.b.", at: 32, source: curated}
    {line: "[1 2 3] | each ", at: 15, source: curated}
    {line: "def foo [] {}; fo", at: 17, source: curated}
    {line: "use std/", at: 8, source: curated}
    {line: "git checkout ", at: 13, source: curated}
    {line: "git checkout fe", at: 15, source: curated}
    {line: "git switch -", at: 12, source: curated}
    {line: "git log --", at: 10, source: curated}
    {line: "cargo b", at: 7, source: curated}
    {line: "cargo build --", at: 14, source: curated}
    {line: "ssh -", at: 5, source: curated}
    {line: "help ", at: 5, source: curated}
    {line: "which ", at: 6, source: curated}
    {line: "hide-env ", at: 9, source: curated}
    {line: "kill ", at: 5, source: curated}
    {line: "ls | where name = a", at: 19, source: curated}
    {line: "ls | where name == a && ", at: 24, source: curated}
    {line: "echo 'a b' | ", at: 13, source: curated}
    {line: "ls out> ", at: 8, source: curated}
    {line: "ls | to ", at: 8, source: curated}
    {line: "into ", at: 5, source: curated}
    {line: "[a b] | str ", at: 12, source: curated}
    {line: "mut x = 1; $x ", at: 14, source: curated}
]

# The lines every cursor position of which is a case: each character boundary, in order.
const swept = [
    "cat 👨‍👩‍👧"
    "cat 日本"
    "cat 'my fi' | lines"
    "cat 'my fi"
    "cat \"my fi\" x"
    "cat it"
    "cat dq"
    "cat é"
    "ls --all=tr"
    "^ls -"
    "ls\ncat my"
    "ls | each {\n  $in.na\n}"
    "echo $\"(ls | get na) and (cat my)\""
    "echo $\"x ($in | wh"
    "[1 2] | each { |x| ls | wh"
    "ls | where name =~ '日'"
    "ls | collect { |x| $x | get na"
    "ls | $in | get na"
    "(ls | get n)"
    "ls | get name.0 | str rep"
    "r#'abc"
    "cat `日"
    "ls ~/Pro"
    "ls ../p"
    "ls | wher\tname"
    "def f [--flag: string@[a b]] {}; f --flag "
    "match 1 { 1 => (ls | get na"
    "ls | update name {|r| $r.na"
    "with-env {A: 1} { ls | get n"
    "do { ls } | get na"
    "if (ls | length) > 1 { cat my"
]

# Every case: the curated ones, then each position of each swept line.
export def cases []: nothing -> list<record> {
    let positions = {|line| $line | split chars | reduce -f [0] {|char, ats| $ats | append (($ats | last) + ($char | encode utf8 | bytes length)) } }
    $curated | append ($swept | each {|line| do $positions $line | each {|at| {line: $line, at: $at, source: probe} } } | flatten)
}
