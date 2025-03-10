//! This module contains some shared code for encoding and decoding various
//! things from the `ty` module, and in particular implements support for
//! "shorthands" which allow to have pointers back into the already encoded
//! stream instead of re-encoding the same thing twice.
//!
//! The functionality in here is shared between persisting to crate metadata and
//! persisting to incr. comp. caches.

use crate::arena::ArenaAllocatable;
use crate::infer::canonical::{CanonicalVarInfo, CanonicalVarInfos};
use crate::mir::{
    self,
    interpret::{AllocId, ConstAllocation},
};
use crate::thir;
use crate::traits;
use crate::ty::subst::SubstsRef;
use crate::ty::{self, AdtDef, Ty};
use rustc_data_structures::fx::FxHashMap;
use rustc_middle::ty::TyCtxt;
use rustc_serialize::{Decodable, Encodable};
use rustc_span::Span;
pub use rustc_type_ir::{TyDecoder, TyEncoder};
use std::hash::Hash;
use std::intrinsics;
use std::marker::DiscriminantKind;

/// The shorthand encoding uses an enum's variant index `usize`
/// and is offset by this value so it never matches a real variant.
/// This offset is also chosen so that the first byte is never < 0x80.
pub const SHORTHAND_OFFSET: usize = 0x80;

pub trait EncodableWithShorthand<E: TyEncoder>: Copy + Eq + Hash {
    type Variant: Encodable<E>;
    fn variant(&self) -> &Self::Variant;
}

#[allow(rustc::usage_of_ty_tykind)]
impl<'tcx, E: TyEncoder<I = TyCtxt<'tcx>>> EncodableWithShorthand<E> for Ty<'tcx> {
    type Variant = ty::TyKind<'tcx>;

    #[inline]
    fn variant(&self) -> &Self::Variant {
        self.kind()
    }
}

impl<'tcx, E: TyEncoder<I = TyCtxt<'tcx>>> EncodableWithShorthand<E> for ty::PredicateKind<'tcx> {
    type Variant = ty::PredicateKind<'tcx>;

    #[inline]
    fn variant(&self) -> &Self::Variant {
        self
    }
}

/// Trait for decoding to a reference.
///
/// This is a separate trait from `Decodable` so that we can implement it for
/// upstream types, such as `FxHashSet`.
///
/// The `TyDecodable` derive macro will use this trait for fields that are
/// references (and don't use a type alias to hide that).
///
/// `Decodable` can still be implemented in cases where `Decodable` is required
/// by a trait bound.
pub trait RefDecodable<'tcx, D: TyDecoder<I = TyCtxt<'tcx>>> {
    fn decode(d: &mut D) -> &'tcx Self;
}

/// Encode the given value or a previously cached shorthand.
pub fn encode_with_shorthand<'tcx, E, T, M>(encoder: &mut E, value: &T, cache: M)
where
    E: TyEncoder<I = TyCtxt<'tcx>>,
    M: for<'b> Fn(&'b mut E) -> &'b mut FxHashMap<T, usize>,
    T: EncodableWithShorthand<E>,
    // The discriminant and shorthand must have the same size.
    T::Variant: DiscriminantKind<Discriminant = isize>,
{
    let existing_shorthand = cache(encoder).get(value).copied();
    if let Some(shorthand) = existing_shorthand {
        encoder.emit_usize(shorthand);
        return;
    }

    let variant = value.variant();

    let start = encoder.position();
    variant.encode(encoder);
    let len = encoder.position() - start;

    // The shorthand encoding uses the same usize as the
    // discriminant, with an offset so they can't conflict.
    let discriminant = intrinsics::discriminant_value(variant);
    assert!(SHORTHAND_OFFSET > discriminant as usize);

    let shorthand = start + SHORTHAND_OFFSET;

    // Get the number of bits that leb128 could fit
    // in the same space as the fully encoded type.
    let leb128_bits = len * 7;

    // Check that the shorthand is a not longer than the
    // full encoding itself, i.e., it's an obvious win.
    if leb128_bits >= 64 || (shorthand as u64) < (1 << leb128_bits) {
        cache(encoder).insert(*value, shorthand);
    }
}

