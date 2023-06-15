use std::{
    collections::{BinaryHeap, HashMap},
    rc::Rc,
    vec,
};

use bimap::BiHashMap;
use mir::{
    syntax::{
        Body, FieldIdx, Literal, Local, Operand, Place, ProjectionElem, Rvalue, TyId, TyKind,
        UintTy,
    },
    tyctxt::TyCtxt,
};
use petgraph::{prelude::EdgeIndex, stable_graph::NodeIndex, visit::EdgeRef, Direction, Graph};
use smallvec::{smallvec, SmallVec};

use crate::mem::{AbstractByte, AllocId, AllocationBuilder, BasicMemory, RunPointer};

type PlaceGraph = Graph<PlaceNode, ProjectionElem>;
pub type PlaceIndex = NodeIndex;
pub type ProjectionIndex = EdgeIndex;
pub type Path = SmallVec<[ProjectionIndex; 4]>;

struct Frame {
    locals: BiHashMap<Local, PlaceIndex>,
    locals_ordered: BinaryHeap<PlaceIndex>,
    return_destination: PlaceIndex,
}

impl Frame {
    fn new(dest: PlaceIndex) -> Self {
        Self {
            locals: BiHashMap::new(),
            locals_ordered: BinaryHeap::new(),
            return_destination: dest,
        }
    }

    fn add_local(&mut self, local: Local, pidx: PlaceIndex) {
        self.locals
            .insert_no_overwrite(local, pidx)
            .expect("did not insert existing local or place");
        self.locals_ordered.push(pidx);
    }

    fn get_by_index(&self, pidx: PlaceIndex) -> Option<Local> {
        self.locals.get_by_right(&pidx).copied()
    }

    fn get_by_local(&self, local: Local) -> Option<PlaceIndex> {
        self.locals.get_by_left(&local).copied()
    }
}

pub struct PlaceTable {
    /// The callstack
    frames: Vec<Frame>,
    index_candidates: HashMap<usize, SmallVec<[Local; 1]>>,

    /// A program-global graph recording all places that can be reached through projections
    places: PlaceGraph,
    memory: BasicMemory,
    tcx: Rc<TyCtxt>,
}

#[derive(Debug, Clone)]
pub struct PlaceNode {
    pub ty: TyId,
    alloc_id: AllocId,
    dataflow: usize,
    moved: bool,

    // Only Tys fitting into a single Run have these
    run_ptr: Option<RunPointer>,

    // Remember the value of simple literals
    val: Option<Literal>,

    // Offsetted pointer value
    offset: Option<isize>,
}

pub trait ToPlaceIndex {
    fn to_place_index(&self, pt: &PlaceTable) -> Option<PlaceIndex>;
}

impl ToPlaceIndex for Place {
    fn to_place_index(&self, pt: &PlaceTable) -> Option<PlaceIndex> {
        pt.get_node(self)
    }
}

impl ToPlaceIndex for Local {
    fn to_place_index(&self, pt: &PlaceTable) -> Option<PlaceIndex> {
        pt.get_node(&Place::from_local(*self))
    }
}

impl ToPlaceIndex for PlaceIndex {
    fn to_place_index(&self, _: &PlaceTable) -> Option<PlaceIndex> {
        Some(*self)
    }
}

impl<T> ToPlaceIndex for &T
where
    T: ToPlaceIndex,
{
    fn to_place_index(&self, pt: &PlaceTable) -> Option<PlaceIndex> {
        (*self).to_place_index(pt)
    }
}

impl PlaceTable {
    pub fn new(tcx: Rc<TyCtxt>) -> Self {
        Self {
            frames: vec![Frame::new(/* fn0 dummy */ PlaceIndex::new(usize::MAX))],
            index_candidates: HashMap::new(),
            places: Graph::default(),
            memory: BasicMemory::new(),
            tcx,
        }
    }

    fn current_frame_mut(&mut self) -> &mut Frame {
        self.frames.last_mut().expect("call stack isn't empty")
    }

    fn current_frame(&self) -> &Frame {
        self.frames.last().expect("call stack isn't empty")
    }

    pub fn enter_fn0(&mut self, body: &Body) {
        // Declare return place
        self.allocate_local(Local::RET, body.return_ty());
        // Declare args
        body.args_decl_iter().for_each(|(local, decl)| {
            let pidx = self.allocate_local(local, decl.ty);
            // encourage use of args
            self.update_dataflow(pidx, 5);
        });
    }

