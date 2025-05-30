//! Declare symbol for `BindingIdentifier`s

use std::ptr;

use oxc_ast::{AstKind, ast::*};
use oxc_ecmascript::{BoundNames, IsSimpleParameterList};
use oxc_span::GetSpan;
use oxc_syntax::{
    scope::{ScopeFlags, ScopeId},
    symbol::SymbolFlags,
};

use crate::SemanticBuilder;

pub trait Binder<'a> {
    #[expect(unused_variables)]
    fn bind(&self, builder: &mut SemanticBuilder<'a>) {}
}

impl<'a> Binder<'a> for VariableDeclarator<'a> {
    fn bind(&self, builder: &mut SemanticBuilder<'a>) {
        let (includes, excludes) = match self.kind {
            VariableDeclarationKind::Const
            | VariableDeclarationKind::Using
            | VariableDeclarationKind::AwaitUsing => (
                SymbolFlags::BlockScopedVariable | SymbolFlags::ConstVariable,
                SymbolFlags::BlockScopedVariableExcludes,
            ),
            VariableDeclarationKind::Let => {
                (SymbolFlags::BlockScopedVariable, SymbolFlags::BlockScopedVariableExcludes)
            }
            VariableDeclarationKind::Var => {
                (SymbolFlags::FunctionScopedVariable, SymbolFlags::FunctionScopedVariableExcludes)
            }
        };

        if self.kind.is_lexical() {
            self.id.bound_names(&mut |ident| {
                let symbol_id = builder.declare_symbol(ident.span, &ident.name, includes, excludes);
                ident.symbol_id.set(Some(symbol_id));
            });
        } else {
            // ------------------ var hosting ------------------
            let mut target_scope_id = builder.current_scope_id;
            let mut var_scope_ids = vec![];

            // Collect all scopes where variable hoisting can occur
            for scope_id in builder.scoping.scope_ancestors(target_scope_id) {
                let flags = builder.scoping.scope_flags(scope_id);
                if flags.is_var() {
                    target_scope_id = scope_id;
                    break;
                }
                var_scope_ids.push(scope_id);
            }

            self.id.bound_names(&mut |ident| {
                let span = ident.span;
                let name = ident.name;
                let mut declared_symbol_id = None;

                for &scope_id in &var_scope_ids {
                    if let Some(symbol_id) =
                        builder.check_redeclaration(scope_id, span, &name, excludes, true)
                    {
                        builder.add_redeclare_variable(symbol_id, includes, span);
                        declared_symbol_id = Some(symbol_id);

                        // remove current scope binding and add to target scope
                        // avoid same symbols appear in multi-scopes
                        builder.scoping.remove_binding(scope_id, &name);
                        builder.scoping.add_binding(target_scope_id, &name, symbol_id);
                        builder.scoping.symbol_scope_ids[symbol_id] = target_scope_id;
                        break;
                    }
                }

                // If a variable is already declared in the hoisted scopes,
                // we don't need to create another symbol with the same name
                // to make sure they point to the same symbol.
                let symbol_id = declared_symbol_id.unwrap_or_else(|| {
                    builder.declare_symbol_on_scope(
                        span,
                        &name,
                        target_scope_id,
                        includes,
                        excludes,
                    )
                });
                ident.symbol_id.set(Some(symbol_id));

                // Finally, add the variable to all hoisted scopes
                // to support redeclaration checks when declaring variables with the same name later.
                for &scope_id in &var_scope_ids {
                    builder.hoisting_variables.entry(scope_id).or_default().insert(name, symbol_id);
                }
            });
        }

        // Save `@__NO_SIDE_EFFECTS__` for function initializers.
        if let BindingPatternKind::BindingIdentifier(id) = &self.id.kind {
            if let Some(symbol_id) = id.symbol_id.get() {
                if let Some(init) = &self.init {
                    if match init {
                        Expression::FunctionExpression(func) => func.pure,
                        Expression::ArrowFunctionExpression(func) => func.pure,
                        _ => false,
                    } {
                        builder.scoping.no_side_effects.insert(symbol_id);
                    }
                }
            }
        }
    }
}

