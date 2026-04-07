use oxc_ast::{
    AstKind,
    ast::{Argument, CallExpression, Expression, FunctionBody, Statement},
};
use oxc_ast_visit::{Visit, walk};
use oxc_diagnostics::OxcDiagnostic;
use oxc_macros::declare_oxc_lint;
use oxc_span::{CompactStr, GetSpan, Span};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::{
    context::LintContext,
    rule::Rule,
    rules::PossibleJestNode,
    utils::{JestGeneralFnKind, get_node_name_vec, parse_general_jest_fn_call},
};

fn expect_in_unhandled_promise(span: Span) -> OxcDiagnostic {
    OxcDiagnostic::warn("Expect in a promise chain must be awaited or returned")
        .with_help("Either `await` the promise, `return` it, or use `expect().resolves`/`expect().rejects`.")
        .with_label(span)
}

#[derive(Debug, Default, Clone)]
pub struct ValidExpectInPromise;

declare_oxc_lint!(
    /// ### What it does
    ///
    /// Ensures that `expect` calls inside promise chains (`.then()`, `.catch()`,
    /// `.finally()`) are properly awaited or returned from the test.
    ///
    /// ### Why is this bad?
    ///
    /// When `expect` is called inside a promise callback that is not awaited or
    /// returned, the test may pass even if the assertion fails because the test
    /// completes before the promise resolves. This leads to silently passing
    /// tests with broken assertions.
    ///
    /// ### Examples
    ///
    /// Examples of **incorrect** code for this rule:
    /// ```javascript
    /// it('tests something', () => {
    ///   somePromise.then(value => {
    ///     expect(value).toBe('foo');
    ///   });
    /// });
    /// ```
    ///
    /// Examples of **correct** code for this rule:
    /// ```javascript
    /// it('tests something', async () => {
    ///   await somePromise.then(value => {
    ///     expect(value).toBe('foo');
    ///   });
    /// });
    ///
    /// it('tests something', () => {
    ///   return somePromise.then(value => {
    ///     expect(value).toBe('foo');
    ///   });
    /// });
    /// ```
    ValidExpectInPromise,
    jest,
    correctness
);

impl Rule for ValidExpectInPromise {
    fn run_on_jest_node<'a, 'c>(
        &self,
        jest_node: &PossibleJestNode<'a, 'c>,
        ctx: &'c LintContext<'a>,
    ) {
        let node = jest_node.node;

        let AstKind::CallExpression(call_expr) = node.kind() else {
            return;
        };

        if call_expr.arguments.len() < 2 {
            return;
        }

        let Some(parsed_jest_fn) = parse_general_jest_fn_call(call_expr, jest_node, ctx) else {
            return;
        };

        if parsed_jest_fn
            .kind
            .to_general()
            .is_some_and(|test_kind| matches!(test_kind, JestGeneralFnKind::Describe))
        {
            return;
        };

        let Some(callback) = call_expr.arguments.get(1) else {
            return;
        };

        let Some(callback_body) = get_callback_body(callback) else {
            return;
        };

        // If the callback has any parameters, the test uses callback-style async
        // (the param is `done`) — promise handling is not required.
        // For `.each()` variants, the original rule also bails on any param.
        if get_callback_param_count(callback) > 0 {
            return;
        }

        // Arrow with expression body: `() => expr` — the expression is implicitly returned.
        // This is always valid (the promise is returned from the test).
        if is_expression_arrow(callback) {
            return;
        }

        let mut pending_promises: FxHashMap<CompactStr, Span> = FxHashMap::default();
        let mut return_found = false;

        process_statements(
            &callback_body.statements,
            &mut pending_promises,
            &mut return_found,
            ctx,
        );

        // Everything still in pending_promises was never properly awaited or returned
        for (_name, span) in &pending_promises {
            ctx.diagnostic(expect_in_unhandled_promise(*span));
        }
    }
}

fn process_statements<'a>(
    statements: &'a oxc_allocator::Vec<'a, Statement<'a>>,
    pending_promises: &mut FxHashMap<CompactStr, Span>,
    return_found: &mut bool,
    ctx: &LintContext<'a>,
) {
    for statement in statements {
        // Single-pass scan: detect promise+expect AND resolved identifiers
        let mut scanner = PromiseExpectScanner::new();
        scanner.visit_statement(statement);

        // After a return, any statement with expect-in-promise is unreachable.
        if *return_found {
            if scanner.found_expect_in_promise {
                ctx.diagnostic(expect_in_unhandled_promise(statement.span()));
            }
            continue;
        }

        match statement {
            Statement::VariableDeclaration(decl) => {
                for name in &scanner.resolved_names {
                    pending_promises.remove(name.as_str());
                }
                for declarator in &decl.declarations {
                    let Some(init) = &declarator.init else {
                        continue;
                    };
                    let Some(ident) = declarator.id.get_binding_identifier() else {
                        continue;
                    };
                    let mut init_scanner = PromiseExpectScanner::new();
                    init_scanner.visit_expression(init);
                    if init_scanner.found_expect_in_promise {
                        pending_promises
                            .insert(CompactStr::from(ident.name.as_str()), declarator.span);
                    }
                }
            }
            Statement::ExpressionStatement(expr_stmt) => {
                match &expr_stmt.expression {
                    Expression::AssignmentExpression(assign_expr) => {
                        if let Some(name) =
                            assign_expr.left.as_simple_assignment_target()
                                .and_then(|t| t.get_identifier_name())
                        {
                            let rhs_references_self =
                                expression_contains_identifier(&assign_expr.right, name);

                            if !rhs_references_self {
                                if let Some(old_span) =
                                    pending_promises.remove(CompactStr::from(name).as_str())
                                {
                                    ctx.diagnostic(expect_in_unhandled_promise(old_span));
                                }
                            }

                            let mut assign_scanner = PromiseExpectScanner::new();
                            assign_scanner.visit_expression(&assign_expr.right);
                            if assign_scanner.found_expect_in_promise {
                                pending_promises
                                    .insert(CompactStr::from(name), expr_stmt.span);
                            }
                        } else {
                            if scanner.found_expect_in_promise {
                                ctx.diagnostic(expect_in_unhandled_promise(
                                    expr_stmt.span,
                                ));
                            }
                        }
                    }
                    Expression::AwaitExpression(_) => {
                        for name in &scanner.resolved_names {
                            pending_promises.remove(name.as_str());
                        }
                    }
                    _ => {
                        for name in &scanner.resolved_names {
                            pending_promises.remove(name.as_str());
                        }
                        if scanner.found_expect_in_promise
                            && is_top_level_promise_chain(&expr_stmt.expression)
                        {
                            ctx.diagnostic(expect_in_unhandled_promise(
                                expr_stmt.span,
                            ));
                        }
                    }
                }
            }
            Statement::ReturnStatement(return_stmt) => {
                if let Some(arg) = &return_stmt.argument {
                    if let Some(name) = ident_name_of(arg) {
                        pending_promises.remove(name);
                    }
                }
                for name in &scanner.resolved_names {
                    pending_promises.remove(name.as_str());
                }
                *return_found = true;
            }
            // Block statements — recurse to handle reassignments inside blocks
            Statement::BlockStatement(block) => {
                process_statements(
                    &block.body,
                    pending_promises,
                    return_found,
                    ctx,
                );
            }
            _ => {
                for name in &scanner.resolved_names {
                    pending_promises.remove(name.as_str());
                }
            }
        }
    }
}

