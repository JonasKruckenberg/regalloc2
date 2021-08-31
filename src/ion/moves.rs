/*
 * The following license applies to this file, which was initially
 * derived from the files `js/src/jit/BacktrackingAllocator.h` and
 * `js/src/jit/BacktrackingAllocator.cpp` in Mozilla Firefox:
 *
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/.
 *
 * Since the initial port, the design has been substantially evolved
 * and optimized.
 */

//! Move resolution.

use super::{
    Env, InsertMovePrio, InsertedMove, LiveRangeFlag, LiveRangeIndex, RedundantMoveEliminator,
    VRegIndex, SLOT_NONE,
};

use crate::moves::ParallelMoves;
use crate::{
    Allocation, Block, Edit, Function, Inst, InstPosition, OperandConstraint, OperandKind,
    OperandPos, ProgPoint, RegClass, VReg,
};
use smallvec::{smallvec, SmallVec};
use std::fmt::Debug;

impl<'a, F: Function> Env<'a, F> {
    pub fn is_start_of_block(&self, pos: ProgPoint) -> bool {
        let block = self.cfginfo.insn_block[pos.inst().index()];
        pos == self.cfginfo.block_entry[block.index()]
    }
    pub fn is_end_of_block(&self, pos: ProgPoint) -> bool {
        let block = self.cfginfo.insn_block[pos.inst().index()];
        pos == self.cfginfo.block_exit[block.index()]
    }

    pub fn insert_move(
        &mut self,
        pos: ProgPoint,
        prio: InsertMovePrio,
        from_alloc: Allocation,
        to_alloc: Allocation,
        to_vreg: Option<VReg>,
    ) {
        log::trace!(
            "insert_move: pos {:?} prio {:?} from_alloc {:?} to_alloc {:?}",
            pos,
            prio,
            from_alloc,
            to_alloc
        );
        match (from_alloc.as_reg(), to_alloc.as_reg()) {
            (Some(from), Some(to)) => {
                assert_eq!(from.class(), to.class());
            }
            _ => {}
        }
        self.inserted_moves.push(InsertedMove {
            pos,
            prio,
            from_alloc,
            to_alloc,
            to_vreg,
        });
    }

    pub fn get_alloc(&self, inst: Inst, slot: usize) -> Allocation {
        let inst_allocs = &self.allocs[self.inst_alloc_offsets[inst.index()] as usize..];
        inst_allocs[slot]
    }

    pub fn set_alloc(&mut self, inst: Inst, slot: usize, alloc: Allocation) {
        let inst_allocs = &mut self.allocs[self.inst_alloc_offsets[inst.index()] as usize..];
        inst_allocs[slot] = alloc;
    }

    pub fn get_alloc_for_range(&self, range: LiveRangeIndex) -> Allocation {
        log::trace!("get_alloc_for_range: {:?}", range);
        let bundle = self.ranges[range.index()].bundle;
        log::trace!(" -> bundle: {:?}", bundle);
        let bundledata = &self.bundles[bundle.index()];
        log::trace!(" -> allocation {:?}", bundledata.allocation);
        if bundledata.allocation != Allocation::none() {
            bundledata.allocation
        } else {
            log::trace!(" -> spillset {:?}", bundledata.spillset);
            log::trace!(
                " -> spill slot {:?}",
                self.spillsets[bundledata.spillset.index()].slot
            );
            self.spillslots[self.spillsets[bundledata.spillset.index()].slot.index()].alloc
        }
    }