impl<'a> Binder<'a> for Class<'a> {
    fn bind(&self, builder: &mut SemanticBuilder) {
        let Some(ident) = &self.id else { return };
        let symbol_id = builder.declare_symbol(
            ident.span,
            &ident.name,
            SymbolFlags::Class,
            SymbolFlags::ClassExcludes,
        );
        ident.symbol_id.set(Some(symbol_id));
    }
}

/// Check for Annex B `if (foo) function a() {} else function b() {}`
fn is_function_part_of_if_statement(function: &Function, builder: &SemanticBuilder) -> bool {
    if builder.current_scope_flags().is_strict_mode() {
        return false;
    }
    let Some(AstKind::IfStatement(stmt)) = builder.nodes.parent_kind(builder.current_node_id)
    else {
        return false;
    };
    if let Statement::FunctionDeclaration(func) = &stmt.consequent {
        if ptr::eq(func.as_ref(), function) {
            return true;
        }
    }
    if let Some(Statement::FunctionDeclaration(func)) = &stmt.alternate {
        if ptr::eq(func.as_ref(), function) {
            return true;
        }
    }
    false
}

impl<'a> Binder<'a> for Function<'a> {
    fn bind(&self, builder: &mut SemanticBuilder) {
        if let Some(ident) = &self.id {
            if is_function_part_of_if_statement(self, builder) {
                let symbol_id = builder.scoping.create_symbol(
                    ident.span,
                    ident.name.into(),
                    SymbolFlags::Function,
                    ScopeId::new(u32::MAX - 1), // Not bound to any scope.
                    builder.current_node_id,
                );
                ident.symbol_id.set(Some(symbol_id));
            } else {
                let symbol_id = builder.declare_symbol(
                    ident.span,
                    &ident.name,
                    SymbolFlags::Function,
                    if builder.source_type.is_typescript() {
                        SymbolFlags::FunctionExcludes
                    } else {
                        // `var x; function x() {}` is valid in non-strict mode, but `TypeScript`
                        // doesn't care about non-strict mode, so we need to exclude this,
                        // and further check in checker.
                        SymbolFlags::FunctionExcludes - SymbolFlags::FunctionScopedVariable
                    },
                );
                ident.symbol_id.set(Some(symbol_id));
            }
        }

        // Bind scope flags: GetAccessor | SetAccessor
        if let Some(AstKind::ObjectProperty(prop)) =
            builder.nodes.parent_kind(builder.current_node_id)
        {
            let flags = builder.scoping.scope_flags_mut(builder.current_scope_id);
            match prop.kind {
                PropertyKind::Get => *flags |= ScopeFlags::GetAccessor,
                PropertyKind::Set => *flags |= ScopeFlags::SetAccessor,
                PropertyKind::Init => {}
            }
        }

        // Save `@__NO_SIDE_EFFECTS__`
        if self.pure {
            if let Some(symbold_id) = self.id.as_ref().and_then(|id| id.symbol_id.get()) {
                builder.scoping.no_side_effects.insert(symbold_id);
            }
        }
    }
}

impl<'a> Binder<'a> for BindingRestElement<'a> {
    // Binds the FormalParameters's rest of a function or method.
    fn bind(&self, builder: &mut SemanticBuilder) {
        let parent_kind = builder.nodes.parent_kind(builder.current_node_id).unwrap();
        let AstKind::FormalParameters(parameters) = parent_kind else {
            return;
        };

        if parameters.kind.is_signature() {
            return;
        }

        let includes = SymbolFlags::FunctionScopedVariable;
        let excludes =
            SymbolFlags::FunctionScopedVariable | SymbolFlags::FunctionScopedVariableExcludes;
        self.bound_names(&mut |ident| {
            let symbol_id = builder.declare_symbol(ident.span, &ident.name, includes, excludes);
            ident.symbol_id.set(Some(symbol_id));
        });
    }
}