/// Extracts the identifier name from simple expressions:
/// - `promise` → Some("promise")
/// Does NOT handle member expressions or calls — just plain identifiers.
fn ident_name_of<'a>(expr: &'a Expression<'a>) -> Option<&'a str> {
    match expr {
        Expression::Identifier(ident) => Some(ident.name.as_str()),
        _ => None,
    }
}

/// Walks down the callee chain of `expect(x).resolves.not.toBe(2)` to find
/// the arguments of the innermost `expect(...)` call.
/// Handles arbitrary depth of member expression chains.
fn find_expect_args<'a>(call_expr: &'a CallExpression<'a>) -> Option<&'a oxc_allocator::Vec<'a, Argument<'a>>> {
    if let Expression::Identifier(ident) = &call_expr.callee {
        if ident.name == "expect" {
            return Some(&call_expr.arguments);
        }
    }
    // Walk through member chain: the callee may be `expect(x).resolves.not.toBe`
    // We need to find the CallExpression for `expect(x)` inside this chain.
    find_expect_call_in_chain(&call_expr.callee)
}

fn find_expect_call_in_chain<'a>(expr: &'a Expression<'a>) -> Option<&'a oxc_allocator::Vec<'a, Argument<'a>>> {
    match expr {
        Expression::CallExpression(call) => find_expect_args(call),
        _ => {
            let member = expr.as_member_expression()?;
            find_expect_call_in_chain(member.object())
        }
    }
}

/// Returns `true` if the expression contains a reference to the given identifier name.
/// Used to check if `somePromise = somePromise.then(...)` continues the same chain.
fn expression_contains_identifier(expr: &Expression, name: &str) -> bool {
    let mut finder = IdentifierFinder { name, found: false };
    finder.visit_expression(expr);
    finder.found
}

struct IdentifierFinder<'b> {
    name: &'b str,
    found: bool,
}

impl<'a, 'b> Visit<'a> for IdentifierFinder<'b> {
    fn visit_identifier_reference(&mut self, ident: &oxc_ast::ast::IdentifierReference<'a>) {
        if ident.name == self.name {
            self.found = true;
        }
    }
}

/// Returns `true` if the expression is directly a promise chain call (`.then/.catch/.finally`)
/// at the top level, not nested inside object literals or other function arguments.
/// Follows the chain: `x.then(cb).catch(cb2)` → walks down `.catch` → `.then` → true.
fn is_top_level_promise_chain(expr: &Expression) -> bool {
    let Expression::CallExpression(call_expr) = expr else {
        return false;
    };
    let Some(member) = call_expr.callee.as_member_expression() else {
        return false;
    };
    let Some(prop) = member.static_property_name() else {
        return false;
    };
    if matches!(prop, "then" | "catch" | "finally") {
        return true;
    }
    // Could be `somePromise.then(cb).someOtherMethod()` — check the object
    false
}

/// Returns `true` if the callback is an arrow function with an expression body
/// (i.e. `() => expr` rather than `() => { ... }`), meaning the expression is implicitly returned.
fn is_expression_arrow(callback: &Argument) -> bool {
    matches!(callback, Argument::ArrowFunctionExpression(arrow) if arrow.expression)
}

fn get_callback_body<'a>(callback_argument: &'a Argument<'a>) -> Option<&'a FunctionBody<'a>> {
    match callback_argument {
        Argument::ArrowFunctionExpression(arrow_fn) => Some(&arrow_fn.body),
        Argument::FunctionExpression(fn_expr) => fn_expr.body.as_ref().map(AsRef::as_ref),
        _ => None,
    }
}

fn get_callback_param_count(callback: &Argument) -> usize {
    match callback {
        Argument::ArrowFunctionExpression(arrow_fn) => arrow_fn.params.items.len(),
        Argument::FunctionExpression(fn_expr) => fn_expr.params.items.len(),
        _ => 0,
    }
}