    pub fn apply_allocations_and_insert_moves(&mut self) {
        log::trace!("apply_allocations_and_insert_moves");
        log::trace!("blockparam_ins: {:?}", self.blockparam_ins);
        log::trace!("blockparam_outs: {:?}", self.blockparam_outs);

        // Now that all splits are done, we can pay the cost once to
        // sort VReg range lists and update with the final ranges.
        for vreg in &mut self.vregs {
            for entry in &mut vreg.ranges {
                entry.range = self.ranges[entry.index.index()].range;
            }
            vreg.ranges.sort_unstable_by_key(|entry| entry.range.from);
        }

        /// We create "half-moves" in order to allow a single-scan
        /// strategy with a subsequent sort. Basically, the key idea
        /// is that as our single scan through a range for a vreg hits
        /// upon the source or destination of an edge-move, we emit a
        /// "half-move". These half-moves are carefully keyed in a
        /// particular sort order (the field order below is
        /// significant!) so that all half-moves on a given (from, to)
        /// block-edge appear contiguously, and then all moves from a
        /// given vreg appear contiguously. Within a given from-vreg,
        /// pick the first `Source` (there should only be one, but
        /// imprecision in liveranges due to loop handling sometimes
        /// means that a blockparam-out is also recognized as a normal-out),
        /// and then for each `Dest`, copy the source-alloc to that
        /// dest-alloc.
        #[derive(Clone, Debug, PartialEq, Eq)]
        struct HalfMove {
            key: u64,
            alloc: Allocation,
        }
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
        #[repr(u8)]
        enum HalfMoveKind {
            Source = 0,
            Dest = 1,
        }
        fn half_move_key(
            from_block: Block,
            to_block: Block,
            to_vreg: VRegIndex,
            kind: HalfMoveKind,
        ) -> u64 {
            assert!(from_block.index() < 1 << 21);
            assert!(to_block.index() < 1 << 21);
            assert!(to_vreg.index() < 1 << 21);
            ((from_block.index() as u64) << 43)
                | ((to_block.index() as u64) << 22)
                | ((to_vreg.index() as u64) << 1)
                | (kind as u8 as u64)
        }
        impl HalfMove {
            fn from_block(&self) -> Block {
                Block::new(((self.key >> 43) & ((1 << 21) - 1)) as usize)
            }
            fn to_block(&self) -> Block {
                Block::new(((self.key >> 22) & ((1 << 21) - 1)) as usize)
            }
            fn to_vreg(&self) -> VRegIndex {
                VRegIndex::new(((self.key >> 1) & ((1 << 21) - 1)) as usize)
            }
            fn kind(&self) -> HalfMoveKind {
                if self.key & 1 == 1 {
                    HalfMoveKind::Dest
                } else {
                    HalfMoveKind::Source
                }
            }
        }

        let mut half_moves: Vec<HalfMove> = Vec::with_capacity(6 * self.func.num_insts());
        let mut reuse_input_insts = Vec::with_capacity(self.func.num_insts() / 2);

        let mut blockparam_in_idx = 0;
        let mut blockparam_out_idx = 0;
        let mut prog_move_src_idx = 0;
        let mut prog_move_dst_idx = 0;
        for vreg in 0..self.vregs.len() {
            let vreg = VRegIndex::new(vreg);

            let pinned_alloc = if self.vregs[vreg.index()].is_pinned {
                self.func.is_pinned_vreg(self.vreg_regs[vreg.index()])
            } else {
                None
            };

            // For each range in each vreg, insert moves or
            // half-moves.  We also scan over `blockparam_ins` and
            // `blockparam_outs`, which are sorted by (block, vreg),
            // and over program-move srcs/dsts to fill in allocations.
            let mut prev = LiveRangeIndex::invalid();
            for range_idx in 0..self.vregs[vreg.index()].ranges.len() {
                let entry = self.vregs[vreg.index()].ranges[range_idx];
                let alloc = pinned_alloc
                    .map(|preg| Allocation::reg(preg))
                    .unwrap_or_else(|| self.get_alloc_for_range(entry.index));
                let range = entry.range;
                log::trace!(
                    "apply_allocations: vreg {:?} LR {:?} with range {:?} has alloc {:?} (pinned {:?})",
                    vreg,
                    entry.index,
                    range,
                    alloc,
                    pinned_alloc,
                );
                debug_assert!(alloc != Allocation::none());

                if self.annotations_enabled {
                    self.annotate(
                        range.from,
                        format!(
                            " <<< start v{} in {} (range{}) (bundle{})",
                            vreg.index(),
                            alloc,
                            entry.index.index(),
                            self.ranges[entry.index.index()].bundle.raw_u32(),
                        ),
                    );
                    self.annotate(
                        range.to,
                        format!(
                            "     end   v{} in {} (range{}) (bundle{}) >>>",
                            vreg.index(),
                            alloc,
                            entry.index.index(),
                            self.ranges[entry.index.index()].bundle.raw_u32(),
                        ),
                    );
                }

                // Does this range follow immediately after a prior
                // range in the same block? If so, insert a move (if
                // the allocs differ). We do this directly rather than
                // with half-moves because we eagerly know both sides
                // already (and also, half-moves are specific to
                // inter-block transfers).
                //
                // Note that we do *not* do this if there is also a
                // def as the first use in the new range: it's
                // possible that an old liverange covers the Before
                // pos of an inst, a new liverange covers the After
                // pos, and the def also happens at After. In this
                // case we don't want to an insert a move after the
                // instruction copying the old liverange.
                //
                // Note also that we assert that the new range has to
                // start at the Before-point of an instruction; we
                // can't insert a move that logically happens just
                // before After (i.e. in the middle of a single
                // instruction).
                //
                // Also note that this case is not applicable to
                // pinned vregs (because they are always in one PReg).
                if pinned_alloc.is_none() && prev.is_valid() {
                    let prev_alloc = self.get_alloc_for_range(prev);
                    let prev_range = self.ranges[prev.index()].range;
                    let first_is_def =
                        self.ranges[entry.index.index()].has_flag(LiveRangeFlag::StartsAtDef);
                    debug_assert!(prev_alloc != Allocation::none());

                    if prev_range.to == range.from
                        && !self.is_start_of_block(range.from)
                        && !first_is_def
                    {
                        log::trace!(
                            "prev LR {} abuts LR {} in same block; moving {} -> {} for v{}",
                            prev.index(),
                            entry.index.index(),
                            prev_alloc,
                            alloc,
                            vreg.index()
                        );
                        assert_eq!(range.from.pos(), InstPosition::Before);
                        self.insert_move(
                            range.from,
                            InsertMovePrio::Regular,
                            prev_alloc,
                            alloc,
                            Some(self.vreg_regs[vreg.index()]),
                        );
                    }
                }

                // The block-to-block edge-move logic is not
                // applicable to pinned vregs, which are always in one
                // PReg (so never need moves within their own vreg
                // ranges).
                if pinned_alloc.is_none() {
                    // Scan over blocks whose ends are covered by this
                    // range. For each, for each successor that is not
                    // already in this range (hence guaranteed to have the
                    // same allocation) and if the vreg is live, add a
                    // Source half-move.
                    let mut block = self.cfginfo.insn_block[range.from.inst().index()];
                    while block.is_valid() && block.index() < self.func.num_blocks() {
                        if range.to < self.cfginfo.block_exit[block.index()].next() {
                            break;
                        }
                        log::trace!("examining block with end in range: block{}", block.index());
                        for &succ in self.func.block_succs(block) {
                            log::trace!(
                                " -> has succ block {} with entry {:?}",
                                succ.index(),
                                self.cfginfo.block_entry[succ.index()]
                            );
                            if range.contains_point(self.cfginfo.block_entry[succ.index()]) {
                                continue;
                            }
                            log::trace!(" -> out of this range, requires half-move if live");
                            if self.is_live_in(succ, vreg) {
                                log::trace!("  -> live at input to succ, adding halfmove");
                                half_moves.push(HalfMove {
                                    key: half_move_key(block, succ, vreg, HalfMoveKind::Source),
                                    alloc,
                                });
                            }
                        }

                        // Scan forward in `blockparam_outs`, adding all
                        // half-moves for outgoing values to blockparams
                        // in succs.
                        log::trace!(
                            "scanning blockparam_outs for v{} block{}: blockparam_out_idx = {}",
                            vreg.index(),
                            block.index(),
                            blockparam_out_idx,
                        );
                        while blockparam_out_idx < self.blockparam_outs.len() {
                            let (from_vreg, from_block, to_block, to_vreg) =
                                self.blockparam_outs[blockparam_out_idx];
                            if (from_vreg, from_block) > (vreg, block) {
                                break;
                            }
                            if (from_vreg, from_block) == (vreg, block) {
                                log::trace!(
                                    " -> found: from v{} block{} to v{} block{}",
                                    from_vreg.index(),
                                    from_block.index(),
                                    to_vreg.index(),
                                    to_vreg.index()
                                );
                                half_moves.push(HalfMove {
                                    key: half_move_key(
                                        from_block,
                                        to_block,
                                        to_vreg,
                                        HalfMoveKind::Source,
                                    ),
                                    alloc,
                                });

                                if self.annotations_enabled {
                                    self.annotate(
                                        self.cfginfo.block_exit[block.index()],
                                        format!(
                                            "blockparam-out: block{} to block{}: v{} to v{} in {}",
                                            from_block.index(),
                                            to_block.index(),
                                            from_vreg.index(),
                                            to_vreg.index(),
                                            alloc
                                        ),
                                    );
                                }
                            }

                            blockparam_out_idx += 1;
                        }

                        block = block.next();
                    }

                    // Scan over blocks whose beginnings are covered by
                    // this range and for which the vreg is live at the
                    // start of the block. For each, for each predecessor,
                    // add a Dest half-move.
                    let mut block = self.cfginfo.insn_block[range.from.inst().index()];
                    if self.cfginfo.block_entry[block.index()] < range.from {
                        block = block.next();
                    }
                    while block.is_valid() && block.index() < self.func.num_blocks() {
                        if self.cfginfo.block_entry[block.index()] >= range.to {
                            break;
                        }

                        // Add half-moves for blockparam inputs.
                        log::trace!(
                            "scanning blockparam_ins at vreg {} block {}: blockparam_in_idx = {}",
                            vreg.index(),
                            block.index(),
                            blockparam_in_idx
                        );
                        while blockparam_in_idx < self.blockparam_ins.len() {
                            let (to_vreg, to_block, from_block) =
                                self.blockparam_ins[blockparam_in_idx];
                            if (to_vreg, to_block) > (vreg, block) {
                                break;
                            }
                            if (to_vreg, to_block) == (vreg, block) {
                                half_moves.push(HalfMove {
                                    key: half_move_key(
                                        from_block,
                                        to_block,
                                        to_vreg,
                                        HalfMoveKind::Dest,
                                    ),
                                    alloc,
                                });
                                log::trace!(
                                    "match: blockparam_in: v{} in block{} from block{} into {}",
                                    to_vreg.index(),
                                    to_block.index(),
                                    from_block.index(),
                                    alloc,
                                );
                                #[cfg(debug_assertions)]
                                {
                                    if log::log_enabled!(log::Level::Trace) {
                                        self.annotate(
                                            self.cfginfo.block_entry[block.index()],
                                            format!(
                                                "blockparam-in: block{} to block{}:into v{} in {}",
                                                from_block.index(),
                                                to_block.index(),
                                                to_vreg.index(),
                                                alloc
                                            ),
                                        );
                                    }
                                }
                            }
                            blockparam_in_idx += 1;
                        }

                        if !self.is_live_in(block, vreg) {
                            block = block.next();
                            continue;
                        }

                        log::trace!(
                            "scanning preds at vreg {} block {} for ends outside the range",
                            vreg.index(),
                            block.index()
                        );

                        // Now find any preds whose ends are not in the
                        // same range, and insert appropriate moves.
                        for &pred in self.func.block_preds(block) {
                            log::trace!(
                                "pred block {} has exit {:?}",
                                pred.index(),
                                self.cfginfo.block_exit[pred.index()]
                            );
                            if range.contains_point(self.cfginfo.block_exit[pred.index()]) {
                                continue;
                            }
                            log::trace!(" -> requires half-move");
                            half_moves.push(HalfMove {
                                key: half_move_key(pred, block, vreg, HalfMoveKind::Dest),
                                alloc,
                            });
                        }

                        block = block.next();
                    }

                    // If this is a blockparam vreg and the start of block
                    // is in this range, add to blockparam_allocs.
                    let (blockparam_block, blockparam_idx) =
                        self.cfginfo.vreg_def_blockparam[vreg.index()];
                    if blockparam_block.is_valid()
                        && range.contains_point(self.cfginfo.block_entry[blockparam_block.index()])
                    {
                        self.blockparam_allocs.push((
                            blockparam_block,
                            blockparam_idx,
                            vreg,
                            alloc,
                        ));
                    }
                }

                // Scan over def/uses and apply allocations.
                for use_idx in 0..self.ranges[entry.index.index()].uses.len() {
                    let usedata = self.ranges[entry.index.index()].uses[use_idx];
                    log::trace!("applying to use: {:?}", usedata);
                    debug_assert!(range.contains_point(usedata.pos));
                    let inst = usedata.pos.inst();
                    let slot = usedata.slot;
                    let operand = usedata.operand;
                    // Safepoints add virtual uses with no slots;
                    // avoid these.
                    if slot != SLOT_NONE {
                        self.set_alloc(inst, slot as usize, alloc);
                    }
                    if let OperandConstraint::Reuse(_) = operand.constraint() {
                        reuse_input_insts.push(inst);
                    }
                }

                // Scan over program move srcs/dsts to fill in allocations.

                // Move srcs happen at `After` of a given
                // inst. Compute [from, to) semi-inclusive range of
                // inst indices for which we should fill in the source
                // with this LR's allocation.
                //
                // range from inst-Before or inst-After covers cur
                // inst's After; so includes move srcs from inst.
                let move_src_start = (vreg, range.from.inst());
                // range to (exclusive) inst-Before or inst-After
                // covers only prev inst's After; so includes move
                // srcs to (exclusive) inst.
                let move_src_end = (vreg, range.to.inst());
                log::trace!(
                    "vreg {:?} range {:?}: looking for program-move sources from {:?} to {:?}",
                    vreg,
                    range,
                    move_src_start,
                    move_src_end
                );
                while prog_move_src_idx < self.prog_move_srcs.len()
                    && self.prog_move_srcs[prog_move_src_idx].0 < move_src_start
                {
                    log::trace!(" -> skipping idx {}", prog_move_src_idx);
                    prog_move_src_idx += 1;
                }
                while prog_move_src_idx < self.prog_move_srcs.len()
                    && self.prog_move_srcs[prog_move_src_idx].0 < move_src_end
                {
                    log::trace!(
                        " -> setting idx {} ({:?}) to alloc {:?}",
                        prog_move_src_idx,
                        self.prog_move_srcs[prog_move_src_idx].0,
                        alloc
                    );
                    self.prog_move_srcs[prog_move_src_idx].1 = alloc;
                    prog_move_src_idx += 1;
                }

                // move dsts happen at Before point.
                //
                // Range from inst-Before includes cur inst, while inst-After includes only next inst.
                let move_dst_start = if range.from.pos() == InstPosition::Before {
                    (vreg, range.from.inst())
                } else {
                    (vreg, range.from.inst().next())
                };
                // Range to (exclusive) inst-Before includes prev
                // inst, so to (exclusive) cur inst; range to
                // (exclusive) inst-After includes cur inst, so to
                // (exclusive) next inst.
                let move_dst_end = if range.to.pos() == InstPosition::Before {
                    (vreg, range.to.inst())
                } else {
                    (vreg, range.to.inst().next())
                };
                log::trace!(
                    "vreg {:?} range {:?}: looking for program-move dests from {:?} to {:?}",
                    vreg,
                    range,
                    move_dst_start,
                    move_dst_end
                );
                while prog_move_dst_idx < self.prog_move_dsts.len()
                    && self.prog_move_dsts[prog_move_dst_idx].0 < move_dst_start
                {
                    log::trace!(" -> skipping idx {}", prog_move_dst_idx);
                    prog_move_dst_idx += 1;
                }
                while prog_move_dst_idx < self.prog_move_dsts.len()
                    && self.prog_move_dsts[prog_move_dst_idx].0 < move_dst_end
                {
                    log::trace!(
                        " -> setting idx {} ({:?}) to alloc {:?}",
                        prog_move_dst_idx,
                        self.prog_move_dsts[prog_move_dst_idx].0,
                        alloc
                    );
                    self.prog_move_dsts[prog_move_dst_idx].1 = alloc;
                    prog_move_dst_idx += 1;
                }

                prev = entry.index;
            }
        }

        // Sort the half-moves list. For each (from, to,
        // from-vreg) tuple, find the from-alloc and all the
        // to-allocs, and insert moves on the block edge.
        half_moves.sort_unstable_by_key(|h| h.key);
        log::trace!("halfmoves: {:?}", half_moves);
        self.stats.halfmoves_count = half_moves.len();

        let mut i = 0;
        while i < half_moves.len() {
            // Find a Source.
            while i < half_moves.len() && half_moves[i].kind() != HalfMoveKind::Source {
                i += 1;
            }
            if i >= half_moves.len() {
                break;
            }
            let src = &half_moves[i];
            i += 1;

            // Find all Dests.
            let dest_key = src.key | 1;
            let first_dest = i;
            while i < half_moves.len() && half_moves[i].key == dest_key {
                i += 1;
            }
            let last_dest = i;

            log::trace!(
                "halfmove match: src {:?} dests {:?}",
                src,
                &half_moves[first_dest..last_dest]
            );

            // Determine the ProgPoint where moves on this (from, to)
            // edge should go:
            // - If there is more than one in-edge to `to`, then
            //   `from` must have only one out-edge; moves go at tail of
            //   `from` just before last Branch/Ret.
            // - Otherwise, there must be at most one in-edge to `to`,
            //   and moves go at start of `to`.
            let from_last_insn = self.func.block_insns(src.from_block()).last();
            let to_first_insn = self.func.block_insns(src.to_block()).first();
            let from_is_ret = self.func.is_ret(from_last_insn);
            let to_is_entry = self.func.entry_block() == src.to_block();
            let from_outs =
                self.func.block_succs(src.from_block()).len() + if from_is_ret { 1 } else { 0 };
            let to_ins =
                self.func.block_preds(src.to_block()).len() + if to_is_entry { 1 } else { 0 };

            let (insertion_point, prio) = if to_ins > 1 && from_outs <= 1 {
                (
                    // N.B.: though semantically the edge moves happen
                    // after the branch, we must insert them before
                    // the branch because otherwise, of course, they
                    // would never execute. This is correct even in
                    // the presence of branches that read register
                    // inputs (e.g. conditional branches on some RISCs
                    // that branch on reg zero/not-zero, or any
                    // indirect branch), but for a very subtle reason:
                    // all cases of such branches will (or should)
                    // have multiple successors, and thus due to
                    // critical-edge splitting, their successors will
                    // have only the single predecessor, and we prefer
                    // to insert at the head of the successor in that
                    // case (rather than here). We make this a
                    // requirement, in fact: the user of this library
                    // shall not read registers in a branch
                    // instruction of there is only one successor per
                    // the given CFG information.
                    ProgPoint::before(from_last_insn),
                    InsertMovePrio::OutEdgeMoves,
                )
            } else if to_ins <= 1 {
                (
                    ProgPoint::before(to_first_insn),
                    InsertMovePrio::InEdgeMoves,
                )
            } else {
                panic!(
                    "Critical edge: can't insert moves between blocks {:?} and {:?}",
                    src.from_block(),
                    src.to_block()
                );
            };

            let mut last = None;
            for dest in first_dest..last_dest {
                let dest = &half_moves[dest];
                if last == Some(dest.alloc) {
                    continue;
                }
                self.insert_move(
                    insertion_point,
                    prio,
                    src.alloc,
                    dest.alloc,
                    Some(self.vreg_regs[dest.to_vreg().index()]),
                );
                last = Some(dest.alloc);
            }
        }

        // Handle multi-fixed-reg constraints by copying.
        for (progpoint, from_preg, to_preg, slot) in
            std::mem::replace(&mut self.multi_fixed_reg_fixups, vec![])
        {
            log::trace!(
                "multi-fixed-move constraint at {:?} from p{} to p{}",
                progpoint,
                from_preg.index(),
                to_preg.index()
            );
            self.insert_move(
                progpoint,
                InsertMovePrio::MultiFixedReg,
                Allocation::reg(self.pregs[from_preg.index()].reg),
                Allocation::reg(self.pregs[to_preg.index()].reg),
                None,
            );
            self.set_alloc(
                progpoint.inst(),
                slot,
                Allocation::reg(self.pregs[to_preg.index()].reg),
            );
        }

        // Handle outputs that reuse inputs: copy beforehand, then set
        // input's alloc to output's.
        //
        // Note that the output's allocation may not *actually* be
        // valid until InstPosition::After, but the reused input may
        // occur at InstPosition::Before. This may appear incorrect,
        // but we make it work by ensuring that all *other* inputs are
        // extended to InstPosition::After so that the def will not
        // interfere. (The liveness computation code does this -- we
        // do not require the user to do so.)
        //
        // One might ask: why not insist that input-reusing defs occur
        // at InstPosition::Before? this would be correct, but would
        // mean that the reused input and the reusing output
        // interfere, *guaranteeing* that every such case would
        // require a move. This is really bad on ISAs (like x86) where
        // reused inputs are ubiquitous.
        //
        // Another approach might be to put the def at Before, and
        // trim the reused input's liverange back to the previous
        // instruction's After. This is kind of OK until (i) a block
        // boundary occurs between the prior inst and this one, or
        // (ii) any moves/spills/reloads occur between the two
        // instructions. We really do need the input to be live at
        // this inst's Before.
        //
        // In principle what we really need is a "BeforeBefore"
        // program point, but we don't want to introduce that
        // everywhere and pay the cost of twice as many ProgPoints
        // throughout the allocator.
        //
        // Or we could introduce a separate move instruction -- this
        // is the approach that regalloc.rs takes with "mod" operands
        // -- but that is also costly.
        //
        // So we take this approach (invented by IonMonkey -- somewhat
        // hard to discern, though see [0] for a comment that makes
        // this slightly less unclear) to avoid interference between
        // the actual reused input and reusing output, ensure
        // interference (hence no incorrectness) between other inputs
        // and the reusing output, and not require a separate explicit
        // move instruction.
        //
        // [0] https://searchfox.org/mozilla-central/rev/3a798ef9252896fb389679f06dd3203169565af0/js/src/jit/shared/Lowering-shared-inl.h#108-110
        for inst in reuse_input_insts {
            let mut input_reused: SmallVec<[usize; 4]> = smallvec![];
            for output_idx in 0..self.func.inst_operands(inst).len() {
                let operand = self.func.inst_operands(inst)[output_idx];
                if let OperandConstraint::Reuse(input_idx) = operand.constraint() {
                    debug_assert!(!input_reused.contains(&input_idx));
                    debug_assert_eq!(operand.pos(), OperandPos::After);
                    input_reused.push(input_idx);
                    let input_alloc = self.get_alloc(inst, input_idx);
                    let output_alloc = self.get_alloc(inst, output_idx);
                    log::trace!(
                        "reuse-input inst {:?}: output {} has alloc {:?}, input {} has alloc {:?}",
                        inst,
                        output_idx,
                        output_alloc,
                        input_idx,
                        input_alloc
                    );
                    if input_alloc != output_alloc {
                        #[cfg(debug_assertions)]
                        {
                            if log::log_enabled!(log::Level::Trace) {
                                self.annotate(
                                    ProgPoint::before(inst),
                                    format!(
                                        " reuse-input-copy: {} -> {}",
                                        input_alloc, output_alloc
                                    ),
                                );
                            }
                        }
                        let input_operand = self.func.inst_operands(inst)[input_idx];
                        self.insert_move(
                            ProgPoint::before(inst),
                            InsertMovePrio::ReusedInput,
                            input_alloc,
                            output_alloc,
                            Some(input_operand.vreg()),
                        );
                        self.set_alloc(inst, input_idx, output_alloc);
                    }
                }
            }
        }

        // Sort the prog-moves lists and insert moves to reify the
        // input program's move operations.
        self.prog_move_srcs
            .sort_unstable_by_key(|((_, inst), _)| *inst);
        self.prog_move_dsts
            .sort_unstable_by_key(|((_, inst), _)| inst.prev());
        let prog_move_srcs = std::mem::replace(&mut self.prog_move_srcs, vec![]);
        let prog_move_dsts = std::mem::replace(&mut self.prog_move_dsts, vec![]);
        assert_eq!(prog_move_srcs.len(), prog_move_dsts.len());
        for (&((_, from_inst), from_alloc), &((to_vreg, to_inst), to_alloc)) in
            prog_move_srcs.iter().zip(prog_move_dsts.iter())
        {
            log::trace!(
                "program move at inst {:?}: alloc {:?} -> {:?} (v{})",
                from_inst,
                from_alloc,
                to_alloc,
                to_vreg.index(),
            );
            assert!(!from_alloc.is_none());
            assert!(!to_alloc.is_none());
            assert_eq!(from_inst, to_inst.prev());
            // N.B.: these moves happen with the *same* priority as
            // LR-to-LR moves, because they work just like them: they
            // connect a use at one progpoint (move-After) with a def
            // at an adjacent progpoint (move+1-Before), so they must
            // happen in parallel with all other LR-to-LR moves.
            self.insert_move(
                ProgPoint::before(to_inst),
                InsertMovePrio::Regular,
                from_alloc,
                to_alloc,
                Some(self.vreg_regs[to_vreg.index()]),
            );
        }
    }

