use cairo_lang_compiler::db::RootDatabase;
use cairo_lang_defs::plugin::PluginDiagnostic;
use cairo_lang_filesystem::span::TextSpan;
use cairo_lang_semantic::diagnostic::SemanticDiagnosticKind;
use cairo_lang_semantic::SemanticDiagnostic;
use cairo_lang_syntax::node::ast::{Expr, ExprBinary, ExprMatch, Pattern};
use cairo_lang_syntax::node::db::SyntaxGroup;
use cairo_lang_syntax::node::{SyntaxNode, TypedSyntaxNode};
use cairo_lang_utils::Upcast;
use log::debug;

use crate::lints::bool_comparison::generate_fixed_text_for_comparison;
use crate::lints::double_comparison;
use crate::lints::single_match::is_expr_unit;
use crate::plugin::{diagnostic_kind_from_message, CairoLintKind};

mod import_fixes;
pub use import_fixes::{apply_import_fixes, collect_unused_imports, ImportFix};

/// Represents a fix for a diagnostic, containing the span of code to be replaced
/// and the suggested replacement.
#[derive(Debug, Clone)]
pub struct Fix {
    pub span: TextSpan,
    pub suggestion: String,
}

/// Attempts to fix a semantic diagnostic.
///
/// This function is the entry point for fixing semantic diagnostics. It examines the
/// diagnostic kind and delegates to specific fix functions based on the diagnostic type.
///
/// # Arguments
///
/// * `db` - A reference to the RootDatabase
/// * `diag` - A reference to the SemanticDiagnostic to be fixed
///
/// # Returns
///
/// An `Option<(SyntaxNode, String)>` where the `SyntaxNode` represents the node to be
/// replaced, and the `String` is the suggested replacement. Returns `None` if no fix
/// is available for the given diagnostic.
pub fn fix_semantic_diagnostic(db: &RootDatabase, diag: &SemanticDiagnostic) -> Option<(SyntaxNode, String)> {
    match diag.kind {
        SemanticDiagnosticKind::UnusedVariable => Fixer.fix_unused_variable(db, diag),
        SemanticDiagnosticKind::PluginDiagnostic(ref plugin_diag) => Fixer.fix_plugin_diagnostic(db, diag, plugin_diag),
        SemanticDiagnosticKind::UnusedImport(_) => {
            debug!("Unused imports should be handled in preemptively");
            None
        }
        _ => {
            debug!("No fix available for diagnostic: {:?}", diag.kind);
            None
        }
    }
}

#[derive(Default)]
pub struct Fixer;
impl Fixer {
    /// Fixes an unused variable by prefixing it with an underscore.
    ///
    /// # Arguments
    ///
    /// * `db` - A reference to the RootDatabase
    /// * `diag` - A reference to the SemanticDiagnostic for the unused variable
    ///
    /// # Returns
    ///
    /// An `Option<(SyntaxNode, String)>` containing the node to be replaced and the
    /// suggested replacement (the variable name prefixed with an underscore).
    pub fn fix_unused_variable(&self, db: &RootDatabase, diag: &SemanticDiagnostic) -> Option<(SyntaxNode, String)> {
        let node = diag.stable_location.syntax_node(db.upcast());
        let suggestion = format!("_{}", node.get_text(db.upcast()));
        Some((node, suggestion))
    }

    /// Fixes a destructuring match by converting it to an if-let expression.
    ///
    /// This method handles matches with two arms, where one arm is a wildcard (_)
    /// and the other is either an enum or struct pattern.
    ///
    /// # Arguments
    ///
    /// * `db` - A reference to the SyntaxGroup
    /// * `node` - The SyntaxNode representing the match expression
    ///
    /// # Returns
    ///
    /// A `String` containing the if-let expression that replaces the match.
    ///
    /// # Panics
    ///
    /// Panics if the diagnostic is incorrect (i.e., the match doesn't have the expected structure).
    pub fn fix_destruct_match(&self, db: &dyn SyntaxGroup, node: SyntaxNode) -> String {
        let match_expr = ExprMatch::from_syntax_node(db, node.clone());
        let arms = match_expr.arms(db).elements(db);
        let first_arm = &arms[0];
        let second_arm = &arms[1];
        let (pattern, first_expr) =
            match (&first_arm.patterns(db).elements(db)[0], &second_arm.patterns(db).elements(db)[0]) {
                (Pattern::Underscore(_), Pattern::Enum(pat)) => (pat.as_syntax_node(), second_arm),
                (Pattern::Enum(pat), Pattern::Underscore(_)) => (pat.as_syntax_node(), first_arm),
                (Pattern::Underscore(_), Pattern::Struct(pat)) => (pat.as_syntax_node(), second_arm),
                (Pattern::Struct(pat), Pattern::Underscore(_)) => (pat.as_syntax_node(), first_arm),
                (Pattern::Enum(pat1), Pattern::Enum(pat2)) => {
                    if is_expr_unit(second_arm.expression(db), db) {
                        (pat1.as_syntax_node(), first_arm)
                    } else {
                        (pat2.as_syntax_node(), second_arm)
                    }
                }
                (_, _) => panic!("Incorrect diagnostic"),
            };
        let mut pattern_span = pattern.span(db);
        pattern_span.end = pattern.span_start_without_trivia(db);
        let indent = node.get_text(db).chars().take_while(|c| c.is_whitespace()).collect::<String>();
        let trivia = pattern.clone().get_text_of_span(db, pattern_span).trim().to_string();
        let trivia = if trivia.is_empty() { trivia } else { format!("{indent}{trivia}\n") };
        format!(
            "{trivia}{indent}if let {} = {} {{ {} }}",
            pattern.get_text_without_trivia(db),
            match_expr.expr(db).as_syntax_node().get_text_without_trivia(db),
            first_expr.expression(db).as_syntax_node().get_text_without_trivia(db),
        )
    }

