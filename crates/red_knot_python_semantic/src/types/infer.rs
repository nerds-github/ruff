//! We have Salsa queries for inferring types at three different granularities: scope-level,
//! definition-level, and expression-level.
//!
//! Scope-level inference is for when we are actually checking a file, and need to check types for
//! everything in that file's scopes, or give a linter access to types of arbitrary expressions
//! (via the [`HasTy`](crate::semantic_model::HasTy) trait).
//!
//! Definition-level inference allows us to look up the types of symbols in other scopes (e.g. for
//! imports) with the minimum inference necessary, so that if we're looking up one symbol from a
//! very large module, we can avoid a bunch of unnecessary work. Definition-level inference also
//! allows us to handle import cycles without getting into a cycle of scope-level inference
//! queries.
//!
//! The expression-level inference query is needed in only a few cases. Since some assignments can
//! have multiple targets (via `x = y = z` or unpacking `(x, y) = z`, they can be associated with
//! multiple definitions (one per assigned symbol). In order to avoid inferring the type of the
//! right-hand side once per definition, we infer it as a standalone query, so its result will be
//! cached by Salsa. We also need the expression-level query for inferring types in type guard
//! expressions (e.g. the test clause of an `if` statement.)
//!
//! Inferring types at any of the three region granularities returns a [`TypeInference`], which
//! holds types for every [`Definition`] and expression within the inferred region.
//!
//! Some type expressions can require deferred evaluation. This includes all type expressions in
//! stub files, or annotation expressions in modules with `from __future__ import annotations`, or
//! stringified annotations. We have a fourth Salsa query for inferring the deferred types
//! associated with a particular definition. Scope-level inference infers deferred types for all
//! definitions once the rest of the types in the scope have been inferred.
use std::borrow::Cow;
use std::num::NonZeroU32;

use itertools::Itertools;
use ruff_db::files::File;
use ruff_db::parsed::parsed_module;
use ruff_python_ast::name::Name;
use ruff_python_ast::{self as ast, AnyNodeRef, Expr, ExprContext, UnaryOp};
use rustc_hash::FxHashMap;
use salsa;
use salsa::plumbing::AsId;

use crate::module_name::ModuleName;
use crate::module_resolver::{file_to_module, resolve_module};
use crate::semantic_index::ast_ids::{HasScopedAstId, HasScopedUseId, ScopedExpressionId};
use crate::semantic_index::definition::{
    AssignmentKind, Definition, DefinitionKind, DefinitionNodeKey, ExceptHandlerDefinitionKind,
};
use crate::semantic_index::expression::Expression;
use crate::semantic_index::semantic_index;
use crate::semantic_index::symbol::{NodeWithScopeKind, NodeWithScopeRef, ScopeId};
use crate::semantic_index::SemanticIndex;
use crate::stdlib::builtins_module_scope;
use crate::types::diagnostic::{
    TypeCheckDiagnostic, TypeCheckDiagnostics, TypeCheckDiagnosticsBuilder,
};
use crate::types::{
    bindings_ty, builtins_symbol_ty, declarations_ty, global_symbol_ty, symbol_ty,
    typing_extensions_symbol_ty, BytesLiteralType, ClassType, FunctionType, IterationOutcome,
    KnownClass, KnownFunction, SliceLiteralType, StringLiteralType, Truthiness, TupleType, Type,
    TypeArrayDisplay, UnionBuilder, UnionType,
};
use crate::util::subscript::{PyIndex, PySlice};
use crate::Db;

/// Infer all types for a [`ScopeId`], including all definitions and expressions in that scope.
/// Use when checking a scope, or needing to provide a type for an arbitrary expression in the
/// scope.
#[salsa::tracked(return_ref)]
pub(crate) fn infer_scope_types<'db>(db: &'db dyn Db, scope: ScopeId<'db>) -> TypeInference<'db> {
    let file = scope.file(db);
    let _span =
        tracing::trace_span!("infer_scope_types", scope=?scope.as_id(), file=%file.path(db))
            .entered();

    // Using the index here is fine because the code below depends on the AST anyway.
    // The isolation of the query is by the return inferred types.
    let index = semantic_index(db, file);

    TypeInferenceBuilder::new(db, InferenceRegion::Scope(scope), index).finish()
}

/// Cycle recovery for [`infer_definition_types()`]: for now, just [`Type::Unknown`]
/// TODO fixpoint iteration
fn infer_definition_types_cycle_recovery<'db>(
    db: &'db dyn Db,
    _cycle: &salsa::Cycle,
    input: Definition<'db>,
) -> TypeInference<'db> {
    tracing::trace!("infer_definition_types_cycle_recovery");
    let mut inference = TypeInference::empty(input.scope(db));
    let category = input.category(db);
    if category.is_declaration() {
        inference.declarations.insert(input, Type::Unknown);
    }
    if category.is_binding() {
        inference.bindings.insert(input, Type::Unknown);
    }
    // TODO we don't fill in expression types for the cycle-participant definitions, which can
    // later cause a panic when looking up an expression type.
    inference
}

/// Infer all types for a [`Definition`] (including sub-expressions).
/// Use when resolving a symbol name use or public type of a symbol.
#[salsa::tracked(return_ref, recovery_fn=infer_definition_types_cycle_recovery)]
pub(crate) fn infer_definition_types<'db>(
    db: &'db dyn Db,
    definition: Definition<'db>,
) -> TypeInference<'db> {
    let file = definition.file(db);
    let _span = tracing::trace_span!(
        "infer_definition_types",
        definition = ?definition.as_id(),
        file = %file.path(db)
    )
    .entered();

    let index = semantic_index(db, file);

    TypeInferenceBuilder::new(db, InferenceRegion::Definition(definition), index).finish()
}

/// Infer types for all deferred type expressions in a [`Definition`].
///
/// Deferred expressions are type expressions (annotations, base classes, aliases...) in a stub
/// file, or in a file with `from __future__ import annotations`, or stringified annotations.
#[salsa::tracked(return_ref)]
pub(crate) fn infer_deferred_types<'db>(
    db: &'db dyn Db,
    definition: Definition<'db>,
) -> TypeInference<'db> {
    let file = definition.file(db);
    let _span = tracing::trace_span!(
        "infer_deferred_types",
        definition = ?definition.as_id(),
        file = %file.path(db)
    )
    .entered();

    let index = semantic_index(db, file);

    TypeInferenceBuilder::new(db, InferenceRegion::Deferred(definition), index).finish()
}

/// Infer all types for an [`Expression`] (including sub-expressions).
/// Use rarely; only for cases where we'd otherwise risk double-inferring an expression: RHS of an
/// assignment, which might be unpacking/multi-target and thus part of multiple definitions, or a
/// type narrowing guard expression (e.g. if statement test node).
#[allow(unused)]
#[salsa::tracked(return_ref)]
pub(crate) fn infer_expression_types<'db>(
    db: &'db dyn Db,
    expression: Expression<'db>,
) -> TypeInference<'db> {
    let file = expression.file(db);
    let _span =
        tracing::trace_span!("infer_expression_types", expression=?expression.as_id(), file=%file.path(db))
            .entered();

    let index = semantic_index(db, file);

    TypeInferenceBuilder::new(db, InferenceRegion::Expression(expression), index).finish()
}

