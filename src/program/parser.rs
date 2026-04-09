//! Chumsky-based parser for the `.cfe` program language.
//!
//! The entry point is [`parse`], which takes source text and a filename and returns
//! either a [`Program`] or a human-readable error string produced by ariadne.

use chumsky::prelude::*;

use super::ast::common::{
    Branch, CommonCondition, IfBlock, ManualStepNode, RunStep, SnapshotMetadataNode,
    WaitForContinueNode, WithEnvFileBlock,
};
use super::ast::crate_ctx::{
    CrateCondition, CrateFilter, CrateSelectCondition, CrateStatement, CrateTypeFilter,
    ForCrateBlock,
};
use super::ast::workspace_ctx::{
    ForCrateInWorkspaceBlock, ForWorkspaceBlock, WorkspaceCondition, WorkspaceFilter,
    WorkspaceSelectCondition, WorkspaceStatement,
};
use super::{GlobalStatement, Program};

/// Errors returned by the parser, rendered via ariadne.
///
/// Each value is an opaque human-readable string containing the full
/// ariadne-formatted diagnostic.  Collecting them separately lets callers
/// choose how to display them.
#[derive(Debug, Clone)]
pub struct ParseError(String);

impl ParseError {
    /// Returns the human-readable diagnostic string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Parse a `.cfe` program from `source` text.
///
/// `filename` is used in ariadne diagnostics to indicate which file an error
/// originated from.  Pass the path of the `.cfe` file on disk.
///
/// # Errors
///
/// Returns one [`ParseError`] per chumsky error encountered.  Errors are
/// formatted with ariadne and include source spans pointing to the offending
/// tokens.
pub fn parse(source: &str, filename: &str) -> Result<Program, Vec<ParseError>> {
    let (program, errors) = program_parser().parse(source).into_output_errors();

    if errors.is_empty()
        && let Some(prog) = program
    {
        return Ok(prog);
    }

    let parse_errors: Vec<ParseError> = errors
        .into_iter()
        .map(|e| format_error(e, source, filename))
        .collect();

    Err(parse_errors)
}

/// Format a single chumsky `Rich` error into an ariadne diagnostic string.
fn format_error(error: Rich<'_, char>, source: &str, filename: &str) -> ParseError {
    use ariadne::{Color, Label, Report, ReportKind, Source};

    let span = error.span();
    let range = span.start..span.end;

    let mut buf = Vec::new();
    Report::build(ReportKind::Error, (filename, range.clone()))
        .with_message(error.to_string())
        .with_label(
            Label::new((filename, range))
                .with_message(error.reason().to_string())
                .with_color(Color::Red),
        )
        .finish()
        .write((filename, Source::from(source)), &mut buf)
        // ariadne writes to a Vec<u8> which never fails
        .unwrap_or(());

    ParseError(String::from_utf8_lossy(&buf).into_owned())
}

// ─── Padding (whitespace + line comments) ────────────────────────────────────

/// Returns a parser that consumes zero or more whitespace characters and
/// `// …` line comments, producing nothing.
///
/// Each iteration of the internal `repeated()` consumes at least one character
/// (a single whitespace character or an entire line comment) to avoid the
/// chumsky "no progress" panic.
fn padding<'src>() -> impl Parser<'src, &'src str, (), extra::Err<Rich<'src, char>>> + Clone {
    // A single whitespace character (space, tab, newline, …).
    let single_ws = any().filter(|c: &char| c.is_whitespace()).ignored();
    // A `//` line comment extending to (but not including) the next newline.
    let line_comment = just("//")
        .then(any().and_is(just('\n').not()).repeated())
        .ignored();
    single_ws.or(line_comment).repeated().ignored()
}

// ─── String literals ──────────────────────────────────────────────────────────

/// Parses a double-quoted string literal, returning the unescaped content as a
/// `String`.  Currently the only supported escape sequence is `\"`.
fn string_literal<'src>()
-> impl Parser<'src, &'src str, String, extra::Err<Rich<'src, char>>> + Clone {
    let escape = just('\\').ignore_then(just('"').to('"'));
    let regular = none_of("\"\\");
    just('"')
        .ignore_then(escape.or(regular).repeated().collect::<String>())
        .then_ignore(just('"'))
        .padded_by(padding())
}

// ─── Keyword helper ───────────────────────────────────────────────────────────

/// Parses a keyword followed by padding.
fn kw<'src>(
    keyword: &'static str,
) -> impl Parser<'src, &'src str, &'src str, extra::Err<Rich<'src, char>>> + Clone {
    text::ascii::keyword(keyword).padded_by(padding())
}

// ─── Punctuation helpers ──────────────────────────────────────────────────────

/// Parses an exact string token followed by padding.
fn sym<'src>(
    symbol: &'static str,
) -> impl Parser<'src, &'src str, &'src str, extra::Err<Rich<'src, char>>> + Clone {
    just(symbol).padded_by(padding())
}

