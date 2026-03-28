use oxc_ast::{
    AstKind,
    ast::{Argument, CallExpression, Expression, FunctionBody, Statement},
};
use oxc_ast_visit::Visit;
use oxc_diagnostics::OxcDiagnostic;
use oxc_macros::declare_oxc_lint;
use oxc_semantic::NodeId;
use oxc_span::{GetSpan, Span};
use rustc_hash::FxHashMap;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::{
    context::LintContext,
    fixer::RuleFixer,
    rule::{DefaultRuleConfig, Rule},
    utils::{
        JestFnKind, JestGeneralFnKind, PossibleJestNode, collect_possible_jest_call_node,
        parse_expect_jest_fn_call, parse_general_jest_fn_call,
    },
};

fn has_assertions_takes_no_arguments(span: Span) -> OxcDiagnostic {
    OxcDiagnostic::warn("`expect.hasAssertions` expects no arguments.")
        .with_help("Remove the arguments from `expect.hasAssertions()`.")
        .with_label(span)
}

fn assertions_requires_one_argument(span: Span) -> OxcDiagnostic {
    OxcDiagnostic::warn("`expect.assertions` expects a single argument of type number.")
        .with_help("Pass a single numeric argument to `expect.assertions()`.")
        .with_label(span)
}

fn assertions_requires_number_argument(span: Span) -> OxcDiagnostic {
    OxcDiagnostic::warn("This argument should be a number.")
        .with_help("Replace this argument with a numeric literal.")
        .with_label(span)
}

