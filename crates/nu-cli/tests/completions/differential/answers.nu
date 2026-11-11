# The answers of the running build to every case of the corpus, as nuon on stdout: what each line
# completes to with the cursor at its offset (values and the spans they replace), and what the line
# editor does there: the line a Tab and an Enter on a lone suggestion leave, and, from builds whose
# `debug complete` has `--editor`, the status line, its anchor, the sign and the history hint.
#
# Run it with the build to check, in a directory with the files the cases expect:
#   nu answers.nu > answers.nuon
#   nu -c 'use answers.nu; answers {|line| $line | debug complete --editor }' > answers.nuon
# `trace` traces one line; a build whose `debug complete` has `--editor` is traced with it.
# The line is cut at the cursor, so a build that can only complete at the end of its input
# (`commandline complete` without `--cursor`) answers the same question.
use corpus.nu cases

# `line` with `item` taken, as the line editor takes a suggestion: its value over its span, the
# cursor where it asks or after the value, and the space it asks for unless one follows.
def take [line: string, item: record]: nothing -> record {
    let bytes = $line | encode utf8
    let end = [$item.span.end ($bytes | bytes length)] | math min
    let start = [$item.span.start $end] | math min
    let before = $bytes | bytes at 0..<$start | decode utf8
    let after = $bytes | bytes at $end.. | decode utf8
    let taken = $before + $item.value + $after
    let cursor = $item.cursor? | default (($before + $item.value) | encode utf8 | bytes length)
    if ($item.append_whitespace? != true) or ($item.cursor? != null) {
        {line: $taken, cursor: $cursor}
    } else if ($after | str starts-with ' ') {
        {line: $taken, cursor: ($cursor + 1)}
    } else {
        {line: ($before + $item.value + ' ' + $after), cursor: ($cursor + 1)}
    }
}

# What a Tab does on `line` (the cursor at its end) with `shown` just answered, by the line
# editor's Tab law: a lone suggestion is taken (`completions.quick`), several type the start they
# share when it goes further than what is typed (`completions.partial`), and otherwise the first is
# highlighted and the line stays.
def tab [line: string, shown: list]: nothing -> record {
    let unchanged = {line: $line, cursor: ($line | encode utf8 | bytes length)}
    let config = $env.config.completions
    if ($shown | is-empty) { return $unchanged }
    if ($shown | length) == 1 and $config.quick { return (take $line $shown.0) }
    let end = $shown.0.span.end
    if not $config.partial or ($shown | any {|item| $item.span.end != $end }) { return $unchanged }
    let heads = $shown | each {|item| ($line | encode utf8 | bytes at 0..<($item.span.start) | decode utf8) + $item.value }
    let shared = $heads | reduce {|head, shared|
        $shared | split chars | zip ($head | split chars) | take while {|pair| $pair.0 == $pair.1 } | each { first } | str join
    } | str trim --right
    let typed = $line | encode utf8 | bytes at 0..<$end | decode utf8
    if ($shared | str length) <= ($typed | str length) or not ($shared | str starts-with $typed) {
        return $unchanged
    }
    let start = $shown | get span.start | math min
    let value = $shared | encode utf8 | bytes at $start.. | decode utf8
    take $line {value: $value, span: {start: $start, end: $end}, append_whitespace: false}
}

export def main [trace?: closure] {
    let trace = if $trace == null { {|line| $line | debug complete } } else { $trace }
    cases | each {|case|
        let typed = $case.line | encode utf8 | bytes at 0..<($case.at) | decode utf8
        let items = try {
            $typed | commandline complete --detailed
            | each {|item| {value: $item.value, start: ($item.span?.start? | default null), end: ($item.span?.end? | default null)} }
        } catch {|error| [{error: ($error.msg)}] }
        let traced = try { do $trace $typed } catch {|error| {shown: [], error: $error.msg} }
        let shown = $traced.shown | select value span append_whitespace cursor
        let lone = if ($shown | length) == 1 { take $typed $shown.0 }
        {
            line: $case.line, at: $case.at, source: $case.source, items: $items
            tab: (tab $typed $shown), enter: $lone, editor: ($traced.editor? | default null)
        }
    }
    | to nuon
}
