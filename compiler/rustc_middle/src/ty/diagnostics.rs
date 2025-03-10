//! Diagnostics related methods for `Ty`.

use std::ops::ControlFlow;

use crate::ty::{
    fold::TypeFoldable, Const, ConstKind, DefIdTree, ExistentialPredicate, InferTy,
    PolyTraitPredicate, Ty, TyCtxt, TypeSuperFoldable, TypeVisitor,
};

use rustc_data_structures::fx::FxHashMap;
use rustc_errors::{Applicability, Diagnostic, DiagnosticArgValue, IntoDiagnosticArg};
use rustc_hir as hir;
use rustc_hir::def_id::DefId;
use rustc_hir::WherePredicate;
use rustc_span::Span;
use rustc_type_ir::sty::TyKind::*;

impl<'tcx> IntoDiagnosticArg for Ty<'tcx> {
    fn into_diagnostic_arg(self) -> DiagnosticArgValue<'static> {
        format!("{}", self).into_diagnostic_arg()
    }
}

impl<'tcx> Ty<'tcx> {
    /// Similar to `Ty::is_primitive`, but also considers inferred numeric values to be primitive.
    pub fn is_primitive_ty(self) -> bool {
        matches!(
            self.kind(),
            Bool | Char
                | Str
                | Int(_)
                | Uint(_)
                | Float(_)
                | Infer(
                    InferTy::IntVar(_)
                        | InferTy::FloatVar(_)
                        | InferTy::FreshIntTy(_)
                        | InferTy::FreshFloatTy(_)
                )
        )
    }

    /// Whether the type is succinctly representable as a type instead of just referred to with a
    /// description in error messages. This is used in the main error message.
    pub fn is_simple_ty(self) -> bool {
        match self.kind() {
            Bool
            | Char
            | Str
            | Int(_)
            | Uint(_)
            | Float(_)
            | Infer(
                InferTy::IntVar(_)
                | InferTy::FloatVar(_)
                | InferTy::FreshIntTy(_)
                | InferTy::FreshFloatTy(_),
            ) => true,
            Ref(_, x, _) | Array(x, _) | Slice(x) => x.peel_refs().is_simple_ty(),
            Tuple(tys) if tys.is_empty() => true,
            _ => false,
        }
    }

    /// Whether the type is succinctly representable as a type instead of just referred to with a
    /// description in error messages. This is used in the primary span label. Beyond what
    /// `is_simple_ty` includes, it also accepts ADTs with no type arguments and references to
    /// ADTs with no type arguments.
    pub fn is_simple_text(self) -> bool {
        match self.kind() {
            Adt(_, substs) => substs.non_erasable_generics().next().is_none(),
            Ref(_, ty, _) => ty.is_simple_text(),
            _ => self.is_simple_ty(),
        }
    }
}

pub trait IsSuggestable<'tcx> {
    /// Whether this makes sense to suggest in a diagnostic.
    ///
    /// We filter out certain types and constants since they don't provide
    /// meaningful rendered suggestions when pretty-printed. We leave some
    /// nonsense, such as region vars, since those render as `'_` and are
    /// usually okay to reinterpret as elided lifetimes.
    fn is_suggestable(self, tcx: TyCtxt<'tcx>) -> bool;
}

impl<'tcx, T> IsSuggestable<'tcx> for T
where
    T: TypeFoldable<'tcx>,
{
    fn is_suggestable(self, tcx: TyCtxt<'tcx>) -> bool {
        self.visit_with(&mut IsSuggestableVisitor { tcx }).is_continue()
    }
}

pub fn suggest_arbitrary_trait_bound<'tcx>(
    tcx: TyCtxt<'tcx>,
    generics: &hir::Generics<'_>,
    err: &mut Diagnostic,
    trait_pred: PolyTraitPredicate<'tcx>,
) -> bool {
    if !trait_pred.is_suggestable(tcx) {
        return false;
    }

    let param_name = trait_pred.skip_binder().self_ty().to_string();
    let constraint = trait_pred.print_modifiers_and_trait_path().to_string();
    let param = generics.params.iter().find(|p| p.name.ident().as_str() == param_name);

    // Skip, there is a param named Self
    if param.is_some() && param_name == "Self" {
        return false;
    }

    // Suggest a where clause bound for a non-type parameter.
    err.span_suggestion_verbose(
        generics.tail_span_for_predicate_suggestion(),
        &format!(
            "consider {} `where` clause, but there might be an alternative better way to express \
             this requirement",
            if generics.where_clause_span.is_empty() { "introducing a" } else { "extending the" },
        ),
        format!("{} {}: {}", generics.add_where_or_trailing_comma(), param_name, constraint),
        Applicability::MaybeIncorrect,
    );
    true
}

