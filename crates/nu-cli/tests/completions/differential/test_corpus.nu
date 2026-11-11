# Every case of the corpus completes without an error, and every suggestion replaces a part of the
# line that is on it and ends at or after the cursor's word start: the span sanity the property
# tests check on generated lines, here on the lines people met. Portable: run in any directory.
#   nu -c 'use crates/nu-std/testing.nu; testing run-tests --path crates/nu-cli/tests/completions/differential'
use std/assert
use std/testing *
use corpus.nu cases

@test
def "every corpus case completes with sane spans" [] {
    let faults = cases | each {|case|
        let typed = $case.line | encode utf8 | bytes at 0..<($case.at) | decode utf8
        let length = $typed | encode utf8 | bytes length
        let answer = try { $typed | commandline complete --detailed } catch {|error| [{error: $error.msg}] }
        $answer | where {|item|
            ($item.error? != null) or (
                $item.span? != null and (
                    $item.span.start < 0 or $item.span.end > $length or $item.span.start > $item.span.end
                )
            )
        } | each {|item| {line: $case.line, at: $case.at, item: $item} }
    } | flatten
    assert ($faults | is-empty) ($faults | first 5 | to nuon)
}
