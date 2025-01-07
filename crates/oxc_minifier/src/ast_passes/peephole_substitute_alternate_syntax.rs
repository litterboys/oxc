use oxc_allocator::Vec;
use oxc_ast::{ast::*, NONE};
use oxc_ecmascript::{
    constant_evaluation::{ConstantEvaluation, ValueType},
    ToInt32, ToJsString, ToNumber,
};
use oxc_semantic::IsGlobalReference;
use oxc_span::{GetSpan, SPAN};
use oxc_syntax::{
    es_target::ESTarget,
    identifier::is_identifier_name,
    number::NumberBase,
    operator::{BinaryOperator, UnaryOperator},
};
use oxc_traverse::{traverse_mut_with_ctx, Ancestor, ReusableTraverseCtx, Traverse, TraverseCtx};

use crate::{node_util::Ctx, CompressorPass};

/// A peephole optimization that minimizes code by simplifying conditional
/// expressions, replacing IFs with HOOKs, replacing object constructors
/// with literals, and simplifying returns.
/// <https://github.com/google/closure-compiler/blob/v20240609/src/com/google/javascript/jscomp/PeepholeSubstituteAlternateSyntax.java>
pub struct PeepholeSubstituteAlternateSyntax {
    target: ESTarget,
    /// Do not compress syntaxes that are hard to analyze inside the fixed loop.
    /// e.g. Do not compress `undefined -> void 0`, `true` -> `!0`.
    /// Opposite of `late` in Closure Compiler.
    in_fixed_loop: bool,

    // states
    in_define_export: bool,

    pub(crate) changed: bool,
}

impl<'a> CompressorPass<'a> for PeepholeSubstituteAlternateSyntax {
    fn build(&mut self, program: &mut Program<'a>, ctx: &mut ReusableTraverseCtx<'a>) {
        self.changed = false;
        traverse_mut_with_ctx(self, program, ctx);
    }
}

impl<'a> Traverse<'a> for PeepholeSubstituteAlternateSyntax {
    fn exit_catch_clause(&mut self, catch: &mut CatchClause<'a>, ctx: &mut TraverseCtx<'a>) {
        self.compress_catch_clause(catch, ctx);
    }

    fn exit_object_property(&mut self, prop: &mut ObjectProperty<'a>, ctx: &mut TraverseCtx<'a>) {
        self.try_compress_property_key(&mut prop.key, &mut prop.computed, ctx);
    }

    fn exit_assignment_target_property_property(
        &mut self,
        prop: &mut AssignmentTargetPropertyProperty<'a>,
        ctx: &mut TraverseCtx<'a>,
    ) {
        self.try_compress_property_key(&mut prop.name, &mut prop.computed, ctx);
    }

    fn exit_binding_property(&mut self, prop: &mut BindingProperty<'a>, ctx: &mut TraverseCtx<'a>) {
        self.try_compress_property_key(&mut prop.key, &mut prop.computed, ctx);
    }

    fn exit_method_definition(
        &mut self,
        prop: &mut MethodDefinition<'a>,
        ctx: &mut TraverseCtx<'a>,
    ) {
        self.try_compress_property_key(&mut prop.key, &mut prop.computed, ctx);
    }

    fn exit_property_definition(
        &mut self,
        prop: &mut PropertyDefinition<'a>,
        ctx: &mut TraverseCtx<'a>,
    ) {
        self.try_compress_property_key(&mut prop.key, &mut prop.computed, ctx);
    }

    fn exit_accessor_property(
        &mut self,
        prop: &mut AccessorProperty<'a>,
        ctx: &mut TraverseCtx<'a>,
    ) {
        self.try_compress_property_key(&mut prop.key, &mut prop.computed, ctx);
    }

    fn exit_return_statement(
        &mut self,
        stmt: &mut ReturnStatement<'a>,
        _ctx: &mut TraverseCtx<'a>,
    ) {
        // We may fold `void 1` to `void 0`, so compress it after visiting
        self.compress_return_statement(stmt);
    }

    fn exit_variable_declaration(
        &mut self,
        decl: &mut VariableDeclaration<'a>,
        ctx: &mut TraverseCtx<'a>,
    ) {
        for declarator in decl.declarations.iter_mut() {
            self.compress_variable_declarator(declarator, Ctx(ctx));
        }
    }

    /// Set `in_define_export` flag if this is a top-level statement of form:
    /// ```js
    /// Object.defineProperty(exports, 'Foo', {
    ///   enumerable: true,
    ///   get: function() { return Foo_1.Foo; }
    /// });
    /// ```
    fn enter_call_expression(
        &mut self,
        call_expr: &mut CallExpression<'a>,
        ctx: &mut TraverseCtx<'a>,
    ) {
        if ctx.parent().is_expression_statement()
            && Self::is_object_define_property_exports(call_expr)
        {
            self.in_define_export = true;
        }
    }

    fn exit_call_expression(&mut self, expr: &mut CallExpression<'a>, ctx: &mut TraverseCtx<'a>) {
        self.in_define_export = false;

        self.try_compress_call_expression_arguments(expr, ctx);
    }

    fn exit_expression(&mut self, expr: &mut Expression<'a>, ctx: &mut TraverseCtx<'a>) {
        let ctx = Ctx(ctx);

        // Change syntax
        match expr {
            Expression::ArrowFunctionExpression(e) => self.try_compress_arrow_expression(e, ctx),
            Expression::ChainExpression(e) => self.try_compress_chain_call_expression(e, ctx),
            Expression::BinaryExpression(e) => {
                Self::swap_binary_expressions(e);
                self.try_compress_type_of_equal_string(e);
            }
            Expression::AssignmentExpression(e) => {
                self.try_compress_normal_assignment_to_combined_assignment(e, ctx);
            }
            _ => {}
        }

        // Fold
        if let Some(folded_expr) = match expr {
            Expression::Identifier(ident) => self.try_compress_undefined(ident, ctx),
            Expression::BooleanLiteral(_) => self.try_compress_boolean(expr, ctx),
            Expression::AssignmentExpression(e) => {
                Self::try_compress_assignment_to_update_expression(e, ctx)
            }
            Expression::LogicalExpression(e) => Self::try_compress_is_null_or_undefined(e, ctx),
            Expression::NewExpression(e) => Self::try_fold_new_expression(e, ctx),
            Expression::TemplateLiteral(t) => Self::try_fold_template_literal(t, ctx),
            Expression::BinaryExpression(e) => Self::try_fold_loose_equals_undefined(e, ctx)
                .or_else(|| Self::try_compress_typeof_undefined(e, ctx)),
            Expression::CallExpression(e) => {
                Self::try_fold_literal_constructor_call_expression(e, ctx)
                    .or_else(|| Self::try_fold_simple_function_call(e, ctx))
            }
            _ => None,
        } {
            *expr = folded_expr;
            self.changed = true;
        }
    }
}

impl<'a, 'b> PeepholeSubstituteAlternateSyntax {
    pub fn new(target: ESTarget, in_fixed_loop: bool) -> Self {
        Self { target, in_fixed_loop, in_define_export: false, changed: false }
    }

    fn compress_catch_clause(&mut self, catch: &mut CatchClause<'_>, ctx: &mut TraverseCtx<'a>) {
        if !self.in_fixed_loop && self.target >= ESTarget::ES2019 {
            if let Some(param) = &catch.param {
                if let BindingPatternKind::BindingIdentifier(ident) = &param.pattern.kind {
                    if catch.body.body.is_empty()
                        || ctx.symbols().get_resolved_references(ident.symbol_id()).count() == 0
                    {
                        catch.param = None;
                    }
                };
            }
        }
    }

    fn swap_binary_expressions(e: &mut BinaryExpression<'a>) {
        if e.operator.is_equality()
            && (e.left.is_literal() || e.left.is_no_substitution_template())
            && !e.right.is_literal()
        {
            std::mem::swap(&mut e.left, &mut e.right);
        }
    }