impl<'tcx, E: TyEncoder<I = TyCtxt<'tcx>>> Encodable<E> for Ty<'tcx> {
    fn encode(&self, e: &mut E) {
        encode_with_shorthand(e, self, TyEncoder::type_shorthands);
    }
}

impl<'tcx, E: TyEncoder<I = TyCtxt<'tcx>>> Encodable<E>
    for ty::Binder<'tcx, ty::PredicateKind<'tcx>>
{
    fn encode(&self, e: &mut E) {
        self.bound_vars().encode(e);
        encode_with_shorthand(e, &self.skip_binder(), TyEncoder::predicate_shorthands);
    }
}

impl<'tcx, E: TyEncoder<I = TyCtxt<'tcx>>> Encodable<E> for ty::Predicate<'tcx> {
    fn encode(&self, e: &mut E) {
        self.kind().encode(e);
    }
}

impl<'tcx, E: TyEncoder<I = TyCtxt<'tcx>>> Encodable<E> for ty::Region<'tcx> {
    fn encode(&self, e: &mut E) {
        self.kind().encode(e);
    }
}

impl<'tcx, E: TyEncoder<I = TyCtxt<'tcx>>> Encodable<E> for ty::Const<'tcx> {
    fn encode(&self, e: &mut E) {
        self.0.0.encode(e);
    }
}

impl<'tcx, E: TyEncoder<I = TyCtxt<'tcx>>> Encodable<E> for ConstAllocation<'tcx> {
    fn encode(&self, e: &mut E) {
        self.inner().encode(e)
    }
}

impl<'tcx, E: TyEncoder<I = TyCtxt<'tcx>>> Encodable<E> for AdtDef<'tcx> {
    fn encode(&self, e: &mut E) {
        self.0.0.encode(e)
    }
}

impl<'tcx, E: TyEncoder<I = TyCtxt<'tcx>>> Encodable<E> for AllocId {
    fn encode(&self, e: &mut E) {
        e.encode_alloc_id(self)
    }
}

#[inline]
fn decode_arena_allocable<
    'tcx,
    D: TyDecoder<I = TyCtxt<'tcx>>,
    T: ArenaAllocatable<'tcx> + Decodable<D>,
>(
    decoder: &mut D,
) -> &'tcx T
where
    D: TyDecoder,
{
    decoder.interner().arena.alloc(Decodable::decode(decoder))
}

#[inline]
fn decode_arena_allocable_slice<
    'tcx,
    D: TyDecoder<I = TyCtxt<'tcx>>,
    T: ArenaAllocatable<'tcx> + Decodable<D>,
>(
    decoder: &mut D,
) -> &'tcx [T]
where
    D: TyDecoder,
{
    decoder.interner().arena.alloc_from_iter(<Vec<T> as Decodable<D>>::decode(decoder))
}

impl<'tcx, D: TyDecoder<I = TyCtxt<'tcx>>> Decodable<D> for Ty<'tcx> {
    #[allow(rustc::usage_of_ty_tykind)]
    fn decode(decoder: &mut D) -> Ty<'tcx> {
        // Handle shorthands first, if we have a usize > 0x80.
        if decoder.positioned_at_shorthand() {
            let pos = decoder.read_usize();
            assert!(pos >= SHORTHAND_OFFSET);
            let shorthand = pos - SHORTHAND_OFFSET;

            decoder.cached_ty_for_shorthand(shorthand, |decoder| {
                decoder.with_position(shorthand, Ty::decode)
            })
        } else {
            let tcx = decoder.interner();
            tcx.mk_ty(rustc_type_ir::TyKind::decode(decoder))
        }
    }
}

impl<'tcx, D: TyDecoder<I = TyCtxt<'tcx>>> Decodable<D>
    for ty::Binder<'tcx, ty::PredicateKind<'tcx>>
{
    fn decode(decoder: &mut D) -> ty::Binder<'tcx, ty::PredicateKind<'tcx>> {
        let bound_vars = Decodable::decode(decoder);
        // Handle shorthands first, if we have a usize > 0x80.
        ty::Binder::bind_with_vars(
            if decoder.positioned_at_shorthand() {
                let pos = decoder.read_usize();
                assert!(pos >= SHORTHAND_OFFSET);
                let shorthand = pos - SHORTHAND_OFFSET;

                decoder.with_position(shorthand, ty::PredicateKind::decode)
            } else {
                ty::PredicateKind::decode(decoder)
            },
            bound_vars,
        )
    }
}

