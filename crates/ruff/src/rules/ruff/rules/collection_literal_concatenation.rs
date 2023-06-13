use itertools::Itertools;
use ruff_text_size::TextRange;
use rustpython_parser::ast::{self, Expr, ExprContext, Operator, Ranged};

use ruff_diagnostics::{AutofixKind, Diagnostic, Edit, Fix, Violation};
use ruff_macros::{derive_message_formats, violation};
use ruff_python_ast::helpers::has_comments;
use ruff_python_ast::verbatim_ast;

use crate::checkers::ast::Checker;
use crate::registry::AsRule;

#[violation]
pub struct CollectionLiteralConcatenation {
    expr: String,
}

impl Violation for CollectionLiteralConcatenation {
    const AUTOFIX: AutofixKind = AutofixKind::Sometimes;

    #[derive_message_formats]
    fn message(&self) -> String {
        let CollectionLiteralConcatenation { expr } = self;
        format!("Consider `{expr}` instead of concatenation")
    }

    fn autofix_title(&self) -> Option<String> {
        let CollectionLiteralConcatenation { expr } = self;
        Some(format!("Replace with `{expr}`"))
    }
}

fn make_splat_elts(
    splat_element: verbatim_ast::Expr,
    other_elements: &[Expr],
    splat_at_left: bool,
) -> Vec<verbatim_ast::Expr> {
    let mut new_elts = other_elements
        .iter()
        .map(|e| verbatim_ast::Expr::Verbatim(verbatim_ast::ExprVerbatim { range: e.range() }))
        .collect_vec();
    if splat_at_left {
        new_elts.insert(0, splat_element);
    } else {
        new_elts.push(splat_element);
    }
    new_elts
}

#[derive(Debug, Copy, Clone)]
enum Type {
    List,
    Tuple,
}

/// Recursively merge all the tuples and lists in the expression.
fn concatenate_expressions(expr: &Expr) -> Option<(verbatim_ast::Expr, Type)> {
    let Expr::BinOp(ast::ExprBinOp { left, op: Operator::Add, right, range: _ }) = expr else {
        return None;
    };

    let new_left = match left.as_ref() {
        Expr::BinOp(ast::ExprBinOp { .. }) => match concatenate_expressions(left) {
            Some((new_left, _)) => new_left,
            None => verbatim_ast::Expr::from(left),
        },
        _ => verbatim_ast::Expr::from(left),
    };

    let new_right = match right.as_ref() {
        Expr::BinOp(ast::ExprBinOp { .. }) => match concatenate_expressions(right) {
            Some((new_right, _)) => new_right,
            None => verbatim_ast::Expr::from(right),
        },
        _ => verbatim_ast::Expr::from(right),
    };

    // Figure out which way the splat is, and the type of the collection.
    let (type_, splat_element, other_elements, splat_at_left) = match (&new_left, &new_right) {
        (Expr::List(ast::ExprList { elts: l_elts, .. }), _) => (
            Type::List,
            new_right,
            l_elts.iter().map(verbatim_ast::Expr::from).collect(),
            false,
        ),
        (Expr::Tuple(ast::ExprTuple { elts: l_elts, .. }), _) => (
            Type::Tuple,
            new_right,
            l_elts.iter().map(verbatim_ast::Expr::from).collect(),
            false,
        ),
        (_, Expr::List(ast::ExprList { elts: r_elts, .. })) => (
            Type::List,
            new_left,
            r_elts.iter().map(verbatim_ast::Expr::from).collect(),
            true,
        ),
        (_, Expr::Tuple(ast::ExprTuple { elts: r_elts, .. })) => (
            Type::Tuple,
            new_left,
            r_elts.iter().map(verbatim_ast::Expr::from).collect(),
            true,
        ),
        _ => return None,
    };

    let new_elts = match &splat_element {
        // We'll be a bit conservative here; only calls, names and attribute accesses
        // will be considered as splat elements.
        Expr::Call(_) | Expr::Attribute(_) | Expr::Name(_) => {
            make_splat_elts(splat_element, other_elements, splat_at_left)
        }
        // If the splat element is itself a list/tuple, insert them in the other list/tuple.
        Expr::List(ast::ExprList { elts, .. }) if matches!(type_, Type::List) => {
            other_elements.iter().chain(elts.iter()).cloned().collect()
        }
        Expr::Tuple(ast::ExprTuple { elts, .. }) if matches!(type_, Type::Tuple) => {
            other_elements.iter().chain(elts.iter()).cloned().collect()
        }
        _ => return None,
    };

    let new_expr = match type_ {
        Type::List => verbatim_ast::Expr::List(verbatim_ast::ExprList { elts: new_elts }),
        Type::Tuple => verbatim_ast::Expr::Tuple(verbatim_ast::ExprTuple { elts: new_elts }),
    };

    Some((new_expr, type_))
}

/// RUF005
pub(crate) fn collection_literal_concatenation(checker: &mut Checker, expr: &Expr) {
    // If the expression is already a child of an addition, we'll have analyzed it already.
    if matches!(
        checker.semantic_model().expr_parent(),
        Some(Expr::BinOp(ast::ExprBinOp {
            op: Operator::Add,
            ..
        }))
    ) {
        return;
    }

    let Some((new_expr, type_)) = concatenate_expressions(expr) else {
        return
    };

    let contents = match type_ {
        // Wrap the new expression in parentheses if it was a tuple.
        Type::Tuple => format!("({})", checker.verbatim_generator().expr(&new_expr)),
        Type::List => checker.verbatim_generator().expr(&new_expr),
    };
    let mut diagnostic = Diagnostic::new(
        CollectionLiteralConcatenation {
            expr: contents.clone(),
        },
        expr.range(),
    );
    if checker.patch(diagnostic.kind.rule()) {
        if !has_comments(expr, checker.locator) {
            // This suggestion could be unsafe if the non-literal expression in the
            // expression has overridden the `__add__` (or `__radd__`) magic methods.
            diagnostic.set_fix(Fix::suggested(Edit::range_replacement(
                contents,
                expr.range(),
            )));
        }
    }
    checker.diagnostics.push(diagnostic);
}