    pub fn enter_fn(&mut self, body: &Body, args: &[Operand], return_dest: &Place) {
        // Get the PlaceIndices before frame switch
        enum ArgOperand {
            Copy(PlaceIndex),
            Move(PlaceIndex),
            Constant(Literal),
        }

        let args: Vec<ArgOperand> = args
            .iter()
            .map(|p| match p {
                Operand::Copy(place) => {
                    ArgOperand::Copy(place.to_place_index(self).expect("arg exists"))
                }
                Operand::Move(place) => {
                    let index = place.to_place_index(self).expect("arg exists");
                    // Only whole local can be moved
                    assert!(self.current_frame().get_by_index(index).is_some());
                    ArgOperand::Move(index)
                }
                Operand::Constant(lit) => ArgOperand::Constant(*lit),
            })
            .collect();

        let return_dest = return_dest
            .to_place_index(self)
            .expect("return dest exists");
        self.assign_literal(return_dest, None);

        // Frame switch
        self.frames.push(Frame::new(return_dest));
        self.index_candidates.clear();

        self.allocate_local(Local::RET, body.return_ty());
        body.args_decl_iter()
            .zip(args)
            .for_each(|((local, decl), arg)| {
                let pidx = self.allocate_local(local, decl.ty);

                match &arg {
                    ArgOperand::Copy(source_pidx) | ArgOperand::Move(source_pidx) => {
                        debug_assert!(
                            self.is_place_init(source_pidx),
                            "function arguments must be init"
                        );
                        self.copy_place(pidx, source_pidx);
                    }
                    ArgOperand::Constant(lit) => self.assign_literal(pidx, Some(*lit)),
                }
                if let ArgOperand::Move(source_pidx) = arg {
                    self.memory.deallocate(self.places[source_pidx].alloc_id);
                    self.mark_place_moved(source_pidx);
                }
            });
    }

    pub fn exit_fn(&mut self) {
        // FIXME: this is quite flimsy wrt. statement order
        let callee_ret = Place::RETURN_SLOT
            .to_place_index(self)
            .expect("place exists");
        // Frame switch
        let old_frame = self.frames.pop().expect("call stack isn't empty");
        self.index_candidates.clear(); // Invalidate cache

        // Copy ret
        self.copy_place(old_frame.return_destination, callee_ret);

        // Deinit places
        for pidx in old_frame.locals.right_values() {
            self.memory.deallocate(self.places[*pidx].alloc_id);
        }
    }

    pub fn allocate_local(&mut self, local: Local, ty: TyId) -> PlaceIndex {
        let mut pidx = Default::default();
        self.memory.allocate_with_builder(|builder| {
            pidx = Self::add_place(&mut self.places, ty, &self.tcx, builder, None);
        });
        self.current_frame_mut().add_local(local, pidx);
        pidx
    }

    pub fn deallocate_local(&mut self, local: Local) {
        // FIXME: should we need to remove local from the frame?
        let pidx = local.to_place_index(self).expect("place exists");
        self.memory.deallocate(self.places[pidx].alloc_id);
    }

    fn add_place(
        places: &mut PlaceGraph,
        ty: TyId,
        tcx: &TyCtxt,
        alloc_builder: &mut AllocationBuilder,
        run_ptr: Option<RunPointer>,
    ) -> PlaceIndex {
        let alloc_id = alloc_builder.alloc_id();
        let pidx = if run_ptr.is_some() {
            // If this is called recursively, and our parent (array) already allocated a run
            places.add_node(PlaceNode {
                ty,
                alloc_id,
                dataflow: 0,
                moved: false,
                run_ptr,
                val: None,
                offset: None,
            })
        } else if let Some(size) = BasicMemory::ty_size(ty, tcx) {
            let run_and_offset = alloc_builder.new_run(size);
            places.add_node(PlaceNode {
                ty,
                alloc_id,
                dataflow: 0,
                moved: false,
                run_ptr: Some(RunPointer {
                    alloc_id,
                    run_and_offset,
                    size,
                }),
                val: None,
                offset: None,
            })
        } else {
            places.add_node(PlaceNode {
                ty,
                alloc_id,
                dataflow: 0,
                moved: false,
                run_ptr: None,
                val: None,
                offset: None,
            })
        };
        match ty.kind(tcx) {
            TyKind::Tuple(elems) => elems.iter().enumerate().for_each(|(idx, elem)| {
                let sub_pidx = Self::add_place(places, *elem, tcx, alloc_builder, None);
                places.add_edge(
                    pidx,
                    sub_pidx,
                    ProjectionElem::TupleField(FieldIdx::new(idx)),
                );
            }),
            TyKind::Array(elem_ty, len) => {
                for i in 0..*len {
                    let child_run_ptr = if let Some(run_ptr) = places[pidx].run_ptr {
                        let child_size =
                            BasicMemory::ty_size(*elem_ty, tcx).expect("ty has fixed size");
                        Some(RunPointer {
                            alloc_id,
                            run_and_offset: run_ptr
                                .run_and_offset
                                .offset(i as isize * child_size.bytes() as isize),
                            size: child_size,
                        })
                    } else {
                        None
                    };
                    let elem_pidx =
                        Self::add_place(places, *elem_ty, tcx, alloc_builder, child_run_ptr);
                    places.add_edge(
                        pidx,
                        elem_pidx,
                        ProjectionElem::ConstantIndex { offset: i as u64 },
                    );
                }
            }
            TyKind::Adt(adt) if adt.is_enum() => todo!(),
            TyKind::Adt(adt) => {
                let fields = &adt.variants.first().expect("adt is a struct").fields;
                for (fid, ty) in fields.iter_enumerated() {
                    let field_pidx = Self::add_place(places, *ty, tcx, alloc_builder, None);
                    places.add_edge(pidx, field_pidx, ProjectionElem::Field(fid));
                }
            }
            TyKind::RawPtr(..) => { /* pointer has no subfields  */ }
            _ => { /* primitives, no projection */ }
        }
        pidx
    }