    /// Test `Object.defineProperty(exports, ...)`
    fn is_object_define_property_exports(call_expr: &CallExpression<'a>) -> bool {
        let Some(Argument::Identifier(ident)) = call_expr.arguments.first() else { return false };
        if ident.name != "exports" {
            return false;
        }

        // Use tighter check than `call_expr.callee.is_specific_member_access("Object", "defineProperty")`
        // because we're looking for `Object.defineProperty` specifically, not e.g. `Object['defineProperty']`
        if let Expression::StaticMemberExpression(callee) = &call_expr.callee {
            if let Expression::Identifier(id) = &callee.object {
                if id.name == "Object" && callee.property.name == "defineProperty" {
                    return true;
                }
            }
        }
        false
    }

    /// Transforms `undefined` => `void 0`
    fn try_compress_undefined(
        &self,
        ident: &IdentifierReference<'a>,
        ctx: Ctx<'a, 'b>,
    ) -> Option<Expression<'a>> {
        if self.in_fixed_loop {
            return None;
        }
        if !ctx.is_identifier_undefined(ident) {
            return None;
        }
        Some(ctx.ast.void_0(ident.span))
    }

    /// Transforms boolean expression `true` => `!0` `false` => `!1`.
    /// Do not compress `true` in `Object.defineProperty(exports, 'Foo', {enumerable: true, ...})`.
    fn try_compress_boolean(
        &self,
        expr: &mut Expression<'a>,
        ctx: Ctx<'a, 'b>,
    ) -> Option<Expression<'a>> {
        if self.in_fixed_loop {
            return None;
        }
        let Expression::BooleanLiteral(lit) = expr else { return None };
        if self.in_define_export {
            return None;
        }
        let parent = ctx.ancestry.parent();
        let no_unary = {
            if let Ancestor::BinaryExpressionRight(u) = parent {
                !matches!(
                    u.operator(),
                    BinaryOperator::Addition // Other effect, like string concatenation.
                            | BinaryOperator::Instanceof // Relational operator.
                            | BinaryOperator::In
                            | BinaryOperator::StrictEquality // It checks type, so we should not fold.
                            | BinaryOperator::StrictInequality
                )
            } else {
                false
            }
        };
        // XOR: We should use `!neg` when it is not in binary expression.
        let num = ctx.ast.expression_numeric_literal(
            SPAN,
            if lit.value ^ no_unary { 0.0 } else { 1.0 },
            None,
            NumberBase::Decimal,
        );
        Some(if no_unary {
            num
        } else {
            ctx.ast.expression_unary(SPAN, UnaryOperator::LogicalNot, num)
        })
    }

    /// `() => { return foo })` -> `() => foo`
    fn try_compress_arrow_expression(
        &mut self,
        arrow_expr: &mut ArrowFunctionExpression<'a>,
        ctx: Ctx<'a, 'b>,
    ) {
        if !arrow_expr.expression
            && arrow_expr.body.directives.is_empty()
            && arrow_expr.body.statements.len() == 1
        {
            if let Some(body) = arrow_expr.body.statements.first_mut() {
                if let Statement::ReturnStatement(ret_stmt) = body {
                    let return_stmt_arg =
                        ret_stmt.argument.as_mut().map(|arg| ctx.ast.move_expression(arg));

                    if let Some(return_stmt_arg) = return_stmt_arg {
                        *body = ctx.ast.statement_expression(SPAN, return_stmt_arg);
                        arrow_expr.expression = true;
                        self.changed = true;
                    }
                }
            }
        }
    }

    /// Compress `typeof foo == "undefined"`
    ///
    /// - `typeof foo == "undefined"` (if foo is resolved) -> `foo === undefined`
    /// - `typeof foo != "undefined"` (if foo is resolved) -> `foo !== undefined`
    /// - `typeof foo == "undefined"` -> `typeof foo > "u"`
    /// - `typeof foo != "undefined"` -> `typeof foo < "u"`
    ///
    /// Enabled by `compress.typeofs`
    fn try_compress_typeof_undefined(
        expr: &mut BinaryExpression<'a>,
        ctx: Ctx<'a, 'b>,
    ) -> Option<Expression<'a>> {
        let Expression::UnaryExpression(unary_expr) = &expr.left else { return None };
        if !unary_expr.operator.is_typeof() {
            return None;
        }
        if !expr.right.is_specific_string_literal("undefined") {
            return None;
        }
        let (new_eq_op, new_comp_op) = match expr.operator {
            BinaryOperator::Equality | BinaryOperator::StrictEquality => {
                (BinaryOperator::StrictEquality, BinaryOperator::GreaterThan)
            }
            BinaryOperator::Inequality | BinaryOperator::StrictInequality => {
                (BinaryOperator::StrictInequality, BinaryOperator::LessThan)
            }
            _ => return None,
        };
        if let Expression::Identifier(ident) = &unary_expr.argument {
            if !ctx.is_global_reference(ident) {
                let Expression::UnaryExpression(unary_expr) =
                    ctx.ast.move_expression(&mut expr.left)
                else {
                    unreachable!()
                };
                let right = ctx.ast.void_0(expr.right.span());
                return Some(ctx.ast.expression_binary(
                    expr.span,
                    unary_expr.unbox().argument,
                    new_eq_op,
                    right,
                ));
            }
        };
        let left = ctx.ast.move_expression(&mut expr.left);
        let right = ctx.ast.expression_string_literal(expr.right.span(), "u", None);
        Some(ctx.ast.expression_binary(expr.span, left, new_comp_op, right))
    }

    /// Compress `foo === null || foo === undefined` into `foo == null`.
    ///
    /// `foo === null || foo === undefined` => `foo == null`
    /// `foo !== null && foo !== undefined` => `foo != null`
    ///
    /// This compression assumes that `document.all` is a normal object.
    /// If that assumption does not hold, this compression is not allowed.
    /// - `document.all === null || document.all === undefined` is `false`
    /// - `document.all == null` is `true`
    fn try_compress_is_null_or_undefined(
        expr: &mut LogicalExpression<'a>,
        ctx: Ctx<'a, 'b>,
    ) -> Option<Expression<'a>> {
        let op = expr.operator;
        let target_ops = match op {
            LogicalOperator::Or => (BinaryOperator::StrictEquality, BinaryOperator::Equality),
            LogicalOperator::And => (BinaryOperator::StrictInequality, BinaryOperator::Inequality),
            LogicalOperator::Coalesce => return None,
        };
        if let Some(new_expr) = Self::try_compress_is_null_or_undefined_for_left_and_right(
            &expr.left,
            &expr.right,
            expr.span,
            target_ops,
            ctx,
        ) {
            return Some(new_expr);
        }
        let Expression::LogicalExpression(left) = &mut expr.left else {
            return None;
        };
        if left.operator != op {
            return None;
        }
        Self::try_compress_is_null_or_undefined_for_left_and_right(
            &left.right,
            &expr.right,
            Span::new(left.right.span().start, expr.span.end),
            target_ops,
            ctx,
        )
        .map(|new_expr| {
            ctx.ast.expression_logical(
                expr.span,
                ctx.ast.move_expression(&mut left.left),
                expr.operator,
                new_expr,
            )
        })
    }

    fn try_compress_is_null_or_undefined_for_left_and_right(
        left: &Expression<'a>,
        right: &Expression<'a>,
        span: Span,
        (find_op, replace_op): (BinaryOperator, BinaryOperator),
        ctx: Ctx<'a, 'b>,
    ) -> Option<Expression<'a>> {
        let pair = Self::commutative_pair(
            (&left, &right),
            |a| {
                if let Expression::BinaryExpression(op) = a {
                    if op.operator == find_op {
                        return Self::commutative_pair(
                            (&op.left, &op.right),
                            |a_a| a_a.is_null().then_some(a_a.span()),
                            |a_b| {
                                if let Expression::Identifier(id) = a_b {
                                    Some((a_b.span(), (*id).clone()))
                                } else {
                                    None
                                }
                            },
                        );
                    }
                }
                None
            },
            |b| {
                if let Expression::BinaryExpression(op) = b {
                    if op.operator == find_op {
                        return Self::commutative_pair(
                            (&op.left, &op.right),
                            |b_a| b_a.evaluate_to_undefined().then_some(()),
                            |b_b| {
                                if let Expression::Identifier(id) = b_b {
                                    Some((*id).clone())
                                } else {
                                    None
                                }
                            },
                        )
                        .map(|v| v.1);
                    }
                }
                None
            },
        );
        let ((null_expr_span, (left_id_expr_span, left_id_ref)), right_id_ref) = pair?;
        if left_id_ref.name != right_id_ref.name {
            return None;
        }
        let left_id_expr =
            ctx.ast.expression_identifier_reference(left_id_expr_span, left_id_ref.name);
        let null_expr = ctx.ast.expression_null_literal(null_expr_span);
        Some(ctx.ast.expression_binary(span, left_id_expr, replace_op, null_expr))
    }

    fn commutative_pair<A, F, G, RetF: 'a, RetG: 'a>(
        pair: (&A, &A),
        check_a: F,
        check_b: G,
    ) -> Option<(RetF, RetG)>
    where
        F: Fn(&A) -> Option<RetF>,
        G: Fn(&A) -> Option<RetG>,
    {
        if let Some(a) = check_a(pair.0) {
            if let Some(b) = check_b(pair.1) {
                return Some((a, b));
            }
        } else if let Some(a) = check_a(pair.1) {
            if let Some(b) = check_b(pair.0) {
                return Some((a, b));
            }
        }
        None
    }
    fn try_fold_loose_equals_undefined(
        e: &mut BinaryExpression<'a>,
        ctx: Ctx<'a, 'b>,
    ) -> Option<Expression<'a>> {
        // `foo == void 0` -> `foo == null`, `foo == undefined` -> `foo == null`
        // `foo != void 0` -> `foo == null`, `foo == undefined` -> `foo == null`
        if e.operator == BinaryOperator::Inequality || e.operator == BinaryOperator::Equality {
            let (left, right) = if e.right.is_undefined() || e.right.is_void_0() {
                (
                    ctx.ast.move_expression(&mut e.left),
                    ctx.ast.expression_null_literal(e.right.span()),
                )
            } else if e.left.is_undefined() || e.left.is_void_0() {
                (
                    ctx.ast.move_expression(&mut e.right),
                    ctx.ast.expression_null_literal(e.left.span()),
                )
            } else {
                return None;
            };

            return Some(ctx.ast.expression_binary(e.span, left, e.operator, right));
        }

        None
    }

    /// Removes redundant argument of `ReturnStatement`
    ///
    /// `return undefined` -> `return`
    /// `return void 0` -> `return`
    fn compress_return_statement(&mut self, stmt: &mut ReturnStatement<'a>) {
        if stmt.argument.as_ref().is_some_and(|expr| expr.is_undefined() || expr.is_void_0()) {
            stmt.argument = None;
            self.changed = true;
        }
    }

    fn compress_variable_declarator(
        &mut self,
        decl: &mut VariableDeclarator<'a>,
        ctx: Ctx<'a, 'b>,
    ) {
        // Destructuring Pattern has error throwing side effect.
        if decl.kind.is_const() || decl.id.kind.is_destructuring_pattern() {
            return;
        }
        if decl.init.as_ref().is_some_and(|init| ctx.is_expression_undefined(init)) {
            decl.init = None;
            self.changed = true;
        }
    }

    /// Compress `a = a + b` to `a += b`
    fn try_compress_normal_assignment_to_combined_assignment(
        &mut self,
        expr: &mut AssignmentExpression<'a>,
        ctx: Ctx<'a, 'b>,
    ) {
        if !matches!(expr.operator, AssignmentOperator::Assign) {
            return;
        }
        let AssignmentTarget::AssignmentTargetIdentifier(write_id_ref) = &mut expr.left else {
            return;
        };

        let Expression::BinaryExpression(binary_expr) = &mut expr.right else { return };
        let Some(new_op) = binary_expr.operator.to_assignment_operator() else { return };
        let Expression::Identifier(read_id_ref) = &mut binary_expr.left else { return };
        if write_id_ref.name != read_id_ref.name {
            return;
        }

        expr.operator = new_op;
        expr.right = ctx.ast.move_expression(&mut binary_expr.right);
        self.changed = true;
    }

    fn try_compress_assignment_to_update_expression(
        expr: &mut AssignmentExpression<'a>,
        ctx: Ctx<'a, 'b>,
    ) -> Option<Expression<'a>> {
        let target = expr.left.as_simple_assignment_target_mut()?;
        if !matches!(expr.operator, AssignmentOperator::Subtraction) {
            return None;
        }
        match &expr.right {
            Expression::NumericLiteral(num) if num.value.to_int_32() == 1 => {
                // The `_` will not be placed to the target code.
                let target = std::mem::replace(
                    target,
                    ctx.ast.simple_assignment_target_identifier_reference(SPAN, "_"),
                );
                Some(ctx.ast.expression_update(SPAN, UpdateOperator::Decrement, true, target))
            }
            Expression::UnaryExpression(un)
                if matches!(un.operator, UnaryOperator::UnaryNegation) =>
            {
                let Expression::NumericLiteral(num) = &un.argument else { return None };
                (num.value.to_int_32() == 1).then(|| {
                    // The `_` will not be placed to the target code.
                    let target = std::mem::replace(
                        target,
                        ctx.ast.simple_assignment_target_identifier_reference(SPAN, "_"),
                    );
                    ctx.ast.expression_update(SPAN, UpdateOperator::Increment, true, target)
                })
            }
            _ => None,
        }
    }

    fn is_window_object(expr: &Expression) -> bool {
        expr.as_member_expression()
            .is_some_and(|mem_expr| mem_expr.is_specific_member_access("window", "Object"))
    }

    fn try_fold_new_expression(
        new_expr: &mut NewExpression<'a>,
        ctx: Ctx<'a, 'b>,
    ) -> Option<Expression<'a>> {
        // `new Object` -> `{}`
        if new_expr.arguments.is_empty()
            && (new_expr.callee.is_global_reference_name("Object", ctx.symbols())
                || Self::is_window_object(&new_expr.callee))
        {
            Some(ctx.ast.expression_object(new_expr.span, ctx.ast.vec(), None))
        } else if new_expr.callee.is_global_reference_name("Array", ctx.symbols()) {
            // `new Array` -> `[]`
            if new_expr.arguments.is_empty() {
                Some(Self::empty_array_literal(ctx))
            } else if new_expr.arguments.len() == 1 {
                let arg = new_expr.arguments.get_mut(0).and_then(|arg| arg.as_expression_mut())?;
                // `new Array(0)` -> `[]`
                if arg.is_number_0() {
                    Some(Self::empty_array_literal(ctx))
                }
                // `new Array(8)` -> `Array(8)`
                else if arg.is_number_literal() {
                    Some(Self::array_constructor_call(
                        ctx.ast.move_vec(&mut new_expr.arguments),
                        ctx,
                    ))
                }
                // `new Array(literal)` -> `[literal]`
                else if arg.is_literal() || matches!(arg, Expression::ArrayExpression(_)) {
                    let mut elements = ctx.ast.vec();
                    let element = ArrayExpressionElement::from(ctx.ast.move_expression(arg));
                    elements.push(element);
                    Some(Self::array_literal(elements, ctx))
                }
                // `new Array()` -> `Array()`
                else {
                    Some(Self::array_constructor_call(
                        ctx.ast.move_vec(&mut new_expr.arguments),
                        ctx,
                    ))
                }
            } else {
                // `new Array(1, 2, 3)` -> `[1, 2, 3]`
                let elements = ctx.ast.vec_from_iter(
                    new_expr
                        .arguments
                        .iter_mut()
                        .filter_map(|arg| arg.as_expression_mut())
                        .map(|arg| ArrayExpressionElement::from(ctx.ast.move_expression(arg))),
                );
                Some(Self::array_literal(elements, ctx))
            }
        } else if new_expr.callee.is_global_reference_name("Error", ctx.symbols())
            || new_expr.callee.is_global_reference_name("Function", ctx.symbols())
        {
            Some(ctx.ast.expression_call(
                new_expr.span,
                ctx.ast.move_expression(&mut new_expr.callee),
                Option::<TSTypeParameterInstantiation>::None,
                ctx.ast.move_vec(&mut new_expr.arguments),
                false,
            ))
        } else if new_expr.callee.is_global_reference_name("RegExp", ctx.symbols()) {
            let arguments_len = new_expr.arguments.len();
            if arguments_len == 0
                || (arguments_len >= 1
                    && new_expr.arguments[0].as_expression().map_or(false, |first_argument| {
                        let ty = ValueType::from(first_argument);
                        !ty.is_undetermined() && !ty.is_object()
                    }))
            {
                Some(ctx.ast.expression_call(
                    new_expr.span,
                    ctx.ast.move_expression(&mut new_expr.callee),
                    Option::<TSTypeParameterInstantiation>::None,
                    ctx.ast.move_vec(&mut new_expr.arguments),
                    false,
                ))
            } else {
                None
            }
        } else {
            None
        }
    }

    fn try_fold_literal_constructor_call_expression(
        call_expr: &mut CallExpression<'a>,
        ctx: Ctx<'a, 'b>,
    ) -> Option<Expression<'a>> {
        // `Object()` -> `{}`
        if call_expr.arguments.is_empty()
            && (call_expr.callee.is_global_reference_name("Object", ctx.symbols())
                || Self::is_window_object(&call_expr.callee))
        {
            Some(ctx.ast.expression_object(call_expr.span, ctx.ast.vec(), None))
        } else if call_expr.callee.is_global_reference_name("Array", ctx.symbols()) {
            // `Array()` -> `[]`
            if call_expr.arguments.is_empty() {
                Some(Self::empty_array_literal(ctx))
            } else if call_expr.arguments.len() == 1 {
                let arg = call_expr.arguments.get_mut(0).and_then(|arg| arg.as_expression_mut())?;
                // `Array(0)` -> `[]`
                if arg.is_number_0() {
                    Some(Self::empty_array_literal(ctx))
                }
                // `Array(8)` -> `Array(8)`
                else if arg.is_number_literal() {
                    Some(Self::array_constructor_call(
                        ctx.ast.move_vec(&mut call_expr.arguments),
                        ctx,
                    ))
                }
                // `Array(literal)` -> `[literal]`
                else if arg.is_literal() || matches!(arg, Expression::ArrayExpression(_)) {
                    let mut elements = ctx.ast.vec();
                    let element = ArrayExpressionElement::from(ctx.ast.move_expression(arg));
                    elements.push(element);
                    Some(Self::array_literal(elements, ctx))
                } else {
                    None
                }
            } else {
                // `Array(1, 2, 3)` -> `[1, 2, 3]`
                let elements = ctx.ast.vec_from_iter(
                    call_expr
                        .arguments
                        .iter_mut()
                        .filter_map(|arg| arg.as_expression_mut())
                        .map(|arg| ArrayExpressionElement::from(ctx.ast.move_expression(arg))),
                );
                Some(Self::array_literal(elements, ctx))
            }
        } else {
            None
        }
    }

    /// Fold `Boolean`, `Number`, `String`, `BigInt` constructors.
    fn try_fold_simple_function_call(
        call_expr: &mut CallExpression<'a>,
        ctx: Ctx<'a, 'b>,
    ) -> Option<Expression<'a>> {
        if call_expr.optional || call_expr.arguments.len() >= 2 {
            return None;
        }
        let Expression::Identifier(ident) = &call_expr.callee else { return None };
        let name = ident.name.as_str();
        if !matches!(name, "Boolean" | "Number" | "String" | "BigInt") {
            return None;
        }
        let args = &mut call_expr.arguments;
        let arg = match args.get_mut(0) {
            None => None,
            Some(arg) => Some(arg.as_expression_mut()?),
        };
        if !ctx.is_global_reference(ident) {
            return None;
        }
        let span = call_expr.span;
        match name {
            // `Boolean(a)` -> `!!(a)`
            // http://www.ecma-international.org/ecma-262/6.0/index.html#sec-boolean-constructor-boolean-value
            // and
            // http://www.ecma-international.org/ecma-262/6.0/index.html#sec-logical-not-operator-runtime-semantics-evaluation
            "Boolean" => match arg {
                None => Some(ctx.ast.expression_boolean_literal(span, false)),
                Some(arg) => {
                    if let Expression::UnaryExpression(unary_expr) = arg {
                        if unary_expr.operator == UnaryOperator::LogicalNot {
                            return Some(ctx.ast.move_expression(arg));
                        }
                    }
                    Some(ctx.ast.expression_unary(
                        span,
                        UnaryOperator::LogicalNot,
                        ctx.ast.expression_unary(
                            span,
                            UnaryOperator::LogicalNot,
                            ctx.ast.move_expression(arg),
                        ),
                    ))
                }
            },
            "String" => {
                match arg {
                    // `String()` -> `''`
                    None => Some(ctx.ast.expression_string_literal(span, "", None)),
                    // `String(a)` -> `'' + (a)`
                    Some(arg) => {
                        if !matches!(arg, Expression::Identifier(_) | Expression::CallExpression(_))
                            && !arg.is_literal()
                        {
                            return None;
                        }
                        Some(ctx.ast.expression_binary(
                            span,
                            ctx.ast.expression_string_literal(SPAN, "", None),
                            BinaryOperator::Addition,
                            ctx.ast.move_expression(arg),
                        ))
                    }
                }
            }
            "Number" => Some(ctx.ast.expression_numeric_literal(
                span,
                match arg {
                    None => 0.0,
                    Some(arg) => arg.to_number()?,
                },
                None,
                NumberBase::Decimal,
            )),
            // `BigInt(1n)` -> `1n`
            "BigInt" => match arg {
                None => None,
                Some(arg) => matches!(arg, Expression::BigIntLiteral(_))
                    .then(|| ctx.ast.move_expression(arg)),
            },
            _ => None,
        }
    }

    /// `typeof foo === 'number'` -> `typeof foo == 'number'`
    fn try_compress_type_of_equal_string(&mut self, e: &mut BinaryExpression<'a>) {
        let op = match e.operator {
            BinaryOperator::StrictEquality => BinaryOperator::Equality,
            BinaryOperator::StrictInequality => BinaryOperator::Inequality,
            _ => return,
        };
        if !matches!(&e.left, Expression::UnaryExpression(unary_expr) if unary_expr.operator.is_typeof())
        {
            return;
        }
        if !e.right.is_string_literal() {
            return;
        }
        e.operator = op;
        self.changed = true;
    }

    fn try_compress_chain_call_expression(
        &mut self,
        chain_expr: &mut ChainExpression<'a>,
        ctx: Ctx<'a, 'b>,
    ) {
        if let ChainElement::CallExpression(call_expr) = &mut chain_expr.expression {
            // `window.Object?.()` -> `Object?.()`
            if call_expr.arguments.is_empty() && Self::is_window_object(&call_expr.callee) {
                call_expr.callee =
                    ctx.ast.expression_identifier_reference(call_expr.callee.span(), "Object");
                self.changed = true;
            }
        }
    }

    fn try_fold_template_literal(t: &TemplateLiteral, ctx: Ctx<'a, 'b>) -> Option<Expression<'a>> {
        t.to_js_string().map(|val| ctx.ast.expression_string_literal(t.span(), val, None))
    }

    /// returns an `Array()` constructor call with zero, one, or more arguments, copying from the input
    fn array_constructor_call(
        arguments: Vec<'a, Argument<'a>>,
        ctx: Ctx<'a, 'b>,
    ) -> Expression<'a> {
        let callee = ctx.ast.expression_identifier_reference(SPAN, "Array");
        ctx.ast.expression_call(SPAN, callee, NONE, arguments, false)
    }

    /// returns an array literal `[]` of zero, one, or more elements, copying from the input
    fn array_literal(
        elements: Vec<'a, ArrayExpressionElement<'a>>,
        ctx: Ctx<'a, 'b>,
    ) -> Expression<'a> {
        ctx.ast.expression_array(SPAN, elements, None)
    }

    /// returns a new empty array literal expression: `[]`
    fn empty_array_literal(ctx: Ctx<'a, 'b>) -> Expression<'a> {
        Self::array_literal(ctx.ast.vec(), ctx)
    }

    // https://github.com/swc-project/swc/blob/4e2dae558f60a9f5c6d2eac860743e6c0b2ec562/crates/swc_ecma_minifier/src/compress/pure/properties.rs
    #[allow(clippy::cast_lossless)]
    fn try_compress_property_key(
        &mut self,
        key: &mut PropertyKey<'a>,
        computed: &mut bool,
        ctx: &mut TraverseCtx<'a>,
    ) {
        if self.in_fixed_loop {
            return;
        }
        if let PropertyKey::NumericLiteral(_) = key {
            if *computed {
                *computed = false;
            }
            return;
        };
        let PropertyKey::StringLiteral(s) = key else { return };
        if s.value == "__proto__" || s.value == "constructor" {
            return;
        }
        if *computed {
            *computed = false;
        }
        if is_identifier_name(&s.value) {
            self.changed = true;
            *key = PropertyKey::StaticIdentifier(
                ctx.ast.alloc_identifier_name(s.span, s.value.clone()),
            );
        } else if let Some(value) = Ctx::string_to_equivalent_number_value(s.value.as_str()) {
            self.changed = true;
            *key = PropertyKey::NumericLiteral(ctx.ast.alloc_numeric_literal(
                s.span,
                value,
                None,
                NumberBase::Decimal,
            ));
        }
    }

    // `foo(...[1,2,3])` -> `foo(1,2,3)`
    fn try_compress_call_expression_arguments(
        &mut self,
        node: &mut CallExpression<'a>,
        ctx: &mut TraverseCtx<'a>,
    ) {
        let (new_size, should_fold) =
            node.arguments.iter().fold((0, false), |(mut new_size, mut should_fold), arg| {
                new_size += if let Argument::SpreadElement(spread_el) = arg {
                    if let Expression::ArrayExpression(array_expr) = &spread_el.argument {
                        should_fold = true;
                        array_expr.elements.len()
                    } else {
                        1
                    }
                } else {
                    1
                };

                (new_size, should_fold)
            });

        if should_fold {
            let old_args =
                std::mem::replace(&mut node.arguments, ctx.ast.vec_with_capacity(new_size));
            let new_args = &mut node.arguments;

            for arg in old_args {
                if let Argument::SpreadElement(mut spread_el) = arg {
                    if let Expression::ArrayExpression(array_expr) = &mut spread_el.argument {
                        for el in array_expr.elements.iter_mut() {
                            match el {
                                ArrayExpressionElement::SpreadElement(spread_el) => {
                                    new_args.push(ctx.ast.argument_spread_element(
                                        spread_el.span,
                                        ctx.ast.move_expression(&mut spread_el.argument),
                                    ));
                                }
                                ArrayExpressionElement::Elision(elision) => {
                                    new_args.push(ctx.ast.void_0(elision.span).into());
                                }
                                match_expression!(ArrayExpressionElement) => {
                                    new_args.push(
                                        ctx.ast.move_expression(el.to_expression_mut()).into(),
                                    );
                                }
                            }
                        }
                    } else {
                        new_args.push(ctx.ast.argument_spread_element(
                            spread_el.span,
                            ctx.ast.move_expression(&mut spread_el.argument),
                        ));
                    }
                } else {
                    new_args.push(arg);
                }
            }
            self.changed = true;
        }
    }
}

