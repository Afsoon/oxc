use oxc_ast::{
    AstKind,
    ast::{Argument, BindingPattern, CallExpression, Expression, FunctionBody, Statement},
};
use oxc_ast_visit::Visit;
use oxc_diagnostics::OxcDiagnostic;
use oxc_macros::declare_oxc_lint;
use oxc_semantic::{NodeId, ScopeId};
use oxc_span::{GetSpan, Span};
use oxc_str::CompactStr;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::{
    context::LintContext,
    fixer::RuleFixer,
    rule::{DefaultRuleConfig, Rule},
    utils::{
        JestFnKind, JestGeneralFnKind, PossibleJestNode, collect_possible_jest_call_node,
        get_node_name, parse_general_jest_fn_call,
    },
};

enum ExpectSource<'a> {
    /// Global or import — borrows the file-level prefix (e.g. `"expect"` or `"e"`).
    Global(&'a str),
    /// Vitest fixture arg — owns the prefix (e.g. `"myExpect"`, `"ctx.expect"`).
    Fixture(CompactStr),
}

impl ExpectSource<'_> {
    fn prefix(&self) -> &str {
        match self {
            Self::Global(s) => s,
            Self::Fixture(s) => s.as_str(),
        }
    }

    fn is_shadowed_in(&self, callback: &Expression<'_>, ctx: &LintContext<'_>) -> bool {
        matches!(self, Self::Global(_))
            && callback_scope_id(callback)
                .is_some_and(|id| ctx.scoping().get_binding(id, "expect".into()).is_some())
    }
}

fn has_assertions_takes_no_arguments(span: Span, prefix: &str) -> OxcDiagnostic {
    OxcDiagnostic::warn(format!("`{prefix}.hasAssertions` expects no arguments."))
        .with_help(format!("Remove the arguments from `{prefix}.hasAssertions()`."))
        .with_label(span)
}

fn assertions_requires_one_argument(span: Span, prefix: &str) -> OxcDiagnostic {
    OxcDiagnostic::warn(format!("`{prefix}.assertions` expects a single argument of type number."))
        .with_help(format!("Pass a single numeric argument to `{prefix}.assertions()`."))
        .with_label(span)
}

fn assertions_requires_number_argument(span: Span) -> OxcDiagnostic {
    OxcDiagnostic::warn("This argument should be a number.")
        .with_help("Replace this argument with a numeric literal.")
        .with_label(span)
}