    pub fn copy_place(&mut self, dst: impl ToPlaceIndex, src: impl ToPlaceIndex) {
        let dst = dst.to_place_index(self).expect("place exists");
        let src = src.to_place_index(self).expect("place exists");
        if dst == src {
            return;
        }
        self.update_dataflow(dst, self.places[src].dataflow);
        self.assign_literal(dst, self.places[src].val);

        let (dst_node, src_node) = self.places.index_twice_mut(dst, src);
        assert_eq!(dst_node.ty, src_node.ty);

        if let Some(run_ptr) = src_node.run_ptr {
            self.memory
                .copy(dst_node.run_ptr.expect("dst is terminal"), run_ptr);
        }

        if dst_node.ty.is_any_ptr(&self.tcx) {
            if let Some(pointee) = self.pointee(src) {
                self.set_ref(dst, pointee);
            }
            self.places[dst].offset = self.places[src].offset;
        }
        let projs: Vec<_> = self
            .places
            .edges_directed(dst, Direction::Outgoing)
            .filter_map(|e| (!e.weight().is_deref()).then_some(e.weight()))
            .copied()
            .collect();
        for proj in projs {
            let new_dst = self
                .project_from_node(dst, proj)
                .expect("projection exists");
            let new_src = self
                .project_from_node(src, proj)
                .expect("projection exists");
            self.copy_place(new_dst, new_src);
        }
    }

    pub fn project_from_node(
        &self,
        pidx: PlaceIndex,
        mut proj: ProjectionElem,
    ) -> Option<PlaceIndex> {
        if let ProjectionElem::Index(local) = proj {
            let Some(Literal::Uint(i, UintTy::Usize)) = self.known_val(local) else {
                panic!("projection has a usize knownval");
            };
            proj = ProjectionElem::ConstantIndex { offset: *i as u64 };
        }
        self.places
            .edges_directed(pidx, Direction::Outgoing)
            .find(|edge| edge.weight() == &proj)
            .map(|e| e.target())
    }

    /// Get PlaceIndex from a Place
    fn get_node(&self, place: &Place) -> Option<PlaceIndex> {
        let mut node = self.current_frame().get_by_local(place.local())?;
        let proj_iter = place.projection().iter();
        for proj in proj_iter {
            let next = self.project_from_node(node, *proj);
            if let Some(next) = next {
                node = next;
            } else {
                return None;
            }
        }
        Some(node)
    }

    /// Call update on all transitive superfields of start, *excluding* start
    fn update_transitive_superfields<F>(&mut self, start: PlaceIndex, mut visit: F)
    where
        F: FnMut(&mut Self, PlaceIndex) -> bool,
    {
        let mut to_visit: Vec<NodeIndex> = self.immediate_superfields(start).collect();
        while let Some(node) = to_visit.pop() {
            let cont = visit(self, node);
            if cont {
                to_visit.extend(self.immediate_superfields(node));
            }
        }
    }

    /// Call visit on all transitive subfields of start, *including* start
    fn update_transitive_subfields<F>(&mut self, start: PlaceIndex, mut visit: F)
    where
        F: FnMut(&mut Self, PlaceIndex) -> bool,
    {
        let mut to_visit = vec![start];
        while let Some(node) = to_visit.pop() {
            let cont = visit(self, node);
            if cont {
                to_visit.extend(self.immediate_subfields(node));
            }
        }
    }

    fn visit_transitive_subfields<F>(&self, start: PlaceIndex, mut visit: F)
    where
        F: FnMut(PlaceIndex) -> bool,
    {
        let mut to_visit = vec![start];
        while let Some(node) = to_visit.pop() {
            let cont = visit(node);
            if cont {
                to_visit.extend(self.immediate_subfields(node));
            }
        }
    }

    pub fn update_dataflow(&mut self, target: impl ToPlaceIndex, new_flow: usize) {
        let target = target.to_place_index(self).expect("place exists");
        let new_flow = new_flow.min(100);

        // Subplaces' complexity is overwritten as target's new complexity
        self.update_transitive_subfields(target, |this, place| {
            this.places[place].dataflow = new_flow;
            true
        });

        // Superplaces' complexity is updated to be the max of its children
        self.update_transitive_superfields(target, |this, place| {
            if let Some(max) = this
                .immediate_subfields(place)
                .map(|sub| this.places[sub].dataflow)
                .max()
            {
                this.places[place].dataflow = max;
            }
            true
        })
    }

    pub fn get_dataflow(&self, p: impl ToPlaceIndex) -> usize {
        let pidx = p.to_place_index(self).unwrap();
        let node = &self.places[pidx];
        if node.ty.is_any_ptr(&self.tcx) {
            if let Some(pointee) = self.project_from_node(pidx, ProjectionElem::Deref) {
                self.get_dataflow(pointee)
            } else {
                // Use the initial dataflow
                node.dataflow
            }
        } else {
            node.dataflow
        }
    }