/// A region within which we can infer types.
pub(crate) enum InferenceRegion<'db> {
    /// infer types for a standalone [`Expression`]
    Expression(Expression<'db>),
    /// infer types for a [`Definition`]
    Definition(Definition<'db>),
    /// infer deferred types for a [`Definition`]
    Deferred(Definition<'db>),
    /// infer types for an entire [`ScopeId`]
    Scope(ScopeId<'db>),
}

/// The inferred types for a single region.
#[derive(Debug, Eq, PartialEq)]
pub(crate) struct TypeInference<'db> {
    /// The types of every expression in this region.
    expressions: FxHashMap<ScopedExpressionId, Type<'db>>,

    /// The types of every binding in this region.
    bindings: FxHashMap<Definition<'db>, Type<'db>>,

    /// The types of every declaration in this region.
    declarations: FxHashMap<Definition<'db>, Type<'db>>,

    /// The diagnostics for this region.
    diagnostics: TypeCheckDiagnostics,

    /// Are there deferred type expressions in this region?
    has_deferred: bool,

    /// The scope belong to this region.
    scope: ScopeId<'db>,
}

impl<'db> TypeInference<'db> {
    pub(crate) fn empty(scope: ScopeId<'db>) -> Self {
        Self {
            expressions: FxHashMap::default(),
            bindings: FxHashMap::default(),
            declarations: FxHashMap::default(),
            diagnostics: TypeCheckDiagnostics::default(),
            has_deferred: false,
            scope,
        }
    }

    #[track_caller]
    pub(crate) fn expression_ty(&self, expression: ScopedExpressionId) -> Type<'db> {
        self.expressions[&expression]
    }

    pub(crate) fn try_expression_ty(&self, expression: ScopedExpressionId) -> Option<Type<'db>> {
        self.expressions.get(&expression).copied()
    }

    #[track_caller]
    pub(crate) fn binding_ty(&self, definition: Definition<'db>) -> Type<'db> {
        self.bindings[&definition]
    }

    #[track_caller]
    pub(crate) fn declaration_ty(&self, definition: Definition<'db>) -> Type<'db> {
        self.declarations[&definition]
    }

    pub(crate) fn diagnostics(&self) -> &[std::sync::Arc<TypeCheckDiagnostic>] {
        &self.diagnostics
    }

    fn shrink_to_fit(&mut self) {
        self.expressions.shrink_to_fit();
        self.bindings.shrink_to_fit();
        self.declarations.shrink_to_fit();
        self.diagnostics.shrink_to_fit();
    }
}

/// Builder to infer all types in a region.
///
/// A builder is used by creating it with [`new()`](TypeInferenceBuilder::new), and then calling
/// [`finish()`](TypeInferenceBuilder::finish) on it, which returns the resulting
/// [`TypeInference`].
///
/// There are a few different kinds of methods in the type inference builder, and the naming
/// distinctions are a bit subtle.
///
/// The `finish` method calls [`infer_region`](TypeInferenceBuilder::infer_region), which delegates
/// to one of [`infer_region_scope`](TypeInferenceBuilder::infer_region_scope),
/// [`infer_region_definition`](TypeInferenceBuilder::infer_region_definition), or
/// [`infer_region_expression`](TypeInferenceBuilder::infer_region_expression), depending which
/// kind of [`InferenceRegion`] we are inferring types for.
///
/// Scope inference starts with the scope body, walking all statements and expressions and
/// recording the types of each expression in the [`TypeInference`] result. Most of the methods
/// here (with names like `infer_*_statement` or `infer_*_expression` or some other node kind) take
/// a single AST node and are called as part of this AST visit.
///
/// When the visit encounters a node which creates a [`Definition`], we look up the definition in
/// the semantic index and call the [`infer_definition_types()`] query on it, which creates another
/// [`TypeInferenceBuilder`] just for that definition, and we merge the returned [`TypeInference`]
/// into the one we are currently building for the entire scope. Using the query in this way
/// ensures that if we first infer types for some scattered definitions in a scope, and later for
/// the entire scope, we don't re-infer any types, we re-use the cached inference for those
/// definitions and their sub-expressions.
///
/// Functions with a name like `infer_*_definition` take both a node and a [`Definition`], and are
/// called by [`infer_region_definition`](TypeInferenceBuilder::infer_region_definition).
///
/// So for example we have both
/// [`infer_function_definition_statement`](TypeInferenceBuilder::infer_function_definition_statement),
/// which takes just the function AST node, and
/// [`infer_function_definition`](TypeInferenceBuilder::infer_function_definition), which takes
/// both the node and the [`Definition`] id. The former is called as part of walking the AST, and
/// it just looks up the [`Definition`] for that function in the semantic index and calls
/// [`infer_definition_types()`] on it, which will create a new [`TypeInferenceBuilder`] with
/// [`InferenceRegion::Definition`], and in that builder
/// [`infer_region_definition`](TypeInferenceBuilder::infer_region_definition) will call
/// [`infer_function_definition`](TypeInferenceBuilder::infer_function_definition) to actually
/// infer a type for the definition.
///
/// Similarly, when we encounter a standalone-inferable expression (right-hand side of an
/// assignment, type narrowing guard), we use the [`infer_expression_types()`] query to ensure we
/// don't infer its types more than once.
pub(super) struct TypeInferenceBuilder<'db> {
    db: &'db dyn Db,
    index: &'db SemanticIndex<'db>,
    region: InferenceRegion<'db>,

    // Cached lookups
    file: File,

    /// The type inference results
    types: TypeInference<'db>,

    diagnostics: TypeCheckDiagnosticsBuilder<'db>,
}

impl<'db> TypeInferenceBuilder<'db> {
    /// How big a string do we build before bailing?
    ///
    /// This is a fairly arbitrary number. It should be *far* more than enough
    /// for most use cases, but we can reevaluate it later if useful.
    const MAX_STRING_LITERAL_SIZE: usize = 4096;

    /// Creates a new builder for inferring types in a region.
    pub(super) fn new(
        db: &'db dyn Db,
        region: InferenceRegion<'db>,
        index: &'db SemanticIndex<'db>,
    ) -> Self {
        let (file, scope) = match region {
            InferenceRegion::Expression(expression) => (expression.file(db), expression.scope(db)),
            InferenceRegion::Definition(definition) | InferenceRegion::Deferred(definition) => {
                (definition.file(db), definition.scope(db))
            }
            InferenceRegion::Scope(scope) => (scope.file(db), scope),
        };

        Self {
            db,
            index,
            region,
            file,
            types: TypeInference::empty(scope),
            diagnostics: TypeCheckDiagnosticsBuilder::new(db, file),
        }
    }

    fn extend(&mut self, inference: &TypeInference<'db>) {
        debug_assert_eq!(self.types.scope, inference.scope);

        self.types.bindings.extend(inference.bindings.iter());
        self.types
            .declarations
            .extend(inference.declarations.iter());
        self.types.expressions.extend(inference.expressions.iter());
        self.diagnostics.extend(&inference.diagnostics);
        self.types.has_deferred |= inference.has_deferred;
    }

    fn scope(&self) -> ScopeId<'db> {
        self.types.scope
    }

    /// Are we currently inferring types in file with deferred types?
    /// This is true for stub files and files with `__future__.annotations`
    fn are_all_types_deferred(&self) -> bool {
        self.index.has_future_annotations() || self.file.is_stub(self.db.upcast())
    }

    /// Are we currently inferring deferred types?
    fn is_deferred(&self) -> bool {
        matches!(self.region, InferenceRegion::Deferred(_))
    }

    /// Get the already-inferred type of an expression node.
    ///
    /// PANIC if no type has been inferred for this node.
    fn expression_ty(&self, expr: &ast::Expr) -> Type<'db> {
        self.types
            .expression_ty(expr.scoped_ast_id(self.db, self.scope()))
    }

    /// Infers types in the given [`InferenceRegion`].
    fn infer_region(&mut self) {
        match self.region {
            InferenceRegion::Scope(scope) => self.infer_region_scope(scope),
            InferenceRegion::Definition(definition) => self.infer_region_definition(definition),
            InferenceRegion::Deferred(definition) => self.infer_region_deferred(definition),
            InferenceRegion::Expression(expression) => self.infer_region_expression(expression),
        }
    }

    fn infer_region_scope(&mut self, scope: ScopeId<'db>) {
        let node = scope.node(self.db);
        match node {
            NodeWithScopeKind::Module => {
                let parsed = parsed_module(self.db.upcast(), self.file);
                self.infer_module(parsed.syntax());
            }
            NodeWithScopeKind::Function(function) => self.infer_function_body(function.node()),
            NodeWithScopeKind::Lambda(lambda) => self.infer_lambda_body(lambda.node()),
            NodeWithScopeKind::Class(class) => self.infer_class_body(class.node()),
            NodeWithScopeKind::ClassTypeParameters(class) => {
                self.infer_class_type_params(class.node());
            }
            NodeWithScopeKind::FunctionTypeParameters(function) => {
                self.infer_function_type_params(function.node());
            }
            NodeWithScopeKind::ListComprehension(comprehension) => {
                self.infer_list_comprehension_expression_scope(comprehension.node());
            }
            NodeWithScopeKind::SetComprehension(comprehension) => {
                self.infer_set_comprehension_expression_scope(comprehension.node());
            }
            NodeWithScopeKind::DictComprehension(comprehension) => {
                self.infer_dict_comprehension_expression_scope(comprehension.node());
            }
            NodeWithScopeKind::GeneratorExpression(generator) => {
                self.infer_generator_expression_scope(generator.node());
            }
        }

        if self.types.has_deferred {
            let mut deferred_expression_types: FxHashMap<ScopedExpressionId, Type<'db>> =
                FxHashMap::default();
            // invariant: only annotations and base classes are deferred, and both of these only
            // occur within a declaration (annotated assignment, function or class definition)
            for definition in self.types.declarations.keys() {
                if infer_definition_types(self.db, *definition).has_deferred {
                    let deferred = infer_deferred_types(self.db, *definition);
                    deferred_expression_types.extend(deferred.expressions.iter());
                }
            }
            self.types
                .expressions
                .extend(deferred_expression_types.iter());
        }
    }

    fn infer_region_definition(&mut self, definition: Definition<'db>) {
        match definition.kind(self.db) {
            DefinitionKind::Function(function) => {
                self.infer_function_definition(function.node(), definition);
            }
            DefinitionKind::Class(class) => self.infer_class_definition(class.node(), definition),
            DefinitionKind::Import(import) => {
                self.infer_import_definition(import.node(), definition);
            }
            DefinitionKind::ImportFrom(import_from) => {
                self.infer_import_from_definition(
                    import_from.import(),
                    import_from.alias(),
                    definition,
                );
            }
            DefinitionKind::Assignment(assignment) => {
                self.infer_assignment_definition(
                    assignment.target(),
                    assignment.value(),
                    assignment.name(),
                    assignment.kind(),
                    definition,
                );
            }
            DefinitionKind::AnnotatedAssignment(annotated_assignment) => {
                self.infer_annotated_assignment_definition(annotated_assignment.node(), definition);
            }
            DefinitionKind::AugmentedAssignment(augmented_assignment) => {
                self.infer_augment_assignment_definition(augmented_assignment.node(), definition);
            }
            DefinitionKind::For(for_statement_definition) => {
                self.infer_for_statement_definition(
                    for_statement_definition.target(),
                    for_statement_definition.iterable(),
                    for_statement_definition.is_async(),
                    definition,
                );
            }
            DefinitionKind::NamedExpression(named_expression) => {
                self.infer_named_expression_definition(named_expression.node(), definition);
            }
            DefinitionKind::Comprehension(comprehension) => {
                self.infer_comprehension_definition(
                    comprehension.iterable(),
                    comprehension.target(),
                    comprehension.is_first(),
                    comprehension.is_async(),
                    definition,
                );
            }
            DefinitionKind::Parameter(parameter) => {
                self.infer_parameter_definition(parameter, definition);
            }
            DefinitionKind::ParameterWithDefault(parameter_with_default) => {
                self.infer_parameter_with_default_definition(parameter_with_default, definition);
            }
            DefinitionKind::WithItem(with_item) => {
                self.infer_with_item_definition(with_item.target(), with_item.node(), definition);
            }
            DefinitionKind::MatchPattern(match_pattern) => {
                self.infer_match_pattern_definition(
                    match_pattern.pattern(),
                    match_pattern.index(),
                    definition,
                );
            }
            DefinitionKind::ExceptHandler(except_handler_definition) => {
                self.infer_except_handler_definition(except_handler_definition, definition);
            }
        }
    }

    fn infer_region_deferred(&mut self, definition: Definition<'db>) {
        match definition.kind(self.db) {
            DefinitionKind::Function(function) => self.infer_function_deferred(function.node()),
            DefinitionKind::Class(class) => self.infer_class_deferred(class.node()),
            DefinitionKind::AnnotatedAssignment(_annotated_assignment) => {
                // TODO self.infer_annotated_assignment_deferred(annotated_assignment.node());
            }
            _ => {}
        }
    }

    fn infer_region_expression(&mut self, expression: Expression<'db>) {
        self.infer_expression(expression.node_ref(self.db));
    }

    /// Raise a diagnostic if the given type cannot be divided by zero.
    ///
    /// Expects the resolved type of the left side of the binary expression.
    fn check_division_by_zero(&mut self, expr: &ast::ExprBinOp, left: Type<'db>) {
        match left {
            Type::BooleanLiteral(_) | Type::IntLiteral(_) => {}
            Type::Instance(cls)
                if [KnownClass::Float, KnownClass::Int, KnownClass::Bool]
                    .iter()
                    .any(|&k| cls.is_known(self.db, k)) => {}
            _ => return,
        };

        let (op, by_zero) = match expr.op {
            ast::Operator::Div => ("divide", "by zero"),
            ast::Operator::FloorDiv => ("floor divide", "by zero"),
            ast::Operator::Mod => ("reduce", "modulo zero"),
            _ => return,
        };

        self.diagnostics.add(
            expr.into(),
            "division-by-zero",
            format_args!(
                "Cannot {op} object of type `{}` {by_zero}",
                left.display(self.db)
            ),
        );
    }

    fn add_binding(&mut self, node: AnyNodeRef, binding: Definition<'db>, ty: Type<'db>) {
        debug_assert!(binding.is_binding(self.db));
        let use_def = self.index.use_def_map(binding.file_scope(self.db));
        let declarations = use_def.declarations_at_binding(binding);
        let undeclared_ty = if declarations.may_be_undeclared() {
            Some(Type::Unknown)
        } else {
            None
        };
        let mut bound_ty = ty;
        let declared_ty = declarations_ty(self.db, declarations, undeclared_ty).unwrap_or_else(
            |(ty, conflicting)| {
                // TODO point out the conflicting declarations in the diagnostic?
                let symbol_table = self.index.symbol_table(binding.file_scope(self.db));
                let symbol_name = symbol_table.symbol(binding.symbol(self.db)).name();
                self.diagnostics.add(
                    node,
                    "conflicting-declarations",
                    format_args!(
                        "Conflicting declared types for `{symbol_name}`: {}",
                        conflicting.display(self.db)
                    ),
                );
                ty
            },
        );
        if !bound_ty.is_assignable_to(self.db, declared_ty) {
            self.diagnostics
                .add_invalid_assignment(node, declared_ty, bound_ty);
            // allow declarations to override inference in case of invalid assignment
            bound_ty = declared_ty;
        };

        self.types.bindings.insert(binding, bound_ty);
    }

    fn add_declaration(&mut self, node: AnyNodeRef, declaration: Definition<'db>, ty: Type<'db>) {
        debug_assert!(declaration.is_declaration(self.db));
        let use_def = self.index.use_def_map(declaration.file_scope(self.db));
        let prior_bindings = use_def.bindings_at_declaration(declaration);
        // unbound_ty is Never because for this check we don't care about unbound
        let inferred_ty = bindings_ty(self.db, prior_bindings, Some(Type::Never));
        let ty = if inferred_ty.is_assignable_to(self.db, ty) {
            ty
        } else {
            self.diagnostics.add(
                node,
                "invalid-declaration",
                format_args!(
                    "Cannot declare type `{}` for inferred type `{}`",
                    ty.display(self.db),
                    inferred_ty.display(self.db)
                ),
            );
            Type::Unknown
        };
        self.types.declarations.insert(declaration, ty);
    }

    fn add_declaration_with_binding(
        &mut self,
        node: AnyNodeRef,
        definition: Definition<'db>,
        declared_ty: Type<'db>,
        inferred_ty: Type<'db>,
    ) {
        debug_assert!(definition.is_binding(self.db));
        debug_assert!(definition.is_declaration(self.db));
        let inferred_ty = if inferred_ty.is_assignable_to(self.db, declared_ty) {
            inferred_ty
        } else {
            self.diagnostics
                .add_invalid_assignment(node, declared_ty, inferred_ty);
            // if the assignment is invalid, fall back to assuming the annotation is correct
            declared_ty
        };
        self.types.declarations.insert(definition, declared_ty);
        self.types.bindings.insert(definition, inferred_ty);
    }

    fn infer_module(&mut self, module: &ast::ModModule) {
        self.infer_body(&module.body);
    }

    fn infer_class_type_params(&mut self, class: &ast::StmtClassDef) {
        let type_params = class
            .type_params
            .as_deref()
            .expect("class type params scope without type params");

        self.infer_type_parameters(type_params);

        if let Some(arguments) = class.arguments.as_deref() {
            self.infer_arguments(arguments);
        }
    }

    fn infer_class_body(&mut self, class: &ast::StmtClassDef) {
        self.infer_body(&class.body);
    }

    fn infer_function_type_params(&mut self, function: &ast::StmtFunctionDef) {
        let type_params = function
            .type_params
            .as_deref()
            .expect("function type params scope without type params");

        // TODO: defer annotation resolution in stubs, with __future__.annotations, or stringified
        self.infer_optional_expression(function.returns.as_deref());
        self.infer_type_parameters(type_params);
        self.infer_parameters(&function.parameters);
    }

    fn infer_function_body(&mut self, function: &ast::StmtFunctionDef) {
        self.infer_body(&function.body);
    }

    fn infer_body(&mut self, suite: &[ast::Stmt]) {
        for statement in suite {
            self.infer_statement(statement);
        }
    }

    fn infer_statement(&mut self, statement: &ast::Stmt) {
        match statement {
            ast::Stmt::FunctionDef(function) => self.infer_function_definition_statement(function),
            ast::Stmt::ClassDef(class) => self.infer_class_definition_statement(class),
            ast::Stmt::Expr(ast::StmtExpr { range: _, value }) => {
                self.infer_expression(value);
            }
            ast::Stmt::If(if_statement) => self.infer_if_statement(if_statement),
            ast::Stmt::Try(try_statement) => self.infer_try_statement(try_statement),
            ast::Stmt::With(with_statement) => self.infer_with_statement(with_statement),
            ast::Stmt::Match(match_statement) => self.infer_match_statement(match_statement),
            ast::Stmt::Assign(assign) => self.infer_assignment_statement(assign),
            ast::Stmt::AnnAssign(assign) => self.infer_annotated_assignment_statement(assign),
            ast::Stmt::AugAssign(aug_assign) => {
                self.infer_augmented_assignment_statement(aug_assign);
            }
            ast::Stmt::TypeAlias(type_statement) => self.infer_type_alias_statement(type_statement),
            ast::Stmt::For(for_statement) => self.infer_for_statement(for_statement),
            ast::Stmt::While(while_statement) => self.infer_while_statement(while_statement),
            ast::Stmt::Import(import) => self.infer_import_statement(import),
            ast::Stmt::ImportFrom(import) => self.infer_import_from_statement(import),
            ast::Stmt::Assert(assert_statement) => self.infer_assert_statement(assert_statement),
            ast::Stmt::Raise(raise) => self.infer_raise_statement(raise),
            ast::Stmt::Return(ret) => self.infer_return_statement(ret),
            ast::Stmt::Delete(delete) => self.infer_delete_statement(delete),
            ast::Stmt::Break(_)
            | ast::Stmt::Continue(_)
            | ast::Stmt::Pass(_)
            | ast::Stmt::IpyEscapeCommand(_)
            | ast::Stmt::Global(_)
            | ast::Stmt::Nonlocal(_) => {
                // No-op
            }
        }
    }

    fn infer_definition(&mut self, node: impl Into<DefinitionNodeKey>) {
        let definition = self.index.definition(node);
        let result = infer_definition_types(self.db, definition);
        self.extend(result);
    }

    fn infer_function_definition_statement(&mut self, function: &ast::StmtFunctionDef) {
        self.infer_definition(function);
    }

    fn infer_function_definition(
        &mut self,
        function: &ast::StmtFunctionDef,
        definition: Definition<'db>,
    ) {
        let ast::StmtFunctionDef {
            range: _,
            is_async: _,
            name,
            type_params,
            parameters,
            returns,
            body: _,
            decorator_list,
        } = function;

        let decorator_tys: Box<[Type]> = decorator_list
            .iter()
            .map(|decorator| self.infer_decorator(decorator))
            .collect();

        for default in parameters
            .iter_non_variadic_params()
            .filter_map(|param| param.default.as_deref())
        {
            self.infer_expression(default);
        }

        // If there are type params, parameters and returns are evaluated in that scope, that is, in
        // `infer_function_type_params`, rather than here.
        if type_params.is_none() {
            self.infer_parameters(parameters);

            // TODO: this should also be applied to parameter annotations.
            if self.are_all_types_deferred() {
                self.types.has_deferred = true;
            } else {
                self.infer_optional_annotation_expression(returns.as_deref());
            }
        }

        let function_kind = match &**name {
            "reveal_type" if definition.is_typing_definition(self.db) => {
                Some(KnownFunction::RevealType)
            }
            "isinstance" if definition.is_builtin_definition(self.db) => {
                Some(KnownFunction::IsInstance)
            }
            _ => None,
        };
        let function_ty = Type::FunctionLiteral(FunctionType::new(
            self.db,
            name.id.clone(),
            function_kind,
            definition,
            decorator_tys,
        ));

        self.add_declaration_with_binding(function.into(), definition, function_ty, function_ty);
    }

    fn infer_parameters(&mut self, parameters: &ast::Parameters) {
        let ast::Parameters {
            range: _,
            posonlyargs: _,
            args: _,
            vararg,
            kwonlyargs: _,
            kwarg,
        } = parameters;

        for param_with_default in parameters.iter_non_variadic_params() {
            self.infer_parameter_with_default(param_with_default);
        }
        if let Some(vararg) = vararg {
            self.infer_parameter(vararg);
        }
        if let Some(kwarg) = kwarg {
            self.infer_parameter(kwarg);
        }
    }

    fn infer_parameter_with_default(&mut self, parameter_with_default: &ast::ParameterWithDefault) {
        let ast::ParameterWithDefault {
            range: _,
            parameter,
            default: _,
        } = parameter_with_default;

        self.infer_optional_expression(parameter.annotation.as_deref());
    }

    fn infer_parameter(&mut self, parameter: &ast::Parameter) {
        let ast::Parameter {
            range: _,
            name: _,
            annotation,
        } = parameter;

        self.infer_optional_expression(annotation.as_deref());
    }

    fn infer_parameter_with_default_definition(
        &mut self,
        parameter_with_default: &ast::ParameterWithDefault,
        definition: Definition<'db>,
    ) {
        // TODO(dhruvmanila): Infer types from annotation or default expression
        // TODO check that default is assignable to parameter type
        self.infer_parameter_definition(&parameter_with_default.parameter, definition);
    }

    fn infer_parameter_definition(
        &mut self,
        parameter: &ast::Parameter,
        definition: Definition<'db>,
    ) {
        // TODO(dhruvmanila): Annotation expression is resolved at the enclosing scope, infer the
        // parameter type from there
        let annotated_ty = Type::Todo;
        if parameter.annotation.is_some() {
            self.add_declaration_with_binding(
                parameter.into(),
                definition,
                annotated_ty,
                annotated_ty,
            );
        } else {
            self.add_binding(parameter.into(), definition, annotated_ty);
        }
    }

    fn infer_class_definition_statement(&mut self, class: &ast::StmtClassDef) {
        self.infer_definition(class);
    }

    fn infer_class_definition(&mut self, class: &ast::StmtClassDef, definition: Definition<'db>) {
        let ast::StmtClassDef {
            range: _,
            name,
            type_params,
            decorator_list,
            arguments: _,
            body: _,
        } = class;

        for decorator in decorator_list {
            self.infer_decorator(decorator);
        }

        let body_scope = self
            .index
            .node_scope(NodeWithScopeRef::Class(class))
            .to_scope_id(self.db, self.file);

        let maybe_known_class = file_to_module(self.db, body_scope.file(self.db))
            .as_ref()
            .and_then(|module| KnownClass::maybe_from_module(module, name.as_str()));

        let class_ty = Type::ClassLiteral(ClassType::new(
            self.db,
            name.id.clone(),
            definition,
            body_scope,
            maybe_known_class,
        ));

        self.add_declaration_with_binding(class.into(), definition, class_ty, class_ty);

        // if there are type parameters, then the keywords and bases are within that scope
        // and we don't need to run inference here
        if type_params.is_none() {
            for keyword in class.keywords() {
                self.infer_expression(&keyword.value);
            }

            // Inference of bases deferred in stubs
            // TODO also defer stringified generic type parameters
            if self.are_all_types_deferred() {
                self.types.has_deferred = true;
            } else {
                for base in class.bases() {
                    self.infer_expression(base);
                }
            }
        }
    }

    fn infer_function_deferred(&mut self, function: &ast::StmtFunctionDef) {
        self.infer_optional_annotation_expression(function.returns.as_deref());
    }

    fn infer_class_deferred(&mut self, class: &ast::StmtClassDef) {
        if self.are_all_types_deferred() {
            for base in class.bases() {
                self.infer_expression(base);
            }
        }
    }

    fn infer_if_statement(&mut self, if_statement: &ast::StmtIf) {
        let ast::StmtIf {
            range: _,
            test,
            body,
            elif_else_clauses,
        } = if_statement;

        self.infer_expression(test);
        self.infer_body(body);

        for clause in elif_else_clauses {
            let ast::ElifElseClause {
                range: _,
                test,
                body,
            } = clause;

            self.infer_optional_expression(test.as_ref());

            self.infer_body(body);
        }
    }

    fn infer_try_statement(&mut self, try_statement: &ast::StmtTry) {
        let ast::StmtTry {
            range: _,
            body,
            handlers,
            orelse,
            finalbody,
            is_star: _,
        } = try_statement;

        self.infer_body(body);

        for handler in handlers {
            let ast::ExceptHandler::ExceptHandler(handler) = handler;
            let ast::ExceptHandlerExceptHandler {
                type_: handled_exceptions,
                name: symbol_name,
                body,
                range: _,
            } = handler;

            // If `symbol_name` is `Some()` and `handled_exceptions` is `None`,
            // it's invalid syntax (something like `except as e:`).
            // However, it's obvious that the user *wanted* `e` to be bound here,
            // so we'll have created a definition in the semantic-index stage anyway.
            if symbol_name.is_some() {
                self.infer_definition(handler);
            } else {
                self.infer_optional_expression(handled_exceptions.as_deref());
            }

            self.infer_body(body);
        }

        self.infer_body(orelse);
        self.infer_body(finalbody);
    }

    fn infer_with_statement(&mut self, with_statement: &ast::StmtWith) {
        let ast::StmtWith {
            range: _,
            is_async: _,
            items,
            body,
        } = with_statement;

        for item in items {
            let target = item.optional_vars.as_deref();
            if let Some(ast::Expr::Name(name)) = target {
                self.infer_definition(name);
            } else {
                // TODO infer definitions in unpacking assignment
                self.infer_expression(&item.context_expr);
                self.infer_optional_expression(target);
            }
        }

        self.infer_body(body);
    }

    fn infer_with_item_definition(
        &mut self,
        target: &ast::ExprName,
        with_item: &ast::WithItem,
        definition: Definition<'db>,
    ) {
        let expression = self.index.expression(&with_item.context_expr);
        let result = infer_expression_types(self.db, expression);
        self.extend(result);

        // TODO(dhruvmanila): The correct type inference here is the return type of the __enter__
        // method of the context manager.
        let context_expr_ty = self.expression_ty(&with_item.context_expr);

        self.types
            .expressions
            .insert(target.scoped_ast_id(self.db, self.scope()), context_expr_ty);
        self.add_binding(target.into(), definition, context_expr_ty);
    }

    fn infer_except_handler_definition(
        &mut self,
        except_handler_definition: &ExceptHandlerDefinitionKind,
        definition: Definition<'db>,
    ) {
        let node_ty = except_handler_definition
            .handled_exceptions()
            .map(|ty| self.infer_expression(ty))
            // If there is no handled exception, it's invalid syntax;
            // a diagnostic will have already been emitted
            .unwrap_or(Type::Unknown);

        let symbol_ty = if except_handler_definition.is_star() {
            // TODO should be generic --Alex
            //
            // TODO should infer `ExceptionGroup` if all caught exceptions
            // are subclasses of `Exception` --Alex
            builtins_symbol_ty(self.db, "BaseExceptionGroup").to_instance(self.db)
        } else {
            // TODO: anything that's a consistent subtype of
            // `type[BaseException] | tuple[type[BaseException], ...]` should be valid;
            // anything else is invalid and should lead to a diagnostic being reported --Alex
            match node_ty {
                Type::Any | Type::Unknown => node_ty,
                Type::ClassLiteral(class_ty) => Type::Instance(class_ty),
                Type::Tuple(tuple) => UnionType::from_elements(
                    self.db,
                    tuple.elements(self.db).iter().map(|ty| {
                        ty.into_class_literal_type()
                            .map_or(Type::Todo, Type::Instance)
                    }),
                ),
                _ => Type::Todo,
            }
        };

        self.add_binding(
            except_handler_definition.node().into(),
            definition,
            symbol_ty,
        );
    }

    fn infer_match_statement(&mut self, match_statement: &ast::StmtMatch) {
        let ast::StmtMatch {
            range: _,
            subject,
            cases,
        } = match_statement;

        let expression = self.index.expression(subject.as_ref());
        let result = infer_expression_types(self.db, expression);
        self.extend(result);

        for case in cases {
            let ast::MatchCase {
                range: _,
                body,
                pattern,
                guard,
            } = case;
            self.infer_match_pattern(pattern);
            self.infer_optional_expression(guard.as_deref());
            self.infer_body(body);
        }
    }

    fn infer_match_pattern_definition(
        &mut self,
        pattern: &ast::Pattern,
        _index: u32,
        definition: Definition<'db>,
    ) {
        // TODO(dhruvmanila): The correct way to infer types here is to perform structural matching
        // against the subject expression type (which we can query via `infer_expression_types`)
        // and extract the type at the `index` position if the pattern matches. This will be
        // similar to the logic in `self.infer_assignment_definition`.
        self.add_binding(pattern.into(), definition, Type::Todo);
    }

    fn infer_match_pattern(&mut self, pattern: &ast::Pattern) {
        // TODO(dhruvmanila): Add a Salsa query for inferring pattern types and matching against
        // the subject expression: https://github.com/astral-sh/ruff/pull/13147#discussion_r1739424510
        match pattern {
            ast::Pattern::MatchValue(match_value) => {
                self.infer_expression(&match_value.value);
            }
            ast::Pattern::MatchSequence(match_sequence) => {
                for pattern in &match_sequence.patterns {
                    self.infer_match_pattern(pattern);
                }
            }
            ast::Pattern::MatchMapping(match_mapping) => {
                let ast::PatternMatchMapping {
                    range: _,
                    keys,
                    patterns,
                    rest: _,
                } = match_mapping;
                for key in keys {
                    self.infer_expression(key);
                }
                for pattern in patterns {
                    self.infer_match_pattern(pattern);
                }
            }
            ast::Pattern::MatchClass(match_class) => {
                let ast::PatternMatchClass {
                    range: _,
                    cls,
                    arguments,
                } = match_class;
                for pattern in &arguments.patterns {
                    self.infer_match_pattern(pattern);
                }
                for keyword in &arguments.keywords {
                    self.infer_match_pattern(&keyword.pattern);
                }
                self.infer_expression(cls);
            }
            ast::Pattern::MatchAs(match_as) => {
                if let Some(pattern) = &match_as.pattern {
                    self.infer_match_pattern(pattern);
                }
            }
            ast::Pattern::MatchOr(match_or) => {
                for pattern in &match_or.patterns {
                    self.infer_match_pattern(pattern);
                }
            }
            ast::Pattern::MatchStar(_) | ast::Pattern::MatchSingleton(_) => {}
        };
    }

    fn infer_assignment_statement(&mut self, assignment: &ast::StmtAssign) {
        let ast::StmtAssign {
            range: _,
            targets,
            value,
        } = assignment;

        for target in targets {
            self.infer_assignment_target(target, value);
        }
    }

    // TODO: Remove the `value` argument once we handle all possible assignment targets.
    fn infer_assignment_target(&mut self, target: &ast::Expr, value: &ast::Expr) {
        match target {
            ast::Expr::Name(name) => self.infer_definition(name),
            ast::Expr::List(ast::ExprList { elts, .. })
            | ast::Expr::Tuple(ast::ExprTuple { elts, .. }) => {
                for element in elts {
                    self.infer_assignment_target(element, value);
                }
            }
            _ => {
                // TODO: Remove this once we handle all possible assignment targets.
                let expression = self.index.expression(value);
                self.extend(infer_expression_types(self.db, expression));
                self.infer_expression(target);
            }
        }
    }

    fn infer_assignment_definition(
        &mut self,
        target: &ast::Expr,
        value: &ast::Expr,
        name: &ast::ExprName,
        kind: AssignmentKind,
        definition: Definition<'db>,
    ) {
        let expression = self.index.expression(value);
        let result = infer_expression_types(self.db, expression);
        self.extend(result);

        let value_ty = self.expression_ty(value);

        let target_ty = match kind {
            AssignmentKind::Sequence => self.infer_sequence_unpacking(target, value_ty, name),
            AssignmentKind::Name => value_ty,
        };

        self.add_binding(name.into(), definition, target_ty);
        self.types
            .expressions
            .insert(name.scoped_ast_id(self.db, self.scope()), target_ty);
    }

    fn infer_sequence_unpacking(
        &mut self,
        target: &ast::Expr,
        value_ty: Type<'db>,
        name: &ast::ExprName,
    ) -> Type<'db> {
        // The inner function is recursive and only differs in the return type which is an `Option`
        // where if the variable is found, the corresponding type is returned otherwise `None`.
        fn inner<'db>(
            builder: &mut TypeInferenceBuilder<'db>,
            target: &ast::Expr,
            value_ty: Type<'db>,
            name: &ast::ExprName,
        ) -> Option<Type<'db>> {
            match target {
                ast::Expr::Name(target_name) if target_name == name => {
                    return Some(value_ty);
                }
                ast::Expr::Starred(ast::ExprStarred { value, .. }) => {
                    return inner(builder, value, value_ty, name);
                }
                ast::Expr::List(ast::ExprList { elts, .. })
                | ast::Expr::Tuple(ast::ExprTuple { elts, .. }) => match value_ty {
                    Type::Tuple(tuple_ty) => {
                        let starred_index = elts.iter().position(ast::Expr::is_starred_expr);

                        let element_types = if let Some(starred_index) = starred_index {
                            if tuple_ty.len(builder.db) >= elts.len() - 1 {
                                let mut element_types = Vec::with_capacity(elts.len());
                                element_types.extend_from_slice(
                                    // SAFETY: Safe because of the length check above.
                                    &tuple_ty.elements(builder.db)[..starred_index],
                                );

                                // E.g., in `(a, *b, c, d) = ...`, the index of starred element `b`
                                // is 1 and the remaining elements after that are 2.
                                let remaining = elts.len() - (starred_index + 1);
                                // This index represents the type of the last element that belongs
                                // to the starred expression, in an exclusive manner.
                                let starred_end_index = tuple_ty.len(builder.db) - remaining;
                                // SAFETY: Safe because of the length check above.
                                let _starred_element_types = &tuple_ty.elements(builder.db)
                                    [starred_index..starred_end_index];
                                // TODO: Combine the types into a list type. If the
                                // starred_element_types is empty, then it should be `List[Any]`.
                                // combine_types(starred_element_types);
                                element_types.push(Type::Todo);

                                element_types.extend_from_slice(
                                    // SAFETY: Safe because of the length check above.
                                    &tuple_ty.elements(builder.db)[starred_end_index..],
                                );
                                Cow::Owned(element_types)
                            } else {
                                let mut element_types = tuple_ty.elements(builder.db).to_vec();
                                // Subtract 1 to insert the starred expression type at the correct
                                // index.
                                element_types.resize(elts.len() - 1, Type::Unknown);
                                // TODO: This should be `list[Unknown]`
                                element_types.insert(starred_index, Type::Todo);
                                Cow::Owned(element_types)
                            }
                        } else {
                            Cow::Borrowed(tuple_ty.elements(builder.db).as_ref())
                        };

                        for (index, element) in elts.iter().enumerate() {
                            if let Some(ty) = inner(
                                builder,
                                element,
                                element_types.get(index).copied().unwrap_or(Type::Unknown),
                                name,
                            ) {
                                return Some(ty);
                            }
                        }
                    }
                    Type::StringLiteral(string_literal_ty) => {
                        // Deconstruct the string literal to delegate the inference back to the
                        // tuple type for correct handling of starred expressions. We could go
                        // further and deconstruct to an array of `StringLiteral` with each
                        // individual character, instead of just an array of `LiteralString`, but
                        // there would be a cost and it's not clear that it's worth it.
                        let value_ty = Type::Tuple(TupleType::new(
                            builder.db,
                            vec![Type::LiteralString; string_literal_ty.len(builder.db)]
                                .into_boxed_slice(),
                        ));
                        if let Some(ty) = inner(builder, target, value_ty, name) {
                            return Some(ty);
                        }
                    }
                    _ => {
                        let value_ty = if value_ty.is_literal_string() {
                            Type::LiteralString
                        } else {
                            value_ty.iterate(builder.db).unwrap_with_diagnostic(
                                AnyNodeRef::from(target),
                                &mut builder.diagnostics,
                            )
                        };
                        for element in elts {
                            if let Some(ty) = inner(builder, element, value_ty, name) {
                                return Some(ty);
                            }
                        }
                    }
                },
                _ => {}
            }
            None
        }

        inner(self, target, value_ty, name).unwrap_or(Type::Unknown)
    }

    fn infer_annotated_assignment_statement(&mut self, assignment: &ast::StmtAnnAssign) {
        // assignments to non-Names are not Definitions
        if matches!(*assignment.target, ast::Expr::Name(_)) {
            self.infer_definition(assignment);
        } else {
            let ast::StmtAnnAssign {
                range: _,
                annotation,
                value,
                target,
                simple: _,
            } = assignment;
            self.infer_annotation_expression(annotation);
            self.infer_optional_expression(value.as_deref());
            self.infer_expression(target);
        }
    }

    fn infer_annotated_assignment_definition(
        &mut self,
        assignment: &ast::StmtAnnAssign,
        definition: Definition<'db>,
    ) {
        let ast::StmtAnnAssign {
            range: _,
            target,
            annotation,
            value,
            simple: _,
        } = assignment;

        let annotation_ty = self.infer_annotation_expression(annotation);
        if let Some(value) = value {
            let value_ty = self.infer_expression(value);
            self.add_declaration_with_binding(
                assignment.into(),
                definition,
                annotation_ty,
                value_ty,
            );
        } else {
            self.add_declaration(assignment.into(), definition, annotation_ty);
        }

        self.infer_expression(target);
    }

    fn infer_augmented_assignment_statement(&mut self, assignment: &ast::StmtAugAssign) {
        if assignment.target.is_name_expr() {
            self.infer_definition(assignment);
        } else {
            // TODO currently we don't consider assignments to non-Names to be Definitions
            self.infer_augment_assignment(assignment);
        }
    }

    fn infer_augment_assignment_definition(
        &mut self,
        assignment: &ast::StmtAugAssign,
        definition: Definition<'db>,
    ) {
        let target_ty = self.infer_augment_assignment(assignment);
        self.add_binding(assignment.into(), definition, target_ty);
    }

    fn infer_augment_assignment(&mut self, assignment: &ast::StmtAugAssign) -> Type<'db> {
        let ast::StmtAugAssign {
            range: _,
            target,
            op,
            value,
        } = assignment;

        // Resolve the target type, assuming a load context.
        let target_type = match &**target {
            Expr::Name(name) => {
                self.store_expression_type(target, Type::Never);
                self.infer_name_load(name)
            }
            Expr::Attribute(attr) => {
                self.store_expression_type(target, Type::Never);
                self.infer_attribute_load(attr)
            }
            _ => self.infer_expression(target),
        };
        let value_type = self.infer_expression(value);

        // If the target defines, e.g., `__iadd__`, infer the augmented assignment as a call to that
        // dunder.
        if let Type::Instance(class) = target_type {
            let class_member = class.class_member(self.db, op.in_place_dunder());
            if !class_member.is_unbound() {
                let call = class_member.call(self.db, &[target_type, value_type]);
                return match call.return_ty_result(
                    self.db,
                    AnyNodeRef::StmtAugAssign(assignment),
                    &mut self.diagnostics,
                ) {
                    Ok(t) => t,
                    Err(e) => {
                        self.diagnostics.add(
                            assignment.into(),
                            "unsupported-operator",
                            format_args!(
                                "Operator `{op}=` is unsupported between objects of type `{}` and `{}`",
                                target_type.display(self.db),
                                value_type.display(self.db)
                            ),
                        );
                        e.return_ty()
                    }
                };
            }
        }

        let left_ty = target_type;
        let right_ty = value_type;

        self.infer_binary_expression_type(left_ty, right_ty, *op)
            .unwrap_or_else(|| {
                self.diagnostics.add(
                    assignment.into(),
                    "unsupported-operator",
                    format_args!(
                        "Operator `{op}=` is unsupported between objects of type `{}` and `{}`",
                        left_ty.display(self.db),
                        right_ty.display(self.db)
                    ),
                );
                Type::Unknown
            })
    }

    fn infer_type_alias_statement(&mut self, type_alias_statement: &ast::StmtTypeAlias) {
        let ast::StmtTypeAlias {
            range: _,
            name,
            type_params,
            value,
        } = type_alias_statement;
        self.infer_expression(value);
        self.infer_expression(name);
        if let Some(type_params) = type_params {
            self.infer_type_parameters(type_params);
        }
    }

    fn infer_for_statement(&mut self, for_statement: &ast::StmtFor) {
        let ast::StmtFor {
            range: _,
            target,
            iter,
            body,
            orelse,
            is_async: _,
        } = for_statement;

        self.infer_expression(iter);
        // TODO more complex assignment targets
        if let ast::Expr::Name(name) = &**target {
            self.infer_definition(name);
        } else {
            self.infer_expression(target);
        }
        self.infer_body(body);
        self.infer_body(orelse);
    }

    fn infer_for_statement_definition(
        &mut self,
        target: &ast::ExprName,
        iterable: &ast::Expr,
        is_async: bool,
        definition: Definition<'db>,
    ) {
        let expression = self.index.expression(iterable);
        let result = infer_expression_types(self.db, expression);
        self.extend(result);
        let iterable_ty = self.expression_ty(iterable);

        let loop_var_value_ty = if is_async {
            // TODO(Alex): async iterables/iterators!
            Type::Todo
        } else {
            iterable_ty
                .iterate(self.db)
                .unwrap_with_diagnostic(iterable.into(), &mut self.diagnostics)
        };

        self.types.expressions.insert(
            target.scoped_ast_id(self.db, self.scope()),
            loop_var_value_ty,
        );
        self.add_binding(target.into(), definition, loop_var_value_ty);
    }

    fn infer_while_statement(&mut self, while_statement: &ast::StmtWhile) {
        let ast::StmtWhile {
            range: _,
            test,
            body,
            orelse,
        } = while_statement;

        self.infer_expression(test);
        self.infer_body(body);
        self.infer_body(orelse);
    }

    fn infer_import_statement(&mut self, import: &ast::StmtImport) {
        let ast::StmtImport { range: _, names } = import;

        for alias in names {
            self.infer_definition(alias);
        }
    }

    fn infer_import_definition(&mut self, alias: &'db ast::Alias, definition: Definition<'db>) {
        let ast::Alias {
            range: _,
            name,
            asname: _,
        } = alias;

        let module_ty = if let Some(module_name) = ModuleName::new(name) {
            if let Some(module) = self.module_ty_from_name(&module_name) {
                module
            } else {
                self.diagnostics.add_unresolved_module(alias, 0, Some(name));
                Type::Unknown
            }
        } else {
            tracing::debug!("Failed to resolve import due to invalid syntax");
            Type::Unknown
        };

        self.add_declaration_with_binding(alias.into(), definition, module_ty, module_ty);
    }

    fn infer_import_from_statement(&mut self, import: &ast::StmtImportFrom) {
        let ast::StmtImportFrom {
            range: _,
            module: _,
            names,
            level: _,
        } = import;

        for alias in names {
            self.infer_definition(alias);
        }
    }

    fn infer_assert_statement(&mut self, assert: &ast::StmtAssert) {
        let ast::StmtAssert {
            range: _,
            test,
            msg,
        } = assert;

        self.infer_expression(test);
        self.infer_optional_expression(msg.as_deref());
    }

    fn infer_raise_statement(&mut self, raise: &ast::StmtRaise) {
        let ast::StmtRaise {
            range: _,
            exc,
            cause,
        } = raise;
        self.infer_optional_expression(exc.as_deref());
        self.infer_optional_expression(cause.as_deref());
    }

    /// Given a `from .foo import bar` relative import, resolve the relative module
    /// we're importing `bar` from into an absolute [`ModuleName`]
    /// using the name of the module we're currently analyzing.
    ///
    /// - `level` is the number of dots at the beginning of the relative module name:
    ///   - `from .foo.bar import baz` => `level == 1`
    ///   - `from ...foo.bar import baz` => `level == 3`
    /// - `tail` is the relative module name stripped of all leading dots:
    ///   - `from .foo import bar` => `tail == "foo"`
    ///   - `from ..foo.bar import baz` => `tail == "foo.bar"`
    fn relative_module_name(
        &self,
        tail: Option<&str>,
        level: NonZeroU32,
    ) -> Result<ModuleName, ModuleNameResolutionError> {
        let module = file_to_module(self.db, self.file)
            .ok_or(ModuleNameResolutionError::UnknownCurrentModule)?;
        let mut level = level.get();
        if module.kind().is_package() {
            level -= 1;
        }
        let mut module_name = module.name().clone();
        for _ in 0..level {
            module_name = module_name
                .parent()
                .ok_or(ModuleNameResolutionError::TooManyDots)?;
        }
        if let Some(tail) = tail {
            let tail = ModuleName::new(tail).ok_or(ModuleNameResolutionError::InvalidSyntax)?;
            module_name.extend(&tail);
        }
        Ok(module_name)
    }

    fn infer_import_from_definition(
        &mut self,
        import_from: &'db ast::StmtImportFrom,
        alias: &ast::Alias,
        definition: Definition<'db>,
    ) {
        // TODO:
        // - Absolute `*` imports (`from collections import *`)
        // - Relative `*` imports (`from ...foo import *`)
        // - Submodule imports (`from collections import abc`,
        //   where `abc` is a submodule of the `collections` package)
        //
        // For the last item, see the currently skipped tests
        // `follow_relative_import_bare_to_module()` and
        // `follow_nonexistent_import_bare_to_module()`.
        let ast::StmtImportFrom { module, level, .. } = import_from;
        let module = module.as_deref();

        let module_name = if let Some(level) = NonZeroU32::new(*level) {
            tracing::trace!(
                "Resolving imported object `{}` from module `{}` relative to file `{}`",
                alias.name,
                format_import_from_module(level.get(), module),
                self.file.path(self.db),
            );
            self.relative_module_name(module, level)
        } else {
            tracing::trace!(
                "Resolving imported object `{}` from module `{}`",
                alias.name,
                format_import_from_module(*level, module),
            );
            module
                .and_then(ModuleName::new)
                .ok_or(ModuleNameResolutionError::InvalidSyntax)
        };

        let ty = match module_name {
            Ok(module_name) => {
                if let Some(module_ty) = self.module_ty_from_name(&module_name) {
                    let ast::Alias {
                        range: _,
                        name,
                        asname: _,
                    } = alias;

                    let member_ty = module_ty.member(self.db, &ast::name::Name::new(&name.id));

                    if member_ty.is_unbound() {
                        self.diagnostics.add(
                            AnyNodeRef::Alias(alias),
                            "unresolved-import",
                            format_args!("Module `{module_name}` has no member `{name}`",),
                        );

                        Type::Unknown
                    } else {
                        // For possibly-unbound names, just eliminate Unbound from the type; we
                        // must be in a bound path. TODO diagnostic for maybe-unbound import?
                        member_ty.replace_unbound_with(self.db, Type::Never)
                    }
                } else {
                    self.diagnostics
                        .add_unresolved_module(import_from, *level, module);
                    Type::Unknown
                }
            }
            Err(ModuleNameResolutionError::InvalidSyntax) => {
                tracing::debug!("Failed to resolve import due to invalid syntax");
                // Invalid syntax diagnostics are emitted elsewhere.
                Type::Unknown
            }
            Err(ModuleNameResolutionError::TooManyDots) => {
                tracing::debug!(
                    "Relative module resolution `{}` failed: too many leading dots",
                    format_import_from_module(*level, module),
                );
                self.diagnostics
                    .add_unresolved_module(import_from, *level, module);
                Type::Unknown
            }
            Err(ModuleNameResolutionError::UnknownCurrentModule) => {
                tracing::debug!(
                    "Relative module resolution `{}` failed; could not resolve file `{}` to a module",
                    format_import_from_module(*level, module),
                    self.file.path(self.db)
                );
                self.diagnostics
                    .add_unresolved_module(import_from, *level, module);
                Type::Unknown
            }
        };

        self.add_declaration_with_binding(alias.into(), definition, ty, ty);
    }

    fn infer_return_statement(&mut self, ret: &ast::StmtReturn) {
        self.infer_optional_expression(ret.value.as_deref());
    }

    fn infer_delete_statement(&mut self, delete: &ast::StmtDelete) {
        let ast::StmtDelete { range: _, targets } = delete;
        for target in targets {
            self.infer_expression(target);
        }
    }

    fn module_ty_from_name(&self, module_name: &ModuleName) -> Option<Type<'db>> {
        resolve_module(self.db, module_name).map(|module| Type::ModuleLiteral(module.file()))
    }

    fn infer_decorator(&mut self, decorator: &ast::Decorator) -> Type<'db> {
        let ast::Decorator {
            range: _,
            expression,
        } = decorator;

        self.infer_expression(expression)
    }

    fn infer_arguments(&mut self, arguments: &ast::Arguments) -> Vec<Type<'db>> {
        let mut types = Vec::with_capacity(
            arguments
                .args
                .len()
                .saturating_add(arguments.keywords.len()),
        );

        types.extend(arguments.args.iter().map(|arg| self.infer_expression(arg)));

        types.extend(arguments.keywords.iter().map(
            |ast::Keyword {
                 range: _,
                 arg: _,
                 value,
             }| self.infer_expression(value),
        ));

        types
    }

    fn infer_optional_expression(&mut self, expression: Option<&ast::Expr>) -> Option<Type<'db>> {
        expression.map(|expr| self.infer_expression(expr))
    }

    fn infer_optional_annotation_expression(
        &mut self,
        expr: Option<&ast::Expr>,
    ) -> Option<Type<'db>> {
        expr.map(|expr| self.infer_annotation_expression(expr))
    }

    fn infer_expression(&mut self, expression: &ast::Expr) -> Type<'db> {
        let ty = match expression {
            ast::Expr::NoneLiteral(ast::ExprNoneLiteral { range: _ }) => Type::None,
            ast::Expr::NumberLiteral(literal) => self.infer_number_literal_expression(literal),
            ast::Expr::BooleanLiteral(literal) => self.infer_boolean_literal_expression(literal),
            ast::Expr::StringLiteral(literal) => self.infer_string_literal_expression(literal),
            ast::Expr::BytesLiteral(bytes_literal) => {
                self.infer_bytes_literal_expression(bytes_literal)
            }
            ast::Expr::FString(fstring) => self.infer_fstring_expression(fstring),
            ast::Expr::EllipsisLiteral(literal) => self.infer_ellipsis_literal_expression(literal),
            ast::Expr::Tuple(tuple) => self.infer_tuple_expression(tuple),
            ast::Expr::List(list) => self.infer_list_expression(list),
            ast::Expr::Set(set) => self.infer_set_expression(set),
            ast::Expr::Dict(dict) => self.infer_dict_expression(dict),
            ast::Expr::Generator(generator) => self.infer_generator_expression(generator),
            ast::Expr::ListComp(listcomp) => self.infer_list_comprehension_expression(listcomp),
            ast::Expr::DictComp(dictcomp) => self.infer_dict_comprehension_expression(dictcomp),
            ast::Expr::SetComp(setcomp) => self.infer_set_comprehension_expression(setcomp),
            ast::Expr::Name(name) => self.infer_name_expression(name),
            ast::Expr::Attribute(attribute) => self.infer_attribute_expression(attribute),
            ast::Expr::UnaryOp(unary_op) => self.infer_unary_expression(unary_op),
            ast::Expr::BinOp(binary) => self.infer_binary_expression(binary),
            ast::Expr::BoolOp(bool_op) => self.infer_boolean_expression(bool_op),
            ast::Expr::Compare(compare) => self.infer_compare_expression(compare),
            ast::Expr::Subscript(subscript) => self.infer_subscript_expression(subscript),
            ast::Expr::Slice(slice) => self.infer_slice_expression(slice),
            ast::Expr::Named(named) => self.infer_named_expression(named),
            ast::Expr::If(if_expression) => self.infer_if_expression(if_expression),
            ast::Expr::Lambda(lambda_expression) => self.infer_lambda_expression(lambda_expression),
            ast::Expr::Call(call_expression) => self.infer_call_expression(call_expression),
            ast::Expr::Starred(starred) => self.infer_starred_expression(starred),
            ast::Expr::Yield(yield_expression) => self.infer_yield_expression(yield_expression),
            ast::Expr::YieldFrom(yield_from) => self.infer_yield_from_expression(yield_from),
            ast::Expr::Await(await_expression) => self.infer_await_expression(await_expression),
            ast::Expr::IpyEscapeCommand(_) => todo!("Implement Ipy escape command support"),
        };

        self.store_expression_type(expression, ty);

        ty
    }

    fn store_expression_type(&mut self, expression: &ast::Expr, ty: Type<'db>) {
        let expr_id = expression.scoped_ast_id(self.db, self.scope());
        let previous = self.types.expressions.insert(expr_id, ty);
        assert_eq!(previous, None);
    }

    fn infer_number_literal_expression(&mut self, literal: &ast::ExprNumberLiteral) -> Type<'db> {
        let ast::ExprNumberLiteral { range: _, value } = literal;

        match value {
            ast::Number::Int(n) => n
                .as_i64()
                .map(Type::IntLiteral)
                .unwrap_or_else(|| KnownClass::Int.to_instance(self.db)),
            ast::Number::Float(_) => KnownClass::Float.to_instance(self.db),
            ast::Number::Complex { .. } => {
                builtins_symbol_ty(self.db, "complex").to_instance(self.db)
            }
        }
    }

    #[allow(clippy::unused_self)]
    fn infer_boolean_literal_expression(&mut self, literal: &ast::ExprBooleanLiteral) -> Type<'db> {
        let ast::ExprBooleanLiteral { range: _, value } = literal;

        Type::BooleanLiteral(*value)
    }

    fn infer_string_literal_expression(&mut self, literal: &ast::ExprStringLiteral) -> Type<'db> {
        if literal.value.len() <= Self::MAX_STRING_LITERAL_SIZE {
            Type::StringLiteral(StringLiteralType::new(self.db, literal.value.to_str()))
        } else {
            Type::LiteralString
        }
    }

    fn infer_bytes_literal_expression(&mut self, literal: &ast::ExprBytesLiteral) -> Type<'db> {
        // TODO: ignoring r/R prefixes for now, should normalize bytes values
        Type::BytesLiteral(BytesLiteralType::new(
            self.db,
            literal.value.bytes().collect::<Box<[u8]>>(),
        ))
    }

    fn infer_fstring_expression(&mut self, fstring: &ast::ExprFString) -> Type<'db> {
        let ast::ExprFString { range: _, value } = fstring;

        let mut collector = StringPartsCollector::new();
        for part in value {
            // Make sure we iter through every parts to infer all sub-expressions. The `collector`
            // struct ensures we don't allocate unnecessary strings.
            match part {
                ast::FStringPart::Literal(literal) => {
                    collector.push_str(&literal.value);
                }
                ast::FStringPart::FString(fstring) => {
                    for element in &fstring.elements {
                        match element {
                            ast::FStringElement::Expression(expression) => {
                                let ast::FStringExpressionElement {
                                    range: _,
                                    expression,
                                    debug_text: _,
                                    conversion,
                                    format_spec,
                                } = expression;
                                let ty = self.infer_expression(expression);

                                // TODO: handle format specifiers by calling a method
                                // (`Type::format`?) that handles the `__format__` method.
                                // Conversion flags should be handled before calling `__format__`.
                                // https://docs.python.org/3/library/string.html#format-string-syntax
                                if !conversion.is_none() || format_spec.is_some() {
                                    collector.add_expression();
                                } else {
                                    if let Type::StringLiteral(literal) = ty.str(self.db) {
                                        collector.push_str(literal.value(self.db));
                                    } else {
                                        collector.add_expression();
                                    }
                                }
                            }
                            ast::FStringElement::Literal(literal) => {
                                collector.push_str(&literal.value);
                            }
                        }
                    }
                }
            }
        }
        collector.ty(self.db)
    }

    fn infer_ellipsis_literal_expression(
        &mut self,
        _literal: &ast::ExprEllipsisLiteral,
    ) -> Type<'db> {
        builtins_symbol_ty(self.db, "Ellipsis")
    }

    fn infer_tuple_expression(&mut self, tuple: &ast::ExprTuple) -> Type<'db> {
        let ast::ExprTuple {
            range: _,
            elts,
            ctx: _,
            parenthesized: _,
        } = tuple;

        let element_types = elts
            .iter()
            .map(|elt| self.infer_expression(elt))
            .collect::<Vec<_>>();

        Type::Tuple(TupleType::new(self.db, element_types.into_boxed_slice()))
    }

    fn infer_list_expression(&mut self, list: &ast::ExprList) -> Type<'db> {
        let ast::ExprList {
            range: _,
            elts,
            ctx: _,
        } = list;

        for elt in elts {
            self.infer_expression(elt);
        }

        // TODO generic
        KnownClass::List.to_instance(self.db)
    }

    fn infer_set_expression(&mut self, set: &ast::ExprSet) -> Type<'db> {
        let ast::ExprSet { range: _, elts } = set;

        for elt in elts {
            self.infer_expression(elt);
        }

        // TODO generic
        KnownClass::Set.to_instance(self.db)
    }

    fn infer_dict_expression(&mut self, dict: &ast::ExprDict) -> Type<'db> {
        let ast::ExprDict { range: _, items } = dict;

        for item in items {
            self.infer_optional_expression(item.key.as_ref());
            self.infer_expression(&item.value);
        }

        // TODO generic
        KnownClass::Dict.to_instance(self.db)
    }

    /// Infer the type of the `iter` expression of the first comprehension.
    fn infer_first_comprehension_iter(&mut self, comprehensions: &[ast::Comprehension]) {
        let mut comprehensions_iter = comprehensions.iter();
        let Some(first_comprehension) = comprehensions_iter.next() else {
            unreachable!("Comprehension must contain at least one generator");
        };
        self.infer_expression(&first_comprehension.iter);
    }

    fn infer_generator_expression(&mut self, generator: &ast::ExprGenerator) -> Type<'db> {
        let ast::ExprGenerator {
            range: _,
            elt: _,
            generators,
            parenthesized: _,
        } = generator;

        self.infer_first_comprehension_iter(generators);

        // TODO generator type
        Type::Todo
    }

    fn infer_list_comprehension_expression(&mut self, listcomp: &ast::ExprListComp) -> Type<'db> {
        let ast::ExprListComp {
            range: _,
            elt: _,
            generators,
        } = listcomp;

        self.infer_first_comprehension_iter(generators);

        // TODO list type
        Type::Todo
    }

    fn infer_dict_comprehension_expression(&mut self, dictcomp: &ast::ExprDictComp) -> Type<'db> {
        let ast::ExprDictComp {
            range: _,
            key: _,
            value: _,
            generators,
        } = dictcomp;

        self.infer_first_comprehension_iter(generators);

        // TODO dict type
        Type::Todo
    }

    fn infer_set_comprehension_expression(&mut self, setcomp: &ast::ExprSetComp) -> Type<'db> {
        let ast::ExprSetComp {
            range: _,
            elt: _,
            generators,
        } = setcomp;

        self.infer_first_comprehension_iter(generators);

        // TODO set type
        Type::Todo
    }

    fn infer_generator_expression_scope(&mut self, generator: &ast::ExprGenerator) {
        let ast::ExprGenerator {
            range: _,
            elt,
            generators,
            parenthesized: _,
        } = generator;

        self.infer_expression(elt);
        self.infer_comprehensions(generators);
    }

    fn infer_list_comprehension_expression_scope(&mut self, listcomp: &ast::ExprListComp) {
        let ast::ExprListComp {
            range: _,
            elt,
            generators,
        } = listcomp;

        self.infer_expression(elt);
        self.infer_comprehensions(generators);
    }

    fn infer_dict_comprehension_expression_scope(&mut self, dictcomp: &ast::ExprDictComp) {
        let ast::ExprDictComp {
            range: _,
            key,
            value,
            generators,
        } = dictcomp;

        self.infer_expression(key);
        self.infer_expression(value);
        self.infer_comprehensions(generators);
    }

    fn infer_set_comprehension_expression_scope(&mut self, setcomp: &ast::ExprSetComp) {
        let ast::ExprSetComp {
            range: _,
            elt,
            generators,
        } = setcomp;

        self.infer_expression(elt);
        self.infer_comprehensions(generators);
    }

    fn infer_comprehensions(&mut self, comprehensions: &[ast::Comprehension]) {
        let mut comprehensions_iter = comprehensions.iter();
        let Some(first_comprehension) = comprehensions_iter.next() else {
            unreachable!("Comprehension must contain at least one generator");
        };
        self.infer_comprehension(first_comprehension, true);
        for comprehension in comprehensions_iter {
            self.infer_comprehension(comprehension, false);
        }
    }

    fn infer_comprehension(&mut self, comprehension: &ast::Comprehension, is_first: bool) {
        let ast::Comprehension {
            range: _,
            target,
            iter,
            ifs,
            is_async: _,
        } = comprehension;

        if !is_first {
            self.infer_expression(iter);
        }
        // TODO more complex assignment targets
        if let ast::Expr::Name(name) = target {
            self.infer_definition(name);
        } else {
            self.infer_expression(target);
        }
        for expr in ifs {
            self.infer_expression(expr);
        }
    }

    fn infer_comprehension_definition(
        &mut self,
        iterable: &ast::Expr,
        target: &ast::ExprName,
        is_first: bool,
        is_async: bool,
        definition: Definition<'db>,
    ) {
        let expression = self.index.expression(iterable);
        let result = infer_expression_types(self.db, expression);

        // Two things are different if it's the first comprehension:
        // (1) We must lookup the `ScopedExpressionId` of the iterable expression in the outer scope,
        //     because that's the scope we visit it in in the semantic index builder
        // (2) We must *not* call `self.extend()` on the result of the type inference,
        //     because `ScopedExpressionId`s are only meaningful within their own scope, so
        //     we'd add types for random wrong expressions in the current scope
        let iterable_ty = if is_first {
            let lookup_scope = self
                .index
                .parent_scope_id(self.scope().file_scope_id(self.db))
                .expect("A comprehension should never be the top-level scope")
                .to_scope_id(self.db, self.file);
            result.expression_ty(iterable.scoped_ast_id(self.db, lookup_scope))
        } else {
            self.extend(result);
            result.expression_ty(iterable.scoped_ast_id(self.db, self.scope()))
        };

        let target_ty = if is_async {
            // TODO: async iterables/iterators! -- Alex
            Type::Todo
        } else {
            iterable_ty
                .iterate(self.db)
                .unwrap_with_diagnostic(iterable.into(), &mut self.diagnostics)
        };

        self.types
            .expressions
            .insert(target.scoped_ast_id(self.db, self.scope()), target_ty);
        self.add_binding(target.into(), definition, target_ty);
    }

    fn infer_named_expression(&mut self, named: &ast::ExprNamed) -> Type<'db> {
        let definition = self.index.definition(named);
        let result = infer_definition_types(self.db, definition);
        self.extend(result);
        result.binding_ty(definition)
    }

    fn infer_named_expression_definition(
        &mut self,
        named: &ast::ExprNamed,
        definition: Definition<'db>,
    ) -> Type<'db> {
        let ast::ExprNamed {
            range: _,
            target,
            value,
        } = named;

        let value_ty = self.infer_expression(value);
        self.infer_expression(target);

        self.add_binding(named.into(), definition, value_ty);

        value_ty
    }

    fn infer_if_expression(&mut self, if_expression: &ast::ExprIf) -> Type<'db> {
        let ast::ExprIf {
            range: _,
            test,
            body,
            orelse,
        } = if_expression;

        self.infer_expression(test);

        // TODO detect statically known truthy or falsy test
        let body_ty = self.infer_expression(body);
        let orelse_ty = self.infer_expression(orelse);

        UnionType::from_elements(self.db, [body_ty, orelse_ty])
    }

    fn infer_lambda_body(&mut self, lambda_expression: &ast::ExprLambda) {
        self.infer_expression(&lambda_expression.body);
    }

    fn infer_lambda_expression(&mut self, lambda_expression: &ast::ExprLambda) -> Type<'db> {
        let ast::ExprLambda {
            range: _,
            parameters,
            body: _,
        } = lambda_expression;

        if let Some(parameters) = parameters {
            for default in parameters
                .iter_non_variadic_params()
                .filter_map(|param| param.default.as_deref())
            {
                self.infer_expression(default);
            }

            self.infer_parameters(parameters);
        }

        // TODO function type
        Type::Todo
    }

    fn infer_call_expression(&mut self, call_expression: &ast::ExprCall) -> Type<'db> {
        let ast::ExprCall {
            range: _,
            func,
            arguments,
        } = call_expression;

        // TODO: proper typed call signature, representing keyword args etc
        let arg_types = self.infer_arguments(arguments);
        let function_type = self.infer_expression(func);
        function_type
            .call(self.db, arg_types.as_slice())
            .unwrap_with_diagnostic(self.db, func.as_ref().into(), &mut self.diagnostics)
    }

    fn infer_starred_expression(&mut self, starred: &ast::ExprStarred) -> Type<'db> {
        let ast::ExprStarred {
            range: _,
            value,
            ctx: _,
        } = starred;

        let iterable_ty = self.infer_expression(value);
        iterable_ty
            .iterate(self.db)
            .unwrap_with_diagnostic(value.as_ref().into(), &mut self.diagnostics);

        // TODO
        Type::Todo
    }

    fn infer_yield_expression(&mut self, yield_expression: &ast::ExprYield) -> Type<'db> {
        let ast::ExprYield { range: _, value } = yield_expression;

        self.infer_optional_expression(value.as_deref());

        // TODO awaitable type
        Type::Todo
    }

    fn infer_yield_from_expression(&mut self, yield_from: &ast::ExprYieldFrom) -> Type<'db> {
        let ast::ExprYieldFrom { range: _, value } = yield_from;

        let iterable_ty = self.infer_expression(value);
        iterable_ty
            .iterate(self.db)
            .unwrap_with_diagnostic(value.as_ref().into(), &mut self.diagnostics);

        // TODO get type from `ReturnType` of generator
        Type::Todo
    }

    fn infer_await_expression(&mut self, await_expression: &ast::ExprAwait) -> Type<'db> {
        let ast::ExprAwait { range: _, value } = await_expression;

        self.infer_expression(value);

        // TODO awaitable type
        Type::Todo
    }

    /// Look up a name reference that isn't bound in the local scope.
    fn lookup_name(&mut self, name_node: &ast::ExprName) -> Type<'db> {
        let ast::ExprName { id: name, .. } = name_node;
        let file_scope_id = self.scope().file_scope_id(self.db);
        let is_bound = self
            .index
            .symbol_table(file_scope_id)
            .symbol_by_name(name)
            .expect("Symbol table should create a symbol for every Name node")
            .is_bound();

        // In function-like scopes, any local variable (symbol that is bound in this scope) can
        // only have a definition in this scope, or error; it never references another scope.
        // (At runtime, it would use the `LOAD_FAST` opcode.)
        if !is_bound || !self.scope().is_function_like(self.db) {
            // Walk up parent scopes looking for a possible enclosing scope that may have a
            // definition of this name visible to us (would be `LOAD_DEREF` at runtime.)
            for (enclosing_scope_file_id, _) in self.index.ancestor_scopes(file_scope_id) {
                // Class scopes are not visible to nested scopes, and we need to handle global
                // scope differently (because an unbound name there falls back to builtins), so
                // check only function-like scopes.
                let enclosing_scope_id = enclosing_scope_file_id.to_scope_id(self.db, self.file);
                if !enclosing_scope_id.is_function_like(self.db) {
                    continue;
                }
                let enclosing_symbol_table = self.index.symbol_table(enclosing_scope_file_id);
                let Some(enclosing_symbol) = enclosing_symbol_table.symbol_by_name(name) else {
                    continue;
                };
                if enclosing_symbol.is_bound() {
                    // We can return early here, because the nearest function-like scope that
                    // defines a name must be the only source for the nonlocal reference (at
                    // runtime, it is the scope that creates the cell for our closure.) If the name
                    // isn't bound in that scope, we should get an unbound name, not continue
                    // falling back to other scopes / globals / builtins.
                    return symbol_ty(self.db, enclosing_scope_id, name);
                }
            }

            // No nonlocal binding, check module globals. Avoid infinite recursion if `self.scope`
            // already is module globals.
            let ty = if file_scope_id.is_global() {
                Type::Unbound
            } else {
                global_symbol_ty(self.db, self.file, name)
            };

            // Fallback to builtins (without infinite recursion if we're already in builtins.)
            if ty.may_be_unbound(self.db) && Some(self.scope()) != builtins_module_scope(self.db) {
                let mut builtin_ty = builtins_symbol_ty(self.db, name);
                if builtin_ty.is_unbound() && name == "reveal_type" {
                    self.diagnostics.add(
                        name_node.into(),
                        "undefined-reveal",
                        format_args!(
                            "`reveal_type` used without importing it; this is allowed for debugging convenience but will fail at runtime"),
                    );
                    builtin_ty = typing_extensions_symbol_ty(self.db, name);
                }
                ty.replace_unbound_with(self.db, builtin_ty)
            } else {
                ty
            }
        } else {
            Type::Unbound
        }
    }

    /// Infer the type of a [`ast::ExprName`] expression, assuming a load context.
    fn infer_name_load(&mut self, name: &ast::ExprName) -> Type<'db> {
        let ast::ExprName {
            range: _,
            id,
            ctx: _,
        } = name;

        let file_scope_id = self.scope().file_scope_id(self.db);
        let use_def = self.index.use_def_map(file_scope_id);
        let symbol = self
            .index
            .symbol_table(file_scope_id)
            .symbol_id_by_name(id)
            .expect("Expected the symbol table to create a symbol for every Name node");
        // if we're inferring types of deferred expressions, always treat them as public symbols
        let (definitions, may_be_unbound) = if self.is_deferred() {
            (
                use_def.public_bindings(symbol),
                use_def.public_may_be_unbound(symbol),
            )
        } else {
            let use_id = name.scoped_use_id(self.db, self.scope());
            (
                use_def.bindings_at_use(use_id),
                use_def.use_may_be_unbound(use_id),
            )
        };

        let unbound_ty = if may_be_unbound {
            Some(self.lookup_name(name))
        } else {
            None
        };

        let ty = bindings_ty(self.db, definitions, unbound_ty);
        if ty.is_unbound() {
            self.diagnostics.add(
                name.into(),
                "unresolved-reference",
                format_args!("Name `{id}` used when not defined"),
            );
        } else if ty.may_be_unbound(self.db) {
            self.diagnostics.add(
                name.into(),
                "possibly-unresolved-reference",
                format_args!("Name `{id}` used when possibly not defined"),
            );
        }

        ty
    }

    fn infer_name_expression(&mut self, name: &ast::ExprName) -> Type<'db> {
        match name.ctx {
            ExprContext::Load => self.infer_name_load(name),
            ExprContext::Store | ExprContext::Del => Type::Never,
            ExprContext::Invalid => Type::Unknown,
        }
    }

    /// Infer the type of a [`ast::ExprAttribute`] expression, assuming a load context.
    fn infer_attribute_load(&mut self, attribute: &ast::ExprAttribute) -> Type<'db> {
        let ast::ExprAttribute {
            value,
            attr,
            range: _,
            ctx: _,
        } = attribute;

        let value_ty = self.infer_expression(value);
        value_ty.member(self.db, &Name::new(&attr.id))
    }

    fn infer_attribute_expression(&mut self, attribute: &ast::ExprAttribute) -> Type<'db> {
        let ast::ExprAttribute {
            value,
            attr: _,
            range: _,
            ctx,
        } = attribute;

        match ctx {
            ExprContext::Load => self.infer_attribute_load(attribute),
            ExprContext::Store | ExprContext::Del => {
                self.infer_expression(value);
                Type::Never
            }
            ExprContext::Invalid => {
                self.infer_expression(value);
                Type::Unknown
            }
        }
    }

    fn infer_unary_expression(&mut self, unary: &ast::ExprUnaryOp) -> Type<'db> {
        let ast::ExprUnaryOp {
            range: _,
            op,
            operand,
        } = unary;

        let operand_type = self.infer_expression(operand);

        match (op, operand_type) {
            (UnaryOp::UAdd, Type::IntLiteral(value)) => Type::IntLiteral(value),
            (UnaryOp::USub, Type::IntLiteral(value)) => Type::IntLiteral(-value),
            (UnaryOp::Invert, Type::IntLiteral(value)) => Type::IntLiteral(!value),

            (UnaryOp::UAdd, Type::BooleanLiteral(bool)) => Type::IntLiteral(i64::from(bool)),
            (UnaryOp::USub, Type::BooleanLiteral(bool)) => Type::IntLiteral(-i64::from(bool)),
            (UnaryOp::Invert, Type::BooleanLiteral(bool)) => Type::IntLiteral(!i64::from(bool)),

            (UnaryOp::Not, ty) => ty.bool(self.db).negate().into_type(self.db),
            (_, Type::Any) => Type::Any,
            (_, Type::Unknown) => Type::Unknown,
            (op @ (UnaryOp::UAdd | UnaryOp::USub | UnaryOp::Invert), Type::Instance(class)) => {
                let unary_dunder_method = match op {
                    UnaryOp::Invert => "__invert__",
                    UnaryOp::UAdd => "__pos__",
                    UnaryOp::USub => "__neg__",
                    UnaryOp::Not => {
                        unreachable!("Not operator is handled in its own case");
                    }
                };
                let class_member = class.class_member(self.db, unary_dunder_method);
                let call = class_member.call(self.db, &[operand_type]);

                match call.return_ty_result(
                    self.db,
                    AnyNodeRef::ExprUnaryOp(unary),
                    &mut self.diagnostics,
                ) {
                    Ok(t) => t,
                    Err(e) => {
                        self.diagnostics.add(
                            unary.into(),
                            "unsupported-operator",
                            format_args!(
                                "Unary operator `{op}` is unsupported for type `{}`",
                                operand_type.display(self.db),
                            ),
                        );
                        e.return_ty()
                    }
                }
            }
            _ => Type::Todo, // TODO other unary op types
        }
    }

    fn infer_binary_expression(&mut self, binary: &ast::ExprBinOp) -> Type<'db> {
        let ast::ExprBinOp {
            left,
            op,
            right,
            range: _,
        } = binary;

        let left_ty = self.infer_expression(left);
        let right_ty = self.infer_expression(right);

        // Check for division by zero; this doesn't change the inferred type for the expression, but
        // may emit a diagnostic
        if matches!(
            (op, right_ty),
            (
                ast::Operator::Div | ast::Operator::FloorDiv | ast::Operator::Mod,
                Type::IntLiteral(0) | Type::BooleanLiteral(false)
            )
        ) {
            self.check_division_by_zero(binary, left_ty);
        }

        self.infer_binary_expression_type(left_ty, right_ty, *op)
            .unwrap_or_else(|| {
                self.diagnostics.add(
                    binary.into(),
                    "unsupported-operator",
                    format_args!(
                        "Operator `{op}` is unsupported between objects of type `{}` and `{}`",
                        left_ty.display(self.db),
                        right_ty.display(self.db)
                    ),
                );
                Type::Unknown
            })
    }

    fn infer_binary_expression_type(
        &mut self,
        left_ty: Type<'db>,
        right_ty: Type<'db>,
        op: ast::Operator,
    ) -> Option<Type<'db>> {
        match (left_ty, right_ty, op) {
            // When interacting with Todo, Any and Unknown should propagate (as if we fix this
            // `Todo` in the future, the result would then become Any or Unknown, respectively.)
            (Type::Any, _, _) | (_, Type::Any, _) => Some(Type::Any),
            (Type::Unknown, _, _) | (_, Type::Unknown, _) => Some(Type::Unknown),

            (Type::IntLiteral(n), Type::IntLiteral(m), ast::Operator::Add) => Some(
                n.checked_add(m)
                    .map(Type::IntLiteral)
                    .unwrap_or_else(|| KnownClass::Int.to_instance(self.db)),
            ),

            (Type::IntLiteral(n), Type::IntLiteral(m), ast::Operator::Sub) => Some(
                n.checked_sub(m)
                    .map(Type::IntLiteral)
                    .unwrap_or_else(|| KnownClass::Int.to_instance(self.db)),
            ),

            (Type::IntLiteral(n), Type::IntLiteral(m), ast::Operator::Mult) => Some(
                n.checked_mul(m)
                    .map(Type::IntLiteral)
                    .unwrap_or_else(|| KnownClass::Int.to_instance(self.db)),
            ),

            (Type::IntLiteral(_), Type::IntLiteral(_), ast::Operator::Div) => {
                Some(KnownClass::Float.to_instance(self.db))
            }

            (Type::IntLiteral(n), Type::IntLiteral(m), ast::Operator::FloorDiv) => Some(
                n.checked_div(m)
                    .map(Type::IntLiteral)
                    .unwrap_or_else(|| KnownClass::Int.to_instance(self.db)),
            ),

            (Type::IntLiteral(n), Type::IntLiteral(m), ast::Operator::Mod) => Some(
                n.checked_rem(m)
                    .map(Type::IntLiteral)
                    .unwrap_or_else(|| KnownClass::Int.to_instance(self.db)),
            ),

            (Type::IntLiteral(n), Type::IntLiteral(m), ast::Operator::Pow) => {
                let m = u32::try_from(m);
                Some(match m {
                    Ok(m) => n
                        .checked_pow(m)
                        .map(Type::IntLiteral)
                        .unwrap_or_else(|| KnownClass::Int.to_instance(self.db)),
                    Err(_) => KnownClass::Int.to_instance(self.db),
                })
            }

            (Type::BytesLiteral(lhs), Type::BytesLiteral(rhs), ast::Operator::Add) => {
                Some(Type::BytesLiteral(BytesLiteralType::new(
                    self.db,
                    [lhs.value(self.db).as_ref(), rhs.value(self.db).as_ref()]
                        .concat()
                        .into_boxed_slice(),
                )))
            }

            (Type::StringLiteral(lhs), Type::StringLiteral(rhs), ast::Operator::Add) => {
                let lhs_value = lhs.value(self.db).to_string();
                let rhs_value = rhs.value(self.db).as_ref();
                let ty = if lhs_value.len() + rhs_value.len() <= Self::MAX_STRING_LITERAL_SIZE {
                    Type::StringLiteral(StringLiteralType::new(self.db, {
                        (lhs_value + rhs_value).into_boxed_str()
                    }))
                } else {
                    Type::LiteralString
                };
                Some(ty)
            }

            (
                Type::StringLiteral(_) | Type::LiteralString,
                Type::StringLiteral(_) | Type::LiteralString,
                ast::Operator::Add,
            ) => Some(Type::LiteralString),

            (Type::StringLiteral(s), Type::IntLiteral(n), ast::Operator::Mult)
            | (Type::IntLiteral(n), Type::StringLiteral(s), ast::Operator::Mult) => {
                let ty = if n < 1 {
                    Type::StringLiteral(StringLiteralType::new(self.db, ""))
                } else if let Ok(n) = usize::try_from(n) {
                    if n.checked_mul(s.value(self.db).len())
                        .is_some_and(|new_length| new_length <= Self::MAX_STRING_LITERAL_SIZE)
                    {
                        let new_literal = s.value(self.db).repeat(n);
                        Type::StringLiteral(StringLiteralType::new(
                            self.db,
                            new_literal.into_boxed_str(),
                        ))
                    } else {
                        Type::LiteralString
                    }
                } else {
                    Type::LiteralString
                };
                Some(ty)
            }

            (Type::LiteralString, Type::IntLiteral(n), ast::Operator::Mult)
            | (Type::IntLiteral(n), Type::LiteralString, ast::Operator::Mult) => {
                let ty = if n < 1 {
                    Type::StringLiteral(StringLiteralType::new(self.db, ""))
                } else {
                    Type::LiteralString
                };
                Some(ty)
            }

            (Type::Instance(_), Type::IntLiteral(_), op) => {
                self.infer_binary_expression_type(left_ty, KnownClass::Int.to_instance(self.db), op)
            }

            (Type::IntLiteral(_), Type::Instance(_), op) => self.infer_binary_expression_type(
                KnownClass::Int.to_instance(self.db),
                right_ty,
                op,
            ),

            (Type::Instance(_), Type::Tuple(_), op) => self.infer_binary_expression_type(
                left_ty,
                KnownClass::Tuple.to_instance(self.db),
                op,
            ),

            (Type::Tuple(_), Type::Instance(_), op) => self.infer_binary_expression_type(
                KnownClass::Tuple.to_instance(self.db),
                right_ty,
                op,
            ),

            (Type::Instance(_), Type::StringLiteral(_) | Type::LiteralString, op) => {
                self.infer_binary_expression_type(left_ty, KnownClass::Str.to_instance(self.db), op)
            }

            (Type::StringLiteral(_) | Type::LiteralString, Type::Instance(_), op) => self
                .infer_binary_expression_type(KnownClass::Str.to_instance(self.db), right_ty, op),

            (Type::Instance(_), Type::BytesLiteral(_), op) => self.infer_binary_expression_type(
                left_ty,
                KnownClass::Bytes.to_instance(self.db),
                op,
            ),

            (Type::BytesLiteral(_), Type::Instance(_), op) => self.infer_binary_expression_type(
                KnownClass::Bytes.to_instance(self.db),
                right_ty,
                op,
            ),

            (Type::Instance(left_class), Type::Instance(right_class), op) => {
                if left_class != right_class && right_class.is_subclass_of(self.db, left_class) {
                    let reflected_dunder = op.reflected_dunder();
                    let rhs_reflected = right_class.class_member(self.db, reflected_dunder);
                    if !rhs_reflected.is_unbound()
                        && rhs_reflected != left_class.class_member(self.db, reflected_dunder)
                    {
                        return rhs_reflected
                            .call(self.db, &[right_ty, left_ty])
                            .return_ty(self.db)
                            .or_else(|| {
                                left_class
                                    .class_member(self.db, op.dunder())
                                    .call(self.db, &[left_ty, right_ty])
                                    .return_ty(self.db)
                            });
                    }
                }
                left_class
                    .class_member(self.db, op.dunder())
                    .call(self.db, &[left_ty, right_ty])
                    .return_ty(self.db)
                    .or_else(|| {
                        if left_class == right_class {
                            None
                        } else {
                            right_class
                                .class_member(self.db, op.reflected_dunder())
                                .call(self.db, &[right_ty, left_ty])
                                .return_ty(self.db)
                        }
                    })
            }

            (
                Type::BooleanLiteral(b1),
                Type::BooleanLiteral(b2),
                ruff_python_ast::Operator::BitOr,
            ) => Some(Type::BooleanLiteral(b1 | b2)),

            (Type::BooleanLiteral(bool_value), right, op) => self.infer_binary_expression_type(
                Type::IntLiteral(i64::from(bool_value)),
                right,
                op,
            ),
            (left, Type::BooleanLiteral(bool_value), op) => {
                self.infer_binary_expression_type(left, Type::IntLiteral(i64::from(bool_value)), op)
            }
            _ => Some(Type::Todo), // TODO
        }
    }

    fn infer_boolean_expression(&mut self, bool_op: &ast::ExprBoolOp) -> Type<'db> {
        let ast::ExprBoolOp {
            range: _,
            op,
            values,
        } = bool_op;
        Self::infer_chained_boolean_types(
            self.db,
            *op,
            values.iter().map(|value| self.infer_expression(value)),
            values.len(),
        )
    }

    /// Computes the output of a chain of (one) boolean operation, consuming as input an iterator
    /// of types. The iterator is consumed even if the boolean evaluation can be short-circuited,
    /// in order to ensure the invariant that all expressions are evaluated when inferring types.
    fn infer_chained_boolean_types(
        db: &'db dyn Db,
        op: ast::BoolOp,
        values: impl IntoIterator<Item = Type<'db>>,
        n_values: usize,
    ) -> Type<'db> {
        let mut done = false;
        UnionType::from_elements(
            db,
            values.into_iter().enumerate().map(|(i, ty)| {
                if done {
                    Type::Never
                } else {
                    let is_last = i == n_values - 1;
                    match (ty.bool(db), is_last, op) {
                        (Truthiness::Ambiguous, _, _) => ty,
                        (Truthiness::AlwaysTrue, false, ast::BoolOp::And) => Type::Never,
                        (Truthiness::AlwaysFalse, false, ast::BoolOp::Or) => Type::Never,
                        (Truthiness::AlwaysFalse, _, ast::BoolOp::And)
                        | (Truthiness::AlwaysTrue, _, ast::BoolOp::Or) => {
                            done = true;
                            ty
                        }
                        (_, true, _) => ty,
                    }
                }
            }),
        )
    }

    fn infer_compare_expression(&mut self, compare: &ast::ExprCompare) -> Type<'db> {
        let ast::ExprCompare {
            range: _,
            left,
            ops,
            comparators,
        } = compare;

        self.infer_expression(left);
        for right in comparators {
            self.infer_expression(right);
        }

        // https://docs.python.org/3/reference/expressions.html#comparisons
        // > Formally, if `a, b, c, …, y, z` are expressions and `op1, op2, …, opN` are comparison
        // > operators, then `a op1 b op2 c ... y opN z` is equivalent to a `op1 b and b op2 c and
        // ... > y opN z`, except that each expression is evaluated at most once.
        //
        // As some operators (==, !=, <, <=, >, >=) *can* return an arbitrary type, the logic below
        // is shared with the one in `infer_binary_type_comparison`.
        Self::infer_chained_boolean_types(
            self.db,
            ast::BoolOp::And,
            std::iter::once(&**left)
                .chain(comparators)
                .tuple_windows::<(_, _)>()
                .zip(ops)
                .map(|((left, right), op)| {
                    let left_ty = self.expression_ty(left);
                    let right_ty = self.expression_ty(right);

                    self.infer_binary_type_comparison(left_ty, *op, right_ty)
                        .unwrap_or_else(|error| {
                            // Handle unsupported operators (diagnostic, `bool`/`Unknown` outcome)
                            self.diagnostics.add(
                                AnyNodeRef::ExprCompare(compare),
                                "unsupported-operator",
                                format_args!(
                                    "Operator `{}` is not supported for types `{}` and `{}`{}",
                                    error.op,
                                    error.left_ty.display(self.db),
                                    error.right_ty.display(self.db),
                                    if (left_ty, right_ty) == (error.left_ty, error.right_ty) {
                                        String::new()
                                    } else {
                                        format!(
                                            ", in comparing `{}` with `{}`",
                                            left_ty.display(self.db),
                                            right_ty.display(self.db)
                                        )
                                    }
                                ),
                            );

                            match op {
                                // `in, not in, is, is not` always return bool instances
                                ast::CmpOp::In
                                | ast::CmpOp::NotIn
                                | ast::CmpOp::Is
                                | ast::CmpOp::IsNot => KnownClass::Bool.to_instance(self.db),
                                // Other operators can return arbitrary types
                                _ => Type::Unknown,
                            }
                        })
                }),
            ops.len(),
        )
    }

    /// Infers the type of a binary comparison (e.g. 'left == right'). See
    /// `infer_compare_expression` for the higher level logic dealing with multi-comparison
    /// expressions.
    ///
    /// If the operation is not supported, return None (we need upstream context to emit a
    /// diagnostic).
    fn infer_binary_type_comparison(
        &mut self,
        left: Type<'db>,
        op: ast::CmpOp,
        right: Type<'db>,
    ) -> Result<Type<'db>, CompareUnsupportedError<'db>> {
        // Note: identity (is, is not) for equal builtin types is unreliable and not part of the
        // language spec.
        // - `[ast::CompOp::Is]`: return `false` if unequal, `bool` if equal
        // - `[ast::CompOp::IsNot]`: return `true` if unequal, `bool` if equal
        match (left, right) {
            (Type::Union(union), other) => {
                let mut builder = UnionBuilder::new(self.db);
                for element in union.elements(self.db) {
                    builder = builder.add(self.infer_binary_type_comparison(*element, op, other)?);
                }
                Ok(builder.build())
            }
            (other, Type::Union(union)) => {
                let mut builder = UnionBuilder::new(self.db);
                for element in union.elements(self.db) {
                    builder = builder.add(self.infer_binary_type_comparison(other, op, *element)?);
                }
                Ok(builder.build())
            }

            (Type::IntLiteral(n), Type::IntLiteral(m)) => match op {
                ast::CmpOp::Eq => Ok(Type::BooleanLiteral(n == m)),
                ast::CmpOp::NotEq => Ok(Type::BooleanLiteral(n != m)),
                ast::CmpOp::Lt => Ok(Type::BooleanLiteral(n < m)),
                ast::CmpOp::LtE => Ok(Type::BooleanLiteral(n <= m)),
                ast::CmpOp::Gt => Ok(Type::BooleanLiteral(n > m)),
                ast::CmpOp::GtE => Ok(Type::BooleanLiteral(n >= m)),
                ast::CmpOp::Is => {
                    if n == m {
                        Ok(KnownClass::Bool.to_instance(self.db))
                    } else {
                        Ok(Type::BooleanLiteral(false))
                    }
                }
                ast::CmpOp::IsNot => {
                    if n == m {
                        Ok(KnownClass::Bool.to_instance(self.db))
                    } else {
                        Ok(Type::BooleanLiteral(true))
                    }
                }
                // Undefined for (int, int)
                ast::CmpOp::In | ast::CmpOp::NotIn => Err(CompareUnsupportedError {
                    op,
                    left_ty: left,
                    right_ty: right,
                }),
            },
            (Type::IntLiteral(_), Type::Instance(_)) => {
                self.infer_binary_type_comparison(KnownClass::Int.to_instance(self.db), op, right)
            }
            (Type::Instance(_), Type::IntLiteral(_)) => {
                self.infer_binary_type_comparison(left, op, KnownClass::Int.to_instance(self.db))
            }

            // Booleans are coded as integers (False = 0, True = 1)
            (Type::IntLiteral(n), Type::BooleanLiteral(b)) => self.infer_binary_type_comparison(
                Type::IntLiteral(n),
                op,
                Type::IntLiteral(i64::from(b)),
            ),
            (Type::BooleanLiteral(b), Type::IntLiteral(m)) => self.infer_binary_type_comparison(
                Type::IntLiteral(i64::from(b)),
                op,
                Type::IntLiteral(m),
            ),
            (Type::BooleanLiteral(a), Type::BooleanLiteral(b)) => self
                .infer_binary_type_comparison(
                    Type::IntLiteral(i64::from(a)),
                    op,
                    Type::IntLiteral(i64::from(b)),
                ),

            (Type::StringLiteral(salsa_s1), Type::StringLiteral(salsa_s2)) => {
                let s1 = salsa_s1.value(self.db);
                let s2 = salsa_s2.value(self.db);
                match op {
                    ast::CmpOp::Eq => Ok(Type::BooleanLiteral(s1 == s2)),
                    ast::CmpOp::NotEq => Ok(Type::BooleanLiteral(s1 != s2)),
                    ast::CmpOp::Lt => Ok(Type::BooleanLiteral(s1 < s2)),
                    ast::CmpOp::LtE => Ok(Type::BooleanLiteral(s1 <= s2)),
                    ast::CmpOp::Gt => Ok(Type::BooleanLiteral(s1 > s2)),
                    ast::CmpOp::GtE => Ok(Type::BooleanLiteral(s1 >= s2)),
                    ast::CmpOp::In => Ok(Type::BooleanLiteral(s2.contains(s1.as_ref()))),
                    ast::CmpOp::NotIn => Ok(Type::BooleanLiteral(!s2.contains(s1.as_ref()))),
                    ast::CmpOp::Is => {
                        if s1 == s2 {
                            Ok(KnownClass::Bool.to_instance(self.db))
                        } else {
                            Ok(Type::BooleanLiteral(false))
                        }
                    }
                    ast::CmpOp::IsNot => {
                        if s1 == s2 {
                            Ok(KnownClass::Bool.to_instance(self.db))
                        } else {
                            Ok(Type::BooleanLiteral(true))
                        }
                    }
                }
            }
            (Type::StringLiteral(_), _) => {
                self.infer_binary_type_comparison(KnownClass::Str.to_instance(self.db), op, right)
            }
            (_, Type::StringLiteral(_)) => {
                self.infer_binary_type_comparison(left, op, KnownClass::Str.to_instance(self.db))
            }

            (Type::LiteralString, _) => {
                self.infer_binary_type_comparison(KnownClass::Str.to_instance(self.db), op, right)
            }
            (_, Type::LiteralString) => {
                self.infer_binary_type_comparison(left, op, KnownClass::Str.to_instance(self.db))
            }

            (Type::BytesLiteral(salsa_b1), Type::BytesLiteral(salsa_b2)) => {
                let b1 = &**salsa_b1.value(self.db);
                let b2 = &**salsa_b2.value(self.db);
                match op {
                    ast::CmpOp::Eq => Ok(Type::BooleanLiteral(b1 == b2)),
                    ast::CmpOp::NotEq => Ok(Type::BooleanLiteral(b1 != b2)),
                    ast::CmpOp::Lt => Ok(Type::BooleanLiteral(b1 < b2)),
                    ast::CmpOp::LtE => Ok(Type::BooleanLiteral(b1 <= b2)),
                    ast::CmpOp::Gt => Ok(Type::BooleanLiteral(b1 > b2)),
                    ast::CmpOp::GtE => Ok(Type::BooleanLiteral(b1 >= b2)),
                    ast::CmpOp::In => {
                        Ok(Type::BooleanLiteral(memchr::memmem::find(b2, b1).is_some()))
                    }
                    ast::CmpOp::NotIn => {
                        Ok(Type::BooleanLiteral(memchr::memmem::find(b2, b1).is_none()))
                    }
                    ast::CmpOp::Is => {
                        if b1 == b2 {
                            Ok(KnownClass::Bool.to_instance(self.db))
                        } else {
                            Ok(Type::BooleanLiteral(false))
                        }
                    }
                    ast::CmpOp::IsNot => {
                        if b1 == b2 {
                            Ok(KnownClass::Bool.to_instance(self.db))
                        } else {
                            Ok(Type::BooleanLiteral(true))
                        }
                    }
                }
            }
            (Type::BytesLiteral(_), _) => {
                self.infer_binary_type_comparison(KnownClass::Bytes.to_instance(self.db), op, right)
            }
            (_, Type::BytesLiteral(_)) => {
                self.infer_binary_type_comparison(left, op, KnownClass::Bytes.to_instance(self.db))
            }
            (Type::Tuple(lhs), Type::Tuple(rhs)) => {
                // Note: This only works on heterogeneous tuple types.
                let lhs_elements = lhs.elements(self.db);
                let rhs_elements = rhs.elements(self.db);

                let mut lexicographic_type_comparison =
                    |op| self.infer_lexicographic_type_comparison(lhs_elements, op, rhs_elements);

                match op {
                    ast::CmpOp::Eq => lexicographic_type_comparison(RichCompareOperator::Eq),
                    ast::CmpOp::NotEq => lexicographic_type_comparison(RichCompareOperator::Ne),
                    ast::CmpOp::Lt => lexicographic_type_comparison(RichCompareOperator::Lt),
                    ast::CmpOp::LtE => lexicographic_type_comparison(RichCompareOperator::Le),
                    ast::CmpOp::Gt => lexicographic_type_comparison(RichCompareOperator::Gt),
                    ast::CmpOp::GtE => lexicographic_type_comparison(RichCompareOperator::Ge),
                    ast::CmpOp::In | ast::CmpOp::NotIn => {
                        let mut eq_count = 0usize;
                        let mut not_eq_count = 0usize;

                        for ty in rhs_elements {
                            let eq_result = self.infer_binary_type_comparison(
                                Type::Tuple(lhs),
                                ast::CmpOp::Eq,
                                *ty,
                            ).expect("infer_binary_type_comparison should never return None for `CmpOp::Eq`");

                            match eq_result {
                                Type::Todo => return Ok(Type::Todo),
                                ty => match ty.bool(self.db) {
                                    Truthiness::AlwaysTrue => eq_count += 1,
                                    Truthiness::AlwaysFalse => not_eq_count += 1,
                                    Truthiness::Ambiguous => (),
                                },
                            }
                        }

                        if eq_count >= 1 {
                            Ok(Type::BooleanLiteral(op.is_in()))
                        } else if not_eq_count == rhs_elements.len() {
                            Ok(Type::BooleanLiteral(op.is_not_in()))
                        } else {
                            Ok(KnownClass::Bool.to_instance(self.db))
                        }
                    }
                    ast::CmpOp::Is | ast::CmpOp::IsNot => {
                        // - `[ast::CmpOp::Is]`: returns `false` if the elements are definitely unequal, otherwise `bool`
                        // - `[ast::CmpOp::IsNot]`: returns `true` if the elements are definitely unequal, otherwise `bool`
                        let eq_result = lexicographic_type_comparison(RichCompareOperator::Eq)
                            .expect(
                            "infer_binary_type_comparison should never return None for `CmpOp::Eq`",
                        );

                        Ok(match eq_result {
                            Type::Todo => Type::Todo,
                            ty => match ty.bool(self.db) {
                                Truthiness::AlwaysFalse => Type::BooleanLiteral(op.is_is_not()),
                                _ => KnownClass::Bool.to_instance(self.db),
                            },
                        })
                    }
                }
            }

            // Lookup the rich comparison `__dunder__` methods on instances
            (Type::Instance(left_class), Type::Instance(right_class)) => {
                let rich_comparison =
                    |op| perform_rich_comparison(self.db, left_class, right_class, op);
                let membership_test_comparison =
                    |op| perform_membership_test_comparison(self.db, left_class, right_class, op);
                match op {
                    ast::CmpOp::Eq => rich_comparison(RichCompareOperator::Eq),
                    ast::CmpOp::NotEq => rich_comparison(RichCompareOperator::Ne),
                    ast::CmpOp::Lt => rich_comparison(RichCompareOperator::Lt),
                    ast::CmpOp::LtE => rich_comparison(RichCompareOperator::Le),
                    ast::CmpOp::Gt => rich_comparison(RichCompareOperator::Gt),
                    ast::CmpOp::GtE => rich_comparison(RichCompareOperator::Ge),
                    ast::CmpOp::In => membership_test_comparison(MembershipTestCompareOperator::In),
                    ast::CmpOp::NotIn => {
                        membership_test_comparison(MembershipTestCompareOperator::NotIn)
                    }
                    ast::CmpOp::Is => Ok(KnownClass::Bool.to_instance(self.db)),
                    ast::CmpOp::IsNot => Ok(KnownClass::Bool.to_instance(self.db)),
                }
            }
            // TODO: handle more types
            _ => match op {
                ast::CmpOp::Is | ast::CmpOp::IsNot => Ok(KnownClass::Bool.to_instance(self.db)),
                _ => Ok(Type::Todo),
            },
        }
    }

    /// Performs lexicographic comparison between two slices of types.
    ///
    /// For lexicographic comparison, elements from both slices are compared pairwise using
    /// `infer_binary_type_comparison`. If a conclusive result cannot be determined as a `BooleanLiteral`,
    /// it returns `bool`. Returns `None` if the comparison is not supported.
    fn infer_lexicographic_type_comparison(
        &mut self,
        left: &[Type<'db>],
        op: RichCompareOperator,
        right: &[Type<'db>],
    ) -> Result<Type<'db>, CompareUnsupportedError<'db>> {
        // Compare paired elements from left and right slices
        for (l_ty, r_ty) in left.iter().copied().zip(right.iter().copied()) {
            let eq_result = self
                .infer_binary_type_comparison(l_ty, ast::CmpOp::Eq, r_ty)
                .expect("infer_binary_type_comparison should never return None for `CmpOp::Eq`");

            match eq_result {
                // If propagation is required, return the result as is
                Type::Todo => return Ok(Type::Todo),
                ty => match ty.bool(self.db) {
                    // Types are equal, continue to the next pair
                    Truthiness::AlwaysTrue => continue,
                    // Types are not equal, perform the specified comparison and return the result
                    Truthiness::AlwaysFalse => {
                        return self.infer_binary_type_comparison(l_ty, op.into(), r_ty)
                    }
                    // If the intermediate result is ambiguous, we cannot determine the final result as BooleanLiteral.
                    // In this case, we simply return a bool instance.
                    Truthiness::Ambiguous => return Ok(KnownClass::Bool.to_instance(self.db)),
                },
            }
        }

        // At this point, the lengths of the two slices may be different, but the prefix of
        // left and right slices is entirely identical.
        // We return a comparison of the slice lengths based on the operator.
        let (left_len, right_len) = (left.len(), right.len());

        Ok(Type::BooleanLiteral(match op {
            RichCompareOperator::Eq => left_len == right_len,
            RichCompareOperator::Ne => left_len != right_len,
            RichCompareOperator::Lt => left_len < right_len,
            RichCompareOperator::Le => left_len <= right_len,
            RichCompareOperator::Gt => left_len > right_len,
            RichCompareOperator::Ge => left_len >= right_len,
        }))
    }

    fn infer_subscript_expression(&mut self, subscript: &ast::ExprSubscript) -> Type<'db> {
        let ast::ExprSubscript {
            range: _,
            value,
            slice,
            ctx: _,
        } = subscript;

        let value_ty = self.infer_expression(value);
        let slice_ty = self.infer_expression(slice);
        self.infer_subscript_expression_types(value, value_ty, slice_ty)
    }

    fn infer_subscript_expression_types(
        &mut self,
        value_node: &ast::Expr,
        value_ty: Type<'db>,
        slice_ty: Type<'db>,
    ) -> Type<'db> {
        match (value_ty, slice_ty) {
            // Ex) Given `("a", "b", "c", "d")[1]`, return `"b"`
            (Type::Tuple(tuple_ty), Type::IntLiteral(int)) if i32::try_from(int).is_ok() => {
                let elements = tuple_ty.elements(self.db);
                elements
                    .iter()
                    .py_index(i32::try_from(int).expect("checked in branch arm"))
                    .copied()
                    .unwrap_or_else(|_| {
                        self.diagnostics.add_index_out_of_bounds(
                            "tuple",
                            value_node.into(),
                            value_ty,
                            elements.len(),
                            int,
                        );
                        Type::Unknown
                    })
            }
            // Ex) Given `("a", 1, Null)[0:2]`, return `("a", 1)`
            (Type::Tuple(tuple_ty), Type::SliceLiteral(slice_ty)) => {
                let elements = tuple_ty.elements(self.db);
                let (start, stop, step) = slice_ty.as_tuple(self.db);

                if let Ok(new_elements) = elements.py_slice(start, stop, step) {
                    let new_elements: Vec<_> = new_elements.copied().collect();
                    Type::Tuple(TupleType::new(self.db, new_elements.into_boxed_slice()))
                } else {
                    self.diagnostics.add_slice_step_size_zero(value_node.into());
                    Type::Unknown
                }
            }
            // Ex) Given `"value"[1]`, return `"a"`
            (Type::StringLiteral(literal_ty), Type::IntLiteral(int))
                if i32::try_from(int).is_ok() =>
            {
                let literal_value = literal_ty.value(self.db);
                literal_value
                    .chars()
                    .py_index(i32::try_from(int).expect("checked in branch arm"))
                    .map(|ch| {
                        Type::StringLiteral(StringLiteralType::new(
                            self.db,
                            ch.to_string().into_boxed_str(),
                        ))
                    })
                    .unwrap_or_else(|_| {
                        self.diagnostics.add_index_out_of_bounds(
                            "string",
                            value_node.into(),
                            value_ty,
                            literal_value.chars().count(),
                            int,
                        );
                        Type::Unknown
                    })
            }
            // Ex) Given `"value"[1:3]`, return `"al"`
            (Type::StringLiteral(literal_ty), Type::SliceLiteral(slice_ty)) => {
                let literal_value = literal_ty.value(self.db);
                let (start, stop, step) = slice_ty.as_tuple(self.db);

                let chars: Vec<_> = literal_value.chars().collect();
                let result = if let Ok(new_chars) = chars.py_slice(start, stop, step) {
                    let literal: String = new_chars.collect();
                    Type::StringLiteral(StringLiteralType::new(self.db, literal.into_boxed_str()))
                } else {
                    self.diagnostics.add_slice_step_size_zero(value_node.into());
                    Type::Unknown
                };
                result
            }
            // Ex) Given `b"value"[1]`, return `b"a"`
            (Type::BytesLiteral(literal_ty), Type::IntLiteral(int))
                if i32::try_from(int).is_ok() =>
            {
                let literal_value = literal_ty.value(self.db);
                literal_value
                    .iter()
                    .py_index(i32::try_from(int).expect("checked in branch arm"))
                    .map(|byte| {
                        Type::BytesLiteral(BytesLiteralType::new(self.db, [*byte].as_slice()))
                    })
                    .unwrap_or_else(|_| {
                        self.diagnostics.add_index_out_of_bounds(
                            "bytes literal",
                            value_node.into(),
                            value_ty,
                            literal_value.len(),
                            int,
                        );
                        Type::Unknown
                    })
            }
            // Ex) Given `b"value"[1:3]`, return `b"al"`
            (Type::BytesLiteral(literal_ty), Type::SliceLiteral(slice_ty)) => {
                let literal_value = literal_ty.value(self.db);
                let (start, stop, step) = slice_ty.as_tuple(self.db);

                if let Ok(new_bytes) = literal_value.py_slice(start, stop, step) {
                    let new_bytes: Vec<u8> = new_bytes.copied().collect();
                    Type::BytesLiteral(BytesLiteralType::new(self.db, new_bytes.into_boxed_slice()))
                } else {
                    self.diagnostics.add_slice_step_size_zero(value_node.into());
                    Type::Unknown
                }
            }
            // Ex) Given `"value"[True]`, return `"a"`
            (
                Type::Tuple(_) | Type::StringLiteral(_) | Type::BytesLiteral(_),
                Type::BooleanLiteral(bool),
            ) => self.infer_subscript_expression_types(
                value_node,
                value_ty,
                Type::IntLiteral(i64::from(bool)),
            ),
            (value_ty, slice_ty) => {
                // Resolve the value to its class.
                let value_meta_ty = value_ty.to_meta_type(self.db);

                // If the class defines `__getitem__`, return its return type.
                //
                // See: https://docs.python.org/3/reference/datamodel.html#class-getitem-versus-getitem
                let dunder_getitem_method = value_meta_ty.member(self.db, "__getitem__");
                if !dunder_getitem_method.is_unbound() {
                    return dunder_getitem_method
                        .call(self.db, &[slice_ty])
                        .return_ty_result(self.db, value_node.into(), &mut self.diagnostics)
                        .unwrap_or_else(|err| {
                            self.diagnostics.add(
                                value_node.into(),
                                "call-non-callable",
                                format_args!(
                                    "Method `__getitem__` of type `{}` is not callable on object of type `{}`",
                                    err.called_ty().display(self.db),
                                    value_ty.display(self.db),
                                ),
                            );
                            err.return_ty()
                        });
                }

                // Otherwise, if the value is itself a class and defines `__class_getitem__`,
                // return its return type.
                //
                // TODO: lots of classes are only subscriptable at runtime on Python 3.9+,
                // *but* we should also allow them to be subscripted in stubs
                // (and in annotations if `from __future__ import annotations` is enabled),
                // even if the target version is Python 3.8 or lower,
                // despite the fact that there will be no corresponding `__class_getitem__`
                // method in these `sys.version_info` branches.
                if value_ty.is_subtype_of(self.db, KnownClass::Type.to_instance(self.db)) {
                    let dunder_class_getitem_method = value_ty.member(self.db, "__class_getitem__");
                    if !dunder_class_getitem_method.is_unbound() {
                        return dunder_class_getitem_method
                            .call(self.db, &[slice_ty])
                            .return_ty_result(self.db, value_node.into(), &mut self.diagnostics)
                            .unwrap_or_else(|err| {
                                self.diagnostics.add(
                                    value_node.into(),
                                    "call-non-callable",
                                    format_args!(
                                        "Method `__class_getitem__` of type `{}` is not callable on object of type `{}`",
                                        err.called_ty().display(self.db),
                                        value_ty.display(self.db),
                                    ),
                                );
                                err.return_ty()
                            });
                    }

                    if matches!(value_ty, Type::ClassLiteral(class) if class.is_known(self.db, KnownClass::Type))
                    {
                        return KnownClass::GenericAlias.to_instance(self.db);
                    }

                    self.diagnostics.add_non_subscriptable(
                        value_node.into(),
                        value_ty,
                        "__class_getitem__",
                    );
                } else {
                    self.diagnostics.add_non_subscriptable(
                        value_node.into(),
                        value_ty,
                        "__getitem__",
                    );
                }

                Type::Unknown
            }
        }
    }

    fn infer_slice_expression(&mut self, slice: &ast::ExprSlice) -> Type<'db> {
        enum SliceArg {
            Arg(Option<i32>),
            Unsupported,
        }

        let ast::ExprSlice {
            range: _,
            lower,
            upper,
            step,
        } = slice;

        let ty_lower = self.infer_optional_expression(lower.as_deref());
        let ty_upper = self.infer_optional_expression(upper.as_deref());
        let ty_step = self.infer_optional_expression(step.as_deref());

        let type_to_slice_argument = |ty: Option<Type<'db>>| match ty {
            Some(Type::IntLiteral(n)) => match i32::try_from(n) {
                Ok(n) => SliceArg::Arg(Some(n)),
                Err(_) => SliceArg::Unsupported,
            },
            Some(Type::BooleanLiteral(b)) => SliceArg::Arg(Some(i32::from(b))),
            Some(Type::None) => SliceArg::Arg(None),
            Some(Type::Instance(class)) if class.is_known(self.db, KnownClass::NoneType) => {
                SliceArg::Arg(None)
            }
            None => SliceArg::Arg(None),
            _ => SliceArg::Unsupported,
        };

        match (
            type_to_slice_argument(ty_lower),
            type_to_slice_argument(ty_upper),
            type_to_slice_argument(ty_step),
        ) {
            (SliceArg::Arg(lower), SliceArg::Arg(upper), SliceArg::Arg(step)) => {
                Type::SliceLiteral(SliceLiteralType::new(self.db, lower, upper, step))
            }
            _ => KnownClass::Slice.to_instance(self.db),
        }
    }

    fn infer_type_parameters(&mut self, type_parameters: &ast::TypeParams) {
        let ast::TypeParams {
            range: _,
            type_params,
        } = type_parameters;
        for type_param in type_params {
            match type_param {
                ast::TypeParam::TypeVar(typevar) => {
                    let ast::TypeParamTypeVar {
                        range: _,
                        name: _,
                        bound,
                        default,
                    } = typevar;
                    self.infer_optional_expression(bound.as_deref());
                    self.infer_optional_expression(default.as_deref());
                }
                ast::TypeParam::ParamSpec(param_spec) => {
                    let ast::TypeParamParamSpec {
                        range: _,
                        name: _,
                        default,
                    } = param_spec;
                    self.infer_optional_expression(default.as_deref());
                }
                ast::TypeParam::TypeVarTuple(typevar_tuple) => {
                    let ast::TypeParamTypeVarTuple {
                        range: _,
                        name: _,
                        default,
                    } = typevar_tuple;
                    self.infer_optional_expression(default.as_deref());
                }
            }
        }
    }

    pub(super) fn finish(mut self) -> TypeInference<'db> {
        self.infer_region();
        self.types.diagnostics = self.diagnostics.finish();
        self.types.shrink_to_fit();
        self.types
    }
}