/// Port from <https://github.com/google/closure-compiler/blob/v20240609/test/com/google/javascript/jscomp/PeepholeSubstituteAlternateSyntaxTest.java>
#[cfg(test)]
mod test {
    use oxc_allocator::Allocator;
    use oxc_syntax::es_target::ESTarget;

    use crate::tester;

    fn test(source_text: &str, expected: &str) {
        let allocator = Allocator::default();
        let target = ESTarget::ESNext;
        let mut pass = super::PeepholeSubstituteAlternateSyntax::new(target, false);
        tester::test(&allocator, source_text, expected, &mut pass);
    }

    fn test_same(source_text: &str) {
        test(source_text, source_text);
    }

    #[test]
    fn test_fold_return_result() {
        test("function f(){return !1;}", "function f(){return !1}");
        test("function f(){return null;}", "function f(){return null}");
        test("function f(){return void 0;}", "function f(){return}");
        test("function f(){return void foo();}", "function f(){return void foo()}");
        test("function f(){return undefined;}", "function f(){return}");
        // Here we handle the block in dce.
        test("function f(){if(a()){return undefined;}}", "function f(){if(a()){return}}");
    }

    #[test]
    fn test_undefined() {
        test("var x = undefined", "var x");
        test_same("var undefined = 1;function f() {var undefined=2;var x;}");
        test_same("function f(undefined) {}");
        test_same("try {} catch(undefined) {foo(undefined)}");
        test("for (undefined in {}) {}", "for(undefined in {}){}");
        test("undefined++;", "undefined++");
        test("undefined += undefined;", "undefined+=void 0");

        // shadowd
        test_same("(function(undefined) { let x = typeof undefined; })()");

        // destructuring throw error side effect
        test_same("var {} = void 0");
        test_same("var [] = void 0");
    }