impl<'tcx, D: TyDecoder<I = TyCtxt<'tcx>>> Decodable<D> for ty::Predicate<'tcx> {
    fn decode(decoder: &mut D) -> ty::Predicate<'tcx> {
        let predicate_kind = Decodable::decode(decoder);
        decoder.interner().mk_predicate(predicate_kind)
    }
}

impl<'tcx, D: TyDecoder<I = TyCtxt<'tcx>>> Decodable<D> for SubstsRef<'tcx> {
    fn decode(decoder: &mut D) -> Self {
        let len = decoder.read_usize();
        let tcx = decoder.interner();
        tcx.mk_substs(
            (0..len).map::<ty::subst::GenericArg<'tcx>, _>(|_| Decodable::decode(decoder)),
        )
    }
}

impl<'tcx, D: TyDecoder<I = TyCtxt<'tcx>>> Decodable<D> for mir::Place<'tcx> {
    fn decode(decoder: &mut D) -> Self {
        let local: mir::Local = Decodable::decode(decoder);
        let len = decoder.read_usize();
        let projection = decoder.interner().mk_place_elems(
            (0..len).map::<mir::PlaceElem<'tcx>, _>(|_| Decodable::decode(decoder)),
        );
        mir::Place { local, projection }
    }
}

impl<'tcx, D: TyDecoder<I = TyCtxt<'tcx>>> Decodable<D> for ty::Region<'tcx> {
    fn decode(decoder: &mut D) -> Self {
        decoder.interner().mk_region(Decodable::decode(decoder))
    }
}

impl<'tcx, D: TyDecoder<I = TyCtxt<'tcx>>> Decodable<D> for CanonicalVarInfos<'tcx> {
    fn decode(decoder: &mut D) -> Self {
        let len = decoder.read_usize();
        let interned: Vec<CanonicalVarInfo<'tcx>> =
            (0..len).map(|_| Decodable::decode(decoder)).collect();
        decoder.interner().intern_canonical_var_infos(interned.as_slice())
    }
}

impl<'tcx, D: TyDecoder<I = TyCtxt<'tcx>>> Decodable<D> for AllocId {
    fn decode(decoder: &mut D) -> Self {
        decoder.decode_alloc_id()
    }
}

impl<'tcx, D: TyDecoder<I = TyCtxt<'tcx>>> Decodable<D> for ty::SymbolName<'tcx> {
    fn decode(decoder: &mut D) -> Self {
        ty::SymbolName::new(decoder.interner(), &decoder.read_str())
    }
}

macro_rules! impl_decodable_via_ref {
    ($($t:ty),+) => {
        $(impl<'tcx, D: TyDecoder<I = TyCtxt<'tcx>>> Decodable<D> for $t {
            fn decode(decoder: &mut D) -> Self {
                RefDecodable::decode(decoder)
            }
        })*
    }
}

impl<'tcx, D: TyDecoder<I = TyCtxt<'tcx>>> RefDecodable<'tcx, D> for ty::List<Ty<'tcx>> {
    fn decode(decoder: &mut D) -> &'tcx Self {
        let len = decoder.read_usize();
        decoder.interner().mk_type_list((0..len).map::<Ty<'tcx>, _>(|_| Decodable::decode(decoder)))
    }
}

impl<'tcx, D: TyDecoder<I = TyCtxt<'tcx>>> RefDecodable<'tcx, D>
    for ty::List<ty::Binder<'tcx, ty::ExistentialPredicate<'tcx>>>
{
    fn decode(decoder: &mut D) -> &'tcx Self {
        let len = decoder.read_usize();
        decoder.interner().mk_poly_existential_predicates(
            (0..len).map::<ty::Binder<'tcx, _>, _>(|_| Decodable::decode(decoder)),
        )
    }
}