#[derive(Debug)]
enum SuggestChangingConstraintsMessage<'a> {
    RestrictBoundFurther,
    RestrictType { ty: &'a str },
    RestrictTypeFurther { ty: &'a str },
    RemovingQSized,
}

fn suggest_removing_unsized_bound(
    tcx: TyCtxt<'_>,
    generics: &hir::Generics<'_>,
    suggestions: &mut Vec<(Span, String, SuggestChangingConstraintsMessage<'_>)>,
    param: &hir::GenericParam<'_>,
    def_id: Option<DefId>,
) {
    // See if there's a `?Sized` bound that can be removed to suggest that.
    // First look at the `where` clause because we can have `where T: ?Sized`,
    // then look at params.
    let param_def_id = tcx.hir().local_def_id(param.hir_id);
    for (where_pos, predicate) in generics.predicates.iter().enumerate() {
        let WherePredicate::BoundPredicate(predicate) = predicate else {
            continue;
        };
        if !predicate.is_param_bound(param_def_id.to_def_id()) {
            continue;
        };

        for (pos, bound) in predicate.bounds.iter().enumerate() {
            let    hir::GenericBound::Trait(poly, hir::TraitBoundModifier::Maybe) = bound else {
                continue;
            };
            if poly.trait_ref.trait_def_id() != def_id {
                continue;
            }
            let sp = generics.span_for_bound_removal(where_pos, pos);
            suggestions.push((
                sp,
                String::new(),
                SuggestChangingConstraintsMessage::RemovingQSized,
            ));
        }
    }
}

/// Suggest restricting a type param with a new bound.
pub fn suggest_constraining_type_param(
    tcx: TyCtxt<'_>,
    generics: &hir::Generics<'_>,
    err: &mut Diagnostic,
    param_name: &str,
    constraint: &str,
    def_id: Option<DefId>,
) -> bool {
    suggest_constraining_type_params(
        tcx,
        generics,
        err,
        [(param_name, constraint, def_id)].into_iter(),
    )
}

/// Suggest restricting a type param with a new bound.
pub fn suggest_constraining_type_params<'a>(
    tcx: TyCtxt<'_>,
    generics: &hir::Generics<'_>,
    err: &mut Diagnostic,
    param_names_and_constraints: impl Iterator<Item = (&'a str, &'a str, Option<DefId>)>,
) -> bool {
    let mut grouped = FxHashMap::default();
    param_names_and_constraints.for_each(|(param_name, constraint, def_id)| {
        grouped.entry(param_name).or_insert(Vec::new()).push((constraint, def_id))
    });

    let mut applicability = Applicability::MachineApplicable;
    let mut suggestions = Vec::new();

    for (param_name, mut constraints) in grouped {
        let param = generics.params.iter().find(|p| p.name.ident().as_str() == param_name);
        let Some(param) = param else { return false };

        {
            let mut sized_constraints =
                constraints.drain_filter(|(_, def_id)| *def_id == tcx.lang_items().sized_trait());
            if let Some((constraint, def_id)) = sized_constraints.next() {
                applicability = Applicability::MaybeIncorrect;

                err.span_label(
                    param.span,
                    &format!("this type parameter needs to be `{}`", constraint),
                );
                suggest_removing_unsized_bound(tcx, generics, &mut suggestions, param, def_id);
            }
        }

        if constraints.is_empty() {
            continue;
        }

        let mut constraint = constraints.iter().map(|&(c, _)| c).collect::<Vec<_>>();
        constraint.sort();
        constraint.dedup();
        let constraint = constraint.join(" + ");
        let mut suggest_restrict = |span, bound_list_non_empty| {
            suggestions.push((
                span,
                if bound_list_non_empty {
                    format!(" + {}", constraint)
                } else {
                    format!(" {}", constraint)
                },
                SuggestChangingConstraintsMessage::RestrictBoundFurther,
            ))
        };

        // When the type parameter has been provided bounds
        //
        //    Message:
        //      fn foo<T>(t: T) where T: Foo { ... }
        //                            ^^^^^^
        //                            |
        //                            help: consider further restricting this bound with `+ Bar`
        //
        //    Suggestion:
        //      fn foo<T>(t: T) where T: Foo { ... }
        //                                  ^
        //                                  |
        //                                  replace with: ` + Bar`
        //
        // Or, if user has provided some bounds, suggest restricting them:
        //
        //   fn foo<T: Foo>(t: T) { ... }
        //             ---
        //             |
        //             help: consider further restricting this bound with `+ Bar`
        //
        // Suggestion for tools in this case is:
        //
        //   fn foo<T: Foo>(t: T) { ... }
        //          --
        //          |
        //          replace with: `T: Bar +`
        let param_def_id = tcx.hir().local_def_id(param.hir_id);
        if let Some(span) = generics.bounds_span_for_suggestions(param_def_id) {
            suggest_restrict(span, true);
            continue;
        }

        if generics.has_where_clause_predicates {
            // This part is a bit tricky, because using the `where` clause user can
            // provide zero, one or many bounds for the same type parameter, so we
            // have following cases to consider:
            //
            // When the type parameter has been provided zero bounds
            //
            //    Message:
            //      fn foo<X, Y>(x: X, y: Y) where Y: Foo { ... }
            //             - help: consider restricting this type parameter with `where X: Bar`
            //
            //    Suggestion:
            //      fn foo<X, Y>(x: X, y: Y) where Y: Foo { ... }
            //                                           - insert: `, X: Bar`
            suggestions.push((
                generics.tail_span_for_predicate_suggestion(),
                constraints
                    .iter()
                    .map(|&(constraint, _)| format!(", {}: {}", param_name, constraint))
                    .collect::<String>(),
                SuggestChangingConstraintsMessage::RestrictTypeFurther { ty: param_name },
            ));
            continue;
        }

        // Additionally, there may be no `where` clause but the generic parameter has a default:
        //
        //    Message:
        //      trait Foo<T=()> {... }
        //                - help: consider further restricting this type parameter with `where T: Zar`
        //
        //    Suggestion:
        //      trait Foo<T=()> {... }
        //                     - insert: `where T: Zar`
        if matches!(param.kind, hir::GenericParamKind::Type { default: Some(_), .. }) {
            // Suggest a bound, but there is no existing `where` clause *and* the type param has a
            // default (`<T=Foo>`), so we suggest adding `where T: Bar`.
            suggestions.push((
                generics.tail_span_for_predicate_suggestion(),
                format!(" where {}: {}", param_name, constraint),
                SuggestChangingConstraintsMessage::RestrictTypeFurther { ty: param_name },
            ));
            continue;
        }

        // If user has provided a colon, don't suggest adding another:
        //
        //   fn foo<T:>(t: T) { ... }
        //            - insert: consider restricting this type parameter with `T: Foo`
        if let Some(colon_span) = param.colon_span {
            suggestions.push((
                colon_span.shrink_to_hi(),
                format!(" {}", constraint),
                SuggestChangingConstraintsMessage::RestrictType { ty: param_name },
            ));
            continue;
        }

        // If user hasn't provided any bounds, suggest adding a new one:
        //
        //   fn foo<T>(t: T) { ... }
        //          - help: consider restricting this type parameter with `T: Foo`
        suggestions.push((
            param.span.shrink_to_hi(),
            format!(": {}", constraint),
            SuggestChangingConstraintsMessage::RestrictType { ty: param_name },
        ));
    }

    if suggestions.len() == 1 {
        let (span, suggestion, msg) = suggestions.pop().unwrap();

        let s;
        let msg = match msg {
            SuggestChangingConstraintsMessage::RestrictBoundFurther => {
                "consider further restricting this bound"
            }
            SuggestChangingConstraintsMessage::RestrictType { ty } => {
                s = format!("consider restricting type parameter `{}`", ty);
                &s
            }
            SuggestChangingConstraintsMessage::RestrictTypeFurther { ty } => {
                s = format!("consider further restricting type parameter `{}`", ty);
                &s
            }
            SuggestChangingConstraintsMessage::RemovingQSized => {
                "consider removing the `?Sized` bound to make the type parameter `Sized`"
            }
        };

        err.span_suggestion_verbose(span, msg, suggestion, applicability);
    } else if suggestions.len() > 1 {
        err.multipart_suggestion_verbose(
            "consider restricting type parameters",
            suggestions.into_iter().map(|(span, suggestion, _)| (span, suggestion)).collect(),
            applicability,
        );
    }

    true
}

/// Collect al types that have an implicit `'static` obligation that we could suggest `'_` for.
pub struct TraitObjectVisitor<'tcx>(pub Vec<&'tcx hir::Ty<'tcx>>, pub crate::hir::map::Map<'tcx>);