    #[test]
    fn test_fold_true_false_comparison() {
        test("x == true", "x == 1");
        test("x == false", "x == 0");
        test("x != true", "x != 1");
        test("x < true", "x < 1");
        test("x <= true", "x <= 1");
        test("x > true", "x > 1");
        test("x >= true", "x >= 1");

        test("x instanceof true", "x instanceof !0");
        test("x + false", "x + !1");

        // Order: should perform the nearest.
        test("x == x instanceof false", "x == x instanceof !1");
        test("x in x >> true", "x in x >> 1");
        test("x == fake(false)", "x == fake(!1)");

        // The following should not be folded.
        test("x === true", "x === !0");
        test("x !== false", "x !== !1");
    }

    /// Based on https://github.com/terser/terser/blob/58ba5c163fa1684f2a63c7bc19b7ebcf85b74f73/test/compress/assignment.js
    #[test]
    fn test_fold_normal_assignment_to_combined_assignment() {
        test("x = x + 3", "x += 3");
        test("x = x - 3", "x -= 3");
        test("x = x / 3", "x /= 3");
        test("x = x * 3", "x *= 3");
        test("x = x >> 3", "x >>= 3");
        test("x = x << 3", "x <<= 3");
        test("x = x >>> 3", "x >>>= 3");
        test("x = x | 3", "x |= 3");
        test("x = x ^ 3", "x ^= 3");
        test("x = x % 3", "x %= 3");
        test("x = x & 3", "x &= 3");
        test("x = x + g()", "x += g()");
        test("x = x - g()", "x -= g()");
        test("x = x / g()", "x /= g()");
        test("x = x * g()", "x *= g()");
        test("x = x >> g()", "x >>= g()");
        test("x = x << g()", "x <<= g()");
        test("x = x >>> g()", "x >>>= g()");
        test("x = x | g()", "x |= g()");
        test("x = x ^ g()", "x ^= g()");
        test("x = x % g()", "x %= g()");
        test("x = x & g()", "x &= g()");

        test_same("x = 3 + x");
        test_same("x = 3 - x");
        test_same("x = 3 / x");
        test_same("x = 3 * x");
        test_same("x = 3 >> x");
        test_same("x = 3 << x");
        test_same("x = 3 >>> x");
        test_same("x = 3 | x");
        test_same("x = 3 ^ x");
        test_same("x = 3 % x");
        test_same("x = 3 & x");
        test_same("x = g() + x");
        test_same("x = g() - x");
        test_same("x = g() / x");
        test_same("x = g() * x");
        test_same("x = g() >> x");
        test_same("x = g() << x");
        test_same("x = g() >>> x");
        test_same("x = g() | x");
        test_same("x = g() ^ x");
        test_same("x = g() % x");
        test_same("x = g() & x");

        test_same("x = (x -= 2) ^ x");
    }