// ─── WorkspaceCondition parser ────────────────────────────────────────────────

/// Parses a [`WorkspaceCondition`] expression.
///
/// Operator precedence: `!` (tightest) → `&&` → `||` (loosest).
/// Includes common conditions (`ask_user`, `run`, `file_exists`, `working_directory_clean`)
/// plus `standalone` and `has_members`.
fn workspace_condition_parser<'src>()
-> impl Parser<'src, &'src str, WorkspaceCondition, extra::Err<Rich<'src, char>>> + Clone {
    recursive(|cond| {
        // Common leaf wrapped in WorkspaceCondition::Common ───────────────────
        let common_leaf = {
            let str_lit = string_literal();

            let ask_user = kw("ask_user")
                .ignore_then(str_lit.clone())
                .map(|q| WorkspaceCondition::Common(CommonCondition::AskUser(q)));

            let run_cond = kw("run")
                .ignore_then(str_lit.clone())
                .then(str_lit.clone().repeated().collect::<Vec<_>>())
                .map(|(command, args)| {
                    WorkspaceCondition::Common(CommonCondition::RunCommand { command, args })
                });

            let file_exists = kw("file_exists")
                .ignore_then(str_lit.clone())
                .map(|f| WorkspaceCondition::Common(CommonCondition::FileExists(f)));

            let wdc = kw("working_directory_clean").to(WorkspaceCondition::Common(
                CommonCondition::WorkingDirectoryClean,
            ));

            let git_config_equals = kw("git_config")
                .ignore_then(str_lit.clone())
                .then_ignore(sym("=="))
                .then(str_lit.clone())
                .map(|(key, value)| {
                    WorkspaceCondition::Common(CommonCondition::GitConfigEquals { key, value })
                });

            choice((ask_user, run_cond, file_exists, wdc, git_config_equals))
        };

        // Workspace-specific leaf conditions ──────────────────────────────────
        let standalone = kw("standalone").to(WorkspaceCondition::Standalone);
        let has_members = kw("has_members").to(WorkspaceCondition::HasMembers);

        let paren = cond.clone().delimited_by(sym("("), sym(")"));

        let atom = choice((common_leaf, standalone, has_members, paren));

        // `!` prefix ──────────────────────────────────────────────────────────
        let not_expr = sym("!")
            .repeated()
            .foldr(atom, |_, inner| WorkspaceCondition::Not(Box::new(inner)));

        // `&&` ────────────────────────────────────────────────────────────────
        let and_expr = not_expr.clone().foldl(
            sym("&&").ignore_then(not_expr).repeated(),
            |lhs, rhs| match lhs {
                WorkspaceCondition::And(mut cs) => {
                    cs.push(rhs);
                    WorkspaceCondition::And(cs)
                }
                other => WorkspaceCondition::And(vec![other, rhs]),
            },
        );

        // `||` ────────────────────────────────────────────────────────────────
        and_expr.clone().foldl(
            sym("||").ignore_then(and_expr).repeated(),
            |lhs, rhs| match lhs {
                WorkspaceCondition::Or(mut cs) => {
                    cs.push(rhs);
                    WorkspaceCondition::Or(cs)
                }
                other => WorkspaceCondition::Or(vec![other, rhs]),
            },
        )
    })
}

// ─── CrateCondition parser ────────────────────────────────────────────────────