    pub fn is_place_moved(&self, p: impl ToPlaceIndex) -> bool {
        let pidx = p.to_place_index(self).expect("place exists");
        self.places[pidx].moved
    }

    pub fn mark_place_moved(&mut self, p: impl ToPlaceIndex) {
        let pidx = p.to_place_index(self).expect("place exists");

        self.update_transitive_subfields(pidx, |this, place| {
            this.places[place].moved = true;
            true
        });

        self.update_transitive_superfields(pidx, |this, place| {
            let all_moved = this
                .immediate_subfields(place)
                .all(|p| this.places[p].moved);
            this.places[place].moved = all_moved;
            true
        });
    }

    pub fn mark_place_uninit(&mut self, p: impl ToPlaceIndex) {
        let pidx = p.to_place_index(self).unwrap();

        // If this is a pointer, we have to remove the Deref edge, but not for other projections
        if self.places[pidx].ty.is_any_ptr(&self.tcx) && let Some(old) = self.ref_edge(pidx) {
            self.places.remove_edge(old);
        }

        self.update_transitive_subfields(pidx, |this, place| {
            let node = &this.places[place];
            if let Some(run_ptr) = node.run_ptr {
                this.memory
                    .bytes_mut(run_ptr)
                    .iter_mut()
                    .for_each(|b| *b = AbstractByte::Uninit);
                false
            } else {
                true
            }
        });
    }

    pub fn mark_place_init(&mut self, p: impl ToPlaceIndex) {
        let pidx = p.to_place_index(self).unwrap();
        self.update_transitive_subfields(pidx, |this, place| {
            let node = &this.places[place];
            if let Some(run_ptr) = node.run_ptr {
                this.memory
                    .bytes_mut(run_ptr)
                    .iter_mut()
                    .for_each(|b| *b = AbstractByte::Init(None));
                false
            } else {
                true
            }
        });
    }

    /// Returns the pointee in reference -[Deref]-> pointee, if one exists
    fn pointee(&self, reference: PlaceIndex) -> Option<PlaceIndex> {
        assert!(self.places[reference].ty.is_any_ptr(&self.tcx));
        self.places
            .edges_directed(reference, Direction::Outgoing)
            .next()
            .map(|deref| deref.target())
    }

    /// Returns the edge in reference -[edge: Deref]-> pointee, if one exists
    fn ref_edge(&self, reference: PlaceIndex) -> Option<ProjectionIndex> {
        assert!(self.places[reference].ty.is_any_ptr(&self.tcx));
        self.places
            .edges_directed(reference, Direction::Outgoing)
            .next()
            .map(|deref| deref.id())
    }

    /// Creates an edge reference -[Deref]-> pointee
    pub fn set_ref(&mut self, reference: impl ToPlaceIndex, pointee: impl ToPlaceIndex) {
        let reference = reference.to_place_index(self).expect("place exists");
        let pointee = pointee.to_place_index(self).expect("place exists");

        assert_eq!(
            self.places[reference].ty.pointee_ty(&self.tcx).unwrap(),
            self.places[pointee].ty
        );

        // Remove any old reference edges
        if let Some(old) = self.ref_edge(reference) {
            self.places.remove_edge(old);
        }
        assert_eq!(
            self.places
                .edges_directed(reference, Direction::Outgoing)
                .count(),
            0
        );
        self.places[reference].offset = None;

        self.update_dataflow(reference, self.places[pointee].dataflow);

        // Add new reference
        self.places
            .add_edge(reference, pointee, ProjectionElem::Deref);
    }

    pub fn is_place_live(&self, p: impl ToPlaceIndex) -> bool {
        let Some(pidx) = p.to_place_index(self) else {
            return false;
        };
        let node = &self.places[pidx];
        self.memory.is_live(node.alloc_id)
    }

    pub fn is_place_init(&self, p: impl ToPlaceIndex) -> bool {
        if !self.is_place_live(&p) {
            return false;
        }
        let pidx = p.to_place_index(self).unwrap();
        let node = &self.places[pidx];
        if let Some(run_ptr) = node.run_ptr {
            self.memory.bytes(run_ptr).iter().all(|b| b.is_init())
        } else {
            self.immediate_subfields(pidx)
                .all(|sub| self.is_place_init(sub))
        }
    }

