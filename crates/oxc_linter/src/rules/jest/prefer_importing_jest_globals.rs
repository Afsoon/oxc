use std::borrow::Cow;

use itertools::Itertools;
use oxc_ast::{
    AstKind,
    ast::{
        Argument, BindingPattern, Expression, ImportDeclarationSpecifier,
        ImportOrExportKind, Statement,
    },
};
use oxc_diagnostics::OxcDiagnostic;
use oxc_macros::declare_oxc_lint;
use oxc_span::{GetSpan, Span};
use rustc_hash::FxHashSet;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    context::LintContext,
    fixer::RuleFixer,
    rule::Rule,
    utils::{
        JestFnKind, JestGeneralFnKind, ParsedJestFnCallNew, PossibleJestNode,
        collect_possible_jest_call_node, parse_jest_fn_call,
    },
};

fn prefer_importing_jest_globals_diagnostic(span: Span, globals: &str) -> OxcDiagnostic {
    OxcDiagnostic::warn(format!(
        "Import the following Jest functions from `@jest/globals`: {globals}"
    ))
    .with_label(span)
}

#[derive(Debug, Clone)]
pub struct PreferImportingJestGlobals(Box<PreferImportingJestGlobalsConfig>);

impl Default for PreferImportingJestGlobals {
    fn default() -> Self {
        Self(Box::new(PreferImportingJestGlobalsConfig::default()))
    }
}