/// Annotation expressions.
impl<'db> TypeInferenceBuilder<'db> {
    fn infer_annotation_expression(&mut self, expression: &ast::Expr) -> Type<'db> {
        // https://typing.readthedocs.io/en/latest/spec/annotations.html#grammar-token-expression-grammar-annotation_expression
        match expression {
            // TODO: parse the expression and check whether it is a string annotation, since they
            // can be annotation expressions distinct from type expressions.
            // https://typing.readthedocs.io/en/latest/spec/annotations.html#string-annotations
            ast::Expr::StringLiteral(_literal) => Type::Todo,

            // Annotation expressions also get special handling for `*args` and `**kwargs`.
            ast::Expr::Starred(starred) => self.infer_starred_expression(starred),

            // All other annotation expressions are (possibly) valid type expressions, so handle
            // them there instead.
            type_expr => self.infer_type_expression(type_expr),
        }
    }
}

/// Type expressions
impl<'db> TypeInferenceBuilder<'db> {
    fn infer_type_expression(&mut self, expression: &ast::Expr) -> Type<'db> {
        // https://typing.readthedocs.io/en/latest/spec/annotations.html#grammar-token-expression-grammar-type_expression
        // TODO: this does not include any of the special forms, and is only a
        //   stub of the forms other than a standalone name in scope.