impl<'a> Binder<'a> for FormalParameter<'a> {
    // Binds the FormalParameter of a function or method.
    fn bind(&self, builder: &mut SemanticBuilder) {
        let parent_kind = builder.nodes.parent_kind(builder.current_node_id).unwrap();
        let AstKind::FormalParameters(parameters) = parent_kind else { unreachable!() };

        if parameters.kind.is_signature() {
            return;
        }

        let includes = SymbolFlags::FunctionScopedVariable;

        let is_not_allowed_duplicate_parameters = matches!(
                parameters.kind,
                // ArrowFormalParameters: UniqueFormalParameters
                FormalParameterKind::ArrowFormalParameters |
                // UniqueFormalParameters : FormalParameters
                // * It is a Syntax Error if BoundNames of FormalParameters contains any duplicate elements.
                FormalParameterKind::UniqueFormalParameters
            ) ||
            // Multiple occurrences of the same BindingIdentifier in a FormalParameterList is only allowed for functions which have simple parameter lists and which are not defined in strict mode code.
            builder.strict_mode() ||
            // FormalParameters : FormalParameterList
            // * It is a Syntax Error if IsSimpleParameterList of FormalParameterList is false and BoundNames of FormalParameterList contains any duplicate elements.
            !parameters.is_simple_parameter_list();

        let excludes = if is_not_allowed_duplicate_parameters {
            SymbolFlags::FunctionScopedVariable | SymbolFlags::FunctionScopedVariableExcludes
        } else {
            SymbolFlags::FunctionScopedVariableExcludes
        };

        self.bound_names(&mut |ident| {
            let symbol_id = builder.declare_symbol(ident.span, &ident.name, includes, excludes);
            ident.symbol_id.set(Some(symbol_id));
        });
    }
}

impl<'a> Binder<'a> for CatchParameter<'a> {
    fn bind(&self, builder: &mut SemanticBuilder) {
        let current_scope_id = builder.current_scope_id;
        // https://tc39.es/ecma262/#sec-variablestatements-in-catch-blocks
        // It is a Syntax Error if any element of the BoundNames of CatchParameter also occurs in the VarDeclaredNames of Block
        // unless CatchParameter is CatchParameter : BindingIdentifier
        if let BindingPatternKind::BindingIdentifier(ident) = &self.pattern.kind {
            let includes = SymbolFlags::FunctionScopedVariable | SymbolFlags::CatchVariable;
            let symbol_id =
                builder.declare_shadow_symbol(&ident.name, ident.span, current_scope_id, includes);
            ident.symbol_id.set(Some(symbol_id));
        } else {
            self.pattern.bound_names(&mut |ident| {
                let symbol_id = builder.declare_symbol(
                    ident.span,
                    &ident.name,
                    SymbolFlags::BlockScopedVariable | SymbolFlags::CatchVariable,
                    SymbolFlags::BlockScopedVariableExcludes,
                );
                ident.symbol_id.set(Some(symbol_id));
            });
        }
    }
}

fn declare_symbol_for_import_specifier(
    ident: &BindingIdentifier,
    is_type: bool,
    builder: &mut SemanticBuilder,
) {
    let includes = if is_type
        || builder.nodes.parent_kind(builder.current_node_id).is_some_and(
            |decl| matches!(decl, AstKind::ImportDeclaration(decl) if decl.import_kind.is_type()),
        ) {
        SymbolFlags::TypeImport
    } else {
        SymbolFlags::Import
    };

    let symbol_id = builder.declare_symbol(
        ident.span,
        &ident.name,
        includes,
        SymbolFlags::ImportBindingExcludes,
    );
    ident.symbol_id.set(Some(symbol_id));
}

impl<'a> Binder<'a> for ImportSpecifier<'a> {
    fn bind(&self, builder: &mut SemanticBuilder) {
        declare_symbol_for_import_specifier(&self.local, self.import_kind.is_type(), builder);
    }
}