    #[test]
    fn test_fold_subtraction_assignment() {
        test("x -= 1", "--x");
        test("x -= -1", "++x");
        test_same("x -= 2");
        test_same("x += 1"); // The string concatenation may be triggered, so we don't fold this.
        test_same("x += -1");
    }

    #[test]
    fn test_fold_literal_object_constructors() {
        test("x = new Object", "x = ({})");
        test("x = new Object()", "x = ({})");
        test("x = Object()", "x = ({})");

        test_same("x = (function f(){function Object(){this.x=4}return new Object();})();");
    }

    #[test]
    fn test_fold_literal_object_constructors_on_window() {
        test("x = new window.Object", "x = ({})");
        test("x = new window.Object()", "x = ({})");

        // Mustn't fold optional chains
        test("x = window.Object()", "x = ({})");
        test("x = window.Object?.()", "x = Object?.()");

        test(
            "x = (function f(){function Object(){this.x=4};return new window.Object;})();",
            "x = (function f(){function Object(){this.x=4}return {};})();",
        );
    }

    #[test]
    fn test_fold_literal_array_constructors() {
        test("x = new Array", "x = []");
        test("x = new Array()", "x = []");
        test("x = Array()", "x = []");
        // do not fold optional chains
        test_same("x = Array?.()");

        // One argument
        test("x = new Array(0)", "x = []");
        test("x = new Array(\"a\")", "x = [\"a\"]");
        test("x = new Array(7)", "x = Array(7)");
        test("x = new Array(y)", "x = Array(y)");
        test("x = new Array(foo())", "x = Array(foo())");
        test("x = Array(0)", "x = []");
        test("x = Array(\"a\")", "x = [\"a\"]");
        test_same("x = Array(7)");
        test_same("x = Array(y)");
        test_same("x = Array(foo())");

        // 1+ arguments
        test("x = new Array(1, 2, 3, 4)", "x = [1, 2, 3, 4]");
        test("x = Array(1, 2, 3, 4)", "x = [1, 2, 3, 4]");
        test("x = new Array('a', 1, 2, 'bc', 3, {}, 'abc')", "x = ['a', 1, 2, 'bc', 3, {}, 'abc']");
        test("x = Array('a', 1, 2, 'bc', 3, {}, 'abc')", "x = ['a', 1, 2, 'bc', 3, {}, 'abc']");
        test("x = new Array(Array(1, '2', 3, '4'))", "x = [[1, '2', 3, '4']]");
        test("x = Array(Array(1, '2', 3, '4'))", "x = [[1, '2', 3, '4']]");
        test(
            "x = new Array(Object(), Array(\"abc\", Object(), Array(Array())))",
            "x = [{}, [\"abc\", {}, [[]]]]",
        );
        test(
            "x = new Array(Object(), Array(\"abc\", Object(), Array(Array())))",
            "x = [{}, [\"abc\", {}, [[]]]]",
        );
    }