        let ty = match expression {
            ast::Expr::Name(name) => {
                debug_assert_eq!(name.ctx, ast::ExprContext::Load);
                self.infer_name_expression(name).to_instance(self.db)
            }

            ast::Expr::Attribute(attribute_expression) => {
                debug_assert_eq!(attribute_expression.ctx, ast::ExprContext::Load);
                self.infer_attribute_expression(attribute_expression)
                    .to_instance(self.db)
            }

            ast::Expr::NoneLiteral(_literal) => Type::None,

            // TODO: parse the expression and check whether it is a string annotation.
            // https://typing.readthedocs.io/en/latest/spec/annotations.html#string-annotations
            ast::Expr::StringLiteral(_literal) => Type::Todo,

            // TODO: an Ellipsis literal *on its own* does not have any meaning in annotation
            // expressions, but is meaningful in the context of a number of special forms.
            ast::Expr::EllipsisLiteral(_literal) => Type::Todo,

            // Other literals do not have meaningful values in the annotation expression context.
            // However, we will we want to handle these differently when working with special forms,
            // since (e.g.) `123` is not valid in an annotation expression but `Literal[123]` is.
            ast::Expr::BytesLiteral(_literal) => Type::Todo,
            ast::Expr::NumberLiteral(_literal) => Type::Todo,
            ast::Expr::BooleanLiteral(_literal) => Type::Todo,

            ast::Expr::Subscript(subscript) => {
                let ast::ExprSubscript {
                    value,
                    slice,
                    ctx: _,
                    range: _,
                } = subscript;

                let value_ty = self.infer_expression(value);

                if value_ty
                    .into_class_literal_type()
                    .is_some_and(|class| class.is_known(self.db, KnownClass::Tuple))
                {
                    self.infer_tuple_type_expression(slice)
                } else {
                    self.infer_type_expression(slice);
                    // TODO: many other kinds of subscripts
                    Type::Todo
                }
            }

            ast::Expr::BinOp(binary) => {
                match binary.op {
                    // PEP-604 unions are okay, e.g., `int | str`
                    ast::Operator::BitOr => {
                        let left_ty = self.infer_type_expression(&binary.left);
                        let right_ty = self.infer_type_expression(&binary.right);
                        UnionType::from_elements(self.db, [left_ty, right_ty])
                    }
                    // anything else is an invalid annotation:
                    _ => {
                        self.infer_binary_expression(binary);
                        Type::Unknown
                    }
                }
            }

            // TODO PEP 646
            ast::Expr::Starred(starred) => {
                self.infer_starred_expression(starred);
                Type::Todo
            }

            // Forms which are invalid in the context of annotation expressions: we infer their
            // nested expressions as normal expressions, but the type of the top-level expression is
            // always `Type::Unknown` in these cases.
            ast::Expr::BoolOp(bool_op) => {
                self.infer_boolean_expression(bool_op);
                Type::Unknown
            }
            ast::Expr::Named(named) => {
                self.infer_named_expression(named);
                Type::Unknown
            }
            ast::Expr::UnaryOp(unary) => {
                self.infer_unary_expression(unary);
                Type::Unknown
            }
            ast::Expr::Lambda(lambda_expression) => {
                self.infer_lambda_expression(lambda_expression);
                Type::Unknown
            }
            ast::Expr::If(if_expression) => {
                self.infer_if_expression(if_expression);
                Type::Unknown
            }
            ast::Expr::Dict(dict) => {
                self.infer_dict_expression(dict);
                Type::Unknown
            }
            ast::Expr::Set(set) => {
                self.infer_set_expression(set);
                Type::Unknown
            }
            ast::Expr::ListComp(listcomp) => {
                self.infer_list_comprehension_expression(listcomp);
                Type::Unknown
            }
            ast::Expr::SetComp(setcomp) => {
                self.infer_set_comprehension_expression(setcomp);
                Type::Unknown
            }
            ast::Expr::DictComp(dictcomp) => {
                self.infer_dict_comprehension_expression(dictcomp);
                Type::Unknown
            }
            ast::Expr::Generator(generator) => {
                self.infer_generator_expression(generator);
                Type::Unknown
            }
            ast::Expr::Await(await_expression) => {
                self.infer_await_expression(await_expression);
                Type::Unknown
            }
            ast::Expr::Yield(yield_expression) => {
                self.infer_yield_expression(yield_expression);
                Type::Unknown
            }
            ast::Expr::YieldFrom(yield_from) => {
                self.infer_yield_from_expression(yield_from);
                Type::Unknown
            }
            ast::Expr::Compare(compare) => {
                self.infer_compare_expression(compare);
                Type::Unknown
            }
            ast::Expr::Call(call_expr) => {
                self.infer_call_expression(call_expr);
                Type::Unknown
            }
            ast::Expr::FString(fstring) => {
                self.infer_fstring_expression(fstring);
                Type::Unknown
            }
            ast::Expr::List(list) => {
                self.infer_list_expression(list);
                Type::Unknown
            }
            ast::Expr::Tuple(tuple) => {
                self.infer_tuple_expression(tuple);
                Type::Unknown
            }
            ast::Expr::Slice(slice) => {
                self.infer_slice_expression(slice);
                Type::Unknown
            }

            ast::Expr::IpyEscapeCommand(_) => todo!("Implement Ipy escape command support"),
        };