impl<'a> Binder<'a> for ImportDefaultSpecifier<'a> {
    fn bind(&self, builder: &mut SemanticBuilder) {
        declare_symbol_for_import_specifier(&self.local, false, builder);
    }
}

impl<'a> Binder<'a> for ImportNamespaceSpecifier<'a> {
    fn bind(&self, builder: &mut SemanticBuilder) {
        declare_symbol_for_import_specifier(&self.local, false, builder);
    }
}

impl<'a> Binder<'a> for TSImportEqualsDeclaration<'a> {
    fn bind(&self, builder: &mut SemanticBuilder) {
        declare_symbol_for_import_specifier(&self.id, false, builder);
    }
}

impl<'a> Binder<'a> for TSTypeAliasDeclaration<'a> {
    fn bind(&self, builder: &mut SemanticBuilder) {
        let symbol_id = builder.declare_symbol(
            self.id.span,
            &self.id.name,
            SymbolFlags::TypeAlias,
            SymbolFlags::TypeAliasExcludes,
        );
        self.id.symbol_id.set(Some(symbol_id));
    }
}

impl<'a> Binder<'a> for TSInterfaceDeclaration<'a> {
    fn bind(&self, builder: &mut SemanticBuilder) {
        let symbol_id = builder.declare_symbol(
            self.id.span,
            &self.id.name,
            SymbolFlags::Interface,
            SymbolFlags::InterfaceExcludes,
        );
        self.id.symbol_id.set(Some(symbol_id));
    }
}

impl<'a> Binder<'a> for TSEnumDeclaration<'a> {
    fn bind(&self, builder: &mut SemanticBuilder) {
        let is_const = self.r#const;
        let includes = if is_const { SymbolFlags::ConstEnum } else { SymbolFlags::RegularEnum };
        let excludes = if is_const {
            SymbolFlags::ConstEnumExcludes
        } else {
            SymbolFlags::RegularEnumExcludes
        };
        let symbol_id = builder.declare_symbol(self.id.span, &self.id.name, includes, excludes);
        self.id.symbol_id.set(Some(symbol_id));
    }
}

impl<'a> Binder<'a> for TSEnumMember<'a> {
    fn bind(&self, builder: &mut SemanticBuilder) {
        builder.declare_symbol(
            self.span,
            self.id.static_name().as_str(),
            SymbolFlags::EnumMember,
            SymbolFlags::EnumMemberExcludes,
        );
    }
}

impl<'a> Binder<'a> for TSModuleDeclaration<'a> {
    fn bind(&self, builder: &mut SemanticBuilder) {
        // do not bind `global` for `declare global { ... }`
        if self.kind == TSModuleDeclarationKind::Global {
            return;
        }

        // At declaration time a module has no value declaration it is only when a value declaration
        // is made inside a the scope of a module that the symbol is modified
        let ambient = if self.declare { SymbolFlags::Ambient } else { SymbolFlags::None };
        let symbol_id = builder.declare_symbol(
            self.id.span(),
            self.id.name().as_str(),
            SymbolFlags::NameSpaceModule | ambient,
            SymbolFlags::None,
        );

        if let TSModuleDeclarationName::Identifier(id) = &self.id {
            id.symbol_id.set(Some(symbol_id));
        }
    }
}

impl<'a> Binder<'a> for TSTypeParameter<'a> {
    fn bind(&self, builder: &mut SemanticBuilder) {
        let scope_id = if matches!(
            builder.nodes.parent_kind(builder.current_node_id),
            Some(AstKind::TSInferType(_))
        ) {
            builder
                .scoping
                .scope_ancestors(builder.current_scope_id)
                .find(|scope_id| builder.scoping.scope_flags(*scope_id).is_ts_conditional())
        } else {
            None
        };

        let symbol_id = builder.declare_symbol_on_scope(
            self.name.span,
            &self.name.name,
            scope_id.unwrap_or(builder.current_scope_id),
            SymbolFlags::TypeParameter,
            SymbolFlags::TypeParameterExcludes,
        );
        self.name.symbol_id.set(Some(symbol_id));
    }
}