impl<'tcx, D: TyDecoder<I = TyCtxt<'tcx>>> Decodable<D> for ty::Const<'tcx> {
    fn decode(decoder: &mut D) -> Self {
        decoder.interner().mk_const(Decodable::decode(decoder))
    }
}

impl<'tcx, D: TyDecoder<I = TyCtxt<'tcx>>> RefDecodable<'tcx, D> for [ty::ValTree<'tcx>] {
    fn decode(decoder: &mut D) -> &'tcx Self {
        decoder.interner().arena.alloc_from_iter(
            (0..decoder.read_usize()).map(|_| Decodable::decode(decoder)).collect::<Vec<_>>(),
        )
    }
}

impl<'tcx, D: TyDecoder<I = TyCtxt<'tcx>>> Decodable<D> for ConstAllocation<'tcx> {
    fn decode(decoder: &mut D) -> Self {
        decoder.interner().intern_const_alloc(Decodable::decode(decoder))
    }
}

impl<'tcx, D: TyDecoder<I = TyCtxt<'tcx>>> Decodable<D> for AdtDef<'tcx> {
    fn decode(decoder: &mut D) -> Self {
        decoder.interner().intern_adt_def(Decodable::decode(decoder))
    }
}

impl<'tcx, D: TyDecoder<I = TyCtxt<'tcx>>> RefDecodable<'tcx, D>
    for [(ty::Predicate<'tcx>, Span)]
{
    fn decode(decoder: &mut D) -> &'tcx Self {
        decoder.interner().arena.alloc_from_iter(
            (0..decoder.read_usize()).map(|_| Decodable::decode(decoder)).collect::<Vec<_>>(),
        )
    }
}

impl<'tcx, D: TyDecoder<I = TyCtxt<'tcx>>> RefDecodable<'tcx, D>
    for [thir::abstract_const::Node<'tcx>]
{
    fn decode(decoder: &mut D) -> &'tcx Self {
        decoder.interner().arena.alloc_from_iter(
            (0..decoder.read_usize()).map(|_| Decodable::decode(decoder)).collect::<Vec<_>>(),
        )
    }
}

impl<'tcx, D: TyDecoder<I = TyCtxt<'tcx>>> RefDecodable<'tcx, D>
    for [thir::abstract_const::NodeId]
{
    fn decode(decoder: &mut D) -> &'tcx Self {
        decoder.interner().arena.alloc_from_iter(
            (0..decoder.read_usize()).map(|_| Decodable::decode(decoder)).collect::<Vec<_>>(),
        )
    }
}

impl<'tcx, D: TyDecoder<I = TyCtxt<'tcx>>> RefDecodable<'tcx, D>
    for ty::List<ty::BoundVariableKind>
{
    fn decode(decoder: &mut D) -> &'tcx Self {
        let len = decoder.read_usize();
        decoder.interner().mk_bound_variable_kinds(
            (0..len).map::<ty::BoundVariableKind, _>(|_| Decodable::decode(decoder)),
        )
    }
}

impl_decodable_via_ref! {
    &'tcx ty::TypeckResults<'tcx>,
    &'tcx ty::List<Ty<'tcx>>,
    &'tcx ty::List<ty::Binder<'tcx, ty::ExistentialPredicate<'tcx>>>,
    &'tcx traits::ImplSource<'tcx, ()>,
    &'tcx mir::Body<'tcx>,
    &'tcx mir::UnsafetyCheckResult,
    &'tcx mir::BorrowCheckResult<'tcx>,
    &'tcx mir::coverage::CodeRegion,
    &'tcx ty::List<ty::BoundVariableKind>
}

#[macro_export]
macro_rules! __impl_decoder_methods {
    ($($name:ident -> $ty:ty;)*) => {
        $(
            #[inline]
            fn $name(&mut self) -> $ty {
                self.opaque.$name()
            }
        )*
    }
}