        let expr_id = expression.scoped_ast_id(self.db, self.scope());
        let previous = self.types.expressions.insert(expr_id, ty);
        assert!(previous.is_none());

        ty
    }

    /// Given the slice of a `tuple[]` annotation, return the type that the annotation represents
    fn infer_tuple_type_expression(&mut self, tuple_slice: &ast::Expr) -> Type<'db> {
        /// In most cases, if a subelement of the tuple is inferred as `Todo`,
        /// we should only infer `Todo` for that specific subelement.
        /// Certain specific AST nodes can however change the meaning of the entire tuple,
        /// however: for example, `tuple[int, ...]` or `tuple[int, *tuple[str, ...]]` are a
        /// homogeneous tuple and a partly homogeneous tuple (respectively) due to the `...`
        /// and the starred expression (respectively), Neither is supported by us right now,
        /// so we should infer `Todo` for the *entire* tuple if we encounter one of those elements.
        /// Even a subscript subelement could alter the type of the entire tuple
        /// if the subscript is `Unpack[]` (which again, we don't yet support).
        fn element_could_alter_type_of_whole_tuple(element: &ast::Expr, element_ty: Type) -> bool {
            element_ty.is_todo()
                && matches!(
                    element,
                    ast::Expr::EllipsisLiteral(_) | ast::Expr::Starred(_) | ast::Expr::Subscript(_)
                )
        }

        // TODO:
        // - homogeneous tuples
        // - PEP 646
        match tuple_slice {
            ast::Expr::Tuple(elements) => {
                let mut element_types = Vec::with_capacity(elements.len());

                // Whether to infer `Todo` for the whole tuple
                // (see docstring for `element_could_alter_type_of_whole_tuple`)
                let mut return_todo = false;

                for element in elements {
                    let element_ty = self.infer_type_expression(element);
                    return_todo |= element_could_alter_type_of_whole_tuple(element, element_ty);
                    element_types.push(element_ty);
                }

                if return_todo {
                    Type::Todo
                } else {
                    Type::Tuple(TupleType::new(self.db, element_types.into_boxed_slice()))
                }
            }
            single_element => {
                let single_element_ty = self.infer_type_expression(single_element);
                if element_could_alter_type_of_whole_tuple(single_element, single_element_ty) {
                    Type::Todo
                } else {
                    Type::Tuple(TupleType::new(self.db, Box::from([single_element_ty])))
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RichCompareOperator {
    Eq,
    Ne,
    Gt,
    Ge,
    Lt,
    Le,
}

impl From<RichCompareOperator> for ast::CmpOp {
    fn from(value: RichCompareOperator) -> Self {
        match value {
            RichCompareOperator::Eq => ast::CmpOp::Eq,
            RichCompareOperator::Ne => ast::CmpOp::NotEq,
            RichCompareOperator::Lt => ast::CmpOp::Lt,
            RichCompareOperator::Le => ast::CmpOp::LtE,
            RichCompareOperator::Gt => ast::CmpOp::Gt,
            RichCompareOperator::Ge => ast::CmpOp::GtE,
        }
    }
}

impl RichCompareOperator {
    #[must_use]
    const fn dunder(self) -> &'static str {
        match self {
            RichCompareOperator::Eq => "__eq__",
            RichCompareOperator::Ne => "__ne__",
            RichCompareOperator::Lt => "__lt__",
            RichCompareOperator::Le => "__le__",
            RichCompareOperator::Gt => "__gt__",
            RichCompareOperator::Ge => "__ge__",
        }
    }

    #[must_use]
    const fn reflect(self) -> Self {
        match self {
            RichCompareOperator::Eq => RichCompareOperator::Eq,
            RichCompareOperator::Ne => RichCompareOperator::Ne,
            RichCompareOperator::Lt => RichCompareOperator::Gt,
            RichCompareOperator::Le => RichCompareOperator::Ge,
            RichCompareOperator::Gt => RichCompareOperator::Lt,
            RichCompareOperator::Ge => RichCompareOperator::Le,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MembershipTestCompareOperator {
    In,
    NotIn,
}

impl From<MembershipTestCompareOperator> for ast::CmpOp {
    fn from(value: MembershipTestCompareOperator) -> Self {
        match value {
            MembershipTestCompareOperator::In => ast::CmpOp::In,
            MembershipTestCompareOperator::NotIn => ast::CmpOp::NotIn,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CompareUnsupportedError<'db> {
    op: ast::CmpOp,
    left_ty: Type<'db>,
    right_ty: Type<'db>,
}

fn format_import_from_module(level: u32, module: Option<&str>) -> String {
    format!(
        "{}{}",
        ".".repeat(level as usize),
        module.unwrap_or_default()
    )
}

/// Various ways in which resolving a [`ModuleName`]
/// from an [`ast::StmtImport`] or [`ast::StmtImportFrom`] node might fail
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum ModuleNameResolutionError {
    /// The import statement has invalid syntax
    InvalidSyntax,

    /// We couldn't resolve the file we're currently analyzing back to a module
    /// (Only necessary for relative import statements)
    UnknownCurrentModule,

    /// The relative import statement seems to take us outside of the module search path
    /// (e.g. our current module is `foo.bar`, and the relative import statement in `foo.bar`
    /// is `from ....baz import spam`)
    TooManyDots,
}

/// Struct collecting string parts when inferring a formatted string. Infers a string literal if the
/// concatenated string is small enough, otherwise infers a literal string.
///
/// If the formatted string contains an expression (with a representation unknown at compile time),
/// infers an instance of `builtins.str`.
struct StringPartsCollector {
    concatenated: Option<String>,
    expression: bool,
}

impl StringPartsCollector {
    fn new() -> Self {
        Self {
            concatenated: Some(String::new()),
            expression: false,
        }
    }

    fn push_str(&mut self, literal: &str) {
        if let Some(mut concatenated) = self.concatenated.take() {
            if concatenated.len().saturating_add(literal.len())
                <= TypeInferenceBuilder::MAX_STRING_LITERAL_SIZE
            {
                concatenated.push_str(literal);
                self.concatenated = Some(concatenated);
            } else {
                self.concatenated = None;
            }
        }
    }

    fn add_expression(&mut self) {
        self.concatenated = None;
        self.expression = true;
    }

    fn ty(self, db: &dyn Db) -> Type {
        if self.expression {
            KnownClass::Str.to_instance(db)
        } else if let Some(concatenated) = self.concatenated {
            Type::StringLiteral(StringLiteralType::new(db, concatenated.into_boxed_str()))
        } else {
            Type::LiteralString
        }
    }
}

/// Rich comparison in Python are the operators `==`, `!=`, `<`, `<=`, `>`, and `>=`. Their
/// behaviour can be edited for classes by implementing corresponding dunder methods.
/// This function performs rich comparison between two instances and returns the resulting type.
/// see `<https://docs.python.org/3/reference/datamodel.html#object.__lt__>`
fn perform_rich_comparison<'db>(
    db: &'db dyn Db,
    left_class: ClassType<'db>,
    right_class: ClassType<'db>,
    op: RichCompareOperator,
) -> Result<Type<'db>, CompareUnsupportedError<'db>> {
    // The following resource has details about the rich comparison algorithm:
    // https://snarky.ca/unravelling-rich-comparison-operators/
    //
    // TODO: this currently gives the return type even if the arg types are invalid
    // (e.g. int.__lt__ with string instance should be errored, currently bool)

    let call_dunder =
        |op: RichCompareOperator, left_class: ClassType<'db>, right_class: ClassType<'db>| {
            left_class
                .class_member(db, op.dunder())
                .call(
                    db,
                    &[Type::Instance(left_class), Type::Instance(right_class)],
                )
                .return_ty(db)
        };

    // The reflected dunder has priority if the right-hand side is a strict subclass of the left-hand side.
    if left_class != right_class && right_class.is_subclass_of(db, left_class) {
        call_dunder(op.reflect(), right_class, left_class)
            .or_else(|| call_dunder(op, left_class, right_class))
    } else {
        call_dunder(op, left_class, right_class)
            .or_else(|| call_dunder(op.reflect(), right_class, left_class))
    }
    .or_else(|| {
        // When no appropriate method returns any value other than NotImplemented,
        // the `==` and `!=` operators will fall back to `is` and `is not`, respectively.
        // refer to `<https://docs.python.org/3/reference/datamodel.html#object.__eq__>`
        if matches!(op, RichCompareOperator::Eq | RichCompareOperator::Ne) {
            Some(KnownClass::Bool.to_instance(db))
        } else {
            None
        }
    })
    .ok_or_else(|| CompareUnsupportedError {
        op: op.into(),
        left_ty: Type::Instance(left_class),
        right_ty: Type::Instance(right_class),
    })
}

/// Performs a membership test (`in` and `not in`) between two instances and returns the resulting type, or `None` if the test is unsupported.
/// The behavior can be customized in Python by implementing `__contains__`, `__iter__`, or `__getitem__` methods.
/// See `<https://docs.python.org/3/reference/datamodel.html#object.__contains__>`
/// and `<https://docs.python.org/3/reference/expressions.html#membership-test-details>`
fn perform_membership_test_comparison<'db>(
    db: &'db dyn Db,
    left_class: ClassType<'db>,
    right_class: ClassType<'db>,
    op: MembershipTestCompareOperator,
) -> Result<Type<'db>, CompareUnsupportedError<'db>> {
    let (left_instance, right_instance) = (Type::Instance(left_class), Type::Instance(right_class));

    let contains_dunder = right_class.class_member(db, "__contains__");

    let compare_result_opt = if contains_dunder.is_unbound() {
        // iteration-based membership test
        match right_instance.iterate(db) {
            IterationOutcome::Iterable { .. } => Some(KnownClass::Bool.to_instance(db)),
            IterationOutcome::NotIterable { .. } => None,
        }
    } else {
        // If `__contains__` is available, it is used directly for the membership test.
        contains_dunder
            .call(db, &[right_instance, left_instance])
            .return_ty(db)
    };

    compare_result_opt
        .map(|ty| {
            if matches!(ty, Type::Todo) {
                return Type::Todo;
            }

            match op {
                MembershipTestCompareOperator::In => ty.bool(db).into_type(db),
                MembershipTestCompareOperator::NotIn => ty.bool(db).negate().into_type(db),
            }
        })
        .ok_or_else(|| CompareUnsupportedError {
            op: op.into(),
            left_ty: left_instance,
            right_ty: right_instance,
        })
}

#[cfg(test)]
mod tests {

    use anyhow::Context;

    use crate::db::tests::TestDb;
    use crate::program::{Program, SearchPathSettings};
    use crate::python_version::PythonVersion;
    use crate::semantic_index::definition::Definition;
    use crate::semantic_index::symbol::FileScopeId;
    use crate::semantic_index::{global_scope, semantic_index, symbol_table, use_def_map};
    use crate::types::{
        check_types, global_symbol_ty, infer_definition_types, symbol_ty, TypeCheckDiagnostics,
    };
    use crate::{HasTy, ProgramSettings, SemanticModel};
    use ruff_db::files::{system_path_to_file, File};
    use ruff_db::parsed::parsed_module;
    use ruff_db::system::{DbWithTestSystem, SystemPathBuf};
    use ruff_db::testing::assert_function_query_was_not_run;
    use ruff_python_ast::name::Name;

    use super::TypeInferenceBuilder;

    fn setup_db() -> TestDb {
        let db = TestDb::new();

        let src_root = SystemPathBuf::from("/src");
        db.memory_file_system()
            .create_directory_all(&src_root)
            .unwrap();

        Program::from_settings(
            &db,
            &ProgramSettings {
                target_version: PythonVersion::default(),
                search_paths: SearchPathSettings::new(src_root),
            },
        )
        .expect("Valid search path settings");

        db
    }

    fn setup_db_with_custom_typeshed<'a>(
        typeshed: &str,
        files: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) -> anyhow::Result<TestDb> {
        let mut db = TestDb::new();
        let src_root = SystemPathBuf::from("/src");

        db.write_files(files)
            .context("Failed to write test files")?;

        Program::from_settings(
            &db,
            &ProgramSettings {
                target_version: PythonVersion::default(),
                search_paths: SearchPathSettings {
                    custom_typeshed: Some(SystemPathBuf::from(typeshed)),
                    ..SearchPathSettings::new(src_root)
                },
            },
        )
        .context("Failed to create Program")?;

        Ok(db)
    }

    #[track_caller]
    fn assert_public_ty(db: &TestDb, file_name: &str, symbol_name: &str, expected: &str) {
        let file = system_path_to_file(db, file_name).expect("file to exist");

        let ty = global_symbol_ty(db, file, symbol_name);
        assert_eq!(
            ty.display(db).to_string(),
            expected,
            "Mismatch for symbol '{symbol_name}' in '{file_name}'"
        );
    }

    #[track_caller]
    fn assert_scope_ty(
        db: &TestDb,
        file_name: &str,
        scopes: &[&str],
        symbol_name: &str,
        expected: &str,
    ) {
        let file = system_path_to_file(db, file_name).expect("file to exist");
        let index = semantic_index(db, file);
        let mut file_scope_id = FileScopeId::global();
        let mut scope = file_scope_id.to_scope_id(db, file);
        for expected_scope_name in scopes {
            file_scope_id = index
                .child_scopes(file_scope_id)
                .next()
                .unwrap_or_else(|| panic!("scope of {expected_scope_name}"))
                .0;
            scope = file_scope_id.to_scope_id(db, file);
            assert_eq!(scope.name(db), *expected_scope_name);
        }

        let ty = symbol_ty(db, scope, symbol_name);
        assert_eq!(ty.display(db).to_string(), expected);
    }

    #[track_caller]
    fn assert_diagnostic_messages(diagnostics: &TypeCheckDiagnostics, expected: &[&str]) {
        let messages: Vec<&str> = diagnostics
            .iter()
            .map(|diagnostic| diagnostic.message())
            .collect();
        assert_eq!(&messages, expected);
    }

    #[track_caller]
    fn assert_file_diagnostics(db: &TestDb, filename: &str, expected: &[&str]) {
        let file = system_path_to_file(db, filename).unwrap();
        let diagnostics = check_types(db, file);

        assert_diagnostic_messages(&diagnostics, expected);
    }

    #[test]
    fn from_import_with_no_module_name() -> anyhow::Result<()> {
        // This test checks that invalid syntax in a `StmtImportFrom` node
        // leads to the type being inferred as `Unknown`
        let mut db = setup_db();
        db.write_file("src/foo.py", "from import bar")?;
        assert_public_ty(&db, "src/foo.py", "bar", "Unknown");
        Ok(())
    }

    #[test]
    fn resolve_base_class_by_name() -> anyhow::Result<()> {
        let mut db = setup_db();

        db.write_dedented(
            "src/mod.py",
            "
            class Base:
                pass

            class Sub(Base):
                pass
            ",
        )?;

        let mod_file = system_path_to_file(&db, "src/mod.py").expect("file to exist");
        let ty = global_symbol_ty(&db, mod_file, "Sub");

        let class = ty.expect_class_literal();

        let base_names: Vec<_> = class
            .bases(&db)
            .map(|base_ty| format!("{}", base_ty.display(&db)))
            .collect();

        assert_eq!(base_names, vec!["Literal[Base]"]);

        Ok(())
    }

    #[test]
    fn resolve_method() -> anyhow::Result<()> {
        let mut db = setup_db();

        db.write_dedented(
            "src/mod.py",
            "
            class C:
                def f(self): pass
            ",
        )?;

        let mod_file = system_path_to_file(&db, "src/mod.py").unwrap();
        let ty = global_symbol_ty(&db, mod_file, "C");
        let class_id = ty.expect_class_literal();
        let member_ty = class_id.class_member(&db, &Name::new_static("f"));
        let func = member_ty.expect_function_literal();

        assert_eq!(func.name(&db), "f");
        Ok(())
    }

    #[test]
    fn not_literal_string() -> anyhow::Result<()> {
        let mut db = setup_db();
        let content = format!(
            r#"
        v = not "{y}"
        w = not 10*"{y}"
        x = not "{y}"*10
        z = not 0*"{y}"
        u = not (-100)*"{y}"
        "#,
            y = "a".repeat(TypeInferenceBuilder::MAX_STRING_LITERAL_SIZE + 1),
        );
        db.write_dedented("src/a.py", &content)?;

        assert_public_ty(&db, "src/a.py", "v", "bool");
        assert_public_ty(&db, "src/a.py", "w", "bool");
        assert_public_ty(&db, "src/a.py", "x", "bool");
        assert_public_ty(&db, "src/a.py", "z", "Literal[True]");
        assert_public_ty(&db, "src/a.py", "u", "Literal[True]");

        Ok(())
    }

    #[test]
    fn multiplied_string() -> anyhow::Result<()> {
        let mut db = setup_db();

        db.write_dedented(
            "src/a.py",
            &format!(
                r#"
            w = 2 * "hello"
            x = "goodbye" * 3
            y = "a" * {y}
            z = {z} * "b"
            a = 0 * "hello"
            b = -3 * "hello"
            "#,
                y = TypeInferenceBuilder::MAX_STRING_LITERAL_SIZE,
                z = TypeInferenceBuilder::MAX_STRING_LITERAL_SIZE + 1
            ),
        )?;

        assert_public_ty(&db, "src/a.py", "w", r#"Literal["hellohello"]"#);
        assert_public_ty(&db, "src/a.py", "x", r#"Literal["goodbyegoodbyegoodbye"]"#);
        assert_public_ty(
            &db,
            "src/a.py",
            "y",
            &format!(
                r#"Literal["{}"]"#,
                "a".repeat(TypeInferenceBuilder::MAX_STRING_LITERAL_SIZE)
            ),
        );
        assert_public_ty(&db, "src/a.py", "z", "LiteralString");
        assert_public_ty(&db, "src/a.py", "a", r#"Literal[""]"#);
        assert_public_ty(&db, "src/a.py", "b", r#"Literal[""]"#);

        Ok(())
    }

    #[test]
    fn multiplied_literal_string() -> anyhow::Result<()> {
        let mut db = setup_db();
        let content = format!(
            r#"
        v = "{y}"
        w = 10*"{y}"
        x = "{y}"*10
        z = 0*"{y}"
        u = (-100)*"{y}"
        "#,
            y = "a".repeat(TypeInferenceBuilder::MAX_STRING_LITERAL_SIZE + 1),
        );
        db.write_dedented("src/a.py", &content)?;

        assert_public_ty(&db, "src/a.py", "v", "LiteralString");
        assert_public_ty(&db, "src/a.py", "w", "LiteralString");
        assert_public_ty(&db, "src/a.py", "x", "LiteralString");
        assert_public_ty(&db, "src/a.py", "z", r#"Literal[""]"#);
        assert_public_ty(&db, "src/a.py", "u", r#"Literal[""]"#);
        Ok(())
    }

    #[test]
    fn truncated_string_literals_become_literal_string() -> anyhow::Result<()> {
        let mut db = setup_db();
        let content = format!(
            r#"
        w = "{y}"
        x = "a" + "{z}"
        "#,
            y = "a".repeat(TypeInferenceBuilder::MAX_STRING_LITERAL_SIZE + 1),
            z = "a".repeat(TypeInferenceBuilder::MAX_STRING_LITERAL_SIZE),
        );
        db.write_dedented("src/a.py", &content)?;

        assert_public_ty(&db, "src/a.py", "w", "LiteralString");
        assert_public_ty(&db, "src/a.py", "x", "LiteralString");

        Ok(())
    }

    #[test]
    fn adding_string_literals_and_literal_string() -> anyhow::Result<()> {
        let mut db = setup_db();
        let content = format!(
            r#"
        v = "{y}"
        w = "{y}" + "a"
        x = "a" + "{y}"
        z = "{y}" + "{y}"
        "#,
            y = "a".repeat(TypeInferenceBuilder::MAX_STRING_LITERAL_SIZE + 1),
        );
        db.write_dedented("src/a.py", &content)?;

        assert_public_ty(&db, "src/a.py", "v", "LiteralString");
        assert_public_ty(&db, "src/a.py", "w", "LiteralString");
        assert_public_ty(&db, "src/a.py", "x", "LiteralString");
        assert_public_ty(&db, "src/a.py", "z", "LiteralString");

        Ok(())
    }

    #[test]
    fn bytes_type() -> anyhow::Result<()> {
        let mut db = setup_db();

        db.write_dedented(
            "src/a.py",
            "
            w = b'red' b'knot'
            x = b'hello'
            y = b'world' + b'!'
            z = b'\\xff\\x00'
            ",
        )?;

        assert_public_ty(&db, "src/a.py", "w", "Literal[b\"redknot\"]");
        assert_public_ty(&db, "src/a.py", "x", "Literal[b\"hello\"]");
        assert_public_ty(&db, "src/a.py", "y", "Literal[b\"world!\"]");
        assert_public_ty(&db, "src/a.py", "z", "Literal[b\"\\xff\\x00\"]");

        Ok(())
    }

    #[test]
    fn ellipsis_type() -> anyhow::Result<()> {
        let mut db = setup_db();

        db.write_dedented(
            "src/a.py",
            "
            x = ...
            ",
        )?;

        // TODO: sys.version_info, and need to understand @final and @type_check_only
        assert_public_ty(&db, "src/a.py", "x", "EllipsisType | Unknown");

        Ok(())
    }

    #[test]
    fn function_return_type() -> anyhow::Result<()> {
        let mut db = setup_db();

        db.write_file("src/a.py", "def example() -> int: return 42")?;

        let mod_file = system_path_to_file(&db, "src/a.py").unwrap();
        let function = global_symbol_ty(&db, mod_file, "example").expect_function_literal();
        let returns = function.return_type(&db);
        assert_eq!(returns.display(&db).to_string(), "int");

        Ok(())
    }

    #[test]
    fn import_cycle() -> anyhow::Result<()> {
        let mut db = setup_db();

        db.write_dedented(
            "src/a.py",
            "
            class A: pass
            import b
            class C(b.B): pass
            ",
        )?;
        db.write_dedented(
            "src/b.py",
            "
            from a import A
            class B(A): pass
            ",
        )?;

        let a = system_path_to_file(&db, "src/a.py").expect("file to exist");
        let c_ty = global_symbol_ty(&db, a, "C");
        let c_class = c_ty.expect_class_literal();
        let mut c_bases = c_class.bases(&db);
        let b_ty = c_bases.next().unwrap();
        let b_class = b_ty.expect_class_literal();
        assert_eq!(b_class.name(&db), "B");
        let mut b_bases = b_class.bases(&db);
        let a_ty = b_bases.next().unwrap();
        let a_class = a_ty.expect_class_literal();
        assert_eq!(a_class.name(&db), "A");

        Ok(())
    }

    /// An unbound function local that has definitions in the scope does not fall back to globals.
    #[test]
    fn unbound_function_local() -> anyhow::Result<()> {
        let mut db = setup_db();

        db.write_dedented(
            "src/a.py",
            "
            x = 1
            def f():
                y = x
                x = 2
            ",
        )?;

        let file = system_path_to_file(&db, "src/a.py").expect("file to exist");
        let index = semantic_index(&db, file);
        let function_scope = index
            .child_scopes(FileScopeId::global())
            .next()
            .unwrap()
            .0
            .to_scope_id(&db, file);
        let y_ty = symbol_ty(&db, function_scope, "y");
        let x_ty = symbol_ty(&db, function_scope, "x");

        assert_eq!(y_ty.display(&db).to_string(), "Unbound");
        assert_eq!(x_ty.display(&db).to_string(), "Literal[2]");

        Ok(())
    }

    /// A name reference to a never-defined symbol in a function is implicitly a global lookup.
    #[test]
    fn implicit_global_in_function() -> anyhow::Result<()> {
        let mut db = setup_db();

        db.write_dedented(
            "src/a.py",
            "
            x = 1
            def f():
                y = x
            ",
        )?;

        let file = system_path_to_file(&db, "src/a.py").expect("file to exist");
        let index = semantic_index(&db, file);
        let function_scope = index
            .child_scopes(FileScopeId::global())
            .next()
            .unwrap()
            .0
            .to_scope_id(&db, file);
        let y_ty = symbol_ty(&db, function_scope, "y");
        let x_ty = symbol_ty(&db, function_scope, "x");

        assert_eq!(x_ty.display(&db).to_string(), "Unbound");
        assert_eq!(y_ty.display(&db).to_string(), "Literal[1]");

        Ok(())
    }

    #[test]
    fn local_inference() -> anyhow::Result<()> {
        let mut db = setup_db();

        db.write_file("/src/a.py", "x = 10")?;
        let a = system_path_to_file(&db, "/src/a.py").unwrap();

        let parsed = parsed_module(&db, a);

        let statement = parsed.suite().first().unwrap().as_assign_stmt().unwrap();
        let model = SemanticModel::new(&db, a);

        let literal_ty = statement.value.ty(&model);

        assert_eq!(format!("{}", literal_ty.display(&db)), "Literal[10]");

        Ok(())
    }

    #[test]
    fn builtin_symbol_vendored_stdlib() -> anyhow::Result<()> {
        let mut db = setup_db();

        db.write_file("/src/a.py", "c = copyright")?;

        assert_public_ty(&db, "/src/a.py", "c", "Literal[copyright]");

        Ok(())
    }

    #[test]
    fn builtin_symbol_custom_stdlib() -> anyhow::Result<()> {
        let db = setup_db_with_custom_typeshed(
            "/typeshed",
            [
                ("/src/a.py", "c = copyright"),
                (
                    "/typeshed/stdlib/builtins.pyi",
                    "def copyright() -> None: ...",
                ),
                ("/typeshed/stdlib/VERSIONS", "builtins: 3.8-"),
            ],
        )?;

        assert_public_ty(&db, "/src/a.py", "c", "Literal[copyright]");

        Ok(())
    }

    #[test]
    fn unknown_builtin_later_defined() -> anyhow::Result<()> {
        let db = setup_db_with_custom_typeshed(
            "/typeshed",
            [
                ("/src/a.py", "x = foo"),
                ("/typeshed/stdlib/builtins.pyi", "foo = bar; bar = 1"),
                ("/typeshed/stdlib/VERSIONS", "builtins: 3.8-"),
            ],
        )?;

        assert_public_ty(&db, "/src/a.py", "x", "Unbound");

        Ok(())
    }

    #[test]
    fn str_builtin() -> anyhow::Result<()> {
        let mut db = setup_db();
        db.write_file("/src/a.py", "x = str")?;
        assert_public_ty(&db, "/src/a.py", "x", "Literal[str]");
        Ok(())
    }

    #[test]
    fn deferred_annotation_builtin() -> anyhow::Result<()> {
        let mut db = setup_db();
        db.write_file("/src/a.pyi", "class C(object): pass")?;
        let file = system_path_to_file(&db, "/src/a.pyi").unwrap();
        let ty = global_symbol_ty(&db, file, "C");

        let base = ty
            .expect_class_literal()
            .bases(&db)
            .next()
            .expect("there should be at least one base");

        assert_eq!(base.display(&db).to_string(), "Literal[object]");

        Ok(())
    }

    #[test]
    fn deferred_annotation_in_stubs_always_resolve() -> anyhow::Result<()> {
        let mut db = setup_db();

        // Stub files should always resolve deferred annotations
        db.write_dedented(
            "/src/stub.pyi",
            "
            def get_foo() -> Foo: ...
            class Foo: ...
            foo = get_foo()
            ",
        )?;
        assert_public_ty(&db, "/src/stub.pyi", "foo", "Foo");

        Ok(())
    }

    #[test]
    fn deferred_annotations_regular_source_fails() -> anyhow::Result<()> {
        let mut db = setup_db();

        // In (regular) source files, deferred annotations are *not* resolved
        // Also tests imports from `__future__` that are not annotations
        db.write_dedented(
            "/src/source.py",
            "
            from __future__ import with_statement as annotations
            def get_foo() -> Foo: ...
            class Foo: ...
            foo = get_foo()
            ",
        )?;
        assert_public_ty(&db, "/src/source.py", "foo", "Unknown");

        Ok(())
    }

    #[test]
    fn deferred_annotation_in_sources_with_future_resolves() -> anyhow::Result<()> {
        let mut db = setup_db();

        // In source files with `__future__.annotations`, deferred annotations are resolved
        db.write_dedented(
            "/src/source_with_future.py",
            "
            from __future__ import annotations
            def get_foo() -> Foo: ...
            class Foo: ...
            foo = get_foo()
            ",
        )?;
        assert_public_ty(&db, "/src/source_with_future.py", "foo", "Foo");

        Ok(())
    }

    #[test]
    fn nonlocal_name_reference() -> anyhow::Result<()> {
        let mut db = setup_db();

        db.write_dedented(
            "/src/a.py",
            "
            def f():
                x = 1
                def g():
                    y = x
            ",
        )?;

        assert_scope_ty(&db, "/src/a.py", &["f", "g"], "y", "Literal[1]");

        Ok(())
    }

    #[test]
    fn nonlocal_name_reference_multi_level() -> anyhow::Result<()> {
        let mut db = setup_db();

        db.write_dedented(
            "/src/a.py",
            "
            def f():
                x = 1
                def g():
                    def h():
                        y = x
            ",
        )?;

        assert_scope_ty(&db, "/src/a.py", &["f", "g", "h"], "y", "Literal[1]");

        Ok(())
    }

    #[test]
    fn nonlocal_name_reference_skips_class_scope() -> anyhow::Result<()> {
        let mut db = setup_db();

        db.write_dedented(
            "/src/a.py",
            "
            def f():
                x = 1
                class C:
                    x = 2
                    def g():
                        y = x
            ",
        )?;

        assert_scope_ty(&db, "/src/a.py", &["f", "C", "g"], "y", "Literal[1]");

        Ok(())
    }

    #[test]
    fn nonlocal_name_reference_skips_annotation_only_assignment() -> anyhow::Result<()> {
        let mut db = setup_db();

        db.write_dedented(
            "/src/a.py",
            "
            def f():
                x = 1
                def g():
                    // it's pretty weird to have an annotated assignment in a function where the
                    // name is otherwise not defined; maybe should be an error?
                    x: int
                    def h():
                        y = x
            ",
        )?;

        assert_scope_ty(&db, "/src/a.py", &["f", "g", "h"], "y", "Literal[1]");

        Ok(())
    }

    #[test]
    fn exception_handler_with_invalid_syntax() -> anyhow::Result<()> {
        let mut db = setup_db();

        db.write_dedented(
            "src/a.py",
            "
            from typing_extensions import reveal_type

            try:
                print
            except as e:
                reveal_type(e)
            ",
        )?;

        assert_file_diagnostics(&db, "src/a.py", &["Revealed type is `Unknown`"]);

        Ok(())
    }

    #[test]
    fn basic_comprehension() -> anyhow::Result<()> {
        let mut db = setup_db();

        db.write_dedented(
            "src/a.py",
            "
            def foo():
                [x for y in IterableOfIterables() for x in y]

            class IntIterator:
                def __next__(self) -> int:
                    return 42

            class IntIterable:
                def __iter__(self) -> IntIterator:
                    return IntIterator()

            class IteratorOfIterables:
                def __next__(self) -> IntIterable:
                    return IntIterable()

            class IterableOfIterables:
                def __iter__(self) -> IteratorOfIterables:
                    return IteratorOfIterables()
            ",
        )?;

        assert_scope_ty(&db, "src/a.py", &["foo", "<listcomp>"], "x", "int");
        assert_scope_ty(&db, "src/a.py", &["foo", "<listcomp>"], "y", "IntIterable");

        Ok(())
    }

    #[test]
    fn comprehension_inside_comprehension() -> anyhow::Result<()> {
        let mut db = setup_db();

        db.write_dedented(
            "src/a.py",
            "
            def foo():
                [[x for x in iter1] for y in iter2]

            class IntIterator:
                def __next__(self) -> int:
                    return 42

            class IntIterable:
                def __iter__(self) -> IntIterator:
                    return IntIterator()

            iter1 = IntIterable()
            iter2 = IntIterable()
            ",
        )?;

        assert_scope_ty(
            &db,
            "src/a.py",
            &["foo", "<listcomp>", "<listcomp>"],
            "x",
            "int",
        );
        assert_scope_ty(&db, "src/a.py", &["foo", "<listcomp>"], "y", "int");

        Ok(())
    }

    #[test]
    fn inner_comprehension_referencing_outer_comprehension() -> anyhow::Result<()> {
        let mut db = setup_db();

        db.write_dedented(
            "src/a.py",
            "
            def foo():
                [[x for x in y] for y in z]

            class IntIterator:
                def __next__(self) -> int:
                    return 42

            class IntIterable:
                def __iter__(self) -> IntIterator:
                    return IntIterator()

            class IteratorOfIterables:
                def __next__(self) -> IntIterable:
                    return IntIterable()

            class IterableOfIterables:
                def __iter__(self) -> IteratorOfIterables:
                    return IteratorOfIterables()

            z = IterableOfIterables()
            ",
        )?;

        assert_scope_ty(
            &db,
            "src/a.py",
            &["foo", "<listcomp>", "<listcomp>"],
            "x",
            "int",
        );
        assert_scope_ty(&db, "src/a.py", &["foo", "<listcomp>"], "y", "IntIterable");

        Ok(())
    }

    #[test]
    fn comprehension_with_unbound_iter() -> anyhow::Result<()> {
        let mut db = setup_db();

        db.write_dedented("src/a.py", "[z for z in x]")?;

        assert_scope_ty(&db, "src/a.py", &["<listcomp>"], "x", "Unbound");

        // Iterating over an `Unbound` yields `Unknown`:
        assert_scope_ty(&db, "src/a.py", &["<listcomp>"], "z", "Unknown");

        // TODO: not the greatest error message in the world! --Alex
        assert_file_diagnostics(
            &db,
            "src/a.py",
            &[
                "Name `x` used when not defined",
                "Object of type `Unbound` is not iterable",
            ],
        );

        Ok(())
    }

    #[test]
    fn comprehension_with_not_iterable_iter_in_second_comprehension() -> anyhow::Result<()> {
        let mut db = setup_db();

        db.write_dedented(
            "src/a.py",
            "
            def foo():
                [z for x in IntIterable() for z in x]

            class IntIterator:
                def __next__(self) -> int:
                    return 42

            class IntIterable:
                def __iter__(self) -> IntIterator:
                    return IntIterator()
            ",
        )?;

        assert_scope_ty(&db, "src/a.py", &["foo", "<listcomp>"], "x", "int");
        assert_scope_ty(&db, "src/a.py", &["foo", "<listcomp>"], "z", "Unknown");
        assert_file_diagnostics(&db, "src/a.py", &["Object of type `int` is not iterable"]);

        Ok(())
    }

    #[test]
    fn dict_comprehension_variable_key() -> anyhow::Result<()> {
        let mut db = setup_db();

        db.write_dedented(
            "src/a.py",
            "
            def foo():
                {x: 0 for x in IntIterable()}

            class IntIterator:
                def __next__(self) -> int:
                    return 42

            class IntIterable:
                def __iter__(self) -> IntIterator:
                    return IntIterator()
            ",
        )?;

        assert_scope_ty(&db, "src/a.py", &["foo", "<dictcomp>"], "x", "int");
        assert_file_diagnostics(&db, "src/a.py", &[]);

        Ok(())
    }

    #[test]
    fn dict_comprehension_variable_value() -> anyhow::Result<()> {
        let mut db = setup_db();

        db.write_dedented(
            "src/a.py",
            "
            def foo():
                {0: x for x in IntIterable()}

            class IntIterator:
                def __next__(self) -> int:
                    return 42

            class IntIterable:
                def __iter__(self) -> IntIterator:
                    return IntIterator()
            ",
        )?;

        assert_scope_ty(&db, "src/a.py", &["foo", "<dictcomp>"], "x", "int");
        assert_file_diagnostics(&db, "src/a.py", &[]);

        Ok(())
    }

    #[test]
    fn comprehension_with_missing_in_keyword() -> anyhow::Result<()> {
        let mut db = setup_db();

        db.write_dedented(
            "src/a.py",
            "
            def foo():
                [z for z IntIterable()]

            class IntIterator:
                def __next__(self) -> int:
                    return 42

            class IntIterable:
                def __iter__(self) -> IntIterator:
                    return IntIterator()
            ",
        )?;

        // We'll emit a diagnostic separately for invalid syntax,
        // but it's reasonably clear here what they *meant* to write,
        // so we'll still infer the correct type:
        assert_scope_ty(&db, "src/a.py", &["foo", "<listcomp>"], "z", "int");
        Ok(())
    }

    #[test]
    fn comprehension_with_missing_iter() -> anyhow::Result<()> {
        let mut db = setup_db();

        db.write_dedented(
            "src/a.py",
            "
            def foo():
                [z for in IntIterable()]

            class IntIterator:
                def __next__(self) -> int:
                    return 42

            class IntIterable:
                def __iter__(self) -> IntIterator:
                    return IntIterator()
            ",
        )?;

        assert_scope_ty(&db, "src/a.py", &["foo", "<listcomp>"], "z", "Unbound");

        // (There is a diagnostic for invalid syntax that's emitted, but it's not listed by `assert_file_diagnostics`)
        assert_file_diagnostics(&db, "src/a.py", &["Name `z` used when not defined"]);

        Ok(())
    }

    #[test]
    fn comprehension_with_missing_for() -> anyhow::Result<()> {
        let mut db = setup_db();
        db.write_dedented("src/a.py", "[z for z in]")?;
        assert_scope_ty(&db, "src/a.py", &["<listcomp>"], "z", "Unknown");
        Ok(())
    }

    #[test]
    fn comprehension_with_missing_in_keyword_and_missing_iter() -> anyhow::Result<()> {
        let mut db = setup_db();
        db.write_dedented("src/a.py", "[z for z]")?;
        assert_scope_ty(&db, "src/a.py", &["<listcomp>"], "z", "Unknown");
        Ok(())
    }

    /// This tests that we understand that `async` comprehensions
    /// do not work according to the synchronous iteration protocol
    #[test]
    fn invalid_async_comprehension() -> anyhow::Result<()> {
        let mut db = setup_db();

        db.write_dedented(
            "src/a.py",
            "
            async def foo():
                [x async for x in Iterable()]
            class Iterator:
                def __next__(self) -> int:
                    return 42
            class Iterable:
                def __iter__(self) -> Iterator:
                    return Iterator()
            ",
        )?;

        // We currently return `Todo` for all async comprehensions,
        // including comprehensions that have invalid syntax
        assert_scope_ty(&db, "src/a.py", &["foo", "<listcomp>"], "x", "@Todo");

        Ok(())
    }

    #[test]
    fn basic_async_comprehension() -> anyhow::Result<()> {
        let mut db = setup_db();

        db.write_dedented(
            "src/a.py",
            "
            async def foo():
                [x async for x in AsyncIterable()]
            class AsyncIterator:
                async def __anext__(self) -> int:
                    return 42
            class AsyncIterable:
                def __aiter__(self) -> AsyncIterator:
                    return AsyncIterator()
            ",
        )?;

        // TODO async iterables/iterators! --Alex
        assert_scope_ty(&db, "src/a.py", &["foo", "<listcomp>"], "x", "@Todo");

        Ok(())
    }

    #[test]
    fn starred_expressions_must_be_iterable() {
        let mut db = setup_db();

        db.write_dedented(
            "src/a.py",
            "
            class NotIterable: pass

            class Iterator:
                def __next__(self) -> int:
                    return 42

            class Iterable:
                def __iter__(self) -> Iterator:

            x = [*NotIterable()]
            y = [*Iterable()]
            ",
        )
        .unwrap();

        assert_file_diagnostics(
            &db,
            "/src/a.py",
            &["Object of type `NotIterable` is not iterable"],
        );
    }

    // Incremental inference tests

    fn first_public_binding<'db>(db: &'db TestDb, file: File, name: &str) -> Definition<'db> {
        let scope = global_scope(db, file);
        use_def_map(db, scope)
            .public_bindings(symbol_table(db, scope).symbol_id_by_name(name).unwrap())
            .next()
            .unwrap()
            .binding
    }

    #[test]
    fn dependency_public_symbol_type_change() -> anyhow::Result<()> {
        let mut db = setup_db();

        db.write_files([
            ("/src/a.py", "from foo import x"),
            ("/src/foo.py", "x = 10\ndef foo(): ..."),
        ])?;

        let a = system_path_to_file(&db, "/src/a.py").unwrap();
        let x_ty = global_symbol_ty(&db, a, "x");

        assert_eq!(x_ty.display(&db).to_string(), "Literal[10]");

        // Change `x` to a different value
        db.write_file("/src/foo.py", "x = 20\ndef foo(): ...")?;

        let a = system_path_to_file(&db, "/src/a.py").unwrap();

        let x_ty_2 = global_symbol_ty(&db, a, "x");

        assert_eq!(x_ty_2.display(&db).to_string(), "Literal[20]");

        Ok(())
    }

    #[test]
    fn dependency_internal_symbol_change() -> anyhow::Result<()> {
        let mut db = setup_db();

        db.write_files([
            ("/src/a.py", "from foo import x"),
            ("/src/foo.py", "x = 10\ndef foo(): y = 1"),
        ])?;

        let a = system_path_to_file(&db, "/src/a.py").unwrap();
        let x_ty = global_symbol_ty(&db, a, "x");

        assert_eq!(x_ty.display(&db).to_string(), "Literal[10]");

        db.write_file("/src/foo.py", "x = 10\ndef foo(): pass")?;

        let a = system_path_to_file(&db, "/src/a.py").unwrap();

        db.clear_salsa_events();

        let x_ty_2 = global_symbol_ty(&db, a, "x");

        assert_eq!(x_ty_2.display(&db).to_string(), "Literal[10]");

        let events = db.take_salsa_events();

        assert_function_query_was_not_run(
            &db,
            infer_definition_types,
            first_public_binding(&db, a, "x"),
            &events,
        );

        Ok(())
    }

    #[test]
    fn dependency_unrelated_symbol() -> anyhow::Result<()> {
        let mut db = setup_db();

        db.write_files([
            ("/src/a.py", "from foo import x"),
            ("/src/foo.py", "x = 10\ny = 20"),
        ])?;

        let a = system_path_to_file(&db, "/src/a.py").unwrap();
        let x_ty = global_symbol_ty(&db, a, "x");

        assert_eq!(x_ty.display(&db).to_string(), "Literal[10]");

        db.write_file("/src/foo.py", "x = 10\ny = 30")?;

        let a = system_path_to_file(&db, "/src/a.py").unwrap();

        db.clear_salsa_events();

        let x_ty_2 = global_symbol_ty(&db, a, "x");

        assert_eq!(x_ty_2.display(&db).to_string(), "Literal[10]");

        let events = db.take_salsa_events();

        assert_function_query_was_not_run(
            &db,
            infer_definition_types,
            first_public_binding(&db, a, "x"),
            &events,
        );
        Ok(())
    }
}