fn have_expect_assertions(span: Span, prefix: &str) -> OxcDiagnostic {
    OxcDiagnostic::warn(format!(
        "Every test should have either `{prefix}.assertions(<number of assertions>)` or `{prefix}.hasAssertions()` as its first expression.",
    ))
    .with_help(format!("Add `{prefix}.hasAssertions()` or `{prefix}.assertions(<number>)` as the first statement in the test."))
    .with_label(span)
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct PreferExpectAssertions(Box<PreferExpectAssertionsConfig>);

impl std::ops::Deref for PreferExpectAssertions {
    type Target = PreferExpectAssertionsConfig;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", default)]
pub struct PreferExpectAssertionsConfig {
    only_functions_with_async_keyword: bool,
    only_functions_with_expect_in_callback: bool,
    only_functions_with_expect_in_loop: bool,
}

// See <https://github.com/oxc-project/oxc/issues/6050> for documentation details.
declare_oxc_lint!(
    /// ### What it does
    ///
    /// FIXME: Briefly describe the rule's purpose.
    ///
    /// ### Why is this bad?
    ///
    /// FIXME: Explain why violating this rule is problematic.
    ///
    /// ### Examples
    ///
    /// Examples of **incorrect** code for this rule:
    /// ```js
    /// FIXME: Add at least one example of code that violates the rule.
    /// ```
    ///
    /// Examples of **correct** code for this rule:
    /// ```js
    /// FIXME: Add at least one example of code that is allowed with the rule.
    /// ```
    PreferExpectAssertions,
    vitest,
    nursery,
    suggestion,
    version = "next",
);

impl Rule for PreferExpectAssertions {
    fn from_configuration(value: serde_json::Value) -> Result<Self, serde_json::error::Error> {
        serde_json::from_value::<DefaultRuleConfig<PreferExpectAssertionsConfig>>(value)
            .map(|c| Self(Box::new(c.into_inner())))
    }

    fn run_once(&self, ctx: &LintContext) {
        let mut possible_jest_nodes = collect_possible_jest_call_node(ctx);
        possible_jest_nodes.sort_unstable_by_key(|n| n.node.id());

        // Resolve the file-level expect local name once (e.g. `"expect"` or `"e"`
        // for `import { expect as e }`). Per-callback vitest fixture overrides
        // are handled in `resolve_expect_source`.
        let file_expect_prefix = resolve_expect_local_name(ctx);

        let mut covered_describe_ids: Vec<NodeId> = Vec::new();

        for jest_node in &possible_jest_nodes {
            self.check_node(jest_node, &file_expect_prefix, &mut covered_describe_ids, ctx);
        }
    }
}

impl PreferExpectAssertions {
    fn check_node<'a>(
        &self,
        jest_node: &PossibleJestNode<'a, '_>,
        file_expect_prefix: &CompactStr,
        covered_describe_ids: &mut Vec<NodeId>,
        ctx: &LintContext<'a>,
    ) {
        let node = jest_node.node;
        let AstKind::CallExpression(call_expr) = node.kind() else {
            return;
        };

        let Some(general) = parse_general_jest_fn_call(call_expr, jest_node, ctx) else {
            return;
        };

        let Some(kind) = general.kind.to_general() else {
            return;
        };

        match kind {
            JestGeneralFnKind::Hook => {
                if general.name.ends_with("Each") {
                    Self::check_each_hook(
                        call_expr,
                        node.id(),
                        file_expect_prefix,
                        covered_describe_ids,
                        ctx,
                    );
                }
            }
            JestGeneralFnKind::Test => {
                self.check_test(
                    call_expr,
                    node.id(),
                    file_expect_prefix,
                    covered_describe_ids,
                    ctx,
                );
            }
            _ => {}
        }
    }

    fn check_each_hook(
        call_expr: &CallExpression<'_>,
        hook_node_id: NodeId,
        file_expect_prefix: &CompactStr,
        covered_describe_ids: &mut Vec<NodeId>,
        ctx: &LintContext<'_>,
    ) {
        let Some(body) = find_test_callback(call_expr).and_then(callback_body) else {
            return;
        };

        let mut scanner = HookScanner::new(file_expect_prefix);
        scanner.visit_function_body(body);

        if !scanner.has_expect_has_assertions {
            return;
        }

        if let Some(args_span) = scanner.has_assertions_invalid_args_span {
            let call_span = scanner.has_assertions_call_span.unwrap();
            let delete_span = Span::new(args_span.start, call_span.end - 1);
            let fixer = RuleFixer::new(FixKind::Suggestion, ctx);
            let suggestion = fixer.delete_range(delete_span).with_message("Remove extra arguments");
            ctx.diagnostic_with_suggestions(
                has_assertions_takes_no_arguments(args_span, file_expect_prefix),
                [suggestion],
            );
        }

        // Find the nearest ancestor describe that contains this hook.
        // If no describe parent exists, use ROOT to indicate top-level coverage.
        let parent_describe_id = ctx
            .nodes()
            .ancestors(hook_node_id)
            .find(|n| matches!(n.kind(), AstKind::CallExpression(c) if is_describe_call(c)))
            .map_or(NodeId::ROOT, oxc_semantic::AstNode::id);

        if !covered_describe_ids.contains(&parent_describe_id) {
            covered_describe_ids.push(parent_describe_id);
        }
    }

    fn check_test<'a>(
        &self,
        call_expr: &'a CallExpression<'a>,
        test_node_id: NodeId,
        file_expect_prefix: &CompactStr,
        covered_describe_ids: &[NodeId],
        ctx: &LintContext<'a>,
    ) {
        if call_expr.arguments.len() < 2 {
            return;
        }

        let Some(callback) = find_test_callback(call_expr) else {
            return;
        };

        let Some(body) = callback_body(callback) else {
            return;
        };

        if is_covered_by_hook(test_node_id, covered_describe_ids, ctx) {
            return;
        }

        let expect_source = resolve_expect_source(callback, file_expect_prefix.as_str(), ctx);

        if self.has_options()
            && !self.should_check(body, is_async_callback(callback), expect_source.prefix())
        {
            return;
        }

        if Self::check_first_statement(body, &expect_source, ctx) {
            return;
        }

        let prefix = expect_source.prefix();
        let insert_pos = Span::new(body.span.start + 1, body.span.start + 1);
        let fixer = RuleFixer::new(FixKind::Suggestion, ctx);
        let suggestions = [
            fixer
                .insert_text_before_range(insert_pos, format!("{prefix}.hasAssertions();"))
                .with_message(format!("Add `{prefix}.hasAssertions()`")),
            fixer
                .insert_text_before_range(insert_pos, format!("{prefix}.assertions();"))
                .with_message(format!("Add `{prefix}.assertions(<number of assertions>)`")),
        ];

        ctx.diagnostic_with_suggestions(
            have_expect_assertions(call_expr.span, prefix),
            suggestions,
        );
    }

    fn has_options(&self) -> bool {
        self.only_functions_with_async_keyword
            || self.only_functions_with_expect_in_callback
            || self.only_functions_with_expect_in_loop
    }

    fn check_first_statement(
        body: &FunctionBody<'_>,
        expect_source: &ExpectSource<'_>,
        ctx: &LintContext<'_>,
    ) -> bool {
        let Some(Statement::ExpressionStatement(first_expr_stmt)) = body.statements.first() else {
            return false;
        };

        let Expression::CallExpression(first_call) = &first_expr_stmt.expression else {
            return false;
        };

        let name = get_node_name(&first_call.callee);
        let prefix = expect_source.prefix();

        if name.ends_with("hasAssertions") {
            validate_has_assertions_args(first_call, prefix, ctx);
            true
        } else if name.ends_with("assertions") {
            validate_assertions_args(first_call, prefix, ctx);
            true
        } else {
            false
        }
    }

    fn should_check(&self, body: &FunctionBody<'_>, is_async: bool, prefix: &str) -> bool {
        if self.only_functions_with_async_keyword && is_async {
            return true;
        }

        if !self.only_functions_with_expect_in_callback && !self.only_functions_with_expect_in_loop
        {
            return false;
        }

        let mut scanner = BodyScanner::new(prefix);
        scanner.visit_function_body(body);

        let has_callback =
            self.only_functions_with_expect_in_callback && scanner.has_expect_in_callback;
        let has_loop = self.only_functions_with_expect_in_loop && scanner.has_expect_in_loop;

        has_callback || has_loop
    }
}