    fn immediate_subfields(&self, pidx: PlaceIndex) -> impl Iterator<Item = PlaceIndex> + '_ {
        self.places
            .edges_directed(pidx, Direction::Outgoing)
            .filter_map(|e| (!e.weight().is_deref()).then_some(e.target()))
    }

    fn immediate_superfields(&self, pidx: PlaceIndex) -> impl Iterator<Item = PlaceIndex> + '_ {
        self.places
            .edges_directed(pidx, Direction::Incoming)
            .filter_map(|e| (!e.weight().is_deref()).then_some(e.source()))
    }

    // Returns an iterator over all places reachable from node through projections
    fn reachable_from_node(&self, pidx: PlaceIndex) -> ProjectionIter<'_> {
        ProjectionIter::new(self, pidx)
    }

    pub fn reachable_nodes(&self) -> impl Iterator<Item = PlacePath> + Clone + '_ {
        let local_iter = self.current_frame().locals_ordered.iter();
        local_iter.flat_map(|&pidx| self.reachable_from_node(pidx))
    }

    /// Whether two places overlap or alias
    pub fn overlap(&self, a: impl ToPlaceIndex, b: impl ToPlaceIndex) -> bool {
        let a = a.to_place_index(self).expect("place exists");
        let b = b.to_place_index(self).expect("place exists");

        if a == b {
            return true;
        }

        if self.places[a].alloc_id != self.places[b].alloc_id {
            return false;
        }

        let mut a_sub: Vec<PlaceIndex> = vec![];
        self.visit_transitive_subfields(a, |sub| {
            a_sub.push(sub);
            true
        });

        let mut b_sub: Vec<PlaceIndex> = vec![];
        self.visit_transitive_subfields(b, |sub| {
            b_sub.push(sub);
            true
        });

        // TODO: should I use a hashmap here?
        for a_node in a_sub {
            for b_node in &b_sub {
                if a_node == *b_node {
                    return true;
                }
            }
        }
        false
    }

    pub fn assign_literal(&mut self, p: impl ToPlaceIndex, val: Option<Literal>) {
        let p = p.to_place_index(self).expect("place exists");
        if let Some(local) = self.current_frame().get_by_index(p) {
            // If place is a local
            if let Some(&Literal::Uint(i, UintTy::Usize)) = self.known_val(p) &&
                    let Some(old) = self.index_candidates.get_mut(&(i as usize)) &&
                    let Some(to_remove) = old.iter().position(|&l| l == local) {
                // unconditionally remove the old entry if it exists
                old.remove(to_remove);
            }
            if let Some(Literal::Uint(i, UintTy::Usize)) = val {
                // If we're assigning a new usize literal to it
                self.index_candidates
                    .entry(i as usize)
                    .or_default()
                    .push(local)
            }
        }

        if let Some(val) = val {
            self.places[p].val = Some(val);
        } else {
            self.update_transitive_subfields(p, |this, node| {
                this.places[node].val = None;
                true
            });
            self.update_transitive_superfields(p, |this, node| {
                this.places[node].val = None;
                true
            });
        }
    }

    pub fn return_dest_stack(&self) -> impl Iterator<Item = PlaceIndex> + '_ {
        self.frames.iter().skip(1).map(|f| f.return_destination)
    }

    pub fn ty(&self, p: impl ToPlaceIndex) -> TyId {
        self.places[p.to_place_index(self).expect("place exists")].ty
    }

    pub fn known_val(&self, p: impl ToPlaceIndex) -> Option<&Literal> {
        self.places[p.to_place_index(self).expect("place exists")]
            .val
            .as_ref()
    }

    // Whether the pointer has been offsetted (and therefore unusable)
    pub fn offseted(&self, p: impl ToPlaceIndex) -> bool {
        let p = p.to_place_index(self).expect("place exists");
        assert!(self.places[p].ty.is_any_ptr(&self.tcx));

        match self.places[p].offset {
            None => false,
            Some(0) => false,
            _ => true,
        }
    }

    pub fn get_offset(&self, p: impl ToPlaceIndex) -> Option<isize> {
        let p = p.to_place_index(self).expect("place exists");
        assert!(self.places[p].ty.is_any_ptr(&self.tcx));

        self.places[p].offset
    }

    pub fn offset_ptr(&mut self, p: impl ToPlaceIndex, offset: isize) {
        let p = p.to_place_index(self).expect("place exists");
        assert!(self.places[p].ty.is_any_ptr(&self.tcx));

        self.places[p].offset = match self.places[p].offset {
            None => Some(offset),
            Some(o) => Some(offset.wrapping_add(o)),
        };
    }

    pub fn has_offset_roundtripped(&self, p: impl ToPlaceIndex) -> bool {
        let p = p.to_place_index(self).expect("place exists");
        assert!(self.places[p].ty.is_any_ptr(&self.tcx));

        self.places[p].offset == Some(0)
    }

    fn locals_with_val(&self, val: usize) -> Vec<Local> {
        if let Some(locals) = self.index_candidates.get(&val) {
            locals.iter().copied().filter(|local|
                if self.is_place_init(local) && !self.is_place_moved(local) && let Some(Literal::Uint(v, UintTy::Usize)) = self.known_val(local) && *v as usize == val {
                    true
                } else {
                    false
                }
            ).collect()
        } else {
            vec![]
        }
    }

    pub fn place_count(&self) -> usize {
        self.places.node_count()
    }
}

#[derive(Debug, Clone)]
pub struct PlacePath {
    source: NodeIndex,
    path: Path,
}