/// Parses a [`CrateCondition`] expression.
///
/// Includes everything from [`common_condition_parser`] plus `type == bin|lib|proc_macro`
/// and `standalone`.
fn crate_condition_parser<'src>()
-> impl Parser<'src, &'src str, CrateCondition, extra::Err<Rich<'src, char>>> + Clone {
    recursive(|cond| {
        let str_lit = string_literal();

        // Common leaves ───────────────────────────────────────────────────────
        let ask_user = kw("ask_user")
            .ignore_then(str_lit.clone())
            .map(|q| CrateCondition::Common(CommonCondition::AskUser(q)));

        let run_cond = kw("run")
            .ignore_then(str_lit.clone())
            .then(str_lit.clone().repeated().collect::<Vec<_>>())
            .map(|(command, args)| {
                CrateCondition::Common(CommonCondition::RunCommand { command, args })
            });

        let file_exists = kw("file_exists")
            .ignore_then(str_lit.clone())
            .map(|f| CrateCondition::Common(CommonCondition::FileExists(f)));

        let wdc = kw("working_directory_clean").to(CrateCondition::Common(
            CommonCondition::WorkingDirectoryClean,
        ));

        let git_config_equals = kw("git_config")
            .ignore_then(str_lit.clone())
            .then_ignore(sym("=="))
            .then(str_lit.clone())
            .map(|(key, value)| {
                CrateCondition::Common(CommonCondition::GitConfigEquals { key, value })
            });

        // Crate-specific leaves ───────────────────────────────────────────────
        let crate_type = kw("type")
            .ignore_then(sym("=="))
            .ignore_then(choice((
                kw("bin").to(CrateTypeFilter::Bin),
                kw("lib").to(CrateTypeFilter::Lib),
                kw("proc_macro").to(CrateTypeFilter::ProcMacro),
                kw("cdylib").to(CrateTypeFilter::CDyLib),
                kw("dylib").to(CrateTypeFilter::DyLib),
                kw("rlib").to(CrateTypeFilter::RLib),
                kw("staticlib").to(CrateTypeFilter::StaticLib),
                kw("bench").to(CrateTypeFilter::Bench),
                kw("test").to(CrateTypeFilter::Test),
                kw("example").to(CrateTypeFilter::Example),
                kw("custom_build").to(CrateTypeFilter::CustomBuild),
            )))
            .map(CrateCondition::CrateType);

        let standalone = kw("standalone").to(CrateCondition::Standalone);

        let paren = cond.clone().delimited_by(sym("("), sym(")"));

        let atom = choice((
            ask_user,
            run_cond,
            file_exists,
            wdc,
            git_config_equals,
            crate_type,
            standalone,
            paren,
        ));

        // `!` prefix ──────────────────────────────────────────────────────────
        let not_expr = sym("!")
            .repeated()
            .foldr(atom, |_, inner| CrateCondition::Not(Box::new(inner)));

        // `&&` ────────────────────────────────────────────────────────────────
        let and_expr = not_expr.clone().foldl(
            sym("&&").ignore_then(not_expr).repeated(),
            |lhs, rhs| match lhs {
                CrateCondition::And(mut cs) => {
                    cs.push(rhs);
                    CrateCondition::And(cs)
                }
                other => CrateCondition::And(vec![other, rhs]),
            },
        );

        // `||` ────────────────────────────────────────────────────────────────
        and_expr.clone().foldl(
            sym("||").ignore_then(and_expr).repeated(),
            |lhs, rhs| match lhs {
                CrateCondition::Or(mut cs) => {
                    cs.push(rhs);
                    CrateCondition::Or(cs)
                }
                other => CrateCondition::Or(vec![other, rhs]),
            },
        )
    })
}

// ─── Select condition parsers ─────────────────────────────────────────────────

/// Parses a [`WorkspaceSelectCondition`] (structural workspace filters only).
fn workspace_select_condition_parser<'src>()
-> impl Parser<'src, &'src str, WorkspaceSelectCondition, extra::Err<Rich<'src, char>>> + Clone {
    recursive(|cond| {
        let standalone = kw("standalone").to(WorkspaceSelectCondition::Standalone);
        let has_members = kw("has_members").to(WorkspaceSelectCondition::HasMembers);
        let paren = cond.clone().delimited_by(sym("("), sym(")"));
        let atom = choice((standalone, has_members, paren));

        let not_expr = sym("!").repeated().foldr(atom, |_, inner| {
            WorkspaceSelectCondition::Not(Box::new(inner))
        });

        let and_expr = not_expr.clone().foldl(
            sym("&&").ignore_then(not_expr).repeated(),
            |lhs, rhs| match lhs {
                WorkspaceSelectCondition::And(mut cs) => {
                    cs.push(rhs);
                    WorkspaceSelectCondition::And(cs)
                }
                other => WorkspaceSelectCondition::And(vec![other, rhs]),
            },
        );

        and_expr.clone().foldl(
            sym("||").ignore_then(and_expr).repeated(),
            |lhs, rhs| match lhs {
                WorkspaceSelectCondition::Or(mut cs) => {
                    cs.push(rhs);
                    WorkspaceSelectCondition::Or(cs)
                }
                other => WorkspaceSelectCondition::Or(vec![other, rhs]),
            },
        )
    })
}

/// Parses a [`CrateSelectCondition`] (structural crate filters only).
fn crate_select_condition_parser<'src>()
-> impl Parser<'src, &'src str, CrateSelectCondition, extra::Err<Rich<'src, char>>> + Clone {
    recursive(|cond| {
        let standalone = kw("standalone").to(CrateSelectCondition::Standalone);
        let crate_type = kw("type")
            .ignore_then(sym("=="))
            .ignore_then(choice((
                kw("bin").to(CrateTypeFilter::Bin),
                kw("lib").to(CrateTypeFilter::Lib),
                kw("proc_macro").to(CrateTypeFilter::ProcMacro),
                kw("cdylib").to(CrateTypeFilter::CDyLib),
                kw("dylib").to(CrateTypeFilter::DyLib),
                kw("rlib").to(CrateTypeFilter::RLib),
                kw("staticlib").to(CrateTypeFilter::StaticLib),
                kw("bench").to(CrateTypeFilter::Bench),
                kw("test").to(CrateTypeFilter::Test),
                kw("example").to(CrateTypeFilter::Example),
                kw("custom_build").to(CrateTypeFilter::CustomBuild),
            )))
            .map(CrateSelectCondition::CrateType);
        let paren = cond.clone().delimited_by(sym("("), sym(")"));
        let atom = choice((standalone, crate_type, paren));

        let not_expr = sym("!")
            .repeated()
            .foldr(atom, |_, inner| CrateSelectCondition::Not(Box::new(inner)));

        let and_expr = not_expr.clone().foldl(
            sym("&&").ignore_then(not_expr).repeated(),
            |lhs, rhs| match lhs {
                CrateSelectCondition::And(mut cs) => {
                    cs.push(rhs);
                    CrateSelectCondition::And(cs)
                }
                other => CrateSelectCondition::And(vec![other, rhs]),
            },
        );

        and_expr.clone().foldl(
            sym("||").ignore_then(and_expr).repeated(),
            |lhs, rhs| match lhs {
                CrateSelectCondition::Or(mut cs) => {
                    cs.push(rhs);
                    CrateSelectCondition::Or(cs)
                }
                other => CrateSelectCondition::Or(vec![other, rhs]),
            },
        )
    })
}