    pub fn resolve_inserted_moves(&mut self) {
        // For each program point, gather all moves together. Then
        // resolve (see cases below).
        let mut i = 0;
        self.inserted_moves
            .sort_unstable_by_key(|m| (m.pos.to_index(), m.prio));

        // Redundant-move elimination state tracker.
        let mut redundant_moves = RedundantMoveEliminator::default();

        fn redundant_move_process_side_effects<'a, F: Function>(
            this: &Env<'a, F>,
            redundant_moves: &mut RedundantMoveEliminator,
            from: ProgPoint,
            to: ProgPoint,
        ) {
            // If any safepoints in range, clear and return.
            // Also, if we cross a block boundary, clear and return.
            if this.cfginfo.insn_block[from.inst().index()]
                != this.cfginfo.insn_block[to.inst().index()]
            {
                redundant_moves.clear();
                return;
            }
            for inst in from.inst().index()..=to.inst().index() {
                if this.func.is_safepoint(Inst::new(inst)) {
                    redundant_moves.clear();
                    return;
                }
            }

            let start_inst = if from.pos() == InstPosition::Before {
                from.inst()
            } else {
                from.inst().next()
            };
            let end_inst = if to.pos() == InstPosition::Before {
                to.inst()
            } else {
                to.inst().next()
            };
            for inst in start_inst.index()..end_inst.index() {
                let inst = Inst::new(inst);
                for (i, op) in this.func.inst_operands(inst).iter().enumerate() {
                    match op.kind() {
                        OperandKind::Def | OperandKind::Mod => {
                            let alloc = this.get_alloc(inst, i);
                            redundant_moves.clear_alloc(alloc);
                        }
                        _ => {}
                    }
                }
                for reg in this.func.inst_clobbers(inst) {
                    redundant_moves.clear_alloc(Allocation::reg(*reg));
                }
            }
        }