impl PlacePath {
    pub fn to_place(&self, pt: &PlaceTable) -> Place {
        let projs: SmallVec<[ProjectionElem; 8]> = self
            .path
            .iter()
            .map(|&proj| {
                let mut proj = pt.places[proj];
                if let ProjectionElem::ConstantIndex { offset } = proj {
                    let local = pt.locals_with_val(offset as usize)[0]; // TODO: randomise this?
                    proj = ProjectionElem::Index(local);
                }
                proj
            })
            .collect();
        Place::from_projected(
            pt.current_frame().get_by_index(self.source).unwrap(),
            &projs,
        )
    }

    pub fn projections<'pt>(
        &'pt self,
        pt: &'pt PlaceTable,
    ) -> impl Iterator<Item = ProjectionElem> + 'pt {
        self.path.iter().map(|e| pt.places[*e])
    }

    pub fn nodes<'pt>(&'pt self, pt: &'pt PlaceTable) -> impl Iterator<Item = PlaceIndex> + 'pt {
        [self.source].into_iter().chain(
            self.path
                .iter()
                .map(|e| pt.places.edge_endpoints(*e).expect("edge exists").1),
        )
    }

    pub fn is_return_proj(&self, pt: &PlaceTable) -> bool {
        pt.current_frame()
            .get_by_index(self.source)
            .expect("source exists")
            == Local::RET
    }

    pub fn target_index(&self, pt: &PlaceTable) -> PlaceIndex {
        if let Some(last_edge) = self.path.last() {
            pt.places
                .edge_endpoints(*last_edge)
                .expect("edge in graph")
                .1
        } else {
            self.source
        }
    }

    pub fn target_node<'pt>(&self, pt: &'pt PlaceTable) -> &'pt PlaceNode {
        let target_idx = self.target_index(pt);
        &pt.places[target_idx]
    }

    pub fn is_local(&self) -> bool {
        self.path.is_empty()
    }
}

/// A depth-first iterator over all reachable projections from a local variable
/// FIXME: this breaks if there's a reference cycle in the graph
#[derive(Clone)]
pub struct ProjectionIter<'pt> {
    pt: &'pt PlaceTable,
    root: PlaceIndex,
    path: Path,
    // Stack of nodes to visit and their depth (number of projections from root)
    to_visit: Vec<(ProjectionIndex, usize)>,

    root_visited: bool,
}

impl<'pt> ProjectionIter<'pt> {
    fn new(pt: &'pt PlaceTable, root: PlaceIndex) -> Self {
        ProjectionIter {
            pt,
            root,
            path: smallvec![],
            to_visit: pt
                .places
                .edges_directed(root, Direction::Outgoing)
                .filter_map(|e| {
                    // Only do indexing if we can find a local as index 
                    // TODO: move this to a function
                    if let ProjectionElem::ConstantIndex { offset } = e.weight() && pt.locals_with_val(*offset as usize).is_empty() {
                        return None;
                    }

                    if e.weight().is_deref() && pt.offseted(e.source()) {
                        return None;
                    }

                    Some((e.id(), 1))
                })
                .collect(),
            root_visited: false,
        }
    }
}

impl<'pt> Iterator for ProjectionIter<'pt> {
    type Item = PlacePath;
    fn next(&mut self) -> Option<Self::Item> {
        if !self.root_visited {
            self.root_visited = true;
            return Some(PlacePath {
                source: self.root,
                path: smallvec![],
            });
        }
        if let Some((edge, depth)) = self.to_visit.pop() {
            let (_, target) = self.pt.places.edge_endpoints(edge).unwrap();
            self.path.truncate(depth - 1);
            self.path.push(edge);

            let new_edges = self.pt.places.edges_directed(target, Direction::Outgoing);
            self.to_visit.extend(new_edges.filter_map(|e| {
                // Do not follow deref edges since we are not root
                if e.weight().is_deref() {
                    return None;
                }

                if let ProjectionElem::ConstantIndex { offset } = e.weight() && self.pt.locals_with_val(*offset as usize).is_empty() {
                    return None;
                }
                Some((e.id(), depth+1))
            }));

            Some(PlacePath {
                source: self.root,
                path: self.path.clone(),
            })
        } else {
            None
        }
    }
}

pub trait HasDataflow {
    fn dataflow(&self, pt: &PlaceTable) -> usize;
}

impl HasDataflow for Place {
    fn dataflow(&self, pt: &PlaceTable) -> usize {
        pt.places[self.to_place_index(pt).expect("place exists")].dataflow
    }
}

impl HasDataflow for Operand {
    fn dataflow(&self, pt: &PlaceTable) -> usize {
        match self {
            Operand::Copy(place) | Operand::Move(place) => place.dataflow(pt),
            Operand::Constant(_) => 1,
        }
    }
}

impl HasDataflow for Rvalue {
    fn dataflow(&self, pt: &PlaceTable) -> usize {
        match self {
            Rvalue::Use(operand) | Rvalue::Cast(operand, _) | Rvalue::UnaryOp(_, operand) => {
                operand.dataflow(pt)
            }
            Rvalue::BinaryOp(_, l, r) | Rvalue::CheckedBinaryOp(_, l, r) => {
                l.dataflow(pt) + r.dataflow(pt)
            }
            Rvalue::Aggregate(_, elems) => elems.iter().map(|op| op.dataflow(pt)).sum(),
            Rvalue::Len(_) => 1,
            Rvalue::Discriminant(place) => place.dataflow(pt),
            Rvalue::AddressOf(_, place) => place.dataflow(pt),
        }
    }
}