impl<'v> hir::intravisit::Visitor<'v> for TraitObjectVisitor<'v> {
    fn visit_ty(&mut self, ty: &'v hir::Ty<'v>) {
        match ty.kind {
            hir::TyKind::TraitObject(
                _,
                hir::Lifetime {
                    name:
                        hir::LifetimeName::ImplicitObjectLifetimeDefault | hir::LifetimeName::Static,
                    ..
                },
                _,
            ) => {
                self.0.push(ty);
            }
            hir::TyKind::OpaqueDef(item_id, _) => {
                self.0.push(ty);
                let item = self.1.item(item_id);
                hir::intravisit::walk_item(self, item);
            }
            _ => {}
        }
        hir::intravisit::walk_ty(self, ty);
    }
}

/// Collect al types that have an implicit `'static` obligation that we could suggest `'_` for.
pub struct StaticLifetimeVisitor<'tcx>(pub Vec<Span>, pub crate::hir::map::Map<'tcx>);

impl<'v> hir::intravisit::Visitor<'v> for StaticLifetimeVisitor<'v> {
    fn visit_lifetime(&mut self, lt: &'v hir::Lifetime) {
        if let hir::LifetimeName::ImplicitObjectLifetimeDefault | hir::LifetimeName::Static =
            lt.name
        {
            self.0.push(lt.span);
        }
    }
}

