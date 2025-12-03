use cow_utils::CowUtils;
use oxc_diagnostics::OxcDiagnostic;
use oxc_macros::declare_oxc_lint;
use oxc_span::Span;

use crate::{context::LintContext, rule::Rule};

fn using_eslint_disable_comment(span: Span) -> OxcDiagnostic {
    OxcDiagnostic::warn("Detected eslint disable comment")
        .with_help("Prefer oxlint-disable instead of eslint-disable")
        .with_label(span)
}

fn using_eslint_disable_next_line_comment(span: Span) -> OxcDiagnostic {
    OxcDiagnostic::warn("Detected eslint disable comment")
        .with_help("Prefer oxlint-disable-next-line instead of eslint-disable-next-line")
        .with_label(span)
}

#[derive(Debug, Default, Clone)]
pub struct NoEslintDisableComments;

declare_oxc_lint!(
    /// ### What it does
    ///
    /// TBA
    /// ```
    NoEslintDisableComments,
    oxc,
    style,
    suggestion
);

impl Rule for NoEslintDisableComments {
    fn run_once(&self, ctx: &LintContext) {
        let comments = ctx.comments();
        for comment in comments {
            let raw_comment = ctx.source_range(comment.content_span());

            if let Some(directive) = find_eslint_comment_directive(raw_comment, comment.is_line()) {
                match directive {
                    "disable" => ctx.diagnostic_with_suggestion(
                        using_eslint_disable_comment(comment.content_span()),
                        |fixer| {
                            fixer.replace(
                                comment.content_span(),
                                raw_comment
                                    .cow_replace("eslint-disable", "oxlint-disable")
                                    .into_owned(),
                            )
                        },
                    ),
                    "disable-next-line" => ctx.diagnostic_with_suggestion(
                        using_eslint_disable_next_line_comment(comment.content_span()),
                        |fixer| {
                            fixer.replace(
                                comment.content_span(),
                                raw_comment
                                    .cow_replace(
                                        "eslint-disable-next-line",
                                        "oxlint-disable-next-line",
                                    )
                                    .into_owned(),
                            )
                        },
                    ),
                    _ => {}
                }
            }
        }
    }
}

pub fn find_eslint_comment_directive(raw: &str, single_line: bool) -> Option<&str> {
    let prefix = "eslint-";

    let mut last_line_start = None;
    let mut char_indices = raw.char_indices().peekable();
    while let Some((_, c)) = char_indices.next() {
        if c == '\n' {
            last_line_start = char_indices.peek().map(|(i, _)| *i);
        }
    }

    let multi_len = last_line_start.unwrap_or(0);
    let line = &raw[multi_len..];

    let index = line.find(prefix)?;
    if !line[..index]
        .chars()
        .all(|c| c.is_whitespace() || if single_line { c == '/' } else { c == '*' || c == '/' })
    {
        return None;
    }

    let start = index + prefix.len();

    for directive in ["disable", "disable-next-line"] {
        if line.get(start..start + directive.len()) == Some(directive) {
            let start = multi_len + index + prefix.len();
            let end = start + directive.len();
            let directive = &raw[start..end];

            debug_assert!(
                matches!(directive, "disable" | "disable-next-line"),
                "Expected one of disable/disable-next-line, got {directive}",
            );

            return Some(directive);
        }
    }

    None
}

#[test]
fn test() {
    use crate::tester::Tester;

    let pass = vec![
        ("function foo() { const a = 2 }", None),
        (
            "// oxlint-disable
            f();
            function f() {}",
            None,
        ),
        (
            "/* oxlint-disable */
            f();
            function f() {}",
            None,
        ),
        (
            "/* oxlint-disable no-use-before-define */
            f();
            function f() {}",
            None,
        ),
        (
            "// oxlint-disable no-use-before-define
            f();
            function f() {}",
            None,
        ),
        (
            "/* oxlint-disable-next-line */
            f();
            function f() {}",
            None,
        ),
        (
            "// oxlint-disable no-use-before-define
            f();
            function f() {}",
            None,
        ),
    ];

    let fail = vec![
        (
            "// eslint-disable
            f();
            function f() {}",
            None,
        ),
        (
            "/* eslint-disable */
            f();
            function f() {}",
            None,
        ),
        (
            "// eslint-disable no-use-before-define
            f();
            function f() {}",
            None,
        ),
        (
            "/* eslint-disable no-use-before-define */
            f();
            function f() {}",
            None,
        ),
        (
            "/* eslint-disable-next-line */
            f();
            function f() {}",
            None,
        ),
        (
            "// eslint-disable-next-line
            f();
            function f() {}",
            None,
        ),
    ];

    let fix = vec![
        (
            "// eslint-disable
            f();
            function f() {}",
            "// oxlint-disable
            f();
            function f() {}",
        ),
        (
            "/* eslint-disable */
            f();
            function f() {}",
            "/* oxlint-disable */
            f();
            function f() {}",
        ),
        (
            "// eslint-disable no-use-before-define
            f();
            function f() {}",
            "// oxlint-disable no-use-before-define
            f();
            function f() {}",
        ),
        (
            "/* eslint-disable no-use-before-define */
            f();
            function f() {}",
            "/* oxlint-disable no-use-before-define */
            f();
            function f() {}",
        ),
        (
            "/* eslint-disable-next-line */
            f();
            function f() {}",
            "/* oxlint-disable-next-line */
            f();
            function f() {}",
        ),
        (
            "// eslint-disable-next-line
            f();
            function f() {}",
            "// oxlint-disable-next-line
            f();
            function f() {}",
        ),
    ];

    Tester::new(NoEslintDisableComments::NAME, NoEslintDisableComments::PLUGIN, pass, fail)
        .expect_fix(fix)
        .test_and_snapshot();
}