fn have_expect_assertions(span: Span) -> OxcDiagnostic {
    OxcDiagnostic::warn(
        "Every test should have either `expect.assertions(<number of assertions>)` or `expect.hasAssertions()` as its first expression.",
    )
    .with_help("Add `expect.hasAssertions()` or `expect.assertions(<number>)` as the first statement in the test.")
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

declare_oxc_lint!(
    /// ### What it does
    ///
    /// Suggest using `expect.assertions()` OR `expect.hasAssertions()` as the
    /// first expression in every test.
    ///
    /// ### Why is this bad?
    ///
    /// Without explicit assertion counts, tests with asynchronous code or
    /// callbacks may pass even if some `expect` calls are never reached,
    /// hiding bugs.
    ///
    /// ### Examples
    ///
    /// Examples of **incorrect** code for this rule:
    /// ```js
    /// test("something", () => {
    ///   expect(true).toBe(true);
    /// });
    /// ```
    ///
    /// Examples of **correct** code for this rule:
    /// ```js
    /// test("something", () => {
    ///   expect.assertions(1);
    ///   expect(true).toBe(true);
    /// });
    /// ```
    PreferExpectAssertions,
    jest,
    nursery,
    suggestion,
    config = PreferExpectAssertionsConfig
);

impl Rule for PreferExpectAssertions {
    fn from_configuration(value: serde_json::Value) -> Result<Self, serde_json::error::Error> {
        serde_json::from_value::<DefaultRuleConfig<PreferExpectAssertionsConfig>>(value)
            .map(|c| Self(Box::new(c.into_inner())))
    }

    fn run_once(&self, ctx: &LintContext) {
        let mut possible_jest_nodes = collect_possible_jest_call_node(ctx);
        possible_jest_nodes.sort_unstable_by_key(|n| n.node.id());

        // Build a span → PossibleJestNode lookup so inner calls (e.g.
        // expect.hasAssertions inside a beforeEach callback) can be
        // resolved with the correct import-alias context.
        let span_lookup: FxHashMap<Span, &PossibleJestNode<'_, '_>> =
            possible_jest_nodes.iter().map(|n| (n.node.span(), n)).collect();

        // Track which describe scopes (by NodeId) are covered by
        // beforeEach/afterEach hooks that contain expect.hasAssertions().
        let mut covered_describe_ids: Vec<NodeId> = Vec::new();

        for jest_node in &possible_jest_nodes {
            self.check_node(jest_node, &span_lookup, &mut covered_describe_ids, ctx);
        }
    }
}

impl PreferExpectAssertions {
    fn check_node<'a>(
        &self,
        jest_node: &PossibleJestNode<'a, '_>,
        span_lookup: &FxHashMap<Span, &PossibleJestNode<'a, '_>>,
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
                    self.check_each_hook(call_expr, node.id(), covered_describe_ids, ctx);
                }
            }
            JestGeneralFnKind::Test => {
                self.check_test(call_expr, node.id(), span_lookup, covered_describe_ids, ctx);
            }
            _ => {}
        }
    }

    fn check_each_hook(
        &self,
        call_expr: &CallExpression<'_>,
        hook_node_id: NodeId,
        covered_describe_ids: &mut Vec<NodeId>,
        ctx: &LintContext<'_>,
    ) {
        let Some(body) = get_callback_body_from_call(call_expr) else {
            return;
        };

        let mut scanner = HookScanner::default();
        scanner.visit_function_body(body);

        if !scanner.has_expect_has_assertions {
            return;
        }

        if let Some(args_span) = scanner.has_assertions_invalid_args_span {
            let call_span = scanner.has_assertions_call_span.unwrap();
            let delete_span = Span::new(args_span.start, call_span.end - 1);
            let fixer = RuleFixer::new(FixKind::Suggestion, ctx);
            let suggestion = fixer
                .delete_range(delete_span)
                .with_message("Remove extra arguments");
            ctx.diagnostic_with_suggestions(
                has_assertions_takes_no_arguments(args_span),
                [suggestion],
            );
        }

        // Find the nearest ancestor describe that contains this hook.
        // If no describe parent exists, use ROOT to indicate top-level coverage.
        let parent_describe_id = ctx
            .nodes()
            .ancestors(hook_node_id)
            .find(|n| matches!(n.kind(), AstKind::CallExpression(c) if is_describe_call(c)))
            .map_or(NodeId::ROOT, |n| n.id());

        if !covered_describe_ids.contains(&parent_describe_id) {
            covered_describe_ids.push(parent_describe_id);
        }
    }

    fn check_test<'a>(
        &self,
        call_expr: &'a CallExpression<'a>,
        test_node_id: NodeId,
        span_lookup: &FxHashMap<Span, &PossibleJestNode<'a, '_>>,
        covered_describe_ids: &[NodeId],
        ctx: &LintContext<'a>,
    ) {
        if call_expr.arguments.len() < 2 {
            return;
        }

        let Some(callback) = get_test_callback(call_expr) else {
            return;
        };

        let Some(body) = get_callback_body(callback) else {
            return;
        };

        // Check if first statement is expect.assertions / expect.hasAssertions
        if self.check_first_statement(body, span_lookup, ctx) {
            return;
        }

        // Check if covered by a beforeEach/afterEach hook in an ancestor describe
        if !covered_describe_ids.is_empty() {
            // ROOT means a top-level hook covers everything
            if covered_describe_ids.contains(&NodeId::ROOT) {
                return;
            }
            let is_covered = ctx.nodes().ancestors(test_node_id).any(|ancestor| {
                matches!(ancestor.kind(), AstKind::CallExpression(c) if is_describe_call(c))
                    && covered_describe_ids.contains(&ancestor.id())
            });
            if is_covered {
                return;
            }
        }

        // If any option is set, check whether this test actually needs assertions
        if self.only_functions_with_async_keyword
            || self.only_functions_with_expect_in_callback
            || self.only_functions_with_expect_in_loop
        {
            let is_async = is_callback_async(callback);

            if !self.should_check(body, is_async) {
                return;
            }
        }

        // Suggest adding `expect.hasAssertions()` or `expect.assertions()` at
        // the start of the test body (matching the OG eslint-plugin-jest rule).
        let insert_pos = Span::new(body.span.start + 1, body.span.start + 1);
        let fixer = RuleFixer::new(FixKind::Suggestion, ctx);
        let suggestions = [
            ("Add `expect.hasAssertions()`", "expect.hasAssertions();"),
            ("Add `expect.assertions(<number of assertions>)`", "expect.assertions();"),
        ]
        .map(|(msg, text)| {
            fixer
                .insert_text_before_range(insert_pos, text)
                .with_message(msg)
        });

        ctx.diagnostic_with_suggestions(have_expect_assertions(call_expr.span), suggestions);
    }

    /// Returns true if first statement is a valid expect.assertions/expect.hasAssertions call.
    /// Also validates the arguments and reports diagnostics for malformed calls.
    fn check_first_statement<'a>(
        &self,
        body: &'a FunctionBody<'a>,
        span_lookup: &FxHashMap<Span, &PossibleJestNode<'a, '_>>,
        ctx: &LintContext<'a>,
    ) -> bool {
        let Some(Statement::ExpressionStatement(first_expr_stmt)) = body.statements.first() else {
            return false;
        };

        let Expression::CallExpression(first_call) = &first_expr_stmt.expression else {
            return false;
        };

        // Look up the inner call in collected jest nodes for proper import resolution
        // Look up the first call by span to get the correct PossibleJestNode
        let Some(inner_jest_node) = span_lookup.get(&first_call.span) else {
            return false;
        };

        let Some(expect_call) = parse_expect_jest_fn_call(first_call, inner_jest_node, ctx) else {
            return false;
        };

        // Check for expect.hasAssertions()
        if expect_call.members.iter().any(|m| m.is_name_equal("hasAssertions")) {
            if let Some(args) = &expect_call.matcher_arguments {
                if !args.is_empty() {
                    if let Some(args_span) = first_call.arguments_span() {
                        // Delete from first arg start to just before closing `)`,
                        // accounting for trailing commas.
                        let delete_span =
                            Span::new(args_span.start, first_call.span.end - 1);
                        let fixer = RuleFixer::new(FixKind::Suggestion, ctx);
                        let suggestion = fixer
                            .delete_range(delete_span)
                            .with_message("Remove extra arguments");
                        ctx.diagnostic_with_suggestions(
                            has_assertions_takes_no_arguments(args_span),
                            [suggestion],
                        );
                    }
                }
            }
            return true;
        }

        // Check for expect.assertions(n)
        if expect_call.members.iter().any(|m| m.is_name_equal("assertions")) {
            if let Some(args) = &expect_call.matcher_arguments {
                match args.len() {
                    0 => {
                        let matcher_span = expect_call
                            .members
                            .iter()
                            .find(|m| m.is_name_equal("assertions"))
                            .map(|m| m.span);
                        if let Some(span) = matcher_span {
                            ctx.diagnostic(assertions_requires_one_argument(span));
                        }
                    }
                    1 => {
                        let arg = &args[0];
                        if !matches!(arg, Argument::NumericLiteral(_)) {
                            ctx.diagnostic(assertions_requires_number_argument(arg.span()));
                        }
                    }
                    _ => {
                        // Extra arguments — suggest removing them (keep only the first).
                        // Delete from after first arg to before closing `)`,
                        // accounting for trailing commas.
                        let extra_start = args[0].span().end;
                        let extra_end = first_call.span.end - 1;
                        let extra_span = Span::new(extra_start, extra_end);
                        let fixer = RuleFixer::new(FixKind::Suggestion, ctx);
                        let suggestion = fixer
                            .delete_range(extra_span)
                            .with_message("Remove extra arguments");
                        ctx.diagnostic_with_suggestions(
                            assertions_requires_one_argument(extra_span),
                            [suggestion],
                        );
                    }
                }
            }
            return true;
        }

        false
    }

    /// Determine if the test function should be checked based on configuration options.
    fn should_check(&self, body: &FunctionBody<'_>, is_async: bool) -> bool {
        if self.only_functions_with_async_keyword && is_async {
            return true;
        }

        if self.only_functions_with_expect_in_callback || self.only_functions_with_expect_in_loop {
            // Start at expression_depth -1 because visit_function_body will
            // immediately increment to 0 for the test callback's own body.
            // Nested callbacks will be at depth >= 1.
            let mut scanner = BodyScanner { expression_depth: -1, ..BodyScanner::default() };
            scanner.visit_function_body(body);

            if self.only_functions_with_expect_in_callback && scanner.has_expect_in_callback {
                return true;
            }

            if self.only_functions_with_expect_in_loop && scanner.has_expect_in_loop {
                return true;
            }
        }

        false
    }
}

/// Scans a hook body for `expect.hasAssertions()` calls at any depth.
#[derive(Default)]
struct HookScanner {
    has_expect_has_assertions: bool,
    /// If hasAssertions was called with arguments, store the args span for diagnostic.
    has_assertions_invalid_args_span: Option<Span>,
    /// The call expression span, used to find the closing `)` for the fix.
    has_assertions_call_span: Option<Span>,
}

impl<'a> Visit<'a> for HookScanner {
    fn visit_call_expression(&mut self, call_expr: &CallExpression<'a>) {
        if let Expression::StaticMemberExpression(member) = &call_expr.callee {
            if member.property.name == "hasAssertions" {
                if let Some(id) = member.object.get_identifier_reference() {
                    if id.name == "expect" {
                        self.has_expect_has_assertions = true;
                        if !call_expr.arguments.is_empty() {
                            self.has_assertions_invalid_args_span = call_expr.arguments_span();
                            self.has_assertions_call_span = Some(call_expr.span);
                        }
                    }
                }
            }
        }
        oxc_ast_visit::walk::walk_call_expression(self, call_expr);
    }
}