    #[test]
    fn test_fold_new_expressions() {
        test("new Error()", "Error()");
        test("new Error('a')", "Error('a')");
        test("new Error('a', { cause: b })", "Error('a', { cause: b })");
        test_same("var Error; new Error()");

        test("new Function()", "Function()");
        test(
            "new Function('a', 'b', 'console.log(a, b)')",
            "Function('a', 'b', 'console.log(a, b)')",
        );
        test_same("var Function; new Function()");

        test("new RegExp()", "RegExp()");
        test("new RegExp('a')", "RegExp('a')");
        test("new RegExp(0)", "RegExp(0)");
        test("new RegExp(null)", "RegExp(null)");
        test("new RegExp('a', 'g')", "RegExp('a', 'g')");
        test_same("new RegExp(foo)");
        test_same("new RegExp(/foo/)");
    }

    #[test]
    #[ignore]
    fn test_split_comma_expressions() {
        // late = false;
        // Don't try to split in expressions.
        test_same("while (foo(), !0) boo()");
        test_same("var a = (foo(), !0);");
        test_same("a = (foo(), !0);");

        // Don't try to split COMMA under LABELs.
        test_same("a:a(),b()");
        test("1, 2, 3, 4", "1; 2; 3; 4");
        test("x = 1, 2, 3", "x = 1; 2; 3");
        test_same("x = (1, 2, 3)");
        test("1, (2, 3), 4", "1; 2; 3; 4");
        test("(x=2), foo()", "x=2; foo()");
        test("foo(), boo();", "foo(); boo()");
        test("(a(), b()), (c(), d());", "a(); b(); c(); d()");
        test("a(); b(); (c(), d());", "a(); b(); c(); d();");
        test("foo(), true", "foo();true");
        test_same("foo();true");
        test("function x(){foo(), !0}", "function x(){foo(); !0}");
        test_same("function x(){foo(); !0}");
    }

    #[test]
    #[ignore]
    fn test_comma1() {
        // late = false;
        test("1, 2", "1; 2");
        // late = true;
        // test_same("1, 2");
    }

    #[test]
    #[ignore]
    fn test_comma2() {
        // late = false;
        test("1, a()", "1; a()");
        test("1, a?.()", "1; a?.()");

        // late = true;
        // test_same("1, a()");
        // test_same("1, a?.()");
    }

    #[test]
    #[ignore]
    fn test_comma3() {
        // late = false;
        test("1, a(), b()", "1; a(); b()");
        test("1, a?.(), b?.()", "1; a?.(); b?.()");

        // late = true;
        // test_same("1, a(), b()");
        // test_same("1, a?.(), b?.()");
    }

    #[test]
    #[ignore]
    fn test_comma4() {
        // late = false;
        test("a(), b()", "a();b()");
        test("a?.(), b?.()", "a?.();b?.()");

        // late = true;
        // test_same("a(), b()");
        // test_same("a?.(), b?.()");
    }

    #[test]
    #[ignore]
    fn test_comma5() {
        // late = false;
        test("a(), b(), 1", "a(); b(); 1");
        test("a?.(), b?.(), 1", "a?.(); b?.(); 1");

        // late = true;
        // test_same("a(), b(), 1");
        // test_same("a?.(), b?.(), 1");
    }

    #[test]
    #[ignore]
    fn test_string_array_splitting() {
        test_same("var x=['1','2','3','4']");
        test_same("var x=['1','2','3','4','5']");
        test("var x=['1','2','3','4','5','6']", "var x='123456'.split('')");
        test("var x=['1','2','3','4','5','00']", "var x='1 2 3 4 5 00'.split(' ')");
        test("var x=['1','2','3','4','5','6','7']", "var x='1234567'.split('')");
        test("var x=['1','2','3','4','5','6','00']", "var x='1 2 3 4 5 6 00'.split(' ')");
        test("var x=[' ,',',',',',',',',',',']", "var x=' ,;,;,;,;,;,'.split(';')");
        test("var x=[',,',' ',',',',',',',',']", "var x=',,; ;,;,;,;,'.split(';')");
        test("var x=['a,',' ',',',',',',',',']", "var x='a,; ;,;,;,;,'.split(';')");

        // all possible delimiters used, leave it alone
        test_same("var x=[',', ' ', ';', '{', '}']");
    }