        let mut last_pos = ProgPoint::before(Inst::new(0));

        while i < self.inserted_moves.len() {
            let start = i;
            let pos = self.inserted_moves[i].pos;
            let prio = self.inserted_moves[i].prio;
            while i < self.inserted_moves.len()
                && self.inserted_moves[i].pos == pos
                && self.inserted_moves[i].prio == prio
            {
                i += 1;
            }
            let moves = &self.inserted_moves[start..i];

            redundant_move_process_side_effects(self, &mut redundant_moves, last_pos, pos);
            last_pos = pos;

            // Gather all the moves with Int class and Float class
            // separately. These cannot interact, so it is safe to
            // have two separate ParallelMove instances. They need to
            // be separate because moves between the two classes are
            // impossible. (We could enhance ParallelMoves to
            // understand register classes and take multiple scratch
            // regs, but this seems simpler.)
            let mut int_moves: SmallVec<[InsertedMove; 8]> = smallvec![];
            let mut float_moves: SmallVec<[InsertedMove; 8]> = smallvec![];
            let mut self_moves: SmallVec<[InsertedMove; 8]> = smallvec![];

            for m in moves {
                if m.from_alloc.is_reg() && m.to_alloc.is_reg() {
                    assert_eq!(m.from_alloc.class(), m.to_alloc.class());
                }
                if m.from_alloc == m.to_alloc {
                    if m.to_vreg.is_some() {
                        self_moves.push(m.clone());
                    }
                    continue;
                }
                match m.from_alloc.class() {
                    RegClass::Int => {
                        int_moves.push(m.clone());
                    }
                    RegClass::Float => {
                        float_moves.push(m.clone());
                    }
                }
            }

            for &(regclass, moves) in
                &[(RegClass::Int, &int_moves), (RegClass::Float, &float_moves)]
            {
                // All moves in `moves` semantically happen in
                // parallel. Let's resolve these to a sequence of moves
                // that can be done one at a time.
                let scratch = self.env.scratch_by_class[regclass as u8 as usize];
                let mut parallel_moves = ParallelMoves::new(Allocation::reg(scratch));
                log::trace!("parallel moves at pos {:?} prio {:?}", pos, prio);
                for m in moves {
                    if (m.from_alloc != m.to_alloc) || m.to_vreg.is_some() {
                        log::trace!(" {} -> {}", m.from_alloc, m.to_alloc,);
                        parallel_moves.add(m.from_alloc, m.to_alloc, m.to_vreg);
                    }
                }

                let resolved = parallel_moves.resolve();

                // If (i) the scratch register is used, and (ii) a
                // stack-to-stack move exists, then we need to
                // allocate an additional scratch spillslot to which
                // we can temporarily spill the scratch reg when we
                // lower the stack-to-stack move to a
                // stack-to-scratch-to-stack sequence.
                let scratch_used = resolved.iter().any(|&(src, dst, _)| {
                    src == Allocation::reg(scratch) || dst == Allocation::reg(scratch)
                });
                let stack_stack_move = resolved
                    .iter()
                    .any(|&(src, dst, _)| src.is_stack() && dst.is_stack());
                let extra_slot = if scratch_used && stack_stack_move {
                    if self.extra_spillslot[regclass as u8 as usize].is_none() {
                        let slot = self.allocate_spillslot(regclass);
                        self.extra_spillslot[regclass as u8 as usize] = Some(slot);
                    }
                    self.extra_spillslot[regclass as u8 as usize]
                } else {
                    None
                };

                let mut scratch_used_yet = false;
                for (src, dst, to_vreg) in resolved {
                    log::trace!("  resolved: {} -> {} ({:?})", src, dst, to_vreg);
                    let action = redundant_moves.process_move(src, dst, to_vreg);
                    if !action.elide {
                        if dst == Allocation::reg(scratch) {
                            scratch_used_yet = true;
                        }
                        if src.is_stack() && dst.is_stack() {
                            if !scratch_used_yet {
                                self.add_edit(
                                    pos,
                                    prio,
                                    Edit::Move {
                                        from: src,
                                        to: Allocation::reg(scratch),
                                        to_vreg,
                                    },
                                );
                                self.add_edit(
                                    pos,
                                    prio,
                                    Edit::Move {
                                        from: Allocation::reg(scratch),
                                        to: dst,
                                        to_vreg,
                                    },
                                );
                            } else {
                                assert!(extra_slot.is_some());
                                self.add_edit(
                                    pos,
                                    prio,
                                    Edit::Move {
                                        from: Allocation::reg(scratch),
                                        to: extra_slot.unwrap(),
                                        to_vreg: None,
                                    },
                                );
                                self.add_edit(
                                    pos,
                                    prio,
                                    Edit::Move {
                                        from: src,
                                        to: Allocation::reg(scratch),
                                        to_vreg,
                                    },
                                );
                                self.add_edit(
                                    pos,
                                    prio,
                                    Edit::Move {
                                        from: Allocation::reg(scratch),
                                        to: dst,
                                        to_vreg,
                                    },
                                );
                                self.add_edit(
                                    pos,
                                    prio,
                                    Edit::Move {
                                        from: extra_slot.unwrap(),
                                        to: Allocation::reg(scratch),
                                        to_vreg: None,
                                    },
                                );
                            }
                        } else {
                            self.add_edit(
                                pos,
                                prio,
                                Edit::Move {
                                    from: src,
                                    to: dst,
                                    to_vreg,
                                },
                            );
                        }
                    } else {
                        log::trace!("    -> redundant move elided");
                    }
                    if let Some((alloc, vreg)) = action.def_alloc {
                        log::trace!(
                            "     -> converted to DefAlloc: alloc {} vreg {}",
                            alloc,
                            vreg
                        );
                        self.add_edit(pos, prio, Edit::DefAlloc { alloc, vreg });
                    }
                }
            }

            for m in &self_moves {
                log::trace!(
                    "self move at pos {:?} prio {:?}: {} -> {} to_vreg {:?}",
                    pos,
                    prio,
                    m.from_alloc,
                    m.to_alloc,
                    m.to_vreg
                );
                let action = redundant_moves.process_move(m.from_alloc, m.to_alloc, m.to_vreg);
                assert!(action.elide);
                if let Some((alloc, vreg)) = action.def_alloc {
                    log::trace!(" -> DefAlloc: alloc {} vreg {}", alloc, vreg);
                    self.add_edit(pos, prio, Edit::DefAlloc { alloc, vreg });
                }
            }
        }