fn validate_has_assertions_args(call: &CallExpression<'_>, prefix: &str, ctx: &LintContext<'_>) {
    if call.arguments.is_empty() {
        return;
    }
    if let Some(args_span) = call.arguments_span() {
        let delete_span = Span::new(args_span.start, call.span.end - 1);
        let fixer = RuleFixer::new(FixKind::Suggestion, ctx);
        let suggestion = fixer.delete_range(delete_span).with_message("Remove extra arguments");
        ctx.diagnostic_with_suggestions(
            has_assertions_takes_no_arguments(args_span, prefix),
            [suggestion],
        );
    }
}

fn validate_assertions_args(call: &CallExpression<'_>, prefix: &str, ctx: &LintContext<'_>) {
    match call.arguments.len() {
        0 => {
            ctx.diagnostic(assertions_requires_one_argument(call.callee.span(), prefix));
        }
        1 => {
            let arg = &call.arguments[0];
            if !matches!(arg, Argument::NumericLiteral(_)) {
                ctx.diagnostic(assertions_requires_number_argument(arg.span()));
            }
        }
        _ => {
            let extra_start = call.arguments[0].span().end;
            let extra_end = call.span.end - 1;
            let extra_span = Span::new(extra_start, extra_end);
            let fixer = RuleFixer::new(FixKind::Suggestion, ctx);
            let suggestion = fixer.delete_range(extra_span).with_message("Remove extra arguments");
            ctx.diagnostic_with_suggestions(
                assertions_requires_one_argument(extra_span, prefix),
                [suggestion],
            );
        }
    }
}

struct HookScanner {
    /// The expected callee name, e.g. `"expect.hasAssertions"` or `"e.hasAssertions"`.
    expected_name: CompactStr,
    has_expect_has_assertions: bool,
    has_assertions_invalid_args_span: Option<Span>,
    has_assertions_call_span: Option<Span>,
}

impl HookScanner {
    fn new(prefix: &str) -> Self {
        Self {
            expected_name: CompactStr::from(format!("{prefix}.hasAssertions")),
            has_expect_has_assertions: false,
            has_assertions_invalid_args_span: None,
            has_assertions_call_span: None,
        }
    }
}