// ─── Shared leaf statements ───────────────────────────────────────────────────

/// Parses a `snapshot_metadata "name";` statement into a [`SnapshotMetadataNode`].
fn snapshot_metadata_parser<'src>()
-> impl Parser<'src, &'src str, SnapshotMetadataNode, extra::Err<Rich<'src, char>>> + Clone {
    kw("snapshot_metadata")
        .ignore_then(string_literal())
        .then_ignore(sym(";"))
        .map(|name| SnapshotMetadataNode { name })
}

/// Parses a `run "cmd" "args"...;` statement into a [`RunStep`].
fn run_step_parser<'src>()
-> impl Parser<'src, &'src str, RunStep, extra::Err<Rich<'src, char>>> + Clone {
    let str_lit = string_literal();
    kw("run")
        .ignore_then(str_lit.clone())
        .then(str_lit.repeated().collect::<Vec<_>>())
        .then_ignore(sym(";"))
        .map(|(command, args)| RunStep { command, args })
}

/// Parses a `manual_step "title" "instructions";` statement into a [`ManualStepNode`].
fn manual_step_parser<'src>()
-> impl Parser<'src, &'src str, ManualStepNode, extra::Err<Rich<'src, char>>> + Clone {
    let str_lit = string_literal();
    kw("manual_step")
        .ignore_then(str_lit.clone())
        .then(str_lit)
        .then_ignore(sym(";"))
        .map(|(title, instructions)| ManualStepNode {
            title,
            instructions,
        })
}

/// Parses a [`WaitForContinueNode`] from `wait_for_continue "description";`.
fn wait_for_continue_parser<'src>()
-> impl Parser<'src, &'src str, WaitForContinueNode, extra::Err<Rich<'src, char>>> + Clone {
    kw("wait_for_continue")
        .ignore_then(string_literal())
        .then_ignore(sym(";"))
        .map(|description| WaitForContinueNode { description })
}

// ─── CrateStatement parser ────────────────────────────────────────────────────

/// Parses a [`CrateStatement`].
fn crate_statement_parser<'src>()
-> impl Parser<'src, &'src str, CrateStatement, extra::Err<Rich<'src, char>>> + Clone {
    recursive(|stmt| {
        let run = run_step_parser().map(CrateStatement::Run);
        let manual = manual_step_parser().map(CrateStatement::ManualStep);
        let snapshot_metadata = snapshot_metadata_parser().map(CrateStatement::SnapshotMetadata);
        let wait_for_continue = wait_for_continue_parser().map(CrateStatement::WaitForContinue);

        let crate_cond = crate_condition_parser();
        let body = stmt.clone().repeated().collect::<Vec<_>>();

        let if_stmt = crate_if_parser(crate_cond, body.clone()).map(CrateStatement::If);

        let with_env_file = kw("with_env_file")
            .ignore_then(string_literal())
            .then(
                stmt.repeated()
                    .collect::<Vec<_>>()
                    .delimited_by(sym("{"), sym("}")),
            )
            .map(|(env_file, statements)| {
                CrateStatement::WithEnvFile(WithEnvFileBlock {
                    env_file,
                    statements,
                })
            });

        choice((
            run,
            manual,
            if_stmt,
            with_env_file,
            snapshot_metadata,
            wait_for_continue,
        ))
    })
}

/// Builds an if/else-if/else parser for the crate context.
fn crate_if_parser<'src>(
    cond_parser: impl Parser<'src, &'src str, CrateCondition, extra::Err<Rich<'src, char>>> + Clone,
    body_parser: impl Parser<'src, &'src str, Vec<CrateStatement>, extra::Err<Rich<'src, char>>> + Clone,
) -> impl Parser<'src, &'src str, IfBlock<CrateCondition, CrateStatement>, extra::Err<Rich<'src, char>>>
+ Clone {
    let branch = kw("if")
        .ignore_then(cond_parser.clone())
        .then(body_parser.clone().delimited_by(sym("{"), sym("}")))
        .map(|(condition, statements)| Branch {
            condition,
            statements,
        });

    let else_if_branch = kw("else")
        .ignore_then(kw("if"))
        .ignore_then(cond_parser)
        .then(body_parser.clone().delimited_by(sym("{"), sym("}")))
        .map(|(condition, statements)| Branch {
            condition,
            statements,
        });

    let else_block = kw("else").ignore_then(body_parser.delimited_by(sym("{"), sym("}")));

    branch
        .then(else_if_branch.repeated().collect::<Vec<_>>())
        .then(else_block.or_not())
        .map(|((first_branch, mut extra_branches), else_stmts)| {
            let mut branches = vec![first_branch];
            branches.append(&mut extra_branches);
            IfBlock {
                branches,
                else_statements: else_stmts.unwrap_or_default(),
            }
        })
}