/// Single-pass visitor that walks an expression/statement subtree and detects:
/// 1. Promise chains (`.then/.catch/.finally`) containing `expect()` calls
/// 2. Identifiers that are "resolved" — awaited or used in `expect(x).resolves/rejects`
struct PromiseExpectScanner {
    /// Whether we are currently inside a promise chain callback.
    in_promise_chain: bool,
    /// Whether we are currently inside an `await` expression.
    in_await: bool,
    /// Set to `true` once we find an `expect()` inside a promise chain callback
    /// that is NOT already inside an `await`.
    found_expect_in_promise: bool,
    /// Identifiers that were properly resolved (awaited, expect().resolves, etc.)
    resolved_names: FxHashSet<CompactStr>,
}

impl PromiseExpectScanner {
    fn new() -> Self {
        Self {
            in_promise_chain: false,
            in_await: false,
            found_expect_in_promise: false,
            resolved_names: FxHashSet::default(),
        }
    }
}

impl<'a> Visit<'a> for PromiseExpectScanner {
    fn visit_call_expression(&mut self, call_expr: &CallExpression<'a>) {
        // Check for `expect(promise).resolves/rejects` — resolves the promise variable
        let callee_name = get_node_name_vec(&call_expr.callee);
        if callee_name.first().is_some_and(|n| n == "expect")
            && callee_name.iter().any(|n| n == "resolves" || n == "rejects")
        {
            if let Some(inner_args) = find_expect_args(call_expr) {
                if let Some(first_arg) = inner_args.first() {
                    if let Some(expr) = first_arg.as_expression() {
                        if let Some(name) = ident_name_of(expr) {
                            self.resolved_names.insert(CompactStr::from(name));
                        }
                    }
                }
            }
        }

        // Check for `Promise.all([p1, p2])`, `Promise.resolve(p)`, etc.
        // These resolve the identifiers passed as arguments.
        if let Some(member) = call_expr.callee.as_member_expression() {
            if let Expression::Identifier(obj) = member.object() {
                if obj.name == "Promise" {
                    let prop = member.static_property_name();
                    if matches!(prop, Some("all" | "allSettled" | "race" | "any")) {
                        // First arg is an array: Promise.all([p1, p2])
                        if let Some(first_arg) = call_expr.arguments.first() {
                            if let Some(Expression::ArrayExpression(arr)) = first_arg.as_expression() {
                                for elem in &arr.elements {
                                    if let Some(expr) = elem.as_expression() {
                                        if let Some(name) = ident_name_of(expr) {
                                            self.resolved_names.insert(CompactStr::from(name));
                                        }
                                    }
                                }
                            }
                        }
                    } else if matches!(prop, Some("resolve" | "reject")) {
                        // First arg is a single value: Promise.resolve(p)
                        if let Some(first_arg) = call_expr.arguments.first() {
                            if let Some(expr) = first_arg.as_expression() {
                                if let Some(name) = ident_name_of(expr) {
                                    self.resolved_names.insert(CompactStr::from(name));
                                }
                            }
                        }
                    }
                }
            }
        }

        // Check if this call is to `.then()`, `.catch()`, or `.finally()`
        let is_chain_call = call_expr
            .callee
            .as_member_expression()
            .and_then(|member| member.static_property_name())
            .is_some_and(|prop| matches!(prop, "then" | "catch" | "finally"));

        if is_chain_call {
            let was_in_chain = self.in_promise_chain;
            // Only flag as promise chain if not already inside an await
            self.in_promise_chain = !self.in_await;
            // Walk the callee (handles chaining: x.then(cb1).catch(cb2))
            self.visit_expression(&call_expr.callee);
            // Only walk the first 2 arguments of .then/.catch/.finally
            // (.then takes at most 2 callbacks; 3rd+ args are non-standard)
            for arg in call_expr.arguments.iter().take(2) {
                self.visit_argument(arg);
            }
            self.in_promise_chain = was_in_chain;
            return;
        }

        // If we're inside a promise chain and see `expect(...)`, flag it
        if self.in_promise_chain {
            if callee_name.first().is_some_and(|n| n == "expect") {
                self.found_expect_in_promise = true;
                return;
            }
        }

        walk::walk_call_expression(self, call_expr);
    }

    fn visit_await_expression(&mut self, await_expr: &oxc_ast::ast::AwaitExpression<'a>) {
        // `await promise` → mark identifier as resolved
        if let Some(name) = ident_name_of(&await_expr.argument) {
            self.resolved_names.insert(CompactStr::from(name));
        }
        // Mark that we're inside await — any promise chain inside is already handled
        let was_in_await = self.in_await;
        self.in_await = true;
        walk::walk_await_expression(self, await_expr);
        self.in_await = was_in_await;
    }
}