impl<'a> Visit<'a> for HookScanner {
    fn visit_call_expression(&mut self, call_expr: &CallExpression<'a>) {
        if get_node_name(&call_expr.callee) == self.expected_name.as_str() {
            self.has_expect_has_assertions = true;
            if !call_expr.arguments.is_empty() {
                self.has_assertions_invalid_args_span = call_expr.arguments_span();
                self.has_assertions_call_span = Some(call_expr.span);
            }
        }
        oxc_ast_visit::walk::walk_call_expression(self, call_expr);
    }
}

struct BodyScanner {
    /// The expect prefix to match (e.g. `"expect"`, `"e"`, `"ctx.expect"`).
    prefix: CompactStr,
    /// Precomputed `"prefix."` for starts_with checks, avoiding allocation per call.
    prefix_dot: CompactStr,
    expression_depth: i32,
    in_loop: bool,
    has_expect_in_callback: bool,
    has_expect_in_loop: bool,
}

impl BodyScanner {
    fn new(prefix: &str) -> Self {
        Self {
            prefix: CompactStr::from(prefix),
            prefix_dot: CompactStr::from(format!("{prefix}.")),
            expression_depth: -1,
            in_loop: false,
            has_expect_in_callback: false,
            has_expect_in_loop: false,
        }
    }

    fn visit_loop(&mut self, walk: impl FnOnce(&mut Self)) {
        let was = self.in_loop;
        self.in_loop = true;
        walk(self);
        self.in_loop = was;
    }

    fn is_expect_call(&self, call_expr: &CallExpression<'_>) -> bool {
        let name = get_node_name(&call_expr.callee);
        name == self.prefix.as_str() || name.starts_with(self.prefix_dot.as_str())
    }
}

impl<'a> Visit<'a> for BodyScanner {
    fn visit_call_expression(&mut self, call_expr: &CallExpression<'a>) {
        if self.is_expect_call(call_expr) {
            if self.expression_depth > 0 {
                self.has_expect_in_callback = true;
            }
            if self.in_loop {
                self.has_expect_in_loop = true;
            }
        }
        oxc_ast_visit::walk::walk_call_expression(self, call_expr);
    }

    fn visit_function_body(&mut self, body: &FunctionBody<'a>) {
        self.expression_depth += 1;
        oxc_ast_visit::walk::walk_function_body(self, body);
        self.expression_depth -= 1;
    }

    fn visit_for_statement(&mut self, it: &oxc_ast::ast::ForStatement<'a>) {
        self.visit_loop(|s| oxc_ast_visit::walk::walk_for_statement(s, it));
    }
    fn visit_for_in_statement(&mut self, it: &oxc_ast::ast::ForInStatement<'a>) {
        self.visit_loop(|s| oxc_ast_visit::walk::walk_for_in_statement(s, it));
    }
    fn visit_for_of_statement(&mut self, it: &oxc_ast::ast::ForOfStatement<'a>) {
        self.visit_loop(|s| oxc_ast_visit::walk::walk_for_of_statement(s, it));
    }
    fn visit_while_statement(&mut self, it: &oxc_ast::ast::WhileStatement<'a>) {
        self.visit_loop(|s| oxc_ast_visit::walk::walk_while_statement(s, it));
    }
    fn visit_do_while_statement(&mut self, it: &oxc_ast::ast::DoWhileStatement<'a>) {
        self.visit_loop(|s| oxc_ast_visit::walk::walk_do_while_statement(s, it));
    }
}

fn is_covered_by_hook(
    test_node_id: NodeId,
    covered_describe_ids: &[NodeId],
    ctx: &LintContext<'_>,
) -> bool {
    if covered_describe_ids.is_empty() {
        return false;
    }
    if covered_describe_ids.contains(&NodeId::ROOT) {
        return true;
    }
    ctx.nodes().ancestors(test_node_id).any(|ancestor| {
        matches!(ancestor.kind(), AstKind::CallExpression(c) if is_describe_call(c))
            && covered_describe_ids.contains(&ancestor.id())
    })
}