    #[test]
    fn test_template_string_to_string() {
        test("`abcde`", "'abcde'");
        test("`ab cd ef`", "'ab cd ef'");
        test_same("`hello ${name}`");
        test_same("tag `hello ${name}`");
        test_same("tag `hello`");
        test("`hello ${'foo'}`", "'hello foo'");
        test("`${2} bananas`", "'2 bananas'");
        test("`This is ${true}`", "'This is true'");
    }

    #[test]
    #[ignore]
    fn test_bind_to_call1() {
        test("(goog.bind(f))()", "f()");
        test("(goog.bind(f,a))()", "f.call(a)");
        test("(goog.bind(f,a,b))()", "f.call(a,b)");

        test("(goog.bind(f))(a)", "f(a)");
        test("(goog.bind(f,a))(b)", "f.call(a,b)");
        test("(goog.bind(f,a,b))(c)", "f.call(a,b,c)");

        test("(goog.partial(f))()", "f()");
        test("(goog.partial(f,a))()", "f(a)");
        test("(goog.partial(f,a,b))()", "f(a,b)");

        test("(goog.partial(f))(a)", "f(a)");
        test("(goog.partial(f,a))(b)", "f(a,b)");
        test("(goog.partial(f,a,b))(c)", "f(a,b,c)");

        test("((function(){}).bind())()", "((function(){}))()");
        test("((function(){}).bind(a))()", "((function(){})).call(a)");
        test("((function(){}).bind(a,b))()", "((function(){})).call(a,b)");

        test("((function(){}).bind())(a)", "((function(){}))(a)");
        test("((function(){}).bind(a))(b)", "((function(){})).call(a,b)");
        test("((function(){}).bind(a,b))(c)", "((function(){})).call(a,b,c)");

        // Without using type information we don't know "f" is a function.
        test_same("(f.bind())()");
        test_same("(f.bind(a))()");
        test_same("(f.bind())(a)");
        test_same("(f.bind(a))(b)");

        // Don't rewrite if the bind isn't the immediate call target
        test_same("(goog.bind(f)).call(g)");
    }

    #[test]
    #[ignore]
    fn test_bind_to_call2() {
        test("(goog$bind(f))()", "f()");
        test("(goog$bind(f,a))()", "f.call(a)");
        test("(goog$bind(f,a,b))()", "f.call(a,b)");

        test("(goog$bind(f))(a)", "f(a)");
        test("(goog$bind(f,a))(b)", "f.call(a,b)");
        test("(goog$bind(f,a,b))(c)", "f.call(a,b,c)");

        test("(goog$partial(f))()", "f()");
        test("(goog$partial(f,a))()", "f(a)");
        test("(goog$partial(f,a,b))()", "f(a,b)");

        test("(goog$partial(f))(a)", "f(a)");
        test("(goog$partial(f,a))(b)", "f(a,b)");
        test("(goog$partial(f,a,b))(c)", "f(a,b,c)");
        // Don't rewrite if the bind isn't the immediate call target
        test_same("(goog$bind(f)).call(g)");
    }

    #[test]
    #[ignore]
    fn test_bind_to_call3() {
        // TODO(johnlenz): The code generator wraps free calls with (0,...) to
        // prevent leaking "this", but the parser doesn't unfold it, making a
        // AST comparison fail.  For now do a string comparison to validate the
        // correct code is in fact generated.
        // The FREE call wrapping should be moved out of the code generator
        // and into a denormalizing pass.
        // disableCompareAsTree();
        // retraverseOnChange = true;
        // late = false;

        test("(goog.bind(f.m))()", "(0,f.m)()");
        test("(goog.bind(f.m,a))()", "f.m.call(a)");

        test("(goog.bind(f.m))(a)", "(0,f.m)(a)");
        test("(goog.bind(f.m,a))(b)", "f.m.call(a,b)");

        test("(goog.partial(f.m))()", "(0,f.m)()");
        test("(goog.partial(f.m,a))()", "(0,f.m)(a)");

        test("(goog.partial(f.m))(a)", "(0,f.m)(a)");
        test("(goog.partial(f.m,a))(b)", "(0,f.m)(a,b)");

        // Without using type information we don't know "f" is a function.
        test_same("f.m.bind()()");
        test_same("f.m.bind(a)()");
        test_same("f.m.bind()(a)");
        test_same("f.m.bind(a)(b)");

        // Don't rewrite if the bind isn't the immediate call target
        test_same("goog.bind(f.m).call(g)");
    }

    #[test]
    #[ignore]
    fn test_rotate_associative_operators() {
        test("a || (b || c); a * (b * c); a | (b | c)", "(a || b) || c; (a * b) * c; (a | b) | c");
        test_same("a % (b % c); a / (b / c); a - (b - c);");
        test("a * (b % c);", "b % c * a");
        test("a * b * (c / d)", "c / d * b * a");
        test("(a + b) * (c % d)", "c % d * (a + b)");
        test_same("(a / b) * (c % d)");
        test_same("(c = 5) * (c % d)");
        test("(a + b) * c * (d % e)", "d % e * c * (a + b)");
        test("!a * c * (d % e)", "d % e * c * !a");
    }

    #[test]
    #[ignore]
    fn nullish_coalesce() {
        test("a ?? (b ?? c);", "(a ?? b) ?? c");
    }

    #[test]
    #[ignore]
    fn test_no_rotate_infinite_loop() {
        test("1/x * (y/1 * (1/z))", "1/x * (y/1) * (1/z)");
        test_same("1/x * (y/1) * (1/z)");
    }

    #[test]
    fn test_fold_arrow_function_return() {
        test("const foo = () => { return 'baz' }", "const foo = () => 'baz'");
        test_same("const foo = () => { foo; return 'baz' }");
    }

    #[test]
    fn test_fold_is_typeof_equals_undefined_resolved() {
        test("var x; typeof x !== 'undefined'", "var x; x !== void 0");
        test("var x; typeof x != 'undefined'", "var x; x !== void 0");
        test("var x; 'undefined' !== typeof x", "var x; x !== void 0");
        test("var x; 'undefined' != typeof x", "var x; x !== void 0");

        test("var x; typeof x === 'undefined'", "var x; x === void 0");
        test("var x; typeof x == 'undefined'", "var x; x === void 0");
        test("var x; 'undefined' === typeof x", "var x; x === void 0");
        test("var x; 'undefined' == typeof x", "var x; x === void 0");

        test(
            "var x; function foo() { typeof x !== 'undefined' }",
            "var x; function foo() { x !== void 0 }",
        );
        test(
            "typeof x !== 'undefined'; function foo() { var x }",
            "typeof x < 'u'; function foo() { var x }",
        );
        test("typeof x !== 'undefined'; { var x }", "x !== void 0; { var x }");
        test("typeof x !== 'undefined'; { let x }", "typeof x < 'u'; { let x }");
        test("typeof x !== 'undefined'; var x", "x !== void 0; var x");
        // input and output both errors with same TDZ error
        test("typeof x !== 'undefined'; let x", "x !== void 0; let x");
    }

    /// Port from <https://github.com/evanw/esbuild/blob/v0.24.2/internal/js_parser/js_parser_test.go#L4658>
    #[test]
    fn test_fold_is_typeof_equals_undefined() {
        test("typeof x !== 'undefined'", "typeof x < 'u'");
        test("typeof x != 'undefined'", "typeof x < 'u'");
        test("'undefined' !== typeof x", "typeof x < 'u'");
        test("'undefined' != typeof x", "typeof x < 'u'");

        test("typeof x === 'undefined'", "typeof x > 'u'");
        test("typeof x == 'undefined'", "typeof x > 'u'");
        test("'undefined' === typeof x", "typeof x > 'u'");
        test("'undefined' == typeof x", "typeof x > 'u'");

        test("typeof x.y === 'undefined'", "typeof x.y > 'u'");
        test("typeof x.y !== 'undefined'", "typeof x.y < 'u'");
    }