    /// Fixes a plugin diagnostic by delegating to the appropriate Fixer method.
    ///
    /// # Arguments
    ///
    /// * `db` - A reference to the RootDatabase
    /// * `diag` - A reference to the SemanticDiagnostic
    /// * `plugin_diag` - A reference to the PluginDiagnostic
    ///
    /// # Returns
    ///
    /// An `Option<(SyntaxNode, String)>` containing the node to be replaced and the
    /// suggested replacement.
    pub fn fix_plugin_diagnostic(
        &self,
        db: &RootDatabase,
        semantic_diag: &SemanticDiagnostic,
        plugin_diag: &PluginDiagnostic,
    ) -> Option<(SyntaxNode, String)> {
        let new_text = match diagnostic_kind_from_message(&plugin_diag.message) {
            CairoLintKind::DoubleParens => {
                self.fix_double_parens(db.upcast(), plugin_diag.stable_ptr.lookup(db.upcast()))
            }
            CairoLintKind::DestructMatch => self.fix_destruct_match(db, plugin_diag.stable_ptr.lookup(db.upcast())),
            CairoLintKind::DoubleComparison => {
                self.fix_double_comparison(db.upcast(), plugin_diag.stable_ptr.lookup(db.upcast()))
            }
            CairoLintKind::BreakUnit => self.fix_break_unit(db, plugin_diag.stable_ptr.lookup(db.upcast())),
            CairoLintKind::BoolComparison => self.fix_bool_comparison(
                db,
                ExprBinary::from_syntax_node(db.upcast(), plugin_diag.stable_ptr.lookup(db.upcast())),
            ),
            _ => return None,
        };

        Some((semantic_diag.stable_location.syntax_node(db.upcast()), new_text))
    }

    pub fn fix_break_unit(&self, db: &dyn SyntaxGroup, node: SyntaxNode) -> String {
        node.get_text(db).replace("break ();", "break;").to_string()
    }

    pub fn fix_bool_comparison(&self, db: &dyn SyntaxGroup, node: ExprBinary) -> String {
        let lhs = node.lhs(db).as_syntax_node().get_text(db);
        let rhs = node.rhs(db).as_syntax_node().get_text(db);

        let result = generate_fixed_text_for_comparison(db, lhs.as_str(), rhs.as_str(), node.clone());
        result
    }

    /// Removes unnecessary double parentheses from a syntax node.
    ///
    /// Simplifies an expression by stripping extra layers of parentheses while preserving
    /// the original formatting and indentation.
    ///
    /// # Arguments
    ///
    /// * `db` - Reference to the `SyntaxGroup` for syntax tree access.
    /// * `node` - The `SyntaxNode` containing the expression.
    ///
    /// # Returns
    ///
    /// A `String` with the simplified expression.
    ///
    /// # Example
    ///
    /// Input: `((x + y))`
    /// Output: `x + y`
    pub fn fix_double_parens(&self, db: &dyn SyntaxGroup, node: SyntaxNode) -> String {
        let mut expr = Expr::from_syntax_node(db, node.clone());

        while let Expr::Parenthesized(inner_expr) = expr {
            expr = inner_expr.expr(db);
        }

        format!(
            "{}{}",
            node.get_text(db).chars().take_while(|c| c.is_whitespace()).collect::<String>(),
            expr.as_syntax_node().get_text_without_trivia(db),
        )
    }

    pub fn fix_double_comparison(&self, db: &dyn SyntaxGroup, node: SyntaxNode) -> String {
        let expr = Expr::from_syntax_node(db, node.clone());

        if let Expr::Binary(binary_op) = expr {
            let lhs = binary_op.lhs(db);
            let rhs = binary_op.rhs(db);
            let middle_op = binary_op.op(db);

            if let (Some(lhs_op), Some(rhs_op)) = (
                double_comparison::extract_binary_operator_expr(&lhs, db),
                double_comparison::extract_binary_operator_expr(&rhs, db),
            ) {
                let simplified_op = double_comparison::determine_simplified_operator(&lhs_op, &rhs_op, &middle_op);

                if let Some(simplified_op) = simplified_op {
                    if let Some(operator_to_replace) = double_comparison::operator_to_replace(lhs_op) {
                        let lhs_text = lhs.as_syntax_node().get_text(db).replace(operator_to_replace, simplified_op);
                        return lhs_text.to_string();
                    }
                }
            }
        }

        node.get_text(db).to_string()
    }
}