fn is_describe_call(call_expr: &CallExpression<'_>) -> bool {
    let callee_name = match &call_expr.callee {
        Expression::Identifier(ident) => ident.name.as_str(),
        Expression::StaticMemberExpression(member) => {
            member.object.get_identifier_reference().map_or("", |id| id.name.as_str())
        }
        Expression::TaggedTemplateExpression(tagged) => match &tagged.tag {
            Expression::StaticMemberExpression(member) => {
                member.object.get_identifier_reference().map_or("", |id| id.name.as_str())
            }
            _ => "",
        },
        _ => "",
    };

    JestFnKind::from(callee_name)
        .to_general()
        .is_some_and(|jest_kind| matches!(jest_kind, JestGeneralFnKind::Describe))
}

fn callback_scope_id(callback: &Expression<'_>) -> Option<ScopeId> {
    match callback {
        Expression::FunctionExpression(func) => func.scope_id.get(),
        Expression::ArrowFunctionExpression(func) => func.scope_id.get(),
        _ => None,
    }
}

fn find_test_callback<'a>(call_expr: &'a CallExpression<'a>) -> Option<&'a Expression<'a>> {
    call_expr.arguments.iter().rev().filter_map(|arg| arg.as_expression()).find(|expr| {
        matches!(expr, Expression::FunctionExpression(_) | Expression::ArrowFunctionExpression(_))
    })
}

fn callback_body<'a>(callback: &'a Expression<'a>) -> Option<&'a FunctionBody<'a>> {
    match callback {
        Expression::FunctionExpression(func) => func.body.as_ref().map(AsRef::as_ref),
        Expression::ArrowFunctionExpression(func) => Some(&func.body),
        _ => None,
    }
}

fn resolve_expect_source<'p>(
    callback: &Expression<'_>,
    file_expect_prefix: &'p str,
    ctx: &LintContext<'_>,
) -> ExpectSource<'p> {
    if let Some(source) = resolve_expect_from_fixture_param(callback, ctx.frameworks().is_vitest())
    {
        return source;
    }

    ExpectSource::Global(file_expect_prefix)
}

fn resolve_expect_from_fixture_param<'p>(
    callback: &Expression<'_>,
    is_vitest: bool,
) -> Option<ExpectSource<'p>> {
    if !is_vitest {
        return None;
    }

    let params = match callback {
        Expression::FunctionExpression(func) => &func.params,
        Expression::ArrowFunctionExpression(func) => &func.params,
        _ => return None,
    };

    let first_param = params.items.first()?;

    match &first_param.pattern {
        BindingPattern::BindingIdentifier(id) => {
            Some(ExpectSource::Fixture(CompactStr::from(format!("{}.expect", id.name))))
        }
        BindingPattern::ObjectPattern(pattern) => {
            let prop = pattern
                .properties
                .iter()
                .find(|p| p.key.static_name().is_some_and(|name| name == "expect"))?;

            let local_name = match &prop.value {
                BindingPattern::BindingIdentifier(id) => id.name.as_str(),
                _ => "expect",
            };
            Some(ExpectSource::Fixture(CompactStr::from(local_name)))
        }
        _ => None,
    }
}

fn resolve_expect_local_name(ctx: &LintContext<'_>) -> CompactStr {
    for entry in &ctx.module_record().import_entries {
        let source = entry.module_request.name();
        if source != "@jest/globals" && source != "vitest" && source != "vite-plus/test" {
            continue;
        }
        if entry.is_type {
            continue;
        }
        let crate::module_record::ImportImportName::Name(import_name) = &entry.import_name else {
            continue;
        };
        if import_name.name() == "expect" {
            return CompactStr::from(entry.local_name.name());
        }
    }
    CompactStr::from("expect")
}

fn is_async_callback(callback: &Expression<'_>) -> bool {
    match callback {
        Expression::FunctionExpression(func) => func.r#async,
        Expression::ArrowFunctionExpression(func) => func.r#async,
        _ => false,
    }
}