impl std::ops::Deref for PreferImportingJestGlobals {
    type Target = PreferImportingJestGlobalsConfig;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase", default)]
pub struct PreferImportingJestGlobalsConfig {
    /// Jest function types to enforce importing for.
    types: Vec<JestFnType>,
}

impl Default for PreferImportingJestGlobalsConfig {
    fn default() -> Self {
        Self {
            types: vec![
                JestFnType::Hook,
                JestFnType::Describe,
                JestFnType::Test,
                JestFnType::Expect,
                JestFnType::Jest,
                JestFnType::Unknown,
            ],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
enum JestFnType {
    Hook,
    Describe,
    Test,
    Expect,
    Jest,
    Unknown,
}

impl JestFnType {
    fn matches(self, kind: JestFnKind) -> bool {
        match (self, kind) {
            (Self::Hook, JestFnKind::General(JestGeneralFnKind::Hook)) => true,
            (Self::Describe, JestFnKind::General(JestGeneralFnKind::Describe)) => true,
            (Self::Test, JestFnKind::General(JestGeneralFnKind::Test)) => true,
            (Self::Expect, JestFnKind::Expect | JestFnKind::ExpectTypeOf) => true,
            (Self::Jest, JestFnKind::General(JestGeneralFnKind::Jest | JestGeneralFnKind::Vitest)) => true,
            (Self::Unknown, JestFnKind::Unknown) => true,
            _ => false,
        }
    }
}

const IMPORT_SOURCE: &str = "@jest/globals";

declare_oxc_lint!(
    /// ### What it does
    ///
    /// Prefer importing Jest globals (`describe`, `test`, `expect`, etc.) from
    /// `@jest/globals` rather than relying on ambient globals.
    ///
    /// ### Why is this bad?
    ///
    /// Using global Jest functions without explicit imports makes dependencies
    /// implicit and can cause issues with type checking, editor tooling, and
    /// when migrating between test runners.
    ///
    /// ### Examples
    ///
    /// Examples of **incorrect** code for this rule:
    /// ```javascript
    /// describe("suite", () => {
    ///   test("foo");
    ///   expect(true).toBeDefined();
    /// });
    /// ```
    ///
    /// Examples of **correct** code for this rule:
    /// ```javascript
    /// import { describe, expect, test } from '@jest/globals';
    /// describe("suite", () => {
    ///   test("foo");
    ///   expect(true).toBeDefined();
    /// });
    /// ```
    PreferImportingJestGlobals,
    jest,
    style,
    fix
);

impl Rule for PreferImportingJestGlobals {
    fn from_configuration(value: serde_json::Value) -> Result<Self, serde_json::error::Error> {
        if value.is_null() {
            return Ok(Self::default());
        }
        let config: PreferImportingJestGlobalsConfig =
            serde_json::from_value(value.get(0).unwrap_or(&value).clone()).unwrap_or_default();
        Ok(Self(Box::new(config)))
    }

    fn run_once(&self, ctx: &LintContext) {
        let possible_jest_nodes = collect_possible_jest_call_node(ctx);
        let mut functions_to_import: FxHashSet<Cow<str>> = FxHashSet::default();
        let mut reporting_span: Option<Span> = None;

        for jest_node in &possible_jest_nodes {
            // Only care about globals (not already imported from @jest/globals)
            if jest_node.original.is_some() {
                continue;
            }

            let node = jest_node.node;
            let AstKind::CallExpression(call_expr) = node.kind() else {
                continue;
            };

            let Some(jest_fn_call) = parse_jest_fn_call(call_expr, jest_node, ctx) else {
                continue;
            };

            let kind = jest_fn_call.kind();
            if !self.types.iter().any(|t| t.matches(kind)) {
                continue;
            }

            // Get the function name (e.g. "describe", "test", "expect")
            let name = match &jest_fn_call {
                ParsedJestFnCallNew::GeneralJest(call) => call.name.clone(),
                ParsedJestFnCallNew::Expect(call)
                | ParsedJestFnCallNew::ExpectTypeOf(call) => call.name.clone(),
            };
            functions_to_import.insert(name);

            if reporting_span.is_none() {
                reporting_span = Some(call_expr.callee.span());
            }
        }

        if functions_to_import.is_empty() {
            return;
        }

        let Some(span) = reporting_span else { return };
        let globals_list = functions_to_import.iter().sorted().join(", ");

        ctx.diagnostic_with_fix(
            prefer_importing_jest_globals_diagnostic(span, &globals_list),
            |fixer| {
                build_fix(ctx, &fixer, &mut functions_to_import)
            },
        );
    }
}

/// Build the fix: merge with existing imports/requires or create new ones.
fn build_fix<'a>(
    ctx: &LintContext<'a>,
    fixer: &RuleFixer<'_, 'a>,
    functions_to_import: &mut FxHashSet<Cow<str>>,
) -> crate::fixer::RuleFix {
    let program = ctx.nodes().program();

    // 1. Try to merge with existing `import ... from '@jest/globals'` (always ESM)
    if let Some(fix) = try_merge_esm_import(ctx, fixer, functions_to_import) {
        return fix;
    }

    // 2. Try to merge with existing `const { ... } = require('@jest/globals')` (always CJS)
    if let Some(fix) = try_merge_cjs_require(ctx, fixer, functions_to_import) {
        return fix;
    }

    // 3. No existing @jest/globals import/require — create a new one.
    // Use `sourceType` from parser options to determine ESM vs CJS,
    // matching the OG rule's behavior.
    let is_module = ctx.source_type().is_module();
    let import_text = create_import_statement(is_module, functions_to_import);

    // Insert after "use strict" directive if present
    if let Some(last_directive) = program.directives.last() {
        return fixer.insert_text_after_range(last_directive.span, format!("\n{import_text}"));
    }

    // Insert after hashbang if present
    if let Some(hashbang) = &program.hashbang {
        return fixer.insert_text_after_range(hashbang.span, format!("\n{import_text}"));
    }

    // Insert at the top of the file
    fixer.insert_text_before_range(Span::empty(0), format!("{import_text}\n"))
}

fn create_import_statement(is_module: bool, functions: &FxHashSet<Cow<str>>) -> String {
    let sorted = functions.iter().sorted().join(", ");
    if is_module {
        format!("import {{ {sorted} }} from '{IMPORT_SOURCE}';")
    } else {
        format!("const {{ {sorted} }} = require('{IMPORT_SOURCE}');")
    }
}

/// Try to find and replace an existing `import ... from '@jest/globals'`.
/// Merges existing specifiers with the new functions, then replaces the entire import.
fn try_merge_esm_import<'a>(
    ctx: &LintContext<'a>,
    fixer: &RuleFixer<'_, 'a>,
    functions_to_import: &mut FxHashSet<Cow<str>>,
) -> Option<crate::fixer::RuleFix> {
    let is_module = true; // We found an ESM import, so output ESM
    let program = ctx.nodes().program();

    let import_decl = program.body.iter().find_map(|stmt| {
        if let Statement::ImportDeclaration(decl) = stmt {
            if decl.source.value == IMPORT_SOURCE
                && decl.import_kind == ImportOrExportKind::Value
            {
                return Some(decl);
            }
        }
        None
    })?;

    // Merge existing specifiers into the set
    for specifier in import_decl.specifiers.iter().flatten() {
        match specifier {
            ImportDeclarationSpecifier::ImportSpecifier(spec) => {
                let imported = match &spec.imported {
                    oxc_ast::ast::ModuleExportName::IdentifierName(id) => {
                        id.name.as_str().to_string()
                    }
                    oxc_ast::ast::ModuleExportName::IdentifierReference(id) => {
                        id.name.as_str().to_string()
                    }
                    oxc_ast::ast::ModuleExportName::StringLiteral(lit) => {
                        format!("'{}'", lit.value)
                    }
                };
                let local = spec.local.name.as_str();
                if local != imported {
                    functions_to_import.insert(Cow::Owned(format!("{imported} as {local}")));
                } else {
                    functions_to_import.insert(Cow::Owned(imported));
                }
            }
            ImportDeclarationSpecifier::ImportDefaultSpecifier(spec) => {
                functions_to_import.insert(Cow::Owned(spec.local.name.to_string()));
            }
            ImportDeclarationSpecifier::ImportNamespaceSpecifier(_) => {}
        }
    }

    let replacement = create_import_statement(is_module, functions_to_import);
    Some(fixer.replace(import_decl.span, replacement))
}

/// Try to find and replace an existing `const { ... } = require('@jest/globals')`.
/// Merges existing destructured names with the new functions, then replaces entirely.
fn try_merge_cjs_require<'a>(
    ctx: &LintContext<'a>,
    fixer: &RuleFixer<'_, 'a>,
    functions_to_import: &mut FxHashSet<Cow<str>>,
) -> Option<crate::fixer::RuleFix> {
    let program = ctx.nodes().program();
    let is_module = ctx.source_type().is_module();

    // Find `const { ... } = require('@jest/globals')` in the program body
    for stmt in &program.body {
        let Statement::VariableDeclaration(var_decl) = stmt else { continue };
        for declarator in &var_decl.declarations {
            let Some(Expression::CallExpression(call)) = &declarator.init else { continue };

            // Check if it's `require('@jest/globals')`
            let is_jest_require = matches!(&call.callee, Expression::Identifier(id) if id.name == "require")
                && call.arguments.len() == 1
                && is_string_arg_matching(&call.arguments[0], IMPORT_SOURCE);

            if !is_jest_require {
                continue;
            }

            // Merge existing destructured properties.
            // Extract the local binding name from each property.
            // Aliases like `'describe': describe` or `describe: context` are
            // preserved as `describe as context` for ESM or `describe: context` for CJS.
            if let BindingPattern::ObjectPattern(pattern) = &declarator.id {
                for prop in &pattern.properties {
                    // Skip computed properties (e.g. `[() => {}]: it`)
                    if prop.computed {
                        continue;
                    }

                    let Some(key_name) = prop.key.static_name() else {
                        continue;
                    };

                    // Skip non-identifier values (e.g. `describe: []`)
                    let BindingPattern::BindingIdentifier(value_ident) = &prop.value else {
                        continue;
                    };
                    let value_name = value_ident.name.as_str();

                    if key_name == value_name {
                        functions_to_import.insert(Cow::Owned(key_name.to_string()));
                    } else if is_module {
                        functions_to_import
                            .insert(Cow::Owned(format!("{key_name} as {value_name}")));
                    } else {
                        functions_to_import
                            .insert(Cow::Owned(format!("{key_name}: {value_name}")));
                    }
                }
            }

            let replacement = create_import_statement(is_module, functions_to_import);
            return Some(fixer.replace(var_decl.span, replacement));
        }
    }

    None
}

fn is_string_arg_matching(arg: &Argument, value: &str) -> bool {
    arg.as_expression().is_some_and(|expr| match expr {
        Expression::StringLiteral(lit) => lit.value == value,
        Expression::TemplateLiteral(tpl) => {
            tpl.quasis.len() == 1
                && tpl.expressions.is_empty()
                && tpl.quasis.first().is_some_and(|q| q.value.raw == value)
        }
        _ => false,
    })
}


#[test]
fn test() {
    use std::path::PathBuf;

    use crate::tester::Tester;

    let pass = vec![
        (
            "// with import
            import { test, expect } from '@jest/globals';
            test('should pass', () => {
                expect(true).toBeDefined();
            });",
            None,
        ),
        (
            "// with import
            import { 'test' as test, expect } from '@jest/globals';
            test('should pass', () => {
                expect(true).toBeDefined();
            });",
            None,
        ),
        (
            "test('should pass', () => {
                expect(true).toBeDefined();
            });",
            Some(serde_json::json!([{ "types": ["jest"] }])),
        ),
        (
            "const { it } = require('@jest/globals');
            it('should pass', () => {
                expect(true).toBeDefined();
            });",
            Some(serde_json::json!([{ "types": ["test"] }])),
        ),
        (
            "// with require
            const { test, expect } = require('@jest/globals');
            test('should pass', () => {
                expect(true).toBeDefined();
            });",
            None,
        ),
        (
            r"const { test, expect } = require(`@jest/globals`);
            test('should pass', () => {
                expect(true).toBeDefined();
            });",
            None,
        ),
        (
            r#"import { it as itChecks } from '@jest/globals';
            itChecks("foo");"#,
            None,
        ),
        (
            r#"import { 'it' as itChecks } from '@jest/globals';
            itChecks("foo");"#,
            None,
        ),
        (
            r#"const { test } = require('@jest/globals');
            test("foo");"#,
            None,
        ),
        (
            r#"const { test } = require('my-test-library');
            test("foo");"#,
            None,
        ),
    ];

    let fail = vec![
        (
            r#"import describe from '@jest/globals';
            describe("suite", () => {
              test("foo");
              expect(true).toBeDefined();
            })"#,
            None,
        ),
        (
            r#"import { describe as context } from '@jest/globals';
            context("suite", () => {
              test("foo");
              expect(true).toBeDefined();
            })"#,
            None,
        ),
        (
            r#"import { describe as context } from '@jest/globals';
            describe("something", () => {
              context("suite", () => {
                test("foo");
                expect(true).toBeDefined();
              })
            })"#,
            None,
        ),
        (
            r#"import { 'describe' as describe } from '@jest/globals';
            describe("suite", () => {
              test("foo");
              expect(true).toBeDefined();
            })"#,
            None,
        ),
        (
            r#"import { 'describe' as context } from '@jest/globals';
            context("suite", () => {
              test("foo");
              expect(true).toBeDefined();
            })"#,
            None,
        ),
        (
            r#"jest.useFakeTimers();
            describe("suite", () => {
              test("foo");
              expect(true).toBeDefined();
            })"#,
            Some(serde_json::json!([{ "types": ["jest"] }])),
        ),
        (
            r#"import React from 'react';
            import { yourFunction } from './yourFile';
            import something from "something";
            import { test } from '@jest/globals';
            import { xit } from '@jest/globals';
            describe("suite", () => {
              test("foo");
              expect(true).toBeDefined();
            })"#,
            None,
        ),
        (
            r#"console.log('hello');
            import * as fs from 'fs';
            const { test, 'describe': describe } = require('@jest/globals');
            describe("suite", () => {
              test("foo");
              expect(true).toBeDefined();
            })"#,
            None,
        ),
        (
            r#"console.log('hello');
            import jestGlobals from '@jest/globals';
            describe("suite", () => {
              test("foo");
              expect(true).toBeDefined();
            })"#,
            None,
        ),
        (
            r#"import { pending } from 'actions';
            describe('foo', () => {
              test.each(['hello', 'world'])("%s", (a) => {});
            });"#,
            None,
        ),
        (
            r#"const {describe} = require('@jest/globals');
            describe("suite", () => {
              test("foo");
              expect(true).toBeDefined();
            })"#,
            None,
        ),
        (
            r#"const {describe: context} = require('@jest/globals');
            context("suite", () => {
              test("foo");
              expect(true).toBeDefined();
            })"#,
            None,
        ),
        (
            r#"const {describe: context} = require('@jest/globals');
            describe("something", () => {
              context("suite", () => {
                test("foo");
                expect(true).toBeDefined();
              })
            })"#,
            None,
        ),
        (
            r#"const {describe: []} = require('@jest/globals');
            describe("something", () => {
              context("suite", () => {
                test("foo");
                expect(true).toBeDefined();
              })
            })"#,
            None,
        ),
        (
            r#"const {describe} = require(`@jest/globals`);
            describe("suite", () => {
              test("foo");
              expect(true).toBeDefined();
            })"#,
            None,
        ),
        (
            r#"const source = 'globals';
            const {describe} = require(`@jest/${source}`);
            describe("suite", () => {
              test("foo");
              expect(true).toBeDefined();
            })"#,
            None,
        ),
        (
            r#"const { [() => {}]: it } = require('@jest/globals');
            describe("suite", () => {
              test("foo");
              expect(true).toBeDefined();
            })"#,
            None,
        ),
        (
            r#"console.log('hello');
            const fs = require('fs');
            const { test, 'describe': describe } = require('@jest/globals');
            describe("suite", () => {
              test("foo");
              expect(true).toBeDefined();
            })"#,
            None,
        ),
        (
            r#"console.log('hello');
            const jestGlobals = require('@jest/globals');
            describe("suite", () => {
              test("foo");
              expect(true).toBeDefined();
            })"#,
            None,
        ),
        (
            r#"const { pending } = require('actions');
            describe('foo', () => {
              test.each(['hello', 'world'])("%s", (a) => {});
            });"#,
            None,
        ),
        (
            r#"describe("suite", () => {
              test("foo");
              expect(true).toBeDefined();
            })"#,
            None,
        ),
        (
            r#"#!/usr/bin/env node
            describe("suite", () => {
              test("foo");
              expect(true).toBeDefined();
            })"#,
            None,
        ),
        (
            r#"// with comment above
            describe("suite", () => {
              test("foo");
              expect(true).toBeDefined();
            })"#,
            None,
        ),
        (
            r#"'use strict';
            describe("suite", () => {
              test("foo");
              expect(true).toBeDefined();
            })"#,
            None,
        ),
        (
            r#"`use strict`;
            describe("suite", () => {
              test("foo");
              expect(true).toBeDefined();
            })"#,
            None,
        ),
        (
            r#"console.log('hello');
            const onClick = jest.fn();
            describe("suite", () => {
              test("foo");
              expect(onClick).toHaveBeenCalled();
            })"#,
            None,
        ),
        (
            r#"console.log('hello');
            const onClick = jest.fn();
            describe("suite", () => {
              test("foo");
              expect(onClick).toHaveBeenCalled();
            })"#,
            None,
        ),
        (
            r#"import describe from '@jest/globals';
            describe("suite", () => {
              test("foo");
              expect(true).toBeDefined();
            })"#,
            None,
        ),
        (
            r#"const {describe} = require('@jest/globals');
            describe("suite", () => {
              test("foo");
              expect(true).toBeDefined();
            })"#,
            None,
        ),
    ];

    let fix = vec![
        (
            r#"import describe from '@jest/globals';
            describe("suite", () => {
              test("foo");
              expect(true).toBeDefined();
            })"#,
            r#"import { describe, expect, test } from '@jest/globals';
            describe("suite", () => {
              test("foo");
              expect(true).toBeDefined();
            })"#,
            None,
            None,
        ),
        (
            r#"import { describe as context } from '@jest/globals';
            context("suite", () => {
              test("foo");
              expect(true).toBeDefined();
            })"#,
            r#"import { describe as context, expect, test } from '@jest/globals';
            context("suite", () => {
              test("foo");
              expect(true).toBeDefined();
            })"#,
            None,
            None,
        ),
        (
            r#"import { describe as context } from '@jest/globals';
            describe("something", () => {
              context("suite", () => {
                test("foo");
                expect(true).toBeDefined();
              })
            })"#,
            r#"import { describe, describe as context, expect, test } from '@jest/globals';
            describe("something", () => {
              context("suite", () => {
                test("foo");
                expect(true).toBeDefined();
              })
            })"#,
            None,
            None,
        ),
        (
            r#"import { 'describe' as describe } from '@jest/globals';
            describe("suite", () => {
              test("foo");
              expect(true).toBeDefined();
            })"#,
            r#"import { 'describe' as describe, expect, test } from '@jest/globals';
            describe("suite", () => {
              test("foo");
              expect(true).toBeDefined();
            })"#,
            None,
            None,
        ),
        (
            r#"import { 'describe' as context } from '@jest/globals';
            context("suite", () => {
              test("foo");
              expect(true).toBeDefined();
            })"#,
            r#"import { 'describe' as context, expect, test } from '@jest/globals';
            context("suite", () => {
              test("foo");
              expect(true).toBeDefined();
            })"#,
            None,
            None,
        ),
        (
            r#"jest.useFakeTimers();
            describe("suite", () => {
              test("foo");
              expect(true).toBeDefined();
            })"#,
            "import { jest } from '@jest/globals';\njest.useFakeTimers();\n            describe(\"suite\", () => {\n              test(\"foo\");\n              expect(true).toBeDefined();\n            })",
            Some(serde_json::json!([{ "types": ["jest"] }])),
            Some(PathBuf::from("test.mjs")),
        ),
        (
            r#"import React from 'react';
            import { yourFunction } from './yourFile';
            import something from "something";
            import { test } from '@jest/globals';
            import { xit } from '@jest/globals';
            describe("suite", () => {
              test("foo");
              expect(true).toBeDefined();
            })"#,
            r#"import React from 'react';
            import { yourFunction } from './yourFile';
            import something from "something";
            import { describe, expect, test } from '@jest/globals';
            import { xit } from '@jest/globals';
            describe("suite", () => {
              test("foo");
              expect(true).toBeDefined();
            })"#,
            None,
            None,
        ),
        (
            r#"console.log('hello');
            import * as fs from 'fs';
            const { test, 'describe': describe } = require('@jest/globals');
            describe("suite", () => {
              test("foo");
              expect(true).toBeDefined();
            })"#,
            r#"console.log('hello');
            import * as fs from 'fs';
            import { describe, expect, test } from '@jest/globals';
            describe("suite", () => {
              test("foo");
              expect(true).toBeDefined();
            })"#,
            None,
            None,
        ),
        (
            r#"console.log('hello');
            import jestGlobals from '@jest/globals';
            describe("suite", () => {
              test("foo");
              expect(true).toBeDefined();
            })"#,
            r#"console.log('hello');
            import { describe, expect, jestGlobals, test } from '@jest/globals';
            describe("suite", () => {
              test("foo");
              expect(true).toBeDefined();
            })"#,
            None,
            None,
        ),
        (
            r#"import { pending } from 'actions';
            describe('foo', () => {
              test.each(['hello', 'world'])("%s", (a) => {});
            });"#,
            "import { describe, test } from '@jest/globals';\nimport { pending } from 'actions';\n            describe('foo', () => {\n              test.each(['hello', 'world'])(\"%s\", (a) => {});\n            });",
            None,
            None,
        ),
        (
            r#"const {describe} = require('@jest/globals');
            describe("suite", () => {
              test("foo");
              expect(true).toBeDefined();
            })"#,
            r#"const { describe, expect, test } = require('@jest/globals');
            describe("suite", () => {
              test("foo");
              expect(true).toBeDefined();
            })"#,
            None,
            None,
        ),
        (
            r#"const {describe: context} = require('@jest/globals');
            context("suite", () => {
              test("foo");
              expect(true).toBeDefined();
            })"#,
            r#"const { describe: context, expect, test } = require('@jest/globals');
            context("suite", () => {
              test("foo");
              expect(true).toBeDefined();
            })"#,
            None,
            None,
        ),
        (
            r#"const {describe: context} = require('@jest/globals');
            describe("something", () => {
              context("suite", () => {
                test("foo");
                expect(true).toBeDefined();
              })
            })"#,
            r#"const { describe, describe: context, expect, test } = require('@jest/globals');
            describe("something", () => {
              context("suite", () => {
                test("foo");
                expect(true).toBeDefined();
              })
            })"#,
            None,
            None,
        ),
        (
            r#"const {describe: []} = require('@jest/globals');
            describe("something", () => {
              context("suite", () => {
                test("foo");
                expect(true).toBeDefined();
              })
            })"#,
            r#"const { describe, expect, test } = require('@jest/globals');
            describe("something", () => {
              context("suite", () => {
                test("foo");
                expect(true).toBeDefined();
              })
            })"#,
            None,
            None,
        ),
        (
            r#"const {describe} = require(`@jest/globals`);
            describe("suite", () => {
              test("foo");
              expect(true).toBeDefined();
            })"#,
            r#"const { describe, expect, test } = require('@jest/globals');
            describe("suite", () => {
              test("foo");
              expect(true).toBeDefined();
            })"#,
            None,
            None,
        ),
        (
            r#"const source = 'globals';
            const {describe} = require(`@jest/${source}`);
            describe("suite", () => {
              test("foo");
              expect(true).toBeDefined();
            })"#,
            "const { expect, test } = require('@jest/globals');\nconst source = 'globals';\n            const {describe} = require(`@jest/${source}`);\n            describe(\"suite\", () => {\n              test(\"foo\");\n              expect(true).toBeDefined();\n            })",
            None,
            None,
        ),
        (
            r#"const { [() => {}]: it } = require('@jest/globals');
            describe("suite", () => {
              test("foo");
              expect(true).toBeDefined();
            })"#,
            r#"const { describe, expect, test } = require('@jest/globals');
            describe("suite", () => {
              test("foo");
              expect(true).toBeDefined();
            })"#,
            None,
            None,
        ),
        (
            r#"console.log('hello');
            const fs = require('fs');
            const { test, 'describe': describe } = require('@jest/globals');
            describe("suite", () => {
              test("foo");
              expect(true).toBeDefined();
            })"#,
            r#"console.log('hello');
            const fs = require('fs');
            const { describe, expect, test } = require('@jest/globals');
            describe("suite", () => {
              test("foo");
              expect(true).toBeDefined();
            })"#,
            None,
            None,
        ),
        (
            r#"console.log('hello');
            const jestGlobals = require('@jest/globals');
            describe("suite", () => {
              test("foo");
              expect(true).toBeDefined();
            })"#,
            r#"console.log('hello');
            const { describe, expect, test } = require('@jest/globals');
            describe("suite", () => {
              test("foo");
              expect(true).toBeDefined();
            })"#,
            None,
            None,
        ),
        (
            r#"const { pending } = require('actions');
            describe('foo', () => {
              test.each(['hello', 'world'])("%s", (a) => {});
            });"#,
            "const { describe, test } = require('@jest/globals');\nconst { pending } = require('actions');\n            describe('foo', () => {\n              test.each(['hello', 'world'])(\"%s\", (a) => {});\n            });",
            None,
            None,
        ),
        (
            r#"describe("suite", () => {
              test("foo");
              expect(true).toBeDefined();
            })"#,
            "const { describe, expect, test } = require('@jest/globals');\ndescribe(\"suite\", () => {\n              test(\"foo\");\n              expect(true).toBeDefined();\n            })",
            None,
            None,
        ),
        (
            r#"#!/usr/bin/env node
            describe("suite", () => {
              test("foo");
              expect(true).toBeDefined();
            })"#,
            "#!/usr/bin/env node\nconst { describe, expect, test } = require('@jest/globals');\n            describe(\"suite\", () => {\n              test(\"foo\");\n              expect(true).toBeDefined();\n            })",
            None,
            None,
        ),
        (
            r#"// with comment above
            describe("suite", () => {
              test("foo");
              expect(true).toBeDefined();
            })"#,
            "const { describe, expect, test } = require('@jest/globals');\n// with comment above\n            describe(\"suite\", () => {\n              test(\"foo\");\n              expect(true).toBeDefined();\n            })",
            None,
            None,
        ),
        (
            r#"'use strict';
            describe("suite", () => {
              test("foo");
              expect(true).toBeDefined();
            })"#,
            "'use strict';\nconst { describe, expect, test } = require('@jest/globals');\n            describe(\"suite\", () => {\n              test(\"foo\");\n              expect(true).toBeDefined();\n            })",
            None,
            None,
        ),
        (
            r#"`use strict`;
            describe("suite", () => {
              test("foo");
              expect(true).toBeDefined();
            })"#,
            "const { describe, expect, test } = require('@jest/globals');\n`use strict`;\n            describe(\"suite\", () => {\n              test(\"foo\");\n              expect(true).toBeDefined();\n            })",
            None,
            None,
        ),
        (
            r#"console.log('hello');
            const onClick = jest.fn();
            describe("suite", () => {
              test("foo");
              expect(onClick).toHaveBeenCalled();
            })"#,
            "const { describe, expect, jest, test } = require('@jest/globals');\nconsole.log('hello');\n            const onClick = jest.fn();\n            describe(\"suite\", () => {\n              test(\"foo\");\n              expect(onClick).toHaveBeenCalled();\n            })",
            None,
            None,
        ),
        (
            r#"console.log('hello');
            const onClick = jest.fn();
            describe("suite", () => {
              test("foo");
              expect(onClick).toHaveBeenCalled();
            })"#,
            "import { describe, expect, jest, test } from '@jest/globals';\nconsole.log('hello');\n            const onClick = jest.fn();\n            describe(\"suite\", () => {\n              test(\"foo\");\n              expect(onClick).toHaveBeenCalled();\n            })",
            None,
            Some(PathBuf::from("test.mjs")),
        ),
        (
            r#"import describe from '@jest/globals';
            describe("suite", () => {
              test("foo");
              expect(true).toBeDefined();
            })"#,
            r#"import { describe, expect, test } from '@jest/globals';
            describe("suite", () => {
              test("foo");
              expect(true).toBeDefined();
            })"#,
            None,
            None,
        ),
        (
            r#"const {describe} = require('@jest/globals');
            describe("suite", () => {
              test("foo");
              expect(true).toBeDefined();
            })"#,
            r#"const { describe, expect, test } = require('@jest/globals');
            describe("suite", () => {
              test("foo");
              expect(true).toBeDefined();
            })"#,
            None,
            None,
        ),
    ];

    Tester::new(PreferImportingJestGlobals::NAME, PreferImportingJestGlobals::PLUGIN, pass, fail)
        .expect_fix(fix)
        .with_jest_plugin(true)
        .test_and_snapshot();
}