macro_rules! impl_arena_allocatable_decoder {
    ([]$args:tt) => {};
    ([decode $(, $attrs:ident)*]
     [$name:ident: $ty:ty]) => {
        impl<'tcx, D: TyDecoder<I = TyCtxt<'tcx>>> RefDecodable<'tcx, D> for $ty {
            #[inline]
            fn decode(decoder: &mut D) -> &'tcx Self {
                decode_arena_allocable(decoder)
            }
        }

        impl<'tcx, D: TyDecoder<I = TyCtxt<'tcx>>> RefDecodable<'tcx, D> for [$ty] {
            #[inline]
            fn decode(decoder: &mut D) -> &'tcx Self {
                decode_arena_allocable_slice(decoder)
            }
        }
    };
}

macro_rules! impl_arena_allocatable_decoders {
    ([$($a:tt $name:ident: $ty:ty,)*]) => {
        $(
            impl_arena_allocatable_decoder!($a [$name: $ty]);
        )*
    }
}

rustc_hir::arena_types!(impl_arena_allocatable_decoders);
arena_types!(impl_arena_allocatable_decoders);

macro_rules! impl_arena_copy_decoder {
    (<$tcx:tt> $($ty:ty,)*) => {
        $(impl<'tcx, D: TyDecoder<I = TyCtxt<'tcx>>> RefDecodable<'tcx, D> for $ty {
            #[inline]
            fn decode(decoder: &mut D) -> &'tcx Self {
                decoder.interner().arena.alloc(Decodable::decode(decoder))
            }
        }

        impl<'tcx, D: TyDecoder<I = TyCtxt<'tcx>>> RefDecodable<'tcx, D> for [$ty] {
            #[inline]
            fn decode(decoder: &mut D) -> &'tcx Self {
                decoder.interner().arena.alloc_from_iter(<Vec<_> as Decodable<D>>::decode(decoder))
            }
        })*
    };
}

impl_arena_copy_decoder! {<'tcx>
    Span,
    rustc_span::symbol::Ident,
    ty::Variance,
    rustc_span::def_id::DefId,
    rustc_span::def_id::LocalDefId,
    (rustc_middle::middle::exported_symbols::ExportedSymbol<'tcx>, rustc_middle::middle::exported_symbols::SymbolExportInfo),
}

#[macro_export]
macro_rules! implement_ty_decoder {
    ($DecoderName:ident <$($typaram:tt),*>) => {
        mod __ty_decoder_impl {
            use std::borrow::Cow;
            use rustc_serialize::Decoder;

            use super::$DecoderName;

            impl<$($typaram ),*> Decoder for $DecoderName<$($typaram),*> {
                $crate::__impl_decoder_methods! {
                    read_u128 -> u128;
                    read_u64 -> u64;
                    read_u32 -> u32;
                    read_u16 -> u16;
                    read_u8 -> u8;
                    read_usize -> usize;

                    read_i128 -> i128;
                    read_i64 -> i64;
                    read_i32 -> i32;
                    read_i16 -> i16;
                    read_i8 -> i8;
                    read_isize -> isize;

                    read_bool -> bool;
                    read_f64 -> f64;
                    read_f32 -> f32;
                    read_char -> char;
                    read_str -> &str;
                }

                #[inline]
                fn read_raw_bytes(&mut self, len: usize) -> &[u8] {
                    self.opaque.read_raw_bytes(len)
                }
            }
        }
    }
}

macro_rules! impl_binder_encode_decode {
    ($($t:ty),+ $(,)?) => {
        $(
            impl<'tcx, E: TyEncoder<I = TyCtxt<'tcx>>> Encodable<E> for ty::Binder<'tcx, $t> {
                fn encode(&self, e: &mut E) {
                    self.bound_vars().encode(e);
                    self.as_ref().skip_binder().encode(e);
                }
            }
            impl<'tcx, D: TyDecoder<I = TyCtxt<'tcx>>> Decodable<D> for ty::Binder<'tcx, $t> {
                fn decode(decoder: &mut D) -> Self {
                    let bound_vars = Decodable::decode(decoder);
                    ty::Binder::bind_with_vars(Decodable::decode(decoder), bound_vars)
                }
            }
        )*
    }
}

impl_binder_encode_decode! {
    &'tcx ty::List<Ty<'tcx>>,
    ty::FnSig<'tcx>,
    ty::ExistentialPredicate<'tcx>,
    ty::TraitRef<'tcx>,
    Vec<ty::GeneratorInteriorTypeCause<'tcx>>,
}