#[test]
fn test() {
    use crate::tester::Tester;

    let pass = vec![
        (r#"test("it1", () => {expect.assertions(0);})"#, None),
        (r#"test("it1", function() {expect.assertions(0);})"#, None),
        (r#"test("it1", function() {expect.hasAssertions();})"#, None),
        (r#"it("it1", function() {expect.assertions(0);})"#, None),
        (r#"test("it1")"#, None),
        (r#"itHappensToStartWithIt("foo", function() {})"#, None),
        (r#"testSomething("bar", function() {})"#, None),
        ("it(async () => {expect.assertions(0);})", None),
        (
            // vitest fixture: destructured expect
            r#"import * as vi from 'vitest';
            test("example-fail", async ({ expect }) => {
                expect.assertions(1);
                await expect(Promise.resolve(null)).resolves.toBeNull();
              });
                "#,
            None,
        ),
        (
            // vitest fixture: expect accessed as property on context param
            r#"import { test } from 'vitest';
            test("ctx param", async (ctx) => {
                ctx.expect.assertions(1);
                await ctx.expect(Promise.resolve(null)).resolves.toBeNull();
              });
                "#,
            None,
        ),
        (
            // vitest fixture: renamed destructured expect
            r#"import { test } from 'vitest';
            test("renamed expect", async ({ expect: myExpect }) => {
                myExpect.assertions(1);
                await myExpect(Promise.resolve(null)).resolves.toBeNull();
              });
                "#,
            None,
        ),
        (
            // vitest fixture: renamed expect with hasAssertions
            r#"import { test } from 'vitest';
            test("renamed hasAssertions", async ({ expect: e }) => {
                e.hasAssertions();
                await e(Promise.resolve(null)).resolves.toBeNull();
              });
                "#,
            None,
        ),
        (
            // vitest fixture: context variable with hasAssertions
            r#"import { test } from 'vitest';
            test("ctx hasAssertions", async (t) => {
                t.expect.hasAssertions();
                await t.expect(Promise.resolve(null)).resolves.toBeNull();
              });
                "#,
            None,
        ),
        (
            // vitest fixture: no expect in params, fallback to global
            r#"import { test, expect } from 'vitest';
            test("global expect", async () => {
                expect.assertions(1);
                await expect(Promise.resolve(null)).resolves.toBeNull();
              });
                "#,
            None,
        ),
        (
            // import reassignment from vitest
            r#"import { expect as e } from 'vitest';
            test("reassigned vitest import", () => {
                e.assertions(1);
                e(true).toBe(true);
              });
                "#,
            None,
        ),
        (
            // Re-exported vitest: renamed expect from a third-party re-export
            r#"import { expect as e } from 'vite-plus/test';
            test("re-exported vitest", () => {
                e.assertions(1);
                e(true).toBe(true);
              });"#,
            None,
        ),
        (
            // Re-exported vitest: global expect from a third-party re-export
            r#"import { expect } from 'vite-plus/test';
            test("re-exported vitest global", () => {
                expect.assertions(1);
                expect(true).toBe(true);
              });"#,
            None,
        ),
        (
            // beforeEach with renamed import covers the describe
            "import { expect as e } from 'vitest';
            describe('suite', () => {
                beforeEach(() => { e.hasAssertions(); });
                it('test', () => {
                    e(true).toBe(true);
                });
            });",
            None,
        ),
        (
            r#"it("it1", () => {
                expect.assertions(0);
                const foo = { bar({ baz }) { baz(); } };
              });
                "#,
            None,
        ),
        (
            "
               const expectNumbersToBeGreaterThan = (numbers, value) => {
                for (let number of numbers) {
                expect(number).toBeGreaterThan(value);
               }
               };

               it('returns numbers that are greater than two', function () {
                expectNumbersToBeGreaterThan(getNumbers(), 2);
               });
               ",
            Some(serde_json::json!([{ "onlyFunctionsWithExpectInLoop": true }])),
        ),
        (
            r#"
               it("returns numbers that are greater than five", function () {
                expect.assertions(2);
                for (const number of getNumbers()) {
                expect(number).toBeGreaterThan(5);
               }
               });
               "#,
            Some(serde_json::json!([{ "onlyFunctionsWithExpectInLoop": true }])),
        ),
        (
            r#"it("returns things that are less than ten", function () {
                expect.hasAssertions();
                for (const thing in things) {
                 expect(thing).toBeLessThan(10);
                }
               });"#,
            Some(serde_json::json!([{ "onlyFunctionsWithExpectInLoop": true }])),
        ),
    ];

    let fail = vec![
        (r#"it("it1", () => foo())"#, None),
        (
            "
            import * as vi from 'vitest';
            it('my test description', ({ expect }) => {
              const a = 1;
              const b = 2;

              expect(sum(a, b)).toBe(a + b);
            })
            ",
            None,
        ),
        (
            "
            it('my test description', (context) => {
              const a = 1;
              const b = 2;

              context.expect(sum(a, b)).toBe(a + b);
            })
            ",
            None,
        ),
        ("it('resolves', () => expect(staged()).toBe(true));", None),
        ("it('resolves', async () => expect(await staged()).toBe(true));", None),
        (r#"it("it1", () => {})"#, None),
        (r#"it("it1", () => { foo()})"#, None),
        (r#"it("it1", function() {var a = 2;})"#, None),
        (r#"it("it1", function() {expect.assertions();})"#, None),
        (r#"it("it1", function() {expect.assertions(1,2);})"#, None),
        (r#"it("it1", function() {expect.assertions(1,2,);})"#, None),
        (r#"it("it1", function() {expect.assertions("1");})"#, None),
        (r#"it("it1", function() {expect.hasAssertions("1");})"#, None),
        (r#"it("it1", function() {expect.hasAssertions("1",);})"#, None),
        (r#"it("it1", function() {expect.hasAssertions("1", "2");})"#, None),
        (
            r#"it("it1", () => {
                expect.hasAssertions();

                for (const number of getNumbers()) {
                  expect(number).toBeGreaterThan(0);
                }
                 });

                 it("it1", () => {
                for (const number of getNumbers()) {
                  expect(number).toBeGreaterThan(0);
                }
                 });"#,
            Some(serde_json::json!([{ "onlyFunctionsWithExpectInLoop": true }])),
        ),
        (
            r#"it("returns numbers that are greater than four", async () => {
                 for (const number of await getNumbers()) {
                expect(number).toBeGreaterThan(4);
                 }
               });

               it("returns numbers that are greater than five", () => {
                 for (const number of getNumbers()) {
                expect(number).toBeGreaterThan(5);
                 }
               });
                "#,
            Some(serde_json::json!([{ "onlyFunctionsWithExpectInLoop": true }])),
        ),
        (
            r#"it("it1", () => {
                const foo = { bar({ baz }) { baz(); } };
              });
                "#,
            None,
        ),
        (
            // vitest fixture: renamed expect, missing assertions
            "import * as vi from 'vitest';
            it('missing assertions', ({ expect: myExpect }) => {
              myExpect(true).toBe(true);
            })
            ",
            None,
        ),
        (
            // vitest fixture: context variable, missing assertions
            "import * as vi from 'vitest';
            it('missing assertions', (ctx) => {
              ctx.expect(true).toBe(true);
            })
            ",
            None,
        ),
        (
            // vitest fixture: renamed expect, assertions with no argument
            r#"import * as vi from 'vitest';
            it("it1", ({ expect: e }) => {e.assertions();})"#,
            None,
        ),
        (
            // vitest fixture: context variable, assertions with string argument
            r#"import * as vi from 'vitest';
            it("it1", (ctx) => {ctx.expect.assertions("1");})"#,
            None,
        ),
        (
            // vitest fixture: renamed expect, hasAssertions with extra arguments
            r#"import * as vi from 'vitest';
            it("it1", ({ expect: e }) => {e.hasAssertions("1");})"#,
            None,
        ),
        (
            // vitest fixture: context variable, assertions with extra arguments
            r#"import * as vi from 'vitest';
            it("it1", (ctx) => {ctx.expect.assertions(1, 2);})"#,
            None,
        ),
        (
            // vitest import reassignment: missing assertions
            r#"import { expect as e } from 'vitest';
            test("reassigned", () => { e(true).toBe(true); });"#,
            None,
        ),
        (
            // Re-exported vitest: missing assertions
            r#"import { expect as e } from 'vite-plus/test';
            test("re-exported missing", () => { e(true).toBe(true); });"#,
            None,
        ),
        (
            // beforeEach uses global `expect.hasAssertions()` but import is renamed to `e`.
            // The hook doesn't match the renamed prefix, so the test is NOT covered.
            "import { expect as e } from 'vitest';
            describe('suite', () => {
                beforeEach(() => { expect.hasAssertions(); });
                it('test', () => {
                    e(true).toBe(true);
                });
            });",
            None,
        ),
    ];

    let fix_import_reassignment = vec![(
        r#"import { expect as e } from 'vitest';
            test("reassigned", () => { e(true).toBe(true); });"#,
        (
            r#"import { expect as e } from 'vitest';
            test("reassigned", () => {e.hasAssertions(); e(true).toBe(true); });"#,
            r#"import { expect as e } from 'vitest';
            test("reassigned", () => {e.assertions(); e(true).toBe(true); });"#,
        ),
    )];

    let fix_two_suggestions = vec![
        (
            r#"test("it1", () => {expect(true).toBe(true);})"#,
            (
                r#"test("it1", () => {expect.hasAssertions();expect(true).toBe(true);})"#,
                r#"test("it1", () => {expect.assertions();expect(true).toBe(true);})"#,
            ),
        ),
        (
            r#"it("it1", () => { foo()})"#,
            (
                r#"it("it1", () => {expect.hasAssertions(); foo()})"#,
                r#"it("it1", () => {expect.assertions(); foo()})"#,
            ),
        ),
        (
            r#"it("it1", function() {var a = 2;})"#,
            (
                r#"it("it1", function() {expect.hasAssertions();var a = 2;})"#,
                r#"it("it1", function() {expect.assertions();var a = 2;})"#,
            ),
        ),
        (
            "import * as vi from 'vitest';
            it('missing assertions', ({ expect: myExpect }) => {
              myExpect(true).toBe(true);
            })",
            (
                "import * as vi from 'vitest';
            it('missing assertions', ({ expect: myExpect }) => {myExpect.hasAssertions();
              myExpect(true).toBe(true);
            })",
                "import * as vi from 'vitest';
            it('missing assertions', ({ expect: myExpect }) => {myExpect.assertions();
              myExpect(true).toBe(true);
            })",
            ),
        ),
        (
            "import * as vi from 'vitest';
            it('missing assertions', (ctx) => {
              ctx.expect(true).toBe(true);
            })",
            (
                "import * as vi from 'vitest';
            it('missing assertions', (ctx) => {ctx.expect.hasAssertions();
              ctx.expect(true).toBe(true);
            })",
                "import * as vi from 'vitest';
            it('missing assertions', (ctx) => {ctx.expect.assertions();
              ctx.expect(true).toBe(true);
            })",
            ),
        ),
    ];

    let fix_remove_args = vec![
        (
            r#"import * as vi from 'vitest';
            it("it1", ({ expect: e }) => {e.hasAssertions("1");})"#,
            r#"import * as vi from 'vitest';
            it("it1", ({ expect: e }) => {e.hasAssertions();})"#,
        ),
        (
            r#"import * as vi from 'vitest';
            it("it1", (ctx) => {ctx.expect.assertions(1, 2);})"#,
            r#"import * as vi from 'vitest';
            it("it1", (ctx) => {ctx.expect.assertions(1);})"#,
        ),
        (
            r#"it("it1", function() {expect.hasAssertions("1");})"#,
            r#"it("it1", function() {expect.hasAssertions();})"#,
        ),
        (
            r#"it("it1", function() {expect.hasAssertions("1",);})"#,
            r#"it("it1", function() {expect.hasAssertions();})"#,
        ),
        (
            r#"it("it1", function() {expect.hasAssertions("1", "2");})"#,
            r#"it("it1", function() {expect.hasAssertions();})"#,
        ),
        (
            r#"it("it1", function() {expect.assertions(1,2);})"#,
            r#"it("it1", function() {expect.assertions(1);})"#,
        ),
        (
            r#"it("it1", function() {expect.assertions(1,2,);})"#,
            r#"it("it1", function() {expect.assertions(1);})"#,
        ),
        (
            r#"beforeEach(() => { expect.hasAssertions("1") })"#,
            r"beforeEach(() => { expect.hasAssertions() })",
        ),
        (
            r#"afterEach(() => { expect.hasAssertions("1") })"#,
            r"afterEach(() => { expect.hasAssertions() })",
        ),
        (
            r#"import { expect as e } from 'vitest';
            beforeEach(() => { e.hasAssertions("1") })"#,
            "import { expect as e } from 'vitest';
            beforeEach(() => { e.hasAssertions() })",
        ),
    ];

    Tester::new(PreferExpectAssertions::NAME, PreferExpectAssertions::PLUGIN, pass, fail)
        .with_vitest_plugin(true)
        .expect_fix(fix_two_suggestions)
        .expect_fix(fix_import_reassignment)
        .expect_fix(fix_remove_args)
        .test_and_snapshot();
}
