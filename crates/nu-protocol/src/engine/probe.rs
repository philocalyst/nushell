//! What the parser was ready to read at one offset of the source.
//!
//! Completion, the status line, the history hint, signature help and the language server all
//! need to know what may be written at the cursor. The parser is the only part of Nushell that
//! knows: it decides, token by token, which [`SyntaxShape`] it expects, which call and parameter
//! a value binds to, and which operator or operand a math expression still lacks. A [`Probe`]
//! asks it to say so for one offset while it parses, as typed records, instead of every consumer
//! walking the finished syntax tree and re-reading the text to guess what an incomplete line meant.
//!
//! A parse without a probe records nothing and costs one `Option` check per hook.

use crate::{
    DeclId, Span, SyntaxShape, Type, VarId,
    ast::{Block, Call, Expr, Expression, Operator, Traverse},
    engine::StateWorkingSet,
};

/// The offset being probed, and what the parser expected there.
#[derive(Debug, Clone, Default)]
pub struct Probe {
    /// The probed offset, in working-set (span) coordinates.
    pub at: usize,
    /// The source being probed. What the parser expects in other files it reads meanwhile, such
    /// as a module the source uses, is not recorded.
    pub source: Span,
    /// Everything the parser was ready to read at `at`, in the order it noticed them.
    pub found: Vec<Expected>,
    /// The variables in scope at `at`, innermost first, each name once.
    pub scope: Vec<(Vec<u8>, VarId)>,
    /// What the parser read only for the probe: the shorter reading of a name of several words
    /// (`foo bar` as `foo` and its argument), whose expectations are among `found`.
    pub readings: Vec<Expression>,
}

impl Probe {
    /// The expectations of the innermost construct around the probe: those recorded by the
    /// construct with the smallest extent. An outer call that holds the cursor in a closure
    /// argument expects a closure there, but the call inside the closure says more.
    ///
    /// A record being written is no site of its own: what goes in it is read from its record.
    pub fn innermost(&self) -> impl Iterator<Item = &Expected> {
        let sites =
            || (self.found.iter()).filter(|found| !matches!(found.what, Expect::RecordKey { .. }));
        let narrowest = sites().map(|found| found.within.len()).min();
        sites().filter(move |found| Some(found.within.len()) == narrowest)
    }

    /// The narrowest of what was found that is an argument of a call, or with `names` also a
    /// command being named. On a tie an argument wins over a name (`git ⌶` is `git`'s argument),
    /// a flag's value or a positional over a flag (the gap after a command takes either), and the
    /// one recorded last over the others: what the parse that stood recorded.
    pub fn narrowest(&self, names: bool) -> Option<&Expected> {
        let rank = |found: &Expected| match found.what {
            Expect::FlagValue { .. } => Some(0),
            Expect::Positional { .. } => Some(1),
            Expect::Flag { .. } | Expect::Redirection => Some(2),
            Expect::Command { .. } | Expect::ExternalArg { .. } if names => Some(3),
            _ => None,
        };
        (self.found.iter().rev())
            .filter_map(|found| Some((found.within.len(), rank(found)?, found)))
            .min_by_key(|(width, rank, _)| (*width, *rank))
            .map(|(.., found)| found)
    }
}

impl Expected {
    /// The command whose argument this is.
    pub fn decl_id(&self) -> Option<DeclId> {
        match self.what {
            Expect::Flag { decl_id }
            | Expect::FlagValue { decl_id, .. }
            | Expect::Positional { decl_id, .. } => Some(decl_id),
            _ => None,
        }
    }
}

/// One thing the parser was ready to read at the probe.
#[derive(Debug, Clone, PartialEq)]
pub struct Expected {
    /// What a completion replaces: the token the probe is in, or an empty span at the probe.
    pub span: Span,
    /// The extent of the construct that expects it: a call, a math expression. The narrowest
    /// one is the most specific reading.
    pub within: Span,
    /// The type flowing into the pipeline element the expectation belongs to.
    pub input: Type,
    /// What may be written.
    pub what: Expect,
}

/// What may be written at the probe.
#[derive(Debug, Clone, PartialEq)]
pub enum Expect {
    /// A command name. `parent` is the command whose subcommands also fit (`str ⌶`).
    Command { parent: Option<DeclId> },
    /// A flag of the call to `decl_id`.
    Flag { decl_id: DeclId },
    /// The value of the flag `long` of the call to `decl_id`.
    FlagValue {
        decl_id: DeclId,
        long: String,
        shape: SyntaxShape,
    },
    /// The positional parameter `index` of the call to `decl_id`, in signature order (the rest
    /// parameter for every index past the others).
    Positional {
        decl_id: DeclId,
        index: usize,
        shape: SyntaxShape,
    },
    /// An argument of an external command, which has no signature.
    ExternalArg { index: usize },
    /// An operator after the operand `lhs` (`size ⌶`).
    Operator { lhs: Box<Expression> },
    /// The right-hand operand of `operator`, whose left-hand side is `lhs` (`size > ⌶`).
    Operand {
        lhs: Box<Expression>,
        operator: Operator,
    },
    /// The operand of a `not` (`where not ⌶`).
    Negated,
    /// A member of a cell path whose value up to the member has type `head` (`$in.⌶`). The path
    /// is the expression the expectation is `within`.
    Member { head: Type },
    /// A variable name.
    Variable,
    /// A pattern of an arm of a `match`, which is compared with what is matched.
    MatchPattern,
    /// The name of an attribute (`@⌶`).
    AttributeName,
    /// The definition an attribute block decorates (`@example; ⌶`).
    Attributable,
    /// The file a redirection writes to (`ls out> ⌶`).
    Redirection,
    /// A key of the record being written, or with `value_of` the value of that key; `keys` are
    /// those it has already (`{a: 1, ⌶`, `{a: ⌶`).
    RecordKey {
        keys: Vec<String>,
        value_of: Option<String>,
    },
}

/// The innermost call in `block` that `is` the one looked for: the call a record of a [`Probe`]
/// is an argument of is the one whose name starts where the record's `within` does.
pub fn innermost_call<'a>(
    block: &'a Block,
    working_set: &'a StateWorkingSet,
    is: impl Fn(&Call) -> bool,
) -> Option<&'a Call> {
    let mut calls = Vec::new();
    block.flat_map(
        working_set,
        &|expr| match &expr.expr {
            Expr::Call(call) if is(call) => vec![&**call],
            _ => vec![],
        },
        &mut calls,
    );
    calls.pop()
}

/// Whether `span` holds `at` or ends at it: a cursor at the end of a word is in that word.
pub fn touches(span: Span, at: usize) -> bool {
    span.start <= at && at <= span.end
}