    #[test]
    fn test_fold_is_null_or_undefined() {
        test("foo === null || foo === undefined", "foo == null");
        test("foo === undefined || foo === null", "foo == null");
        test("foo === null || foo === void 0", "foo == null");
        test("foo === null || foo === void 0 || foo === 1", "foo == null || foo === 1");
        test("foo === 1 || foo === null || foo === void 0", "foo === 1 || foo == null");
        test_same("foo === void 0 || bar === null");
        test_same("foo !== 1 && foo === void 0 || foo === null");
        test_same("foo.a === void 0 || foo.a === null"); // cannot be folded because accessing foo.a might have a side effect

        test("foo !== null && foo !== undefined", "foo != null");
        test("foo !== undefined && foo !== null", "foo != null");
        test("foo !== null && foo !== void 0", "foo != null");
        test("foo !== null && foo !== void 0 && foo !== 1", "foo != null && foo !== 1");
        test("foo !== 1 && foo !== null && foo !== void 0", "foo !== 1 && foo != null");
        test("foo !== 1 || foo !== void 0 && foo !== null", "foo !== 1 || foo != null");
        test_same("foo !== void 0 && bar !== null");
    }

    #[test]
    fn test_fold_loose_equals_undefined() {
        test_same("foo != null");
        test("foo != undefined", "foo != null");
        test("foo != void 0", "foo != null");
        test("undefined != foo", "foo != null");
        test("void 0 != foo", "foo != null");
    }

    #[test]
    fn test_try_compress_type_of_equal_string() {
        test("typeof foo === 'number'", "typeof foo == 'number'");
        test("'number' === typeof foo", "typeof foo == 'number'");
        test("typeof foo === `number`", "typeof foo == 'number'");
        test("`number` === typeof foo", "typeof foo == 'number'");
        test("typeof foo !== 'number'", "typeof foo != 'number'");
        test("'number' !== typeof foo", "typeof foo != 'number'");
        test("typeof foo !== `number`", "typeof foo != 'number'");
        test("`number` !== typeof foo", "typeof foo != 'number'");
    }

    #[test]
    fn test_property_key() {
        // Object Property
        test(
            "({ '0': _, 'a': _, [1]: _, ['1']: _, ['b']: _, ['c.c']: _, '1.1': _, '😊': _, 'd.d': _ })",
            "({  0: _,   a: _,    1: _,     1: _,     b: _,   'c.c': _, '1.1': _, '😊': _, 'd.d': _ })",
        );
        // AssignmentTargetPropertyProperty
        test(
            "({ '0': _, 'a': _, [1]: _, ['1']: _, ['b']: _, ['c.c']: _, '1.1': _, '😊': _, 'd.d': _ } = {})",
            "({  0: _,   a: _,    1: _,   1: _,     b: _,   'c.c': _, '1.1': _, '😊': _, 'd.d': _ } = {})",
        );
        // Binding Property
        test(
            "var { '0': _, 'a': _, [1]: _, ['1']: _, ['b']: _, ['c.c']: _, '1.1': _, '😊': _, 'd.d': _ } = {}",
            "var {  0: _,   a: _,    1: _,   1: _,     b: _,   'c.c': _, '1.1': _, '😊': _, 'd.d': _ } = {}",
        );
        // Method Definition
        test(
            "class F { '0'(){}; 'a'(){}; [1](){}; ['1'](){}; ['b'](){}; ['c.c'](){}; '1.1'(){}; '😊'(){}; 'd.d'(){} }",
            "class F {  0(){};   a(){};    1(){};    1(){};     b(){};   'c.c'(){}; '1.1'(){}; '😊'(){}; 'd.d'(){} }"
        );
        // Property Definition
        test(
            "class F { '0' = _; 'a' = _; [1] = _; ['1'] = _; ['b'] = _; ['c.c'] = _; '1.1' = _; '😊' = _; 'd.d' = _ }",
            "class F {  0 = _;   a = _;    1 = _;    1 = _;     b = _;   'c.c' = _; '1.1' = _; '😊' = _; 'd.d' = _ }"
        );
        // Accessor Property
        test(
            "class F { accessor '0' = _; accessor 'a' = _; accessor [1] = _; accessor ['1'] = _; accessor ['b'] = _; accessor ['c.c'] = _; accessor '1.1' = _; accessor '😊' = _; accessor 'd.d' = _ }",
            "class F { accessor  0 = _;  accessor  a = _;    accessor 1 = _;accessor     1 = _; accessor     b = _; accessor   'c.c' = _; accessor '1.1' = _; accessor '😊' = _; accessor 'd.d' = _ }"
        );
    }

    #[test]
    fn fold_function_spread_args() {
        test_same("f(...a)");
        test_same("f(...a, ...b)");
        test_same("f(...a, b, ...c)");

        test("f(...[])", "f()");
        test("f(...[1])", "f(1)");
        test("f(...[1, 2])", "f(1, 2)");
        test("f(...[1,,,3])", "f(1, void 0, void 0, 3)");
        test("f(a, ...[])", "f(a)");
    }

    #[test]
    fn test_fold_boolean_constructor() {
        test("var a = Boolean(true)", "var a = !0");
        // Don't fold the existence check to preserve behavior
        test("var a = Boolean?.(true)", "var a = Boolean?.(!0)");

        test("var a = Boolean(false)", "var a = !1");
        // Don't fold the existence check to preserve behavior
        test("var a = Boolean?.(false)", "var a = Boolean?.(!1)");

        test("var a = Boolean(1)", "var a = !!1");
        // Don't fold the existence check to preserve behavior
        test_same("var a = Boolean?.(1)");

        test("var a = Boolean(x)", "var a = !!x");
        // Don't fold the existence check to preserve behavior
        test_same("var a = Boolean?.(x)");

        test("var a = Boolean({})", "var a = !!{}");
        // Don't fold the existence check to preserve behavior
        test_same("var a = Boolean?.({})");

        test("var a = Boolean()", "var a = false;");
        test_same("var a = Boolean(!0, !1);");
    }

    #[test]
    fn test_fold_string_constructor() {
        test("String()", "''");
        test("var a = String(23)", "var a = '' + 23");
        // Don't fold the existence check to preserve behavior
        test_same("var a = String?.(23)");

        test("var a = String('hello')", "var a = '' + 'hello'");
        // Don't fold the existence check to preserve behavior
        test_same("var a = String?.('hello')");

        test_same("var a = String('hello', bar());");
        test_same("var a = String({valueOf: function() { return 1; }});");
    }

    #[test]
    fn test_fold_number_constructor() {
        test("Number()", "0");
        test("Number(true)", "1");
        test("Number(false)", "0");
        test("Number('foo')", "NaN");
    }

    #[test]
    fn test_fold_big_int_constructor() {
        test("BigInt(1n)", "1n");
        test_same("BigInt()");
        test_same("BigInt(1)");
    }

    #[test]
    fn optional_catch_binding() {
        test("try {} catch(e) {}", "try {} catch {}");
        test("try {} catch(e) {foo}", "try {} catch {foo}");
        test_same("try {} catch(e) {e}");
        test_same("try {} catch([e]) {}");
        test_same("try {} catch({e}) {}");

        let allocator = Allocator::default();
        let target = ESTarget::ES2018;
        let mut pass = super::PeepholeSubstituteAlternateSyntax::new(target, false);
        let code = "try {} catch(e) {}";
        tester::test(&allocator, code, code, &mut pass);
    }
}
