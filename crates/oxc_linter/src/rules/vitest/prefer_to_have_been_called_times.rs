use oxc_macros::declare_oxc_lint;

use crate::{
    context::LintContext,
    rule::Rule,
    rules::shared::prefer_to_have_been_called_times::{DOCUMENTATION_VITEST, run},
    utils::PossibleJestNode,
};

#[derive(Debug, Default, Clone)]
pub struct PreferToHaveBeenCalledTimes;

declare_oxc_lint!(
    PreferToHaveBeenCalledTimes,
    vitest,
    style,
    fix,
    docs = DOCUMENTATION_VITEST,
    version = "1.34.0",
);

impl Rule for PreferToHaveBeenCalledTimes {
    fn run_on_jest_node<'a, 'c>(
        &self,
        jest_node: &PossibleJestNode<'a, 'c>,
        ctx: &'c LintContext<'a>,
    ) {
        run(jest_node, ctx);
    }
}

#[test]
fn test() {
    use crate::tester::Tester;

    let mut pass = vec![
        "expect.assertions(1)",
        "expect(fn).toHaveBeenCalledTimes",
        "expect(fn.mock.calls).toHaveLength",
        "expect(fn.mock.values).toHaveLength(0)",
        "expect(fn.values.calls).toHaveLength(0)",
        "expect(fn).toHaveBeenCalledTimes(0)",
        "expect(fn).resolves.toHaveBeenCalledTimes(10)",
        "expect(fn).not.toHaveBeenCalledTimes(10)",
        "expect(fn).toHaveBeenCalledTimes(1)",
        "expect(fn).toBeCalledTimes(0);",
        "expect(fn).toHaveBeenCalledTimes(0);",
        "expect(fn);",
        "expect(method.mock.calls[0][0]).toStrictEqual(value);",
        "expect(fn.mock.length).toEqual(1);",
        "expect(fn.mock.calls).toEqual([]);",
        "expect(fn.mock.calls).toContain(1, 2, 3);",
    ];

    let mut fail = vec![
        "expect(method.mock.calls).toHaveLength(1);",
        "expect(method.mock.calls).resolves.toHaveLength(x);",
        r#"expect(method["mock"].calls).toHaveLength(0);"#,
        "expect(my.method.mock.calls).not.toHaveLength(0);",
    ];

    let mut fix = vec![
        (
            "expect(method.mock.calls).toHaveLength(1);",
            "expect(method).toHaveBeenCalledTimes(1);",
            None,
        ),
        (
            "expect(method.mock.calls).toHaveLength(
                1,
            );",
            "expect(method).toHaveBeenCalledTimes(1);",
            None,
        ),
        (
            "expect(method.mock.calls).toHaveLength(
                /* number of calls (one) */
                1,
            );",
            "expect(method).toHaveBeenCalledTimes(1);",
            None,
        ),
        (
            "expect(method.mock.calls).resolves.toHaveLength(x);",
            "expect(method).resolves.toHaveBeenCalledTimes(x);",
            None,
        ),
        (
            r#"expect(method["mock"].calls).toHaveLength(0);"#,
            "expect(method).toHaveBeenCalledTimes(0);",
            None,
        ),
        (
            "expect(my.method.mock.calls).not.toHaveLength(0);",
            "expect(my.method).not.toHaveBeenCalledTimes(0);",
            None,
        ),
    ];

    let pass_vitest = vec![
        "expect.assertions(1)",
        "expect(fn).toHaveBeenCalledTimes",
        "expect(fn.mock.calls).toHaveLength",
        "expect(fn.mock.values).toHaveLength(0)",
        "expect(fn.values.calls).toHaveLength(0)",
        "expect(fn).toHaveBeenCalledTimes(0)",
        "expect(fn).resolves.toHaveBeenCalledTimes(10)",
        "expect(fn).not.toHaveBeenCalledTimes(10)",
        "expect(fn).toHaveBeenCalledTimes(1)",
        "expect(fn).toBeCalledTimes(0);",
        "expect(fn).toHaveBeenCalledTimes(0);",
        "expect(fn);",
        "expect(method.mock.calls[0][0]).toStrictEqual(value);",
        "expect(fn.mock.length).toEqual(1);",
        "expect(fn.mock.calls).toEqual([]);",
        "expect(fn.mock.calls).toContain(1, 2, 3);",
    ];

    pass.extend(pass_vitest);

    let fail_vitest = vec![
        "expect(method.mock.calls).toHaveLength(1);",
        "expect(method.mock.calls).resolves.toHaveLength(x);",
        r#"expect(method["mock"].calls).toHaveLength(0);"#,
        "expect(my.method.mock.calls).not.toHaveLength(0);",
    ];

    fail.extend(fail_vitest);

    let fix_vitest = vec![
        (
            "expect(method.mock.calls).toHaveLength(1);",
            "expect(method).toHaveBeenCalledTimes(1);",
            None,
        ),
        (
            "expect(method.mock.calls).resolves.toHaveLength(x);",
            "expect(method).resolves.toHaveBeenCalledTimes(x);",
            None,
        ),
        (
            r#"expect(method["mock"].calls).toHaveLength(0);"#,
            "expect(method).toHaveBeenCalledTimes(0);",
            None,
        ),
        (
            "expect(my.method.mock.calls).not.toHaveLength(0);",
            "expect(my.method).not.toHaveBeenCalledTimes(0);",
            None,
        ),
    ];

    fix.extend(fix_vitest);

    Tester::new(PreferToHaveBeenCalledTimes::NAME, PreferToHaveBeenCalledTimes::PLUGIN, pass, fail)
        .with_vitest_plugin(true)
        .expect_fix(fix)
        .test_and_snapshot();
}