        // Add edits to describe blockparam locations too. This is
        // required by the checker. This comes after any edge-moves.
        self.blockparam_allocs
            .sort_unstable_by_key(|&(block, idx, _, _)| (block, idx));
        self.stats.blockparam_allocs_count = self.blockparam_allocs.len();
        let mut i = 0;
        while i < self.blockparam_allocs.len() {
            let start = i;
            let block = self.blockparam_allocs[i].0;
            while i < self.blockparam_allocs.len() && self.blockparam_allocs[i].0 == block {
                i += 1;
            }
            let params = &self.blockparam_allocs[start..i];
            let vregs = params
                .iter()
                .map(|(_, _, vreg_idx, _)| self.vreg_regs[vreg_idx.index()])
                .collect::<Vec<_>>();
            let allocs = params
                .iter()
                .map(|(_, _, _, alloc)| *alloc)
                .collect::<Vec<_>>();
            assert_eq!(vregs.len(), self.func.block_params(block).len());
            assert_eq!(allocs.len(), self.func.block_params(block).len());
            for (vreg, alloc) in vregs.into_iter().zip(allocs.into_iter()) {
                self.add_edit(
                    self.cfginfo.block_entry[block.index()],
                    InsertMovePrio::BlockParam,
                    Edit::DefAlloc { alloc, vreg },
                );
            }
        }

