# Compares two runs of answers.nu, case by case, field by field: what the line completes to
# (`items`), what a Tab and an Enter leave (`tab`, `enter`), and, where both runs traced the line
# editor, its status line, anchor, sign and hint. Prints one row per case and field that differ.
#   nu compare.nu before.nuon after.nuon
export def main [before: path, after: path, --fields: list<string> = [items tab enter status anchor sign hint]] {
    let after_rows = open $after
    open $before | enumerate | each {|row|
        let was = $row.item
        let now = $after_rows | get $row.index
        let views = {|answer| {
            items: $answer.items, tab: $answer.tab, enter: $answer.enter
            status: $answer.editor?.status?, anchor: $answer.editor?.anchor?
            sign: $answer.editor?.sign?, hint: $answer.editor?.hint?
        } }
        let was_view = do $views $was
        let now_view = do $views $now
        $fields | where {|field|
            ($field not-in [status anchor sign hint]) or ($was.editor? != null and $now.editor? != null)
        } | where {|field| ($was_view | get $field) != ($now_view | get $field) } | each {|field|
            {line: $was.line, at: $was.at, field: $field, before: ($was_view | get $field), after: ($now_view | get $field)}
        }
    } | flatten
}