#[derive(Default)]
struct BodyScanner {
    /// Nesting depth of function expressions / arrow functions.
    /// 0 = top-level of the test callback body, >0 = inside a nested callback.
    /// Starts at -1 so that the initial visit_function_body brings it to 0.
    expression_depth: i32,
    in_loop: bool,
    has_expect_in_callback: bool,
    has_expect_in_loop: bool,
}

impl BodyScanner {
    fn is_expect_call(call_expr: &CallExpression<'_>) -> bool {
        match &call_expr.callee {
            Expression::Identifier(ident) => ident.name == "expect",
            Expression::StaticMemberExpression(member) => {
                member.object.get_identifier_reference().is_some_and(|id| id.name == "expect")
            }
            _ => false,
        }
    }
}

impl<'a> Visit<'a> for BodyScanner {
    fn visit_call_expression(&mut self, call_expr: &CallExpression<'a>) {
        if Self::is_expect_call(call_expr) {
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

    fn visit_for_statement(&mut self, stmt: &oxc_ast::ast::ForStatement<'a>) {
        let was = self.in_loop;
        self.in_loop = true;
        oxc_ast_visit::walk::walk_for_statement(self, stmt);
        self.in_loop = was;
    }

    fn visit_for_in_statement(&mut self, stmt: &oxc_ast::ast::ForInStatement<'a>) {
        let was = self.in_loop;
        self.in_loop = true;
        oxc_ast_visit::walk::walk_for_in_statement(self, stmt);
        self.in_loop = was;
    }

    fn visit_for_of_statement(&mut self, stmt: &oxc_ast::ast::ForOfStatement<'a>) {
        let was = self.in_loop;
        self.in_loop = true;
        oxc_ast_visit::walk::walk_for_of_statement(self, stmt);
        self.in_loop = was;
    }

    fn visit_while_statement(&mut self, stmt: &oxc_ast::ast::WhileStatement<'a>) {
        let was = self.in_loop;
        self.in_loop = true;
        oxc_ast_visit::walk::walk_while_statement(self, stmt);
        self.in_loop = was;
    }

    fn visit_do_while_statement(&mut self, stmt: &oxc_ast::ast::DoWhileStatement<'a>) {
        let was = self.in_loop;
        self.in_loop = true;
        oxc_ast_visit::walk::walk_do_while_statement(self, stmt);
        self.in_loop = was;
    }
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

fn get_test_callback<'a>(call_expr: &'a CallExpression<'a>) -> Option<&'a Expression<'a>> {
    call_expr.arguments.iter().rev().filter_map(|arg| arg.as_expression()).find(|expr| {
        matches!(expr, Expression::FunctionExpression(_) | Expression::ArrowFunctionExpression(_))
    })
}

fn get_callback_body<'a>(callback: &'a Expression<'a>) -> Option<&'a FunctionBody<'a>> {
    match callback {
        Expression::FunctionExpression(func) => func.body.as_ref().map(AsRef::as_ref),
        Expression::ArrowFunctionExpression(func) => Some(&func.body),
        _ => None,
    }
}

fn get_callback_body_from_call<'a>(
    call_expr: &'a CallExpression<'a>,
) -> Option<&'a FunctionBody<'a>> {
    get_test_callback(call_expr).and_then(get_callback_body)
}