impl<T> HasDataflow for &T
where
    T: HasDataflow,
{
    fn dataflow(&self, pt: &PlaceTable) -> usize {
        (*self).dataflow(pt)
    }
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;

    use mir::{
        syntax::{
            BinOp, FieldIdx, Literal, Local, Mutability, Operand, Place, ProjectionElem, Rvalue,
            TyId, TyKind, UintTy,
        },
        tyctxt::TyCtxt,
    };

    use crate::{
        mem::BasicMemory,
        ptable::{HasDataflow, PlaceIndex, ToPlaceIndex},
    };

    use super::PlaceTable;

    fn prepare_t() -> (PlaceTable, Local, Place, Place, Place, Place, Place) {
        /*
            ┌──────┬──────┐
            │      │      │
            a      │      c
                ┌──b──┐
                │     │
                d     e
        */

        let mut tcx = TyCtxt::from_primitives();
        let t_i16_i32 = tcx.push(TyKind::Tuple(vec![TyCtxt::I16, TyCtxt::I32]));
        let t_root = tcx.push(TyKind::Tuple(vec![TyCtxt::I8, t_i16_i32, TyCtxt::I64]));

        let mut pt = PlaceTable::new(Rc::new(tcx));
        let local = Local::new(1);
        pt.allocate_local(local, t_root);

        let a = Place::from_projected(local, &[ProjectionElem::TupleField(FieldIdx::new(0))]);
        let b = Place::from_projected(local, &[ProjectionElem::TupleField(FieldIdx::new(1))]);
        let c = Place::from_projected(local, &[ProjectionElem::TupleField(FieldIdx::new(2))]);

        let d = b
            .clone()
            .project(ProjectionElem::TupleField(FieldIdx::new(0)))
            .clone();
        let e = b
            .clone()
            .project(ProjectionElem::TupleField(FieldIdx::new(1)))
            .clone();
        (pt, local, a, b, c, d, e)
    }

    #[test]
    fn nested_tuple() {
        let (pt, local, _, b, ..) = prepare_t();

        let visited: Vec<TyId> = pt
            .reachable_from_node(local.to_place_index(&pt).unwrap())
            .map(|ppath| ppath.target_node(&pt).ty)
            .collect();
        let root = pt.get_node(&Place::from_local(local)).unwrap();
        let root_ty = pt.places[root].ty;
        let b = pt.get_node(&b).unwrap();
        let b_ty = pt.places[b].ty;
        assert_eq!(
            &visited,
            &[
                root_ty,     // (i8, (i16, i32), i64)
                TyCtxt::I8,  // i8
                b_ty,        // (i16, 132)
                TyCtxt::I16, // i16
                TyCtxt::I32, // i32
                TyCtxt::I64, // i64
            ]
        );
    }

    #[test]
    fn overlap_check() {
        let (pt, local, a, b, c, d, _) = prepare_t();

        assert!(pt.overlap(&b, local));
        assert!(pt.overlap(local, &b));
        assert!(pt.overlap(local, &d));

        assert!(!pt.overlap(&a, &c));
        assert!(!pt.overlap(&a, &d))
    }

    #[test]
    fn pointers() {
        let mut tcx = TyCtxt::from_primitives();
        // *const i32
        let ptr = tcx.push(TyKind::RawPtr(TyCtxt::I32, Mutability::Not));
        // (*const i32,)
        let inner_ty = tcx.push(TyKind::Tuple(vec![ptr]));
        // *const (*const i32,)
        let ty = tcx.push(TyKind::RawPtr(inner_ty, Mutability::Not));

        let mut pt = PlaceTable::new(Rc::new(tcx));
        let root = Local::new(1);
        pt.allocate_local(root, ty);

        let tuple = Local::new(2);
        pt.allocate_local(tuple, inner_ty);

        let int = Local::new(3);
        pt.allocate_local(int, TyCtxt::I32);

        // root -[Deref]-> tuple -[Field(0)]-> tuple.0 -[Deref]-> int
        let tuple_0 = Place::from_projected(
            tuple,
            &[ProjectionElem::TupleField(FieldIdx::from_usize(0))],
        );
        pt.set_ref(tuple_0.clone(), int);

        pt.set_ref(root, tuple);

        let visited: Vec<PlaceIndex> = pt
            .reachable_from_node(root.to_place_index(&pt).unwrap())
            .map(|ppath| ppath.target_index(&pt))
            .collect();

        // int is not reachable because it is behind a Field projection
        assert_eq!(
            &visited,
            &[
                root.to_place_index(&pt).unwrap(),
                tuple.to_place_index(&pt).unwrap(),
                tuple_0.to_place_index(&pt).unwrap(),
            ]
        );
    }

    #[test]
    fn tuple_projection() {
        let mut tcx = TyCtxt::from_primitives();
        let ty = tcx.push(TyKind::Tuple(vec![TyCtxt::I8, TyCtxt::I32]));

        let mut pt = PlaceTable::new(Rc::new(tcx));
        let local = Local::new(1);
        pt.allocate_local(local, ty);

        let place = pt
            .reachable_from_node(local.to_place_index(&pt).unwrap())
            .filter(|ppath| !ppath.path.is_empty())
            .map(|ppath| ppath.to_place(&pt))
            .next()
            .unwrap();

        assert!(matches!(
            place.projection()[0],
            ProjectionElem::TupleField(..)
        ));
    }

    #[test]
    fn recursive_init() {
        let (mut pt, local, a, b, c, d, e) = prepare_t();

        pt.mark_place_init(&a);
        assert!(pt.is_place_init(&a));
        assert!(!pt.is_place_init(local));

        pt.mark_place_init(&b);
        assert!(pt.is_place_init(&d));
        assert!(pt.is_place_init(&e));
        assert!(!pt.is_place_init(local));

        pt.mark_place_init(&c);
        assert!(pt.is_place_init(local));
    }

    #[test]
    fn recursive_uninit() {
        let (mut pt, local, a, b, c, d, e) = prepare_t();
        pt.mark_place_init(local);

        pt.mark_place_uninit(&d);
        assert!(!pt.is_place_init(&d));

        pt.mark_place_uninit(&e);
        assert!(!pt.is_place_init(&b));

        pt.mark_place_uninit(local);
        assert!(!pt.is_place_init(&a));
        assert!(!pt.is_place_init(&c));
    }

    #[test]
    fn dataflow() {
        let (mut pt, local, a, b, c, d, e) = prepare_t();

        pt.update_dataflow(&a, Rvalue::Use(Operand::Constant(1.into())).dataflow(&pt));
        assert_eq!(a.dataflow(&pt), 1);
        assert_eq!(Place::from(local).dataflow(&pt), 1);
        pt.update_dataflow(&c, Rvalue::Use(Operand::Constant(1.into())).dataflow(&pt));
        assert_eq!(c.dataflow(&pt), 1);
        assert_eq!(Place::from(local).dataflow(&pt), 1);

        pt.update_dataflow(
            &d,
            Rvalue::BinaryOp(
                BinOp::Add,
                Operand::Copy(a.clone()),
                Operand::Copy(c.clone()),
            )
            .dataflow(&pt),
        );
        assert_eq!(d.dataflow(&pt), 2);

        pt.update_dataflow(&e, Rvalue::Use(Operand::Constant(1.into())).dataflow(&pt));
        assert_eq!(b.dataflow(&pt), 2);

        assert_eq!(Place::from(local).dataflow(&pt), 2);
    }

    #[test]
    fn prim_arrays() {
        let mut tcx = TyCtxt::from_primitives();
        let ty = tcx.push(TyKind::Array(TyCtxt::I32, 4));

        let mut pt = PlaceTable::new(Rc::new(tcx));
        let local = Local::new(1);
        let local_pidx = pt.allocate_local(local, ty);

        let one = Local::new(2);
        pt.allocate_local(one, TyCtxt::USIZE);
        pt.assign_literal(one, Some(Literal::Uint(1, UintTy::Usize)));

        let proj = ProjectionElem::Index(one);

        let local_one = pt.project_from_node(local_pidx, proj).unwrap();

        assert_eq!(
            pt.places[local_pidx].alloc_id,
            pt.places[local_one].alloc_id
        );

        assert_eq!(
            pt.places[local_one].run_ptr.unwrap().run_and_offset,
            pt.places[local_pidx]
                .run_ptr
                .unwrap()
                .run_and_offset
                .offset(
                    BasicMemory::ty_size(TyCtxt::I32, &pt.tcx)
                        .unwrap()
                        .bytes_usize() as isize
                )
        )
    }
    #[test]
    fn composite_arrays() {
        let mut tcx = TyCtxt::from_primitives();
        let elem_ty = tcx.push(TyKind::Tuple(vec![TyCtxt::I32, TyCtxt::I64]));
        let ty = tcx.push(TyKind::Array(elem_ty, 4));
        let mut pt = PlaceTable::new(Rc::new(tcx));
        let local = Local::new(1);
        let local_pidx = pt.allocate_local(local, ty);

        let one = Local::new(2);
        pt.allocate_local(one, TyCtxt::USIZE);
        pt.assign_literal(one, Some(Literal::Uint(1, UintTy::Usize)));

        // local[one].0
        let one_zero = Place::from_projected(
            local,
            &[
                ProjectionElem::Index(one),
                ProjectionElem::TupleField(FieldIdx::new(0)),
            ],
        );
        let one_zero = pt.get_node(&one_zero).unwrap();
        assert_eq!(pt.places[one_zero].ty, TyCtxt::I32);
        assert_eq!(pt.places[local_pidx].alloc_id, pt.places[one_zero].alloc_id);
    }
}
