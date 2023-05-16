use std::cmp::max;

use mir::syntax::{Place, ProjectionElem, Ty};
use rand_distr::WeightedIndex;

use crate::ptable::{PlaceIndex, PlacePath, PlaceTable, ToPlaceIndex};

#[derive(Clone, Copy, Default)]
enum PlaceUsage {
    #[default]
    Operand,
    LHS,
    Pointee,
    Argument,
    KnownVal,
}

#[derive(Clone, Default)]
pub struct PlaceSelector {
    tys: Vec<Ty>,
    exclusions: Vec<Place>,
    allow_uninit: bool,
    usage: PlaceUsage,
}

pub type Weight = usize;

const RET_LHS_WEIGH_FACTOR: Weight = 2;
const UNINIT_WEIGHT_FACTOR: Weight = 2;
const DEREF_WEIGHT_FACTOR: Weight = 2;
const LIT_ARG_WEIGHT_FACTOR: Weight = 2;
const PTR_WEIGHT_FACTOR: Weight = 10;

impl PlaceSelector {
    pub fn for_pointee() -> Self {
        Self {
            usage: PlaceUsage::Pointee,
            allow_uninit: true,
            ..Default::default()
        }
    }

    pub fn for_operand() -> Self {
        Self::default()
    }

    pub fn for_argument() -> Self {
        Self {
            usage: PlaceUsage::Argument,
            ..Default::default()
        }
    }

    pub fn for_lhs() -> Self {
        Self {
            usage: PlaceUsage::LHS,
            allow_uninit: true,
            ..Self::default()
        }
    }

    pub fn for_known_val() -> Self {
        Self {
            usage: PlaceUsage::KnownVal,
            ..Self::default()
        }
    }

    pub fn of_ty(self, ty: Ty) -> Self {
        let mut tys = self.tys;
        tys.push(ty);
        Self { tys, ..self }
    }

    pub fn of_tys(self, types: &[Ty]) -> Self {
        let mut tys = self.tys;
        tys.extend(types.iter().cloned());
        Self { tys, ..self }
    }

    pub fn except(self, exclude: &Place) -> Self {
        let mut exclusions = self.exclusions;
        // TODO: More granular place discrimination
        exclusions.push(exclude.clone());
        Self { exclusions, ..self }
    }

    fn into_iter_path(self, pt: &PlaceTable) -> impl Iterator<Item = PlacePath> + Clone + '_ {
        let exclusion_indicies: Vec<PlaceIndex> = self
            .exclusions
            .iter()
            .map(|place| place.to_place_index(pt).expect("excluded place exists"))
            .chain(pt.return_dest_stack())
            .collect();
        pt.reachable_nodes().filter(move |ppath| {
            let index = ppath.target_index(pt);
            let node = ppath.target_node(pt);

            let live = pt.is_place_live(index);

            let ty_allowed = if self.tys.is_empty() {
                true
            } else {
                self.tys.contains(&node.ty)
            };

            let not_excluded = !exclusion_indicies
                .iter()
                .any(|excl| pt.overlap(index, excl));
            let initness_allowed = if self.allow_uninit {
                true
            } else {
                pt.is_place_init(index)
            };

            let literalness = if matches!(self.usage, PlaceUsage::KnownVal) {
                node.val.is_some()
            } else {
                true
            };

            // FIXME: are we allowed to use moved-from places?
            live && ty_allowed && not_excluded && initness_allowed && literalness
        })
    }

    pub fn into_weighted(self, pt: &PlaceTable) -> Option<(Vec<PlacePath>, WeightedIndex<Weight>)> {
        let usage = self.usage;
        let (places, weights): (Vec<PlacePath>, Vec<Weight>) = self
            .into_iter_path(pt)
            .map(|ppath| {
                let place = ppath.target_index(pt);
                let mut weight = match usage {
                    PlaceUsage::Argument => {
                        let mut weight = pt.get_dataflow(place);
                        let node = ppath.target_node(pt);
                        if node.ty.contains(|ty| ty.is_any_ptr()) {
                            weight *= PTR_WEIGHT_FACTOR;
                        }
                        if node.val.is_some() {
                            weight *= LIT_ARG_WEIGHT_FACTOR;
                        }
                        weight
                    }
                    PlaceUsage::LHS => {
                        let mut weight = if !pt.is_place_init(place) {
                            UNINIT_WEIGHT_FACTOR
                        } else {
                            1
                        };
                        if ppath.is_return_proj(pt) {
                            weight *= RET_LHS_WEIGH_FACTOR;
                        }
                        // Avoid assigning to high dataflow places
                        weight = weight * 1000 / max(pt.get_dataflow(place), 1);
                        weight
                    }
                    PlaceUsage::Operand => pt.get_dataflow(place),
                    PlaceUsage::Pointee => 1,
                    PlaceUsage::KnownVal => pt.get_dataflow(place),
                };

                if ppath
                    .projections(pt)
                    .any(|proj| matches!(proj, ProjectionElem::Deref))
                {
                    weight *= DEREF_WEIGHT_FACTOR;
                }

                (ppath, weight)
            })
            .unzip();
        if let Some(weighted_index) = WeightedIndex::new(weights).ok() {
            Some((places, weighted_index))
        } else {
            None
        }
    }

    pub fn into_iter_place(self, pt: &PlaceTable) -> impl Iterator<Item = Place> + Clone + '_ {
        self.into_iter_path(pt).map(|ppath| ppath.to_place(pt))
    }
}

#[cfg(test)]
mod tests {
    extern crate test;
    use mir::syntax::{Local, Place};
    use rand::{
        rngs::SmallRng,
        seq::{IteratorRandom, SliceRandom},
        Rng, SeedableRng,
    };
    use test::Bencher;

    use crate::{ptable::PlaceTable, ty::TyCtxt};

    use super::PlaceSelector;

    fn build_pt(rng: &mut impl Rng) -> PlaceTable {
        let mut pt = PlaceTable::new();
        let tcx = TyCtxt::new(rng);
        for i in 0..=32 {
            let pidx = pt.allocate_local(Local::new(i), tcx.choose_ty(rng));
            if i % 2 == 0 {
                pt.mark_place_init(pidx);
            }
        }
        pt
    }

    #[bench]
    fn bench_select(b: &mut Bencher) {
        let mut rng = SmallRng::seed_from_u64(0);
        let pt = build_pt(&mut rng);

        b.iter(|| {
            PlaceSelector::for_lhs()
                .except(&Place::RETURN_SLOT)
                .into_iter_place(&pt)
                .choose(&mut rng)
                .expect("places not empty");
        })
    }

    #[bench]
    fn bench_materialise_into_vec(b: &mut Bencher) {
        let mut rng = SmallRng::seed_from_u64(0);
        let pt = build_pt(&mut rng);

        b.iter(|| {
            let places: Vec<Place> = PlaceSelector::for_lhs()
                .except(&Place::RETURN_SLOT)
                .into_iter_place(&pt)
                .collect();

            // places.choose(&mut rng).expect("not empty");
            places
                .choose_weighted(&mut rng, |p| p.projection().len())
                .expect("places not empty");
        })
    }
}