fn is_callback_async(callback: &Expression<'_>) -> bool {
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
        (r#"test("nonsense", [])"#, None),
        (r#"test("it1", () => {expect.assertions(0);})"#, None),
        (r#"test("it1", function() {expect.assertions(0);})"#, None),
        (r#"test("it1", function() {expect.hasAssertions();})"#, None),
        (r#"it("it1", function() {expect.assertions(0);})"#, None),
        (
            r#"it("it1", function() {
              expect.assertions(1);
              expect(someValue).toBe(true)
            });"#,
            None,
        ),
        (r#"test("it1")"#, None),
        (r#"itHappensToStartWithIt("foo", function() {})"#, None),
        (r#"testSomething("bar", function() {})"#, None),
        ("it(async () => {expect.assertions(0);})", None),
        (
            r#"it("returns numbers that are greater than four", function() {
              expect.assertions(2);
              for(let thing in things) {
                expect(number).toBeGreaterThan(4);
              }
            });"#,
            None,
        ),
        (
            r#"it("returns numbers that are greater than four", function() {
              expect.hasAssertions();
              for (let i = 0; i < things.length; i++) {
                expect(number).toBeGreaterThan(4);
              }
            });"#,
            None,
        ),
        (
            r#"it("it1", async () => {
              expect.assertions(1);
              expect(someValue).toBe(true)
            });"#,
            Some(serde_json::json!([{ "onlyFunctionsWithAsyncKeyword": true }])),
        ),
        (
            r#"it("it1", function() {
              expect(someValue).toBe(true)
            });"#,
            Some(serde_json::json!([{ "onlyFunctionsWithAsyncKeyword": true }])),
        ),
        (
            r#"it("it1", () => {})"#,
            Some(serde_json::json!([{ "onlyFunctionsWithAsyncKeyword": true }])),
        ),
        (
            r#"it("returns numbers that are greater than four", async () => {
              expect.assertions(2);
              for(let thing in things) {
                expect(number).toBeGreaterThan(4);
              }
            });"#,
            Some(serde_json::json!([{ "onlyFunctionsWithAsyncKeyword": true }])),
        ),
        (
            r#"it("returns numbers that are greater than four", () => {
              for(let thing in things) {
                expect(number).toBeGreaterThan(4);
              }
            });"#,
            Some(serde_json::json!([{ "onlyFunctionsWithAsyncKeyword": true }])),
        ),
        (
            r#"import { expect as pleaseExpect } from '@jest/globals';
            it("returns numbers that are greater than four", function() {
              pleaseExpect.assertions(2);
              for(let thing in things) {
                pleaseExpect(number).toBeGreaterThan(4);
              }
            });"#,
            None,
        ),
        (
            r#"beforeEach(() => expect.hasAssertions());
            it('responds ok', function () {
              client.get('/user', response => {
                expect(response.status).toBe(200);
              });
            });
            it("is a number that is greater than four", () => {
              expect(number).toBeGreaterThan(4);
            });"#,
            None,
        ),
        (
            r#"afterEach(() => {
              expect.hasAssertions();
            });
            it('responds ok', function () {
              client.get('/user', response => {
                expect(response.status).toBe(200);
              });
            });
            it("is a number that is greater than four", () => {
              expect(number).toBeGreaterThan(4);
            });"#,
            None,
        ),
        (
            r#"afterEach(() => {
              expect.hasAssertions();
            });
            it('responds ok', function () {
              client.get('/user', response => {
                expect(response.status).toBe(200);
              });
            });
            it("is a number that is greater than four", () => {
              expect.hasAssertions();
              expect(number).toBeGreaterThan(4);
            });"#,
            None,
        ),
        (
            r#"beforeEach(() => { expect.hasAssertions(); });
            describe('my tests', () => {
              it('responds ok', function () {
                client.get('/user', response => {
                  expect(response.status).toBe(200);
                });
              });
              it("is a number that is greater than four", () => {
                expect.hasAssertions();
                expect(number).toBeGreaterThan(4);
              });
            });"#,
            None,
        ),
        (
            r#"describe('my tests', () => {
              beforeEach(() => { expect.hasAssertions(); });
              describe('left', () => {
                describe('inner', () => {
                  it('responds ok', function () {
                    client.get('/user', response => {
                      expect(response.status).toBe(200);
                    });
                  });
                });
              });
              describe('right', () => {
                it("is a number that is greater than four", () => {
                  expect(number).toBeGreaterThan(4);
                });
              });
            });"#,
            None,
        ),
        (
            r#"describe('my tests', () => {
              beforeEach(() => { expect.hasAssertions(); });
              describe('left', () => {
                it('responds ok', function () {
                  client.get('/user', response => {
                    expect(response.status).toBe(200);
                  });
                });
              });
              describe('right', () => {
                it("is a number that is greater than four", () => {
                  expect(number).toBeGreaterThan(4);
                });
              });
            });"#,
            None,
        ),
        (
            r#"describe('my tests', () => {
              beforeEach(() => { expect.hasAssertions(); });
              describe('left', () => {
                beforeEach(() => { expect.hasAssertions(); });
                it('responds ok', function () {
                  client.get('/user', response => {
                    expect(response.status).toBe(200);
                  });
                });
              });
              describe('right', () => {
                it("is a number that is greater than four", () => {
                  expect(number).toBeGreaterThan(4);
                });
              });
            });"#,
            None,
        ),
        (
            r#"describe('my tests', () => {
              beforeEach(() => { expect.hasAssertions(); });
              describe('left', () => {
                afterEach(() => { expect.hasAssertions(); });
                it('responds ok', function () {
                  client.get('/user', response => {
                    expect(response.status).toBe(200);
                  });
                });
              });
              describe('right', () => {
                it("is a number that is greater than four", () => {
                  expect(number).toBeGreaterThan(4);
                });
              });
            });"#,
            None,
        ),
        (
            r#"describe('my tests', () => {
              beforeEach(() => { expect.hasAssertions(); });
              it('responds ok', function () {
                client.get('/user', response => {
                  expect(response.status).toBe(200);
                });
              });
              it("is a number that is greater than four", () => {
                expect.hasAssertions();
                expect(number).toBeGreaterThan(4);
              });
            });"#,
            None,
        ),
        (
            "beforeEach(() => {
              setTimeout(() => expect.hasAssertions(), 5000);
            });
            it('only returns numbers that are greater than six', () => {
              for (const number of getNumbers()) {
                expect(number).toBeGreaterThan(6);
              }
            });",
            None,
        ),
        (
            "const expectNumbersToBeGreaterThan = (numbers, value) => {
              for (let number of numbers) {
                expect(number).toBeGreaterThan(value);
              }
            };
            it('returns numbers that are greater than two', function () {
              expectNumbersToBeGreaterThan(getNumbers(), 2);
            });",
            Some(serde_json::json!([{ "onlyFunctionsWithExpectInLoop": true }])),
        ),
        (
            r#"it("returns numbers that are greater than five", function () {
              expect.assertions(2);
              for (const number of getNumbers()) {
                expect(number).toBeGreaterThan(5);
              }
            });"#,
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
        (
            "const expectNumbersToBeGreaterThan = (numbers, value) => {
              numbers.forEach(number => {
                expect(number).toBeGreaterThan(value);
              });
            };
            it('returns numbers that are greater than two', function () {
              expectNumbersToBeGreaterThan(getNumbers(), 2);
            });",
            Some(serde_json::json!([{ "onlyFunctionsWithExpectInCallback": true }])),
        ),
        (
            "it('returns numbers that are greater than two', function () {
              expect.assertions(2);
              const expectNumbersToBeGreaterThan = (numbers, value) => {
                for (let number of numbers) {
                  expect(number).toBeGreaterThan(value);
                }
              };
              expectNumbersToBeGreaterThan(getNumbers(), 2);
            });",
            Some(serde_json::json!([{ "onlyFunctionsWithExpectInCallback": true }])),
        ),
        (
            "beforeEach(() => expect.hasAssertions());
            it('returns numbers that are greater than two', function () {
              const expectNumbersToBeGreaterThan = (numbers, value) => {
                for (let number of numbers) {
                  expect(number).toBeGreaterThan(value);
                }
              };
              expectNumbersToBeGreaterThan(getNumbers(), 2);
            });",
            Some(serde_json::json!([{ "onlyFunctionsWithExpectInCallback": true }])),
        ),
        (
            r#"it("returns numbers that are greater than five", function () {
              expect.assertions(2);
              getNumbers().forEach(number => {
                expect(number).toBeGreaterThan(5);
              });
            });"#,
            Some(serde_json::json!([{ "onlyFunctionsWithExpectInCallback": true }])),
        ),
        (
            r#"it("returns things that are less than ten", function () {
              expect.hasAssertions();
              things.forEach(thing => {
                expect(thing).toBeLessThan(10);
              });
            });"#,
            Some(serde_json::json!([{ "onlyFunctionsWithExpectInCallback": true }])),
        ),
        (
            "it('sends the data as a string', () => {
              expect.hasAssertions();
              const stream = openStream();
              stream.on('data', data => {
                expect(data).toBe(expect.any(String));
              });
            });",
            Some(serde_json::json!([{ "onlyFunctionsWithExpectInCallback": true }])),
        ),
        (
            "it('responds ok', function () {
              expect.assertions(1);
              client.get('/user', response => {
                expect(response.status).toBe(200);
              });
            });",
            Some(serde_json::json!([{ "onlyFunctionsWithExpectInCallback": true }])),
        ),
        (
            r#"it.each([1, 2, 3])("returns ok", id => {
              expect.assertions(3);
              client.get(`/users/${id}`, response => {
                expect(response.status).toBe(200);
              });
            });
            it("is a number that is greater than four", () => {
              expect(number).toBeGreaterThan(4);
            });"#,
            Some(serde_json::json!([{ "onlyFunctionsWithExpectInCallback": true }])),
        ),
        (
            "it('is a test', () => {
              expect(expected).toBe(actual);
            });
            describe('my test', () => {
              it('is another test', () => {
                expect(expected).toBe(actual);
              });
            });",
            Some(serde_json::json!([{ "onlyFunctionsWithExpectInCallback": true }])),
        ),
        (
            "it('responds ok', function () {
              expect.assertions(1);
              client.get('/user', response => {
                expect(response.status).toBe(200);
              });
            });
            describe('my test', () => {
              beforeEach(() => expect.hasAssertions());
              it('responds ok', function () {
                client.get('/user', response => {
                  expect(response.status).toBe(200);
                });
              });
            });",
            Some(serde_json::json!([{ "onlyFunctionsWithExpectInCallback": true }])),
        ),
        (
            "it('responds ok', function () {
              expect.assertions(1);
              client.get('/user', response => {
                expect(response.status).toBe(200);
              });
            });
            describe('my test', () => {
              afterEach(() => expect.hasAssertions());
              it('responds ok', function () {
                client.get('/user', response => {
                  expect(response.status).toBe(200);
                });
              });
            });",
            Some(serde_json::json!([{ "onlyFunctionsWithExpectInCallback": true }])),
        ),
        (
            "it('only returns numbers that are greater than zero', async () => {
              expect.hasAssertions();
              for (const number of await getNumbers()) {
                expect(number).toBeGreaterThan(0);
              }
            });",
            Some(
                serde_json::json!([ { "onlyFunctionsWithAsyncKeyword": true, "onlyFunctionsWithExpectInLoop": true, }, ]),
            ),
        ),
        (
            "it('only returns numbers that are greater than zero', async () => {
              expect.assertions(2);
              for (const number of await getNumbers()) {
                expect(number).toBeGreaterThan(0);
              }
            });",
            Some(
                serde_json::json!([ { "onlyFunctionsWithAsyncKeyword": true, "onlyFunctionsWithExpectInLoop": true, }, ]),
            ),
        ),
        (r#"test.each()("is fine", () => { expect.assertions(0); })"#, None),
        (r#"test.each``("is fine", () => { expect.assertions(0); })"#, None),
        (r#"test.each()("is fine", () => { expect.hasAssertions(); })"#, None),
        (r#"test.each``("is fine", () => { expect.hasAssertions(); })"#, None),
        (r#"it.each()("is fine", () => { expect.assertions(0); })"#, None),
        (r#"it.each``("is fine", () => { expect.assertions(0); })"#, None),
        (r#"it.each()("is fine", () => { expect.hasAssertions(); })"#, None),
        (r#"it.each``("is fine", () => { expect.hasAssertions(); })"#, None),
        (
            r#"test.each()("is fine", () => {})"#,
            Some(serde_json::json!([{ "onlyFunctionsWithAsyncKeyword": true }])),
        ),
        (
            r#"test.each``("is fine", () => {})"#,
            Some(serde_json::json!([{ "onlyFunctionsWithAsyncKeyword": true }])),
        ),
        (
            r#"it.each()("is fine", () => {})"#,
            Some(serde_json::json!([{ "onlyFunctionsWithAsyncKeyword": true }])),
        ),
        (
            r#"it.each``("is fine", () => {})"#,
            Some(serde_json::json!([{ "onlyFunctionsWithAsyncKeyword": true }])),
        ),
        (
            "describe.each(['hello'])('%s', () => {
              it('is fine', () => {
                expect.assertions(0);
              });
            });",
            None,
        ),
        (
            r"describe.each``('%s', () => {
              it('is fine', () => {
                expect.assertions(0);
              });
            });",
            None,
        ),
        (
            "describe.each(['hello'])('%s', () => {
              it('is fine', () => {
                expect.hasAssertions();
              });
            });",
            None,
        ),
        (
            r"describe.each``('%s', () => {
              it('is fine', () => {
                expect.hasAssertions();
              });
            });",
            None,
        ),
        (
            "describe.each(['hello'])('%s', () => {
              it.each()('is fine', () => {
                expect.assertions(0);
              });
            });",
            None,
        ),
        (
            r"describe.each``('%s', () => {
              it.each()('is fine', () => {
                expect.assertions(0);
              });
            });",
            None,
        ),
        (
            "describe.each(['hello'])('%s', () => {
              it.each()('is fine', () => {
                expect.hasAssertions();
              });
            });",
            None,
        ),
        (
            r"describe.each``('%s', () => {
              it.each()('is fine', () => {
                expect.hasAssertions();
              });
            });",
            None,
        ),
    ];

    let fail = vec![
        (r#"it("it1", () => foo())"#, None),
        ("it('resolves', () => expect(staged()).toBe(true));", None),
        ("it('resolves', async () => expect(await staged()).toBe(true));", None),
        (r#"it("it1", () => {})"#, None),
        (r#"it("it1", () => { foo()})"#, None),
        (
            r#"it("it1", function() {
              someFunctionToDo();
              someFunctionToDo2();
            });"#,
            None,
        ),
        (
            r#"it("it1", function() {
              someFunctionToDo();
              someFunctionToDo2();
            });
            describe('some tests', () => {
              beforeEach(() => { expect.hasAssertions(); });
              it("it1", function() {
                someFunctionToDo();
                someFunctionToDo2();
              });
            });"#,
            None,
        ),
        (
            r#"it("it1", function() {
              someFunctionToDo();
              someFunctionToDo2();
            });
            describe('some tests', () => {
              afterEach(() => { expect.hasAssertions(); });
              it("it1", function() {
                someFunctionToDo();
                someFunctionToDo2();
              });
            });"#,
            None,
        ),
        (
            r#"describe('some tests', () => {
              it("it1", function() {
                someFunctionToDo();
                someFunctionToDo2();
              });
              beforeEach(() => { expect.hasAssertions(); });
              it("it1", function() {
                someFunctionToDo();
                someFunctionToDo2();
              });
            });"#,
            None,
        ),
        (
            r#"describe('some tests', () => {
              beforeEach(() => { expect.hasAssertions(); });
              it("it1", function() {
                someFunctionToDo();
                someFunctionToDo2();
              });
            });
            it("it1", function() {
              someFunctionToDo();
              someFunctionToDo2();
            });"#,
            None,
        ),
        (
            r#"describe('some tests', () => {
              beforeEach(() => { expect.hasAssertions(); });
              it("it1", function() {
                someFunctionToDo();
                someFunctionToDo2();
              });
            });
            describe('more tests', () => {
              it("it1", function() {
                someFunctionToDo();
                someFunctionToDo2();
              });
            });"#,
            None,
        ),
        (r#"it("it1", function() {var a = 2;})"#, None),
        (r#"it("it1", function() {expect.assertions();})"#, None),
        (r#"it("it1", function() {expect.assertions(1,2);})"#, None),
        (r#"it("it1", function() {expect.assertions(1,2,);})"#, None),
        (r#"it("it1", function() {expect.assertions("1");})"#, None),
        (r#"beforeEach(() => { expect.hasAssertions("1") })"#, None),
        (r#"beforeEach(() => expect.hasAssertions("1"))"#, None),
        (r#"afterEach(() => { expect.hasAssertions("1") })"#, None),
        (r#"afterEach(() => expect.hasAssertions("1"))"#, None),
        (r#"it("it1", function() {expect.hasAssertions("1");})"#, None),
        (r#"it("it1", function() {expect.hasAssertions("1",);})"#, None),
        (r#"it("it1", function() {expect.hasAssertions("1", "2");})"#, None),
        (
            r#"it("it1", function() {
              expect.hasAssertions(() => {
                someFunctionToDo();
                someFunctionToDo2();
              });
            });"#,
            None,
        ),
        (
            r#"it("it1", async function() {
              expect(someValue).toBe(true);
            });"#,
            Some(serde_json::json!([{ "onlyFunctionsWithAsyncKeyword": true }])),
        ),
        (
            r#"it("returns numbers that are greater than four", async () => {
              for(let thing in things) {
                expect(number).toBeGreaterThan(4);
              }
            });"#,
            Some(serde_json::json!([{ "onlyFunctionsWithAsyncKeyword": true }])),
        ),
        (
            r#"it("returns numbers that are greater than four", async () => {
              for (const number of getNumbers()) {
                expect(number).toBeGreaterThan(4);
              }
            });"#,
            Some(serde_json::json!([{ "onlyFunctionsWithAsyncKeyword": true }])),
        ),
        (
            r#"it("returns numbers that are greater than four", async () => {
              for (const number of getNumbers()) {
                expect(number).toBeGreaterThan(4);
              }
            });
            it("returns numbers that are greater than five", () => {
              for (const number of getNumbers()) {
                expect(number).toBeGreaterThan(5);
              }
            });"#,
            Some(serde_json::json!([{ "onlyFunctionsWithAsyncKeyword": true }])),
        ),
        (
            r#"beforeAll(() => { expect.hasAssertions(); });
            it("returns numbers that are greater than four", async () => {
              for (const number of getNumbers()) {
                expect(number).toBeGreaterThan(4);
              }
            });
            it("returns numbers that are greater than five", () => {
              for (const number of getNumbers()) {
                expect(number).toBeGreaterThan(5);
              }
            });"#,
            Some(serde_json::json!([{ "onlyFunctionsWithAsyncKeyword": true }])),
        ),
        (
            r#"afterAll(() => { expect.hasAssertions(); });
            it("returns numbers that are greater than four", async () => {
              for (const number of getNumbers()) {
                expect(number).toBeGreaterThan(4);
              }
            });
            it("returns numbers that are greater than five", () => {
              for (const number of getNumbers()) {
                expect(number).toBeGreaterThan(5);
              }
            });"#,
            Some(serde_json::json!([{ "onlyFunctionsWithAsyncKeyword": true }])),
        ),
        (
            "it('only returns numbers that are greater than six', () => {
              for (const number of getNumbers()) {
                expect(number).toBeGreaterThan(6);
              }
            });",
            Some(serde_json::json!([{ "onlyFunctionsWithExpectInLoop": true }])),
        ),
        (
            "it('returns numbers that are greater than two', function () {
              const expectNumbersToBeGreaterThan = (numbers, value) => {
                for (let number of numbers) {
                  expect(number).toBeGreaterThan(value);
                }
              };
              expectNumbersToBeGreaterThan(getNumbers(), 2);
            });",
            Some(serde_json::json!([{ "onlyFunctionsWithExpectInLoop": true }])),
        ),
        (
            r#"it("only returns numbers that are greater than seven", function () {
              const numbers = getNumbers();
              for (let i = 0; i < numbers.length; i++) {
                expect(numbers[i]).toBeGreaterThan(7);
              }
            });"#,
            Some(serde_json::json!([{ "onlyFunctionsWithExpectInLoop": true }])),
        ),
        (
            "it('has the number two', () => {
              expect(number).toBe(2);
            });
            it('only returns numbers that are less than twenty', () => {
              for (const number of getNumbers()) {
                expect(number).toBeLessThan(20);
              }
            });",
            Some(serde_json::json!([{ "onlyFunctionsWithExpectInLoop": true }])),
        ),
        (
            r#"it("is wrong");
            it("is a test", () => {
              for (const number of getNumbers()) {
                expect(number).toBeGreaterThan(4);
              }
            });"#,
            Some(serde_json::json!([{ "onlyFunctionsWithExpectInLoop": true }])),
        ),
        (
            r#"it("is a number that is greater than four", () => {
              expect(number).toBeGreaterThan(4);
            });
            it("returns numbers that are greater than four", () => {
              for (const number of getNumbers()) {
                expect(number).toBeGreaterThan(4);
              }
            });
            it("returns numbers that are greater than five", () => {
              expect(number).toBeGreaterThan(5);
            });"#,
            Some(serde_json::json!([{ "onlyFunctionsWithExpectInLoop": true }])),
        ),
        (
            r#"describe('my tests', () => {
              beforeEach(expect.hasAssertions);
              it("is a number that is greater than four", () => {
                expect(number).toBeGreaterThan(4);
              });
            });
            describe('more tests', () => {
              it("returns numbers that are greater than four", () => {
                for (const number of getNumbers()) {
                  expect(number).toBeGreaterThan(4);
                }
              });
            });
            it("returns numbers that are greater than five", () => {
              expect(number).toBeGreaterThan(5);
            });"#,
            Some(serde_json::json!([{ "onlyFunctionsWithExpectInLoop": true }])),
        ),
        (
            r#"it.each([1, 2, 3])("returns numbers that are greater than four", () => {
              for (const number of getNumbers()) {
                expect(number).toBeGreaterThan(4);
              }
            });
            it("is a number that is greater than four", () => {
              expect(number).toBeGreaterThan(4);
            });"#,
            Some(serde_json::json!([{ "onlyFunctionsWithExpectInLoop": true }])),
        ),
        (
            r#"it("returns numbers that are greater than four", () => {
              for (const number of getNumbers()) {
                expect(number).toBeGreaterThan(4);
              }
            });
            it("is a number that is greater than four", () => {
              expect(number).toBeGreaterThan(4);
            });"#,
            Some(serde_json::json!([{ "onlyFunctionsWithExpectInLoop": true }])),
        ),
        (
            r#"it("returns numbers that are greater than four", () => {
              for (const number of getNumbers()) {
                expect(number).toBeGreaterThan(4);
              }
            });
            it("is a number that is greater than four", () => {
              expect.hasAssertions();
              expect(number).toBeGreaterThan(4);
            });"#,
            Some(serde_json::json!([{ "onlyFunctionsWithExpectInLoop": true }])),
        ),
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
            });"#,
            Some(serde_json::json!([{ "onlyFunctionsWithExpectInLoop": true }])),
        ),
        (
            r#"it("it1", async () => {
              expect.hasAssertions();
              for (const number of getNumbers()) {
                expect(number).toBeGreaterThan(4);
              }
            });
            it("it1", () => {
              for (const number of getNumbers()) {
                expect(number).toBeGreaterThan(4);
              }
            });"#,
            Some(serde_json::json!([{ "onlyFunctionsWithExpectInLoop": true }])),
        ),
        (
            r#"describe('my tests', () => {
              beforeEach(() => { expect.hasAssertions(); });
              it("it1", async () => {
                for (const number of getNumbers()) {
                  expect(number).toBeGreaterThan(4);
                }
              });
            });
            it("it1", () => {
              for (const number of getNumbers()) {
                expect(number).toBeGreaterThan(4);
              }
            });"#,
            Some(serde_json::json!([{ "onlyFunctionsWithExpectInLoop": true }])),
        ),
        (
            r#"describe('my tests', () => {
              afterEach(() => { expect.hasAssertions(); });
              it("it1", async () => {
                for (const number of getNumbers()) {
                  expect(number).toBeGreaterThan(4);
                }
              });
            });
            it("it1", () => {
              for (const number of getNumbers()) {
                expect(number).toBeGreaterThan(4);
              }
            });"#,
            Some(serde_json::json!([{ "onlyFunctionsWithExpectInLoop": true }])),
        ),
        (
            r#"it.skip.each``("it1", async () => {
              expect.hasAssertions();
              for (const number of getNumbers()) {
                expect(number).toBeGreaterThan(4);
              }
            });
            it("it1", () => {
              for (const number of getNumbers()) {
                expect(number).toBeGreaterThan(4);
              }
            });"#,
            Some(serde_json::json!([{ "onlyFunctionsWithExpectInLoop": true }])),
        ),
        (
            r#"it("it1", async () => {
              for (const number of getNumbers()) {
                expect(number).toBeGreaterThan(4);
              }
            });
            it("it1", () => {
              expect.hasAssertions();
              for (const number of getNumbers()) {
                expect(number).toBeGreaterThan(4);
              }
            });"#,
            Some(serde_json::json!([{ "onlyFunctionsWithExpectInLoop": true }])),
        ),
        (
            r#"describe('my tests', () => {
              it("it1", async () => {
                for (const number of getNumbers()) {
                  expect(number).toBeGreaterThan(4);
                }
              });
            });
            it("it1", () => {
              expect.hasAssertions();
              for (const number of getNumbers()) {
                expect(number).toBeGreaterThan(4);
              }
            });"#,
            Some(serde_json::json!([{ "onlyFunctionsWithExpectInLoop": true }])),
        ),
        (
            "it('sends the data as a string', () => {
              const stream = openStream();
              stream.on('data', data => {
                expect(data).toBe(expect.any(String));
              });
            });",
            Some(serde_json::json!([{ "onlyFunctionsWithExpectInCallback": true }])),
        ),
        (
            "it('responds ok', function () {
              client.get('/user', response => {
                expect(response.status).toBe(200);
              });
            });",
            Some(serde_json::json!([{ "onlyFunctionsWithExpectInCallback": true }])),
        ),
        (
            "it('responds ok', function () {
              client.get('/user', response => {
                expect.assertions(1);
                expect(response.status).toBe(200);
              });
            });",
            Some(serde_json::json!([{ "onlyFunctionsWithExpectInCallback": true }])),
        ),
        (
            "it('responds ok', function () {
              const expectOkResponse = response => {
                expect.assertions(1);
                expect(response.status).toBe(200);
              };
              client.get('/user', expectOkResponse);
            });",
            Some(serde_json::json!([{ "onlyFunctionsWithExpectInCallback": true }])),
        ),
        (
            "it('returns numbers that are greater than two', function () {
              const expectNumberToBeGreaterThan = (number, value) => {
                expect(number).toBeGreaterThan(value);
              };
              expectNumberToBeGreaterThan(1, 2);
            });",
            Some(serde_json::json!([{ "onlyFunctionsWithExpectInCallback": true }])),
        ),
        (
            "it('returns numbers that are greater than two', function () {
              const expectNumbersToBeGreaterThan = (numbers, value) => {
                for (let number of numbers) {
                  expect(number).toBeGreaterThan(value);
                }
              };
              expectNumbersToBeGreaterThan(getNumbers(), 2);
            });",
            Some(serde_json::json!([{ "onlyFunctionsWithExpectInCallback": true }])),
        ),
        (
            "it('only returns numbers that are greater than six', () => {
              getNumbers().forEach(number => {
                expect(number).toBeGreaterThan(6);
              });
            });",
            Some(serde_json::json!([{ "onlyFunctionsWithExpectInCallback": true }])),
        ),
        (
            r#"it("is wrong");
            it('responds ok', function () {
              const expectOkResponse = response => {
                expect.assertions(1);
                expect(response.status).toBe(200);
              };
              client.get('/user', expectOkResponse);
            });"#,
            Some(serde_json::json!([{ "onlyFunctionsWithExpectInCallback": true }])),
        ),
        (
            r#"it("is a number that is greater than four", () => {
              expect(number).toBeGreaterThan(4);
            });
            it('responds ok', function () {
              const expectOkResponse = response => {
                expect(response.status).toBe(200);
              };
              client.get('/user', expectOkResponse);
            });
            it("returns numbers that are greater than five", () => {
              expect(number).toBeGreaterThan(5);
            });"#,
            Some(serde_json::json!([{ "onlyFunctionsWithExpectInCallback": true }])),
        ),
        (
            r#"it("is a number that is greater than four", () => {
              expect(number).toBeGreaterThan(4);
            });
            it("returns numbers that are greater than four", () => {
              getNumbers().map(number => {
                expect(number).toBeGreaterThan(0);
              });
            });
            it("returns numbers that are greater than five", () => {
              expect(number).toBeGreaterThan(5);
            });"#,
            Some(serde_json::json!([{ "onlyFunctionsWithExpectInCallback": true }])),
        ),
        (
            r#"it.each([1, 2, 3])("returns ok", id => {
              client.get(`/users/${id}`, response => {
                expect(response.status).toBe(200);
              });
            });
            it("is a number that is greater than four", () => {
              expect(number).toBeGreaterThan(4);
            });"#,
            Some(serde_json::json!([{ "onlyFunctionsWithExpectInCallback": true }])),
        ),
        (
            r#"it('responds ok', function () {
              client.get('/user', response => {
                expect(response.status).toBe(200);
              });
            });
            it("is a number that is greater than four", () => {
              expect(number).toBeGreaterThan(4);
            });"#,
            Some(serde_json::json!([{ "onlyFunctionsWithExpectInCallback": true }])),
        ),
        (
            r#"it('responds ok', function () {
              client.get('/user', response => {
                expect(response.status).toBe(200);
              });
            });
            it("is a number that is greater than four", () => {
              expect.hasAssertions();
              expect(number).toBeGreaterThan(4);
            });"#,
            Some(serde_json::json!([{ "onlyFunctionsWithExpectInCallback": true }])),
        ),
        (
            r#"it("it1", () => {
              expect.hasAssertions();
              getNumbers().forEach(number => {
                expect(number).toBeGreaterThan(0);
              });
            });
            it("it1", () => {
              getNumbers().forEach(number => {
                expect(number).toBeGreaterThan(0);
              });
            });"#,
            Some(serde_json::json!([{ "onlyFunctionsWithExpectInCallback": true }])),
        ),
        (
            "it('responds ok', function () {
              expect.hasAssertions();
              client.get('/user', response => {
                expect(response.status).toBe(200);
              });
            });
            it('responds not found', function () {
              client.get('/user', response => {
                expect(response.status).toBe(404);
              });
            });",
            Some(serde_json::json!([{ "onlyFunctionsWithExpectInCallback": true }])),
        ),
        (
            r#"it.skip.each``("it1", async () => {
              expect.hasAssertions();
              client.get('/user', response => {
                expect(response.status).toBe(200);
              });
            });
            it("responds ok", () => {
              client.get('/user', response => {
                expect(response.status).toBe(200);
              });
            });"#,
            Some(serde_json::json!([{ "onlyFunctionsWithExpectInCallback": true }])),
        ),
        (
            r#"it("returns numbers that are greater than four", function(expect) {
              expect.assertions(2);
              for(let thing in things) {
                expect(number).toBeGreaterThan(4);
              }
            });"#,
            None,
        ),
        (
            r#"it('only returns numbers that are greater than zero', () => {
              for (const number of getNumbers()) {
                expect(number).toBeGreaterThan(0);
              }
            });
            it("is zero", () => {
              expect.hasAssertions();
              expect(0).toBe(0);
            });"#,
            Some(serde_json::json!([{ "onlyFunctionsWithExpectInLoop": true }])),
        ),
        (
            "it('only returns numbers that are greater than zero', () => {
              expect.hasAssertions();
              for (const number of getNumbers()) {
                expect(number).toBeGreaterThan(0);
              }
            });
            it('only returns numbers that are less than 100', () => {
              for (const number of getNumbers()) {
                expect(number).toBeLessThan(0);
              }
            });",
            Some(serde_json::json!([{ "onlyFunctionsWithExpectInLoop": true }])),
        ),
        (
            r#"it("to be true", async function() {
              expect(someValue).toBe(true);
            });"#,
            Some(
                serde_json::json!([ { "onlyFunctionsWithAsyncKeyword": true, "onlyFunctionsWithExpectInLoop": true, }, ]),
            ),
        ),
        (
            "it('only returns numbers that are greater than zero', async () => {
              for (const number of getNumbers()) {
                expect(number).toBeGreaterThan(0);
              }
            });",
            Some(
                serde_json::json!([ { "onlyFunctionsWithAsyncKeyword": true, "onlyFunctionsWithExpectInLoop": true, }, ]),
            ),
        ),
        (
            r#"test.each()("is not fine", () => {
              expect(someValue).toBe(true);
            });"#,
            None,
        ),
        (
            r#"describe.each()('something', () => {
              it("is not fine", () => {
                expect(someValue).toBe(true);
              });
            });"#,
            None,
        ),
        (
            r#"describe.each()('something', () => {
              test.each()("is not fine", () => {
                expect(someValue).toBe(true);
              });
            });"#,
            None,
        ),
        (
            r#"test.each()("is not fine", async () => {
              expect(someValue).toBe(true);
            });"#,
            Some(serde_json::json!([{ "onlyFunctionsWithAsyncKeyword": true }])),
        ),
        (
            r#"it.each()("is not fine", async () => {
              expect(someValue).toBe(true);
            });"#,
            Some(serde_json::json!([{ "onlyFunctionsWithAsyncKeyword": true }])),
        ),
        (
            r#"describe.each()('something', () => {
              test.each()("is not fine", async () => {
                expect(someValue).toBe(true);
              });
            });"#,
            Some(serde_json::json!([{ "onlyFunctionsWithAsyncKeyword": true }])),
        ),
    ];

    // haveExpectAssertions: two suggestions — expect.hasAssertions() and expect.assertions()
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
    ];

    // hasAssertionsTakesNoArguments / assertionsRequiresOneArgument:
    // suggest removing extra arguments (single suggestion)
    let fix_remove_args = vec![
        // hasAssertions with extra args
        (
            r#"it("it1", function() {expect.hasAssertions("1");})"#,
            r#"it("it1", function() {expect.hasAssertions();})"#,
        ),
        (
            r#"it("it1", function() {expect.hasAssertions("1",);})"#,
            // trailing comma is also removed since we delete from first arg to before `)`
            r#"it("it1", function() {expect.hasAssertions();})"#,
        ),
        (
            r#"it("it1", function() {expect.hasAssertions("1", "2");})"#,
            r#"it("it1", function() {expect.hasAssertions();})"#,
        ),
        // assertions with extra args
        (
            r#"it("it1", function() {expect.assertions(1,2);})"#,
            r#"it("it1", function() {expect.assertions(1);})"#,
        ),
        (
            r#"it("it1", function() {expect.assertions(1,2,);})"#,
            r#"it("it1", function() {expect.assertions(1);})"#,
        ),
        // hasAssertions with extra args in hooks
        (
            r#"beforeEach(() => { expect.hasAssertions("1") })"#,
            r#"beforeEach(() => { expect.hasAssertions() })"#,
        ),
        (
            r#"afterEach(() => { expect.hasAssertions("1") })"#,
            r#"afterEach(() => { expect.hasAssertions() })"#,
        ),
    ];

    Tester::new(PreferExpectAssertions::NAME, PreferExpectAssertions::PLUGIN, pass, fail)
        .with_jest_plugin(true)
        .expect_fix(fix_two_suggestions)
        .expect_fix(fix_remove_args)
        .test_and_snapshot();
}