pub struct IsSuggestableVisitor<'tcx> {
    tcx: TyCtxt<'tcx>,
}

impl<'tcx> TypeVisitor<'tcx> for IsSuggestableVisitor<'tcx> {
    type BreakTy = ();

    fn visit_ty(&mut self, t: Ty<'tcx>) -> ControlFlow<Self::BreakTy> {
        match t.kind() {
            FnDef(..)
            | Closure(..)
            | Infer(..)
            | Generator(..)
            | GeneratorWitness(..)
            | Bound(_, _)
            | Placeholder(_)
            | Error(_) => {
                return ControlFlow::Break(());
            }

            Opaque(did, _) => {
                let parent = self.tcx.parent(*did);
                if let hir::def::DefKind::TyAlias | hir::def::DefKind::AssocTy = self.tcx.def_kind(parent)
                    && let Opaque(parent_did, _) = self.tcx.type_of(parent).kind()
                    && parent_did == did
                {
                    // Okay
                } else {
                    return ControlFlow::Break(());
                }
            }

            Dynamic(dty, _) => {
                for pred in *dty {
                    match pred.skip_binder() {
                        ExistentialPredicate::Trait(_) | ExistentialPredicate::Projection(_) => {
                            // Okay
                        }
                        _ => return ControlFlow::Break(()),
                    }
                }
            }

            Param(param) => {
                // FIXME: It would be nice to make this not use string manipulation,
                // but it's pretty hard to do this, since `ty::ParamTy` is missing
                // sufficient info to determine if it is synthetic, and we don't
                // always have a convenient way of getting `ty::Generics` at the call
                // sites we invoke `IsSuggestable::is_suggestable`.
                if param.name.as_str().starts_with("impl ") {
                    return ControlFlow::Break(());
                }
            }

            _ => {}
        }

        t.super_visit_with(self)
    }

    fn visit_const(&mut self, c: Const<'tcx>) -> ControlFlow<Self::BreakTy> {
        match c.kind() {
            ConstKind::Infer(..)
            | ConstKind::Bound(..)
            | ConstKind::Placeholder(..)
            | ConstKind::Error(..) => {
                return ControlFlow::Break(());
            }
            _ => {}
        }

        c.super_visit_with(self)
    }
}