// ─── WorkspaceStatement parser ────────────────────────────────────────────────

/// Parses a [`WorkspaceStatement`].
fn workspace_statement_parser<'src>()
-> impl Parser<'src, &'src str, WorkspaceStatement, extra::Err<Rich<'src, char>>> + Clone {
    recursive(|stmt| {
        let run = run_step_parser().map(WorkspaceStatement::Run);
        let manual = manual_step_parser().map(WorkspaceStatement::ManualStep);
        let snapshot_metadata =
            snapshot_metadata_parser().map(WorkspaceStatement::SnapshotMetadata);
        let wait_for_continue = wait_for_continue_parser().map(WorkspaceStatement::WaitForContinue);

        let ws_cond = workspace_condition_parser();
        let ws_body = stmt.clone().repeated().collect::<Vec<_>>();

        let if_stmt = workspace_if_parser(ws_cond, ws_body.clone()).map(WorkspaceStatement::If);

        let for_crate_in_ws = kw("for")
            .ignore_then(kw("crate"))
            .ignore_then(kw("in"))
            .ignore_then(kw("workspace"))
            .ignore_then(
                crate_statement_parser()
                    .repeated()
                    .collect::<Vec<_>>()
                    .delimited_by(sym("{"), sym("}")),
            )
            .map(|statements| {
                WorkspaceStatement::ForCrateInWorkspace(ForCrateInWorkspaceBlock { statements })
            });

        let with_env_file = kw("with_env_file")
            .ignore_then(string_literal())
            .then(
                stmt.repeated()
                    .collect::<Vec<_>>()
                    .delimited_by(sym("{"), sym("}")),
            )
            .map(|(env_file, statements)| {
                WorkspaceStatement::WithEnvFile(WithEnvFileBlock {
                    env_file,
                    statements,
                })
            });

        choice((
            run,
            manual,
            if_stmt,
            for_crate_in_ws,
            with_env_file,
            snapshot_metadata,
            wait_for_continue,
        ))
    })
}

/// Builds an if/else-if/else parser for the workspace context.
fn workspace_if_parser<'src>(
    cond_parser: impl Parser<'src, &'src str, WorkspaceCondition, extra::Err<Rich<'src, char>>> + Clone,
    body_parser: impl Parser<'src, &'src str, Vec<WorkspaceStatement>, extra::Err<Rich<'src, char>>>
    + Clone,
) -> impl Parser<
    'src,
    &'src str,
    IfBlock<WorkspaceCondition, WorkspaceStatement>,
    extra::Err<Rich<'src, char>>,
> + Clone {
    let branch = kw("if")
        .ignore_then(cond_parser.clone())
        .then(body_parser.clone().delimited_by(sym("{"), sym("}")))
        .map(|(condition, statements)| Branch {
            condition,
            statements,
        });

    let else_if_branch = kw("else")
        .ignore_then(kw("if"))
        .ignore_then(cond_parser)
        .then(body_parser.clone().delimited_by(sym("{"), sym("}")))
        .map(|(condition, statements)| Branch {
            condition,
            statements,
        });

    let else_block = kw("else").ignore_then(body_parser.delimited_by(sym("{"), sym("}")));

    branch
        .then(else_if_branch.repeated().collect::<Vec<_>>())
        .then(else_block.or_not())
        .map(|((first_branch, mut extra_branches), else_stmts)| {
            let mut branches = vec![first_branch];
            branches.append(&mut extra_branches);
            IfBlock {
                branches,
                else_statements: else_stmts.unwrap_or_default(),
            }
        })
}

// ─── Top-level program parser ─────────────────────────────────────────────────