        // Ensure edits are in sorted ProgPoint order. N.B.: this must
        // be a stable sort! We have to keep the order produced by the
        // parallel-move resolver for all moves within a single sort
        // key.
        self.edits.sort_by_key(|&(pos, prio, _)| (pos, prio));
        self.stats.edits_count = self.edits.len();

        // Add debug annotations.
        if self.annotations_enabled {
            for i in 0..self.edits.len() {
                let &(pos, _, ref edit) = &self.edits[i];
                match edit {
                    &Edit::Move { from, to, to_vreg } => {
                        self.annotate(
                            ProgPoint::from_index(pos),
                            format!("move {} -> {} ({:?})", from, to, to_vreg),
                        );
                    }
                    &Edit::DefAlloc { alloc, vreg } => {
                        let s = format!("defalloc {:?} := {:?}", alloc, vreg);
                        self.annotate(ProgPoint::from_index(pos), s);
                    }
                }
            }
        }
    }

    pub fn add_edit(&mut self, pos: ProgPoint, prio: InsertMovePrio, edit: Edit) {
        match &edit {
            &Edit::Move { from, to, to_vreg } if from == to && to_vreg.is_none() => return,
            &Edit::Move { from, to, .. } if from.is_reg() && to.is_reg() => {
                assert_eq!(from.as_reg().unwrap().class(), to.as_reg().unwrap().class());
            }
            _ => {}
        }

        self.edits.push((pos.to_index(), prio, edit));
    }
}