#[test]
fn test() {
    use crate::tester::Tester;

    let pass = vec![
        ("test('something', () => Promise.resolve().then(() => expect(1).toBe(2)));", None, None),
        ("Promise.resolve().then(() => expect(1).toBe(2))", None, None),
        ("const x = Promise.resolve().then(() => expect(1).toBe(2))", None, None),
        (r#"it.todo("something")"#, None, None),
        (
            "it('is valid', () => {
              const promise = loadNumber().then(number => {
                expect(typeof number).toBe('number');
                return number + 1;
              });
              expect(promise).resolves.toBe(1);
            });",
            None,
            None,
        ),
        (
            "it('is valid', () => {
              const promise = loadNumber().then(number => {
                expect(typeof number).toBe('number');
                return number + 1;
              });
              expect(promise).resolves.not.toBe(2);
            });",
            None,
            None,
        ),
        (
            "it('is valid', () => {
              const promise = loadNumber().then(number => {
                expect(typeof number).toBe('number');
                return number + 1;
              });
              expect(promise).rejects.toBe(1);
            });",
            None,
            None,
        ),
        (
            "it('is valid', () => {
              const promise = loadNumber().then(number => {
                expect(typeof number).toBe('number');
                return number + 1;
              });
              expect(promise).rejects.not.toBe(2);
            });",
            None,
            None,
        ),
        (
            "it('is valid', async () => {
              const promise = loadNumber().then(number => {
                expect(typeof number).toBe('number');
                return number + 1;
              });
              expect(await promise).toBeGreaterThan(1);
            });",
            None,
            None,
        ),
        (
            "it('is valid', async () => {
              const promise = loadNumber().then(number => {
                expect(typeof number).toBe('number');
                return number + 1;
              });
              expect(await promise).resolves.toBeGreaterThan(1);
            });",
            None,
            None,
        ),
        (
            "it('is valid', async () => {
              const promise = loadNumber().then(number => {
                expect(typeof number).toBe('number');
                return number + 1;
              });
              expect(1).toBeGreaterThan(await promise);
            });",
            None,
            None,
        ),
        (
            "it('is valid', async () => {
              const promise = loadNumber().then(number => {
                expect(typeof number).toBe('number');
                return number + 1;
              });
              expect.this.that.is(await promise);
            });",
            None,
            None,
        ),
        (
            "it('is valid', async () => {
              expect(await loadNumber().then(number => {
                expect(typeof number).toBe('number');
                return number + 1;
              })).toBeGreaterThan(1);
            });",
            None,
            None,
        ),
        (
            "it('is valid', async () => {
              const promise = loadNumber().then(number => {
                expect(typeof number).toBe('number');
                return number + 1;
              });
              expect([await promise]).toHaveLength(1);
            });",
            None,
            None,
        ),
        (
            "it('is valid', async () => {
              const promise = loadNumber().then(number => {
                expect(typeof number).toBe('number');
                return number + 1;
              });
              expect([,,await promise,,]).toHaveLength(1);
            });",
            None,
            None,
        ),
        (
            "it('is valid', async () => {
              const promise = loadNumber().then(number => {
                expect(typeof number).toBe('number');
                return number + 1;
              });
              expect([[await promise]]).toHaveLength(1);
            });",
            None,
            None,
        ),
        (
            "it('is valid', async () => {
              const promise = loadNumber().then(number => {
                expect(typeof number).toBe('number');
                return number + 1;
              });
              logValue(await promise);
            });",
            None,
            None,
        ),
        (
            "it('is valid', async () => {
              const promise = loadNumber().then(number => {
                expect(typeof number).toBe('number');
                return 1;
              });
              expect.assertions(await promise);
            });",
            None,
            None,
        ),
        (
            "it('is valid', async () => {
              await loadNumber().then(number => {
                expect(typeof number).toBe('number');
              });
            });",
            None,
            None,
        ),
        (
            "it('it1', () => new Promise((done) => {
              test()
                .then(() => {
                  expect(someThing).toEqual(true);
                  done();
                });
            }));",
            None,
            None,
        ),
        (
            "it('it1', () => {
              return new Promise(done => {
                test().then(() => {
                  expect(someThing).toEqual(true);
                  done();
                });
              });
            });",
            None,
            None,
        ),
        (
            "it('passes', () => {
              Promise.resolve().then(() => {
                grabber.grabSomething();
              });
            });",
            None,
            None,
        ),
        (
            "it('passes', async () => {
              const grabbing = Promise.resolve().then(() => {
                grabber.grabSomething();
              });
              await grabbing;
              expect(grabber.grabbedItems).toHaveLength(1);
            });",
            None,
            None,
        ),
        (
            "const myFn = () => {
              Promise.resolve().then(() => {
                expect(true).toBe(false);
              });
            };",
            None,
            None,
        ),
        (
            "const myFn = () => {
              Promise.resolve().then(() => {
                subject.invokeMethod();
              });
            };",
            None,
            None,
        ),
        (
            "const myFn = () => {
              Promise.resolve().then(() => {
                expect(true).toBe(false);
              });
            };
            it('it1', () => {
              return somePromise.then(() => {
                expect(someThing).toEqual(true);
              });
            });",
            None,
            None,
        ),
        (
            "it('it1', () => new Promise((done) => {
              test()
                .finally(() => {
                  expect(someThing).toEqual(true);
                  done();
                });
            }));",
            None,
            None,
        ),
        (
            "it('it1', () => {
              return somePromise.then(() => {
                expect(someThing).toEqual(true);
              });
            });",
            None,
            None,
        ),
        (
            "it('it1', () => {
              return somePromise.finally(() => {
                expect(someThing).toEqual(true);
              });
            });",
            None,
            None,
        ),
        (
            "it('it1', function() {
              return somePromise.catch(function() {
                expect(someThing).toEqual(true);
              });
            });",
            None,
            None,
        ),
        (
            "xtest('it1', function() {
              return somePromise.catch(function() {
                expect(someThing).toEqual(true);
              });
            });",
            None,
            None,
        ),
        (
            "it('it1', function() {
              return somePromise.then(function() {
                doSomeThingButNotExpect();
              });
            });",
            None,
            None,
        ),
        (
            "it('it1', function() {
              return getSomeThing().getPromise().then(function() {
                expect(someThing).toEqual(true);
              });
            });",
            None,
            None,
        ),
        (
            "it('it1', function() {
              return Promise.resolve().then(function() {
                expect(someThing).toEqual(true);
              });
            });",
            None,
            None,
        ),
        (
            "it('it1', function () {
              return Promise.resolve().then(function () {
                /*fulfillment*/
                expect(someThing).toEqual(true);
              }, function () {
                /*rejection*/
                expect(someThing).toEqual(true);
              });
            });",
            None,
            None,
        ),
        (
            "it('it1', function () {
              Promise.resolve().then(/*fulfillment*/ function () {
              }, undefined, /*rejection*/ function () {
                expect(someThing).toEqual(true)
              })
            });",
            None,
            None,
        ),
        (
            "it('it1', function () {
              return Promise.resolve().then(function () {
                /*fulfillment*/
              }, function () {
                /*rejection*/
                expect(someThing).toEqual(true);
              });
            });",
            None,
            None,
        ),
        (
            "it('it1', function () {
              return somePromise.then()
            });",
            None,
            None,
        ),
        (
            "it('it1', async () => {
              await Promise.resolve().then(function () {
                expect(someThing).toEqual(true)
              });
            });",
            None,
            None,
        ),
        (
            "it('it1', async () => {
              await somePromise.then(() => {
                expect(someThing).toEqual(true)
              });
            });",
            None,
            None,
        ),
        (
            "it('it1', async () => {
              await getSomeThing().getPromise().then(function () {
                expect(someThing).toEqual(true)
              });
            });",
            None,
            None,
        ),
        (
            "it('it1', () => {
              return somePromise.then(() => {
                expect(someThing).toEqual(true);
              })
              .then(() => {
                expect(someThing).toEqual(true);
              })
            });",
            None,
            None,
        ),
        (
            "it('it1', () => {
              return somePromise.then(() => {
                return value;
              })
              .then(value => {
                expect(someThing).toEqual(value);
              })
            });",
            None,
            None,
        ),
        (
            "it('it1', () => {
              return somePromise.then(() => {
                expect(someThing).toEqual(true);
              })
              .then(() => {
                console.log('this is silly');
              })
            });",
            None,
            None,
        ),
        (
            "it('it1', () => {
              return somePromise.then(() => {
                expect(someThing).toEqual(true);
              })
              .catch(() => {
                expect(someThing).toEqual(false);
              })
            });",
            None,
            None,
        ),
        (
            "test('later return', () => {
              const promise = something().then(value => {
                expect(value).toBe('red');
              });
              return promise;
            });",
            None,
            None,
        ),
        (
            "test('later return', async () => {
              const promise = something().then(value => {
                expect(value).toBe('red');
              });
              await promise;
            });",
            None,
            None,
        ),
        (
            "test.only('later return', () => {
              const promise = something().then(value => {
                expect(value).toBe('red');
              });
              return promise;
            });",
            None,
            None,
        ),
        (
            "test('that we bailout if destructuring is used', () => {
              const [promise] = something().then(value => {
                expect(value).toBe('red');
              });
            });",
            None,
            None,
        ),
        (
            "test('that we bailout if destructuring is used', async () => {
              const [promise] = await something().then(value => {
                expect(value).toBe('red');
              });
            });",
            None,
            None,
        ),
        (
            "test('that we bailout if destructuring is used', () => {
              const [promise] = [
                something().then(value => {
                  expect(value).toBe('red');
                })
              ];
            });",
            None,
            None,
        ),
        (
            "test('that we bailout if destructuring is used', () => {
              const {promise} = {
                promise: something().then(value => {
                  expect(value).toBe('red');
                })
              };
            });",
            None,
            None,
        ),
        (
            "test('that we bailout in complex cases', () => {
              promiseSomething({
                timeout: 500,
                promise: something().then(value => {
                  expect(value).toBe('red');
                })
              });
            });",
            None,
            None,
        ),
        (
            "it('shorthand arrow', () =>
              something().then(value => {
                expect(() => {
                  value();
                }).toThrow();
              })
            );",
            None,
            None,
        ),
        (
            "it('crawls for files based on patterns', () => {
              const promise = nodeCrawl({}).then(data => {
                expect(childProcess.spawn).lastCalledWith('find');
              });
              return promise;
            });",
            None,
            None,
        ),
        (
            "it('is a test', async () => {
              const value = await somePromise().then(response => {
                expect(response).toHaveProperty('data');
                return response.data;
              });
              expect(value).toBe('hello world');
            });",
            None,
            None,
        ),
        (
            "it('is a test', async () => {
              return await somePromise().then(response => {
                expect(response).toHaveProperty('data');
                return response.data;
              });
            });",
            None,
            None,
        ),
        (
            "it('is a test', async () => {
              return somePromise().then(response => {
                expect(response).toHaveProperty('data');
                return response.data;
              });
            });",
            None,
            None,
        ),
        (
            "it('is a test', async () => {
              await somePromise().then(response => {
                expect(response).toHaveProperty('data');
                return response.data;
              });
            });",
            None,
            None,
        ),
        (
            "it(
              'test function',
              () => {
                return Builder
                  .getPromiseBuilder()
                  .get().build()
                  .then((data) => {
                    expect(data).toEqual('Hi');
                  });
              }
            );",
            None,
            None,
        ),
        (
            "notATestFunction(
              'not a test function',
              () => {
                Builder
                  .getPromiseBuilder()
                  .get()
                  .build()
                  .then((data) => {
                    expect(data).toEqual('Hi');
                  });
              }
            );",
            None,
            None,
        ),
        (
            "it('is valid', async () => {
              const promiseOne = loadNumber().then(number => {
                expect(typeof number).toBe('number');
              });
              const promiseTwo = loadNumber().then(number => {
                expect(typeof number).toBe('number');
              });
              await promiseTwo;
              await promiseOne;
            });",
            None,
            None,
        ),
        (
            r#"it("it1", () => somePromise.then(() => {
              expect(someThing).toEqual(true)
            }))"#,
            None,
            None,
        ),
        (r#"it("it1", () => somePromise.then(() => expect(someThing).toEqual(true)))"#, None, None),
        (
            "it('promise test with done', (done) => {
              const promise = getPromise();
              promise.then(() => expect(someThing).toEqual(true));
            });",
            None,
            None,
        ),
        (
            "it('name of done param does not matter', (nameDoesNotMatter) => {
              const promise = getPromise();
              promise.then(() => expect(someThing).toEqual(true));
            });",
            None,
            None,
        ),
        (
            "it.each([])('name of done param does not matter', (nameDoesNotMatter) => {
              const promise = getPromise();
              promise.then(() => expect(someThing).toEqual(true));
            });",
            None,
            None,
        ),
        (
            "it.each`\n`('name of done param does not matter', ({}, nameDoesNotMatter) => {
              const promise = getPromise();
              promise.then(() => expect(someThing).toEqual(true));
            });",
            None,
            None,
        ),
        (
            "test('valid-expect-in-promise', async () => {
              const text = await fetch('url')
                  .then(res => res.text())
                  .then(text => text);
              expect(text).toBe('text');
            });",
            None,
            None,
        ),
        (
            "test('promise test', async function () {
              let somePromise = getPromise().then((data) => {
                expect(data).toEqual('foo');
              }), x = 1;
              await somePromise;
            });",
            None,
            None,
        ),
        (
            "test('promise test', async function () {
              let x = 1, somePromise = getPromise().then((data) => {
                expect(data).toEqual('foo');
              });
              await somePromise;
            });",
            None,
            None,
        ),
        (
            "test('promise test', async function () {
              let somePromise = getPromise().then((data) => {
                expect(data).toEqual('foo');
              });
              await somePromise;
              somePromise = getPromise().then((data) => {
                expect(data).toEqual('foo');
              });
              await somePromise;
            });",
            None,
            None,
        ),
        (
            "test('promise test', async function () {
              let somePromise = getPromise().then((data) => {
                expect(data).toEqual('foo');
              });
              await somePromise;
              somePromise = getPromise().then((data) => {
                expect(data).toEqual('foo');
              });
              return somePromise;
            });",
            None,
            None,
        ),
        (
            "test('promise test', async function () {
              let somePromise = getPromise().then((data) => {
                expect(data).toEqual('foo');
              });
              {}
              await somePromise;
            });",
            None,
            None,
        ),
        (
            "test('promise test', async function () {
              const somePromise = getPromise().then((data) => {
                expect(data).toEqual('foo');
              });
              {
                await somePromise;
              }
            });",
            None,
            None,
        ),
        (
            "test('promise test', async function () {
              let somePromise = getPromise().then((data) => {
                expect(data).toEqual('foo');
              });
              {
                await somePromise;
                somePromise = getPromise().then((data) => {
                  expect(data).toEqual('foo');
                });
                await somePromise;
              }
            });",
            None,
            None,
        ),
        (
            "test('promise test', async function () {
              let somePromise = getPromise().then((data) => {
                expect(data).toEqual('foo');
              });
              await somePromise;
              {
                somePromise = getPromise().then((data) => {
                  expect(data).toEqual('foo');
                });
                await somePromise;
              }
            });",
            None,
            None,
        ),
        (
            "test('promise test', async function () {
              let somePromise = getPromise().then((data) => {
                expect(data).toEqual('foo');
              });
              somePromise = somePromise.then((data) => {
                expect(data).toEqual('foo');
              });
              await somePromise;
            });",
            None,
            None,
        ),
        (
            "test('promise test', async function () {
              let somePromise = getPromise().then((data) => {
                expect(data).toEqual('foo');
              });
              somePromise = somePromise
                .then((data) => data)
                .then((data) => data)
                .then((data) => {
                  expect(data).toEqual('foo');
                });
              await somePromise;
            });",
            None,
            None,
        ),
        (
            "test('promise test', async function () {
              let somePromise = getPromise().then((data) => {
                expect(data).toEqual('foo');
              });
              somePromise = somePromise
                .then((data) => data)
                .then((data) => data)
              await somePromise;
            });",
            None,
            None,
        ),
        (
            "test('promise test', async function () {
              let somePromise = getPromise().then((data) => {
                expect(data).toEqual('foo');
              });
              await somePromise;
              {
                somePromise = getPromise().then((data) => {
                  expect(data).toEqual('foo');
                });
                {
                  await somePromise;
                }
              }
            });",
            None,
            None,
        ),
        (
            "test('promise test', async function () {
              const somePromise = getPromise().then((data) => {
                expect(data).toEqual('foo');
              });
              await Promise.all([somePromise]);
            });",
            None,
            None,
        ),
        (
            "test('promise test', async function () {
              const somePromise = getPromise().then((data) => {
                expect(data).toEqual('foo');
              });
              return Promise.all([somePromise]);
            });",
            None,
            None,
        ),
        (
            "test('promise test', async function () {
              const somePromise = getPromise().then((data) => {
                expect(data).toEqual('foo');
              });
              return Promise.resolve(somePromise);
            });",
            None,
            None,
        ),
        (
            "test('promise test', async function () {
              const somePromise = getPromise().then((data) => {
                expect(data).toEqual('foo');
              });
              return Promise.reject(somePromise);
            });",
            None,
            None,
        ),
        (
            "test('promise test', async function () {
              const somePromise = getPromise().then((data) => {
                expect(data).toEqual('foo');
              });
              await Promise.resolve(somePromise);
            });",
            None,
            None,
        ),
        (
            "test('promise test', async function () {
              const somePromise = getPromise().then((data) => {
                expect(data).toEqual('foo');
              });
              await Promise.reject(somePromise);
            });",
            None,
            None,
        ),
        (
            "test('later return', async () => {
              const onePromise = something().then(value => {
                console.log(value);
              });
              const twoPromise = something().then(value => {
                expect(value).toBe('red');
              });
              return Promise.all([onePromise, twoPromise]);
            });",
            None,
            None,
        ),
        (
            "test('later return', async () => {
              const onePromise = something().then(value => {
                console.log(value);
              });
              const twoPromise = something().then(value => {
                expect(value).toBe('red');
              });
              return Promise.allSettled([onePromise, twoPromise]);
            });",
            None,
            None,
        ),
    ];

    let fail = vec![
        (
            "const myFn = () => {
              Promise.resolve().then(() => {
                expect(true).toBe(false);
              });
            };
            it('it1', () => {
              somePromise.then(() => {
                expect(someThing).toEqual(true);
              });
            });",
            None,
            None,
        ),
        (
            "it('it1', () => {
              somePromise.then(() => {
                expect(someThing).toEqual(true);
              });
            });",
            None,
            None,
        ),
        (
            "it('it1', () => {
              somePromise.finally(() => {
                expect(someThing).toEqual(true);
              });
            });",
            None,
            None,
        ),
        (
            "
                   it('it1', () => {
                     somePromise['then'](() => {
                       expect(someThing).toEqual(true);
                     });
                   });
                  ",
            None,
            None,
        ),
        (
            "it('it1', function() {
              getSomeThing().getPromise().then(function() {
                expect(someThing).toEqual(true);
              });
            });",
            None,
            None,
        ),
        (
            "it('it1', function() {
              Promise.resolve().then(function() {
                expect(someThing).toEqual(true);
              });
            });",
            None,
            None,
        ),
        (
            "it('it1', function() {
              somePromise.catch(function() {
                expect(someThing).toEqual(true)
              })
            })",
            None,
            None,
        ),
        (
            "xtest('it1', function() {
              somePromise.catch(function() {
                expect(someThing).toEqual(true)
              })
            })",
            None,
            None,
        ),
        (
            "it('it1', function() {
              somePromise.then(function() {
                expect(someThing).toEqual(true)
              })
            })",
            None,
            None,
        ),
        (
            "it('it1', function () {
              Promise.resolve().then(/*fulfillment*/ function () {
                expect(someThing).toEqual(true);
              }, /*rejection*/ function () {
                expect(someThing).toEqual(true);
              })
            })",
            None,
            None,
        ),
        (
            "it('it1', function () {
              Promise.resolve().then(/*fulfillment*/ function () {
              }, /*rejection*/ function () {
                expect(someThing).toEqual(true)
              })
            });",
            None,
            None,
        ),
        (
            "it('test function', () => {
              Builder.getPromiseBuilder()
                .get()
                .build()
                .then(data => expect(data).toEqual('Hi'));
            });",
            None,
            None,
        ),
        (
            "
                    it('test function', async () => {
                      Builder.getPromiseBuilder()
                        .get()
                        .build()
                        .then(data => expect(data).toEqual('Hi'));
                    });
                  ",
            None,
            None,
        ),
        (
            "it('it1', () => {
              somePromise.then(() => {
                doSomeOperation();
                expect(someThing).toEqual(true);
              })
            });",
            None,
            None,
        ),
        (
            "it('is a test', () => {
              somePromise
                .then(() => {})
                .then(() => expect(someThing).toEqual(value))
            });",
            None,
            None,
        ),
        (
            "it('is a test', () => {
              somePromise
                .then(() => expect(someThing).toEqual(value))
                .then(() => {})
            });",
            None,
            None,
        ),
        (
            "it('is a test', () => {
              somePromise.then(() => {
                return value;
              })
              .then(value => {
                expect(someThing).toEqual(value);
              })
            });",
            None,
            None,
        ),
        (
            "it('is a test', () => {
              somePromise.then(() => {
                expect(someThing).toEqual(true);
              })
              .then(() => {
                console.log('this is silly');
              })
            });",
            None,
            None,
        ),
        (
            "it('is a test', () => {
              somePromise.then(() => {
                // return value;
              })
              .then(value => {
                expect(someThing).toEqual(value);
              })
            });",
            None,
            None,
        ),
        (
            "it('is a test', () => {
              somePromise.then(() => {
                return value;
              })
              .then(value => {
                expect(someThing).toEqual(value);
              })
              return anotherPromise.then(() => expect(x).toBe(y));
            });",
            None,
            None,
        ),
        (
            "it('is a test', () => {
              somePromise
                .then(() => 1)
                .then(x => x + 1)
                .catch(() => -1)
                .then(v => expect(v).toBe(2));
              return anotherPromise.then(() => expect(x).toBe(y));
            });",
            None,
            None,
        ),
        (
            "it('is a test', () => {
              somePromise
                .then(() => 1)
                .then(v => expect(v).toBe(2))
                .then(x => x + 1)
                .catch(() => -1);
              return anotherPromise.then(() => expect(x).toBe(y));
            });",
            None,
            None,
        ),
        (
            "it('it1', () => {
              somePromise.finally(() => {
                doSomeOperation();
                expect(someThing).toEqual(true);
              })
            });",
            None,
            None,
        ),
        (
            r#"test('invalid return', () => {
              const promise = something().then(value => {
                const foo = "foo";
                return expect(value).toBe('red');
              });
            });"#,
            None,
            None,
        ),
        (
            "fit('it1', () => {
              somePromise.then(() => {
                doSomeOperation();
                expect(someThing).toEqual(true);
              })
            });",
            None,
            None,
        ),
        (
            "it.skip('it1', () => {
              somePromise.then(() => {
                doSomeOperation();
                expect(someThing).toEqual(true);
              })
            });",
            None,
            None,
        ),
        (
            "test('later return', async () => {
              const promise = something().then(value => {
                expect(value).toBe('red');
              });
              promise;
            });",
            None,
            None,
        ),
        (
            "test('later return', async () => {
              const promise = something().then(value => {
                expect(value).toBe('red');
              });
              return;
              await promise;
            });",
            None,
            None,
        ),
        (
            "test('later return', async () => {
              const promise = something().then(value => {
                expect(value).toBe('red');
              });
              return 1;
              await promise;
            });",
            None,
            None,
        ),
        (
            "test('later return', async () => {
              const promise = something().then(value => {
                expect(value).toBe('red');
              });
              return [];
              await promise;
            });",
            None,
            None,
        ),
        (
            "test('later return', async () => {
              const promise = something().then(value => {
                expect(value).toBe('red');
              });
              return Promise.all([anotherPromise]);
              await promise;
            });",
            None,
            None,
        ),
        (
            "test('later return', async () => {
              const promise = something().then(value => {
                expect(value).toBe('red');
              });
              return {};
              await promise;
            });",
            None,
            None,
        ),
        (
            "test('later return', async () => {
              const promise = something().then(value => {
                expect(value).toBe('red');
              });
              return Promise.all([]);
              await promise;
            });",
            None,
            None,
        ),
        (
            "test('later return', async () => {
              const promise = something().then(value => {
                expect(value).toBe('red');
              });
              await 1;
            });",
            None,
            None,
        ),
        (
            "test('later return', async () => {
              const promise = something().then(value => {
                expect(value).toBe('red');
              });
              await [];
            });",
            None,
            None,
        ),
        (
            "test('later return', async () => {
              const promise = something().then(value => {
                expect(value).toBe('red');
              });
              await Promise.all([anotherPromise]);
            });",
            None,
            None,
        ),
        (
            "test('later return', async () => {
              const promise = something().then(value => {
                expect(value).toBe('red');
              });
              await {};
            });",
            None,
            None,
        ),
        (
            "test('later return', async () => {
              const promise = something().then(value => {
                expect(value).toBe('red');
              });
              await Promise.all([]);
            });",
            None,
            None,
        ),
        (
            "test('later return', async () => {
              const promise = something().then(value => {
                expect(value).toBe('red');
              }), x = 1;
            });",
            None,
            None,
        ),
        (
            "test('later return', async () => {
              const x = 1, promise = something().then(value => {
                expect(value).toBe('red');
              });
            });",
            None,
            None,
        ),
        (
            "import { test } from '@jest/globals';
            test('later return', async () => {
              const x = 1, promise = something().then(value => {
                expect(value).toBe('red');
              });
            });",
            None,
            None,
        ),
        (
            "it('promise test', () => {
              const somePromise = getThatPromise();
              somePromise.then((data) => {
                expect(data).toEqual('foo');
              });
              expect(somePromise).toBeDefined();
              return somePromise;
            });",
            None,
            None,
        ),
        (
            "test('promise test', function () {
              let somePromise = getThatPromise();
              somePromise.then((data) => {
                expect(data).toEqual('foo');
              });
              expect(somePromise).toBeDefined();
              return somePromise;
            });",
            None,
            None,
        ),
        (
            "test('promise test', async function () {
              let somePromise = getPromise().then((data) => {
                expect(data).toEqual('foo');
              });
              somePromise = null;
              await somePromise;
            });",
            None,
            None,
        ),
        (
            "test('promise test', async function () {
              let somePromise = getPromise().then((data) => {
                expect(data).toEqual('foo');
              });
              somePromise = getPromise().then((data) => {
                expect(data).toEqual('foo');
              });
              await somePromise;
            });",
            None,
            None,
        ),
        (
            "test('promise test', async function () {
              let somePromise = getPromise().then((data) => {
                expect(data).toEqual('foo');
              });
              ({ somePromise } = {})
            });",
            None,
            None,
        ),
        (
            "test('promise test', async function () {
              let somePromise = getPromise().then((data) => {
                expect(data).toEqual('foo');
              });
              {
                somePromise = getPromise().then((data) => {
                  expect(data).toEqual('foo');
                });
                await somePromise;
              }
            });",
            None,
            None,
        ),
        (
            "test('that we error on this destructuring', async () => {
              [promise] = something().then(value => {
                expect(value).toBe('red');
              });
            });",
            None,
            None,
        ),
        (
            "test('that we error on this', () => {
              const promise = something().then(value => {
                expect(value).toBe('red');
              });
              log(promise);
            });",
            None,
            None,
        ),
        (
            "it('is valid', async () => {
              const promise = loadNumber().then(number => {
                expect(typeof number).toBe('number');
                return number + 1;
              });
              expect(promise).toBeInstanceOf(Promise);
            });",
            None,
            None,
        ),
        (
            "it('is valid', async () => {
              const promise = loadNumber().then(number => {
                expect(typeof number).toBe('number');
                return number + 1;
              });
              expect(anotherPromise).resolves.toBe(1);
            });",
            None,
            None,
        ),
        (
            "import { it as promiseThatThis } from '@jest/globals';
            promiseThatThis('is valid', async () => {
              const promise = loadNumber().then(number => {
                expect(typeof number).toBe('number');
                return number + 1;
              });
              expect(anotherPromise).resolves.toBe(1);
            });",
            None,
            None,
        ),
        /*
         * jest alias not supported
        (
            "promiseThatThis('is valid', async () => {
              const promise = loadNumber().then(number => {
                expect(typeof number).toBe('number');
                return number + 1;
              });
              expect(anotherPromise).resolves.toBe(1);
            });",
            None,
            Some(
                serde_json::json!({ "settings": { "jest": { "globalAliases": { "xit": ["promiseThatThis"] } } } }),
            ),
        ),
         */
    ];

    Tester::new(ValidExpectInPromise::NAME, ValidExpectInPromise::PLUGIN, pass, fail)
        .with_jest_plugin(true)
        .test_and_snapshot();
}