/// Builds the top-level [`Program`] parser.
fn program_parser<'src>() -> impl Parser<'src, &'src str, Program, extra::Err<Rich<'src, char>>> {
    let str_lit = string_literal();

    // `select workspaces [where <cond>];`
    let select_workspaces = kw("select")
        .ignore_then(kw("workspaces"))
        .ignore_then(
            kw("where")
                .ignore_then(workspace_select_condition_parser())
                .or_not(),
        )
        .then_ignore(sym(";"))
        .map(|condition| GlobalStatement::SelectWorkspaces(WorkspaceFilter { condition }));

    // `select crates [where <cond>];`
    let select_crates = kw("select")
        .ignore_then(kw("crates"))
        .ignore_then(
            kw("where")
                .ignore_then(crate_select_condition_parser())
                .or_not(),
        )
        .then_ignore(sym(";"))
        .map(|condition| GlobalStatement::SelectCrates(CrateFilter { condition }));

    // `for workspace { ... }`
    let for_workspace = kw("for")
        .ignore_then(kw("workspace"))
        .ignore_then(
            workspace_statement_parser()
                .repeated()
                .collect::<Vec<_>>()
                .delimited_by(sym("{"), sym("}")),
        )
        .map(|statements| GlobalStatement::ForWorkspace(ForWorkspaceBlock { statements }));

    // `for crate { ... }`
    let for_crate = kw("for")
        .ignore_then(kw("crate"))
        .ignore_then(
            crate_statement_parser()
                .repeated()
                .collect::<Vec<_>>()
                .delimited_by(sym("{"), sym("}")),
        )
        .map(|statements| GlobalStatement::ForCrate(ForCrateBlock { statements }));

    // str_lit is not used at the global statement level currently; drop explicitly.
    drop(str_lit);

    // Each alternative starts with a `kw()` call which includes leading `padded_by(padding())`,
    // so inter-statement whitespace and comments are consumed by those keyword parsers.
    // Trailing padding (after the last statement, including a trailing comment or newline) is
    // consumed by the explicit `padding()` before `end()`.
    choice((select_workspaces, select_crates, for_workspace, for_crate))
        .repeated()
        .collect::<Vec<_>>()
        .then_ignore(padding())
        .then_ignore(end())
        .map(|statements| Program { statements })
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    #![expect(
        clippy::panic,
        reason = "test helper functions use panic! on unexpected failures"
    )]
    #![expect(
        clippy::indexing_slicing,
        reason = "test code indexes known positions in parsed program structures"
    )]
    #![expect(
        clippy::unwrap_used,
        reason = "test code uses unwrap_err() to extract error values for assertions"
    )]

    use pretty_assertions::assert_eq;

    use super::*;

    /// Convenience: parse a program and panic on error (for tests only).
    fn parse_ok(src: &str) -> Program {
        parse(src, "<test>").unwrap_or_else(|errors| {
            panic!(
                "parse error:\n{}",
                errors
                    .iter()
                    .map(|e| e.as_str())
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        })
    }

    #[test]
    fn empty_program_parses() {
        assert_eq!(parse_ok(""), Program { statements: vec![] });
    }

    #[test]
    fn comment_only_program_parses() {
        assert_eq!(
            parse_ok("// just a comment\n"),
            Program { statements: vec![] }
        );
    }

    #[test]
    fn select_workspaces_all() {
        let prog = parse_ok("select workspaces;");
        assert_eq!(
            prog.statements,
            vec![GlobalStatement::SelectWorkspaces(WorkspaceFilter {
                condition: None
            })]
        );
    }

    #[test]
    fn select_workspaces_where_standalone() {
        let prog = parse_ok("select workspaces where standalone;");
        assert_eq!(
            prog.statements,
            vec![GlobalStatement::SelectWorkspaces(WorkspaceFilter {
                condition: Some(WorkspaceSelectCondition::Standalone)
            })]
        );
    }

    #[test]
    fn select_workspaces_where_not_standalone() {
        let prog = parse_ok("select workspaces where !standalone;");
        assert_eq!(
            prog.statements,
            vec![GlobalStatement::SelectWorkspaces(WorkspaceFilter {
                condition: Some(WorkspaceSelectCondition::Not(Box::new(
                    WorkspaceSelectCondition::Standalone
                )))
            })]
        );
    }

    #[test]
    fn select_crates_all() {
        let prog = parse_ok("select crates;");
        assert_eq!(
            prog.statements,
            vec![GlobalStatement::SelectCrates(CrateFilter {
                condition: None
            })]
        );
    }

    #[test]
    fn select_crates_where_lib() {
        let prog = parse_ok("select crates where type == lib;");
        assert_eq!(
            prog.statements,
            vec![GlobalStatement::SelectCrates(CrateFilter {
                condition: Some(CrateSelectCondition::CrateType(CrateTypeFilter::Lib))
            })]
        );
    }

    #[test]
    fn for_workspace_with_run() {
        let prog = parse_ok(r#"for workspace { run "cargo" "check"; }"#);
        assert_eq!(
            prog.statements,
            vec![GlobalStatement::ForWorkspace(ForWorkspaceBlock {
                statements: vec![WorkspaceStatement::Run(RunStep {
                    command: "cargo".to_owned(),
                    args: vec!["check".to_owned()],
                })]
            })]
        );
    }

    #[test]
    fn for_workspace_with_manual_step() {
        let prog = parse_ok(r#"for workspace { manual_step "Review" "Check the output."; }"#);
        assert_eq!(
            prog.statements,
            vec![GlobalStatement::ForWorkspace(ForWorkspaceBlock {
                statements: vec![WorkspaceStatement::ManualStep(ManualStepNode {
                    title: "Review".to_owned(),
                    instructions: "Check the output.".to_owned(),
                })]
            })]
        );
    }

    #[test]
    fn for_crate_in_workspace() {
        let prog =
            parse_ok(r#"for workspace { for crate in workspace { run "cargo" "publish"; } }"#);
        assert_eq!(
            prog.statements,
            vec![GlobalStatement::ForWorkspace(ForWorkspaceBlock {
                statements: vec![WorkspaceStatement::ForCrateInWorkspace(
                    ForCrateInWorkspaceBlock {
                        statements: vec![CrateStatement::Run(RunStep {
                            command: "cargo".to_owned(),
                            args: vec!["publish".to_owned()],
                        })]
                    }
                )]
            })]
        );
    }

    #[test]
    fn for_crate_global() {
        let prog = parse_ok(r#"for crate { run "cargo" "clippy"; }"#);
        assert_eq!(
            prog.statements,
            vec![GlobalStatement::ForCrate(ForCrateBlock {
                statements: vec![CrateStatement::Run(RunStep {
                    command: "cargo".to_owned(),
                    args: vec!["clippy".to_owned()],
                })]
            })]
        );
    }

    #[test]
    fn crate_if_type_lib() {
        let prog = parse_ok(r#"for crate { if type == lib { run "cargo" "publish"; } }"#);
        assert_eq!(
            prog.statements,
            vec![GlobalStatement::ForCrate(ForCrateBlock {
                statements: vec![CrateStatement::If(IfBlock {
                    branches: vec![Branch {
                        condition: CrateCondition::CrateType(CrateTypeFilter::Lib),
                        statements: vec![CrateStatement::Run(RunStep {
                            command: "cargo".to_owned(),
                            args: vec!["publish".to_owned()],
                        })],
                    }],
                    else_statements: vec![],
                })]
            })]
        );
    }

    #[test]
    fn workspace_if_with_else() {
        let prog = parse_ok(
            r#"for workspace {
                if working_directory_clean {
                    run "cargo" "release";
                } else {
                    manual_step "Fix it" "Commit your changes first.";
                }
            }"#,
        );
        assert_eq!(
            prog.statements,
            vec![GlobalStatement::ForWorkspace(ForWorkspaceBlock {
                statements: vec![WorkspaceStatement::If(IfBlock {
                    branches: vec![Branch {
                        condition: WorkspaceCondition::Common(
                            CommonCondition::WorkingDirectoryClean
                        ),
                        statements: vec![WorkspaceStatement::Run(RunStep {
                            command: "cargo".to_owned(),
                            args: vec!["release".to_owned()],
                        })],
                    }],
                    else_statements: vec![WorkspaceStatement::ManualStep(ManualStepNode {
                        title: "Fix it".to_owned(),
                        instructions: "Commit your changes first.".to_owned(),
                    })],
                })]
            })]
        );
    }

    #[test]
    fn and_condition_flattens() {
        let prog = parse_ok(
            r#"for crate {
                if type == bin && type == lib { run "x"; }
            }"#,
        );
        match &prog.statements[0] {
            GlobalStatement::ForCrate(b) => match &b.statements[0] {
                CrateStatement::If(ib) => {
                    assert_eq!(ib.branches.len(), 1);
                    assert!(
                        matches!(&ib.branches[0].condition, CrateCondition::And(v) if v.len() == 2)
                    );
                }
                _ => panic!("expected If"),
            },
            _ => panic!("expected ForCrate"),
        }
    }

    #[test]
    fn not_condition() {
        let prog = parse_ok(r#"for crate { if !standalone { run "cargo" "publish"; } }"#);
        match &prog.statements[0] {
            GlobalStatement::ForCrate(b) => match &b.statements[0] {
                CrateStatement::If(ib) => {
                    assert!(matches!(&ib.branches[0].condition, CrateCondition::Not(_)));
                }
                _ => panic!("expected If"),
            },
            _ => panic!("expected ForCrate"),
        }
    }

    #[test]
    fn string_with_escaped_quote() {
        let prog = parse_ok(r#"for workspace { manual_step "Title" "Say \"hello\"."; }"#);
        match &prog.statements[0] {
            GlobalStatement::ForWorkspace(b) => match &b.statements[0] {
                WorkspaceStatement::ManualStep(m) => {
                    assert_eq!(m.instructions, r#"Say "hello"."#);
                }
                _ => panic!("expected ManualStep"),
            },
            _ => panic!("expected ForWorkspace"),
        }
    }

    #[test]
    fn snapshot_metadata_in_crate_context() {
        let prog = parse_ok(r#"for crate { snapshot_metadata "before"; }"#);
        assert_eq!(
            prog.statements,
            vec![GlobalStatement::ForCrate(ForCrateBlock {
                statements: vec![CrateStatement::SnapshotMetadata(SnapshotMetadataNode {
                    name: "before".to_owned(),
                })]
            })]
        );
    }

    #[test]
    fn snapshot_metadata_in_workspace_context() {
        let prog = parse_ok(r#"for workspace { snapshot_metadata "after"; }"#);
        assert_eq!(
            prog.statements,
            vec![GlobalStatement::ForWorkspace(ForWorkspaceBlock {
                statements: vec![WorkspaceStatement::SnapshotMetadata(SnapshotMetadataNode {
                    name: "after".to_owned(),
                })]
            })]
        );
    }

    #[test]
    fn snapshot_metadata_before_and_after_in_loop() {
        let prog = parse_ok(
            r#"for workspace { for crate in workspace { snapshot_metadata "before"; run "cargo" "upgrade"; snapshot_metadata "after"; } }"#,
        );
        match &prog.statements[0] {
            GlobalStatement::ForWorkspace(b) => match &b.statements[0] {
                WorkspaceStatement::ForCrateInWorkspace(fc) => {
                    assert!(matches!(
                        &fc.statements[0],
                        CrateStatement::SnapshotMetadata(n) if n.name == "before"
                    ));
                    assert!(matches!(&fc.statements[1], CrateStatement::Run(_)));
                    assert!(matches!(
                        &fc.statements[2],
                        CrateStatement::SnapshotMetadata(n) if n.name == "after"
                    ));
                }
                _ => panic!("expected ForCrateInWorkspace"),
            },
            _ => panic!("expected ForWorkspace"),
        }
    }

    #[test]
    fn parse_error_reported() {
        let result = parse("select garbage;", "<test>");
        let errors = result.unwrap_err();
        assert!(!errors.is_empty());
        assert!(!errors[0].as_str().is_empty());
    }

    #[test]
    fn complex_program_roundtrip() {
        let src = r#"
// Select all non-standalone workspaces.
select workspaces where !standalone;

// For each workspace, check and optionally publish member crates.
for workspace {
    run "cargo" "check";
    if working_directory_clean {
        for crate in workspace {
            if type == lib {
                run "cargo" "publish";
            }
        }
    }
}

// Also lint all standalone crates.
select crates where standalone;
for crate {
    run "cargo" "clippy" "--" "-D" "warnings";
}
"#;
        let prog = parse_ok(src);
        assert_eq!(prog.statements.len(), 4);
    }

    #[test]
    fn with_env_file_in_crate_context() {
        let prog = parse_ok(r#"for crate { with_env_file ".env" { run "cargo" "build"; } }"#);
        let GlobalStatement::ForCrate(block) = &prog.statements[0] else {
            panic!("expected ForCrate");
        };
        let CrateStatement::WithEnvFile(env_block) = &block.statements[0] else {
            panic!("expected WithEnvFile");
        };
        assert_eq!(env_block.env_file, ".env");
        assert_eq!(env_block.statements.len(), 1);
        let CrateStatement::Run(run) = &env_block.statements[0] else {
            panic!("expected Run");
        };
        assert_eq!(run.command, "cargo");
        assert_eq!(run.args, vec!["build"]);
    }

    #[test]
    fn with_env_file_in_workspace_context() {
        let prog =
            parse_ok(r#"for workspace { with_env_file "secrets.env" { run "deploy" "prod"; } }"#);
        let GlobalStatement::ForWorkspace(block) = &prog.statements[0] else {
            panic!("expected ForWorkspace");
        };
        let WorkspaceStatement::WithEnvFile(env_block) = &block.statements[0] else {
            panic!("expected WithEnvFile");
        };
        assert_eq!(env_block.env_file, "secrets.env");
        assert_eq!(env_block.statements.len(), 1);
    }

    #[test]
    fn nested_with_env_file_blocks() {
        let prog = parse_ok(
            r#"for crate { with_env_file "outer.env" { with_env_file "inner.env" { run "test" "run"; } } }"#,
        );
        let GlobalStatement::ForCrate(block) = &prog.statements[0] else {
            panic!("expected ForCrate");
        };
        let CrateStatement::WithEnvFile(outer) = &block.statements[0] else {
            panic!("expected outer WithEnvFile");
        };
        assert_eq!(outer.env_file, "outer.env");
        let CrateStatement::WithEnvFile(inner) = &outer.statements[0] else {
            panic!("expected inner WithEnvFile");
        };
        assert_eq!(inner.env_file, "inner.env");
    }

    #[test]
    fn with_env_file_with_if_inside() {
        let prog = parse_ok(
            r#"for crate { with_env_file ".env" { if file_exists "check" { run "cmd"; } } }"#,
        );
        let GlobalStatement::ForCrate(block) = &prog.statements[0] else {
            panic!("expected ForCrate");
        };
        let CrateStatement::WithEnvFile(env_block) = &block.statements[0] else {
            panic!("expected WithEnvFile");
        };
        assert_eq!(env_block.statements.len(), 1);
        assert!(matches!(env_block.statements[0], CrateStatement::If(_)));
    }
}
