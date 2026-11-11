/*
 * This file was initially derived from the files
 * `js/src/jit/BacktrackingAllocator.h` and
 * `js/src/jit/BacktrackingAllocator.cpp` in Mozilla Firefox, and was
 * originally licensed under the Mozilla Public License 2.0. We
 * subsequently relicensed it to Apache-2.0 WITH LLVM-exception (see
 * https://github.com/bytecodealliance/regalloc2/issues/7).
 *
 * Since the initial port, the design has been substantially evolved
 * and optimized.
 */

//! Spillslot allocation.

use super::CodeRange;
use super::{
    AllocRegResult, Env, LiveBundleIndex, LiveBundleVec, LiveRangeIndex, LiveRangeKey,
    LiveRangeList, LiveRangeListEntry, PRegIndex, RegTraversalIter, Requirement, SpillSetIndex,
    SpillSlotData, SpillSlotIndex, UseList,
};
use crate::{Allocation, Block, Function, PReg, ProgPoint, SpillSlot};
use alloc::vec::Vec;

impl<'a, F: Function> Env<'a, F> {
    pub fn try_allocating_regs_for_spilled_bundles(&mut self) {
        trace!("allocating regs for spilled bundles");
        let mut scratch = core::mem::take(&mut self.ctx.scratch_conflicts);
        for i in 0..self.ctx.spilled_bundles.len() {
            let bundle = self.ctx.spilled_bundles[i]; // don't borrow self

            if self.ctx.bundles[bundle].ranges.is_empty() {
                continue;
            }

            let class = self.ctx.spillsets[self.ctx.bundles[bundle].spillset].class;
            let hint = self.ctx.spillsets[self.ctx.bundles[bundle].spillset]
                .hint
                .as_valid();

            // This may be an empty-range bundle whose ranges are not
            // sorted; sort all range-lists again here.
            self.ctx.bundles[bundle]
                .ranges
                .sort_unstable_by_key(|entry| entry.range.from);

            let mut success = false;
            self.ctx.output.stats.spill_bundle_reg_probes += 1;
            let limit = self.bundles[bundle].limit.map(|l| l as usize);
            for preg in RegTraversalIter::new(self.env, class, None, hint, bundle.index(), limit) {
                trace!("trying bundle {:?} to preg {:?}", bundle, preg);
                let preg_idx = PRegIndex::new(preg.index());
                if let AllocRegResult::Allocated(_) =
                    self.try_to_allocate_bundle_to_reg(bundle, preg_idx, None, &mut scratch)
                {
                    self.ctx.output.stats.spill_bundle_reg_success += 1;
                    success = true;
                    break;
                }
            }

            if !success {
                trace!(
                    "spilling bundle {:?}: marking spillset {:?} as required",
                    bundle,
                    self.ctx.bundles[bundle].spillset
                );
                self.ctx.spillsets[self.ctx.bundles[bundle].spillset].required = true;
            }
        }
        self.move_bundles_to_spill_bundle_regs(&mut scratch);
        self.ctx.scratch_conflicts = scratch;
        self.extend_bundles_over_loops();
    }

    /// Gives a bundle's register to the rest of its value inside a loop,
    /// when the register is free there.
    ///
    /// A bundle trimmed to its uses leaves the loop header and the back
    /// edge to the spill bundle. If that got a stack slot, or a register
    /// the trimmed bundle could not move into, the value is reloaded
    /// before the use and written back after it on every iteration. Taking
    /// the spill bundle's ranges inside the loop into the bundle's register
    /// moves the reload to the loop's entry and the write to its exits.
    ///
    /// The loop is the innermost one with a use of the bundle in a block
    /// that dominates every backedge source: each iteration that repeats
    /// executes that use, so an iteration saves at least what an entry
    /// costs. A use on a conditional path or in an exit block does not
    /// qualify. The extension is all or nothing: every range of the
    /// spillset inside the loop, outside blocks without successors, must
    /// fit the register, or nothing changes. Only free space is taken.
    fn extend_bundles_over_loops(&mut self) {
        if self.ctx.cfginfo.loops.is_empty() {
            return;
        }
        // Bundles by spillset, built when the first candidate is found.
        let mut by_spillset: Vec<LiveBundleIndex> = Vec::new();
        let mut spillset_start: Vec<u32> = Vec::new();
        let mut pieces: Vec<(LiveRangeIndex, CodeRange)> = Vec::new();
        let mut moved: Vec<LiveBundleIndex> = Vec::new();
        let mut dominating: Vec<u8> = alloc::vec![0; self.func.num_blocks()];
        'bundles: for i in 0..self.ctx.bundles.len() {
            let bundle = LiveBundleIndex::new(i);
            let Some(preg) = self.ctx.bundles[bundle].allocation.as_reg() else {
                continue;
            };
            if self.ctx.pregs[preg.index()].is_stack {
                continue;
            }
            let spillset = self.ctx.bundles[bundle].spillset;
            if spillset.is_invalid() {
                continue;
            }
            let spill_bundle = self.ctx.spillsets[spillset].spill_bundle;
            if spill_bundle.is_invalid() || spill_bundle == bundle {
                continue;
            }
            if self.ctx.bundles[spill_bundle].ranges.is_empty()
                || self.ctx.bundles[spill_bundle].allocation.as_reg() == Some(preg)
            {
                continue;
            }
            let Some(mut lp) = self.loop_with_dominating_use(bundle, &mut dominating) else {
                continue;
            };
            // Most candidates have no spilled part in their loop.
            while !self.bundle_overlaps_loop(spill_bundle, lp) {
                let parent = self.ctx.cfginfo.loops[lp as usize].parent;
                if parent == u32::MAX || !self.bundle_use_dominates_backedges(bundle, parent) {
                    continue 'bundles;
                }
                lp = parent;
            }
            if by_spillset.is_empty() {
                // Counting sort of the bundles by spillset.
                let n = self.ctx.spillsets.len();
                spillset_start.resize(n + 1, 0);
                for b in self.ctx.bundles.iter() {
                    if !b.ranges.is_empty() && b.spillset.is_valid() {
                        spillset_start[b.spillset.index() + 1] += 1;
                    }
                }
                for k in 0..n {
                    spillset_start[k + 1] += spillset_start[k];
                }
                by_spillset.resize(spillset_start[n] as usize, LiveBundleIndex::invalid());
                let mut next: Vec<u32> = spillset_start.clone();
                for k in 0..self.ctx.bundles.len() {
                    let b = LiveBundleIndex::new(k);
                    let ss = self.ctx.bundles[b].spillset;
                    if !self.ctx.bundles[b].ranges.is_empty() && ss.is_valid() {
                        by_spillset[next[ss.index()] as usize] = b;
                        next[ss.index()] += 1;
                    }
                }
            }
            let members = &by_spillset[spillset_start[spillset.index()] as usize
                ..spillset_start[spillset.index() + 1] as usize];
            let mut preg = preg;
            loop {
                let mut done = self.extend_bundle_over_loop(
                    bundle,
                    preg,
                    preg,
                    lp,
                    members,
                    &mut pieces,
                    &mut moved,
                );
                if !done && self.bundle_can_change_reg(bundle) {
                    // The bundle's own register is taken somewhere in the
                    // loop: try the others. A non-preferred register that
                    // nothing uses is left alone, since using it may cost
                    // a save and restore.
                    let class = self.ctx.spillsets[spillset].class as u8 as usize;
                    let preferred = self.env.preferred_regs_by_class[class];
                    let non_preferred = self.env.non_preferred_regs_by_class[class];
                    for (cand, is_preferred) in preferred
                        .into_iter()
                        .map(|r| (r, true))
                        .chain(non_preferred.into_iter().map(|r| (r, false)))
                    {
                        if !is_preferred
                            && self.ctx.pregs[cand.index()].allocations.btree.is_empty()
                        {
                            continue;
                        }
                        if cand != preg
                            && self.extend_bundle_over_loop(
                                bundle,
                                preg,
                                cand,
                                lp,
                                members,
                                &mut pieces,
                                &mut moved,
                            )
                        {
                            preg = cand;
                            done = true;
                            break;
                        }
                    }
                }
                if !done {
                    break;
                }
                let parent = self.ctx.cfginfo.loops[lp as usize].parent;
                if parent == u32::MAX || !self.bundle_use_dominates_backedges(bundle, parent) {
                    break;
                }
                lp = parent;
            }
        }
    }

    /// The innermost loop that has a use of `bundle` in a block dominating
    /// all of the loop's backedge sources.
    ///
    /// `dominating` caches the answer per block for the block's innermost
    /// loop: 0 unknown, 1 no, 2 yes.
    fn loop_with_dominating_use(
        &self,
        bundle: LiveBundleIndex,
        dominating: &mut [u8],
    ) -> Option<u32> {
        let mut best: Option<(u32, u32)> = None;
        for entry in &self.ctx.bundles[bundle].ranges {
            for u in &self.ctx.ranges[entry.index].uses {
                let block = self.ctx.cfginfo.insn_block[u.pos.inst().index()];
                let depth = self.ctx.cfginfo.approx_loop_depth[block.index()];
                let lp = self.ctx.cfginfo.block_loop[block.index()];
                if depth == 0 || lp == u32::MAX || best.is_some_and(|(d, _)| d >= depth) {
                    continue;
                }
                let known = &mut dominating[block.index()];
                if *known == 0 {
                    *known = 1 + self.block_dominates_backedges(block, lp) as u8;
                }
                if *known == 2 {
                    best = Some((depth, lp));
                }
            }
        }
        best.map(|(_, lp)| lp)
    }

    fn bundle_use_dominates_backedges(&self, bundle: LiveBundleIndex, lp: u32) -> bool {
        self.ctx.bundles[bundle].ranges.iter().any(|entry| {
            self.ctx.ranges[entry.index].uses.iter().any(|u| {
                let block = self.ctx.cfginfo.insn_block[u.pos.inst().index()];
                self.ctx.cfginfo.approx_loop_depth[block.index()] > 0
                    && self.block_dominates_backedges(block, lp)
            })
        })
    }

    fn block_dominates_backedges(&self, block: Block, lp: u32) -> bool {
        let span = self.ctx.cfginfo.loops[lp as usize];
        let header = Block::new(span.header as usize);
        if (block.index() as u32) < span.header || block.index() as u32 > span.last {
            return false;
        }
        self.func
            .block_preds(header)
            .iter()
            .filter(|pred| pred.index() >= header.index())
            .all(|&pred| self.ctx.cfginfo.dominates(block, pred))
    }

    fn bundle_overlaps_loop(&self, bundle: LiveBundleIndex, lp: u32) -> bool {
        let span = self.ctx.cfginfo.loops[lp as usize];
        let from = self.ctx.cfginfo.block_entry[span.header as usize];
        let to = self.ctx.cfginfo.block_exit[span.last as usize].next();
        let ranges = &self.ctx.bundles[bundle].ranges;
        let i = ranges.partition_point(|entry| entry.range.to <= from);
        i < ranges.len() && ranges[i].range.from < to
    }

    fn bundle_can_change_reg(&self, bundle: LiveBundleIndex) -> bool {
        self.ctx.bundles[bundle].limit.is_none()
            && matches!(self.compute_requirement(bundle), Ok(Requirement::Register))
    }

    /// Extends `bundle`, now in `cur`, over loop `lp` in register `preg`,
    /// moving the bundle itself there when `preg` differs from `cur`.
    /// `members` are the bundles of its spillset. Returns whether the
    /// value is in `preg` throughout the loop afterwards.
    fn extend_bundle_over_loop(
        &mut self,
        bundle: LiveBundleIndex,
        cur: PReg,
        preg: PReg,
        lp: u32,
        members: &[LiveBundleIndex],
        pieces: &mut Vec<(LiveRangeIndex, CodeRange)>,
        moved: &mut Vec<LiveBundleIndex>,
    ) -> bool {
        let span = self.ctx.cfginfo.loops[lp as usize];
        let from = self.ctx.cfginfo.block_entry[span.header as usize];
        let to = self.ctx.cfginfo.block_exit[span.last as usize].next();
        let preg_idx = PRegIndex::new(preg.index());
        pieces.clear();
        moved.clear();
        if preg != cur {
            for entry in &self.ctx.bundles[bundle].ranges {
                if self.preg_overlaps(preg_idx, entry.range) {
                    return false;
                }
            }
        }
        // Check: every part of the spillset in the loop fits `preg`.
        for &other in members {
            if other == bundle {
                continue;
            }
            let alloc = self.ctx.bundles[other].allocation;
            if alloc.as_reg() == Some(preg) {
                continue;
            }
            let ranges = &self.ctx.bundles[other].ranges;
            let overlaps = ranges
                .iter()
                .any(|entry| entry.range.from < to && entry.range.to > from);
            if !overlaps {
                continue;
            }
            let spill_bundle = self.ctx.spillsets[self.ctx.bundles[other].spillset].spill_bundle;
            if other != spill_bundle {
                // Another bundle of its own: it moves whole or not at all.
                let inside = ranges
                    .iter()
                    .all(|entry| entry.range.from >= from && entry.range.to <= to);
                if !inside
                    || alloc.as_reg().is_none()
                    || self.ctx.bundles[other].limit.is_some()
                    || !matches!(self.compute_requirement(other), Ok(Requirement::Register))
                {
                    return false;
                }
                for entry in ranges {
                    if self.preg_overlaps(preg_idx, entry.range) {
                        return false;
                    }
                }
                moved.push(other);
                continue;
            }
            if alloc
                .as_reg()
                .is_some_and(|r| self.ctx.pregs[r.index()].is_stack)
            {
                return false;
            }
            for entry in ranges {
                let lo = core::cmp::max(entry.range.from, from);
                let hi = core::cmp::min(entry.range.to, to);
                if lo >= hi {
                    continue;
                }
                // Runs of blocks with successors.
                let first = self.ctx.cfginfo.insn_block[lo.inst().index()].index();
                let last = self.ctx.cfginfo.insn_block[hi.prev().inst().index()].index();
                let mut run: Option<CodeRange> = None;
                for block in first..=last {
                    let exits = self.func.block_succs(Block::new(block)).is_empty();
                    if !exits {
                        let b_from = core::cmp::max(lo, self.ctx.cfginfo.block_entry[block]);
                        let b_to = core::cmp::min(hi, self.ctx.cfginfo.block_exit[block].next());
                        match &mut run {
                            Some(r) => r.to = b_to,
                            None => {
                                run = Some(CodeRange {
                                    from: b_from,
                                    to: b_to,
                                })
                            }
                        }
                    }
                    if exits || block == last {
                        if let Some(r) = run.take() {
                            if self.preg_overlaps(preg_idx, r) {
                                return false;
                            }
                            pieces.push((entry.index, r));
                        }
                    }
                }
            }
        }
        if pieces.is_empty() && moved.is_empty() {
            // Nothing to gain from a change of register alone.
            return preg == cur;
        }
        // A value split down to single uses has a spill range that covers
        // those uses' own ranges. What is about to share `preg` must not
        // overlap itself.
        let mut taking: Vec<CodeRange> = pieces.iter().map(|&(_, r)| r).collect();
        for &other in moved.iter() {
            taking.extend(self.ctx.bundles[other].ranges.iter().map(|e| e.range));
        }
        if preg != cur {
            taking.extend(self.ctx.bundles[bundle].ranges.iter().map(|e| e.range));
        }
        taking.sort_unstable_by_key(|r| r.from);
        if taking.windows(2).any(|w| w[0].to > w[1].from) {
            return false;
        }

        // Commit.
        if preg != cur {
            moved.push(bundle);
        }
        for &other in moved.iter() {
            let old = self.ctx.bundles[other].allocation.as_reg().unwrap();
            for entry in &self.ctx.bundles[other].ranges {
                self.ctx.pregs[old.index()]
                    .allocations
                    .btree
                    .remove(&LiveRangeKey::from_range(&entry.range));
                self.ctx.pregs[preg.index()]
                    .allocations
                    .btree
                    .insert(LiveRangeKey::from_range(&entry.range), entry.index);
            }
            self.ctx.bundles[other].allocation = Allocation::reg(preg);
        }
        if pieces.is_empty() {
            return true;
        }
        let spill_bundle = self.ctx.spillsets[self.ctx.bundles[bundle].spillset].spill_bundle;
        let spill_reg = self.ctx.bundles[spill_bundle].allocation.as_reg();
        let mut tails: Vec<LiveRangeIndex> = Vec::new();
        // Pieces are in range order; several may come from one range.
        let mut i = 0;
        while i < pieces.len() {
            let orig = pieces[i].0;
            if let Some(r) = spill_reg {
                let range = self.ctx.ranges[orig].range;
                self.ctx.pregs[r.index()]
                    .allocations
                    .btree
                    .remove(&LiveRangeKey::from_range(&range));
            }
            // `cur` is the part of the original range not yet handled.
            let mut cur = orig;
            while i < pieces.len() && pieces[i].0 == orig {
                let piece = pieces[i].1;
                i += 1;
                if piece.from > self.ctx.ranges[cur].range.from {
                    let tail = self.split_range(cur, piece.from, spill_bundle);
                    self.keep_in_spill_bundle(cur, spill_bundle, spill_reg, &mut tails);
                    cur = tail;
                }
                let rest = if piece.to < self.ctx.ranges[cur].range.to {
                    Some(self.split_range(cur, piece.to, spill_bundle))
                } else {
                    None
                };
                // `cur` is now exactly the piece.
                let range = self.ctx.ranges[cur].range;
                self.ctx.ranges[cur].bundle = bundle;
                self.ctx.bundles[bundle]
                    .ranges
                    .push(LiveRangeListEntry { range, index: cur });
                self.ctx.pregs[preg.index()]
                    .allocations
                    .btree
                    .insert(LiveRangeKey::from_range(&range), cur);
                match rest {
                    Some(rest) => cur = rest,
                    None => {
                        cur = LiveRangeIndex::invalid();
                        break;
                    }
                }
            }
            if cur.is_valid() {
                self.keep_in_spill_bundle(cur, spill_bundle, spill_reg, &mut tails);
            }
        }
        // Rebuild the spill bundle's list from the ranges that still name it.
        let empty = LiveRangeList::new_in(self.ctx.bump());
        let mut kept = core::mem::replace(&mut self.ctx.bundles[spill_bundle].ranges, empty);
        kept.retain(|entry| self.ctx.ranges[entry.index].bundle == spill_bundle);
        for entry in kept.iter_mut() {
            entry.range = self.ctx.ranges[entry.index].range;
        }
        for index in tails {
            let range = self.ctx.ranges[index].range;
            kept.push(LiveRangeListEntry { range, index });
        }
        kept.sort_unstable_by_key(|entry| entry.range.from);
        self.ctx.bundles[spill_bundle].ranges = kept;
        self.ctx.bundles[bundle]
            .ranges
            .sort_unstable_by_key(|entry| entry.range.from);
        true
    }

    fn preg_overlaps(&self, preg: PRegIndex, range: CodeRange) -> bool {
        self.ctx.pregs[preg.index()]
            .allocations
            .btree
            .contains_key(&LiveRangeKey::from_range(&range))
    }

    /// Splits `lr` at `at`; the new range holds `at` onwards and belongs to
    /// `bundle`. Bundle lists and register maps are the caller's to update.
    fn split_range(
        &mut self,
        lr: LiveRangeIndex,
        at: ProgPoint,
        bundle: LiveBundleIndex,
    ) -> LiveRangeIndex {
        let old = self.ctx.ranges[lr].range;
        debug_assert!(old.from < at && at < old.to);
        let new_lr = self.ctx.ranges.add(
            CodeRange {
                from: at,
                to: old.to,
            },
            self.ctx.scratch_bump.clone(),
        );
        self.ctx.ranges[new_lr].vreg = self.ctx.ranges[lr].vreg;
        self.ctx.ranges[new_lr].bundle = bundle;
        let first_use = self.ctx.ranges[lr]
            .uses
            .iter()
            .position(|u| u.pos >= at)
            .unwrap_or(self.ctx.ranges[lr].uses.len());
        let mut rest_uses = UseList::new_in(self.ctx.bump());
        rest_uses.extend(self.ctx.ranges[lr].uses.iter().cloned().skip(first_use));
        self.ctx.ranges[new_lr].uses = rest_uses;
        self.ctx.ranges[lr].uses.truncate(first_use);
        self.ctx.ranges[lr].range.to = at;
        self.recompute_range_properties(lr);
        self.recompute_range_properties(new_lr);
        let vreg = self.ctx.ranges[new_lr].vreg;
        self.ctx.vregs[vreg].ranges.push(LiveRangeListEntry {
            range: self.ctx.ranges[new_lr].range,
            index: new_lr,
        });
        new_lr
    }

    /// Records that `lr`, a part of a split spill-bundle range, stays in
    /// the spill bundle.
    fn keep_in_spill_bundle(
        &mut self,
        lr: LiveRangeIndex,
        spill_bundle: LiveBundleIndex,
        spill_reg: Option<PReg>,
        tails: &mut Vec<LiveRangeIndex>,
    ) {
        let range = self.ctx.ranges[lr].range;
        debug_assert!(self.ctx.ranges[lr].bundle == spill_bundle);
        if let Some(r) = spill_reg {
            self.ctx.pregs[r.index()]
                .allocations
                .btree
                .insert(LiveRangeKey::from_range(&range), lr);
        }
        if !self.ctx.bundles[spill_bundle]
            .ranges
            .iter()
            .any(|entry| entry.index == lr)
        {
            tails.push(lr);
        }
    }

    /// Moves each bundle whose spillset's spill bundle got a register into
    /// that register, when the register is free for all of the bundle.
    ///
    /// Splitting trims a bundle to its uses and gives the rest of the
    /// value's live range to the spill bundle. The trimmed bundles are
    /// allocated first, the spill bundle last, and nothing but a hint ties
    /// their registers together; when the spill bundle cannot take the
    /// hinted register (it crosses a call that clobbers it, say) the value
    /// changes register wherever a trimmed bundle meets the spill bundle.
    /// For a value split before a loop and used inside it that is a move
    /// before the first use and a move back after the last one, on every
    /// iteration, although one register was free throughout.
    ///
    /// Only free space is taken, so no other bundle's allocation changes.
    /// Bundles that need a particular register or a limited set are left
    /// alone.
    fn move_bundles_to_spill_bundle_regs(&mut self, scratch: &mut LiveBundleVec) {
        for i in 0..self.ctx.bundles.len() {
            let bundle = LiveBundleIndex::new(i);
            let Some(old) = self.ctx.bundles[bundle].allocation.as_reg() else {
                continue;
            };
            let spillset = self.ctx.bundles[bundle].spillset;
            if spillset.is_invalid() {
                continue;
            }
            let spill_bundle = self.ctx.spillsets[spillset].spill_bundle;
            if spill_bundle.is_invalid() || spill_bundle == bundle {
                continue;
            }
            let Some(preg) = self.ctx.bundles[spill_bundle].allocation.as_reg() else {
                continue;
            };
            if self.ctx.pregs[preg.index()].is_stack {
                continue;
            }
            let preg_idx = PRegIndex::new(preg.index());
            if old == preg || self.ctx.bundles[bundle].limit.is_some() {
                continue;
            }
            if !matches!(self.compute_requirement(bundle), Ok(Requirement::Register)) {
                continue;
            }
            let old_idx = PRegIndex::new(old.index());
            self.ctx.bundles[bundle].allocation = Allocation::none();
            for entry in &self.ctx.bundles[bundle].ranges {
                self.ctx.pregs[old_idx.index()]
                    .allocations
                    .btree
                    .remove(&LiveRangeKey::from_range(&entry.range));
            }
            if let AllocRegResult::Allocated(_) =
                self.try_to_allocate_bundle_to_reg(bundle, preg_idx, None, scratch)
            {
                continue;
            }
            let back = self.try_to_allocate_bundle_to_reg(bundle, old_idx, None, scratch);
            debug_assert!(matches!(back, AllocRegResult::Allocated(_)));
        }
    }

    pub fn spillslot_can_fit_spillset(
        &mut self,
        spillslot: SpillSlotIndex,
        spillset: SpillSetIndex,
    ) -> bool {
        !self.ctx.spillslots[spillslot.index()]
            .ranges
            .btree
            .contains_key(&LiveRangeKey::from_range(
                &self.ctx.spillsets[spillset].range,
            ))
    }

    pub fn allocate_spillset_to_spillslot(
        &mut self,
        spillset: SpillSetIndex,
        spillslot: SpillSlotIndex,
    ) {
        self.ctx.spillsets[spillset].slot = spillslot;

        let res = self.ctx.spillslots[spillslot.index()].ranges.btree.insert(
            LiveRangeKey::from_range(&self.ctx.spillsets[spillset].range),
            spillset,
        );

        debug_assert!(res.is_none());
    }

    pub fn allocate_spillslots(&mut self) {
        const MAX_ATTEMPTS: usize = 10;

        for spillset in 0..self.ctx.spillsets.len() {
            trace!("allocate spillslot: {}", spillset);
            let spillset = SpillSetIndex::new(spillset);
            if !self.ctx.spillsets[spillset].required {
                continue;
            }
            let class = self.ctx.spillsets[spillset].class as usize;
            // Try a few existing spillslots.
            let mut i = self.ctx.slots_by_class[class].probe_start;
            let mut success = false;
            // Never probe the same element more than once: limit the
            // attempt count to the number of slots in existence.
            for _attempt in
                0..core::cmp::min(self.ctx.slots_by_class[class].slots.len(), MAX_ATTEMPTS)
            {
                // Note: this indexing of `slots` is always valid
                // because either the `slots` list is empty and the
                // iteration limit above consequently means we don't
                // run this loop at all, or else `probe_start` is
                // in-bounds (because it is made so below when we add
                // a slot, and it always takes on the last index `i`
                // after this loop).
                let spillslot = self.ctx.slots_by_class[class].slots[i];

                if self.spillslot_can_fit_spillset(spillslot, spillset) {
                    self.allocate_spillset_to_spillslot(spillset, spillslot);
                    success = true;
                    self.ctx.slots_by_class[class].probe_start = i;
                    break;
                }

                i = self.ctx.slots_by_class[class].next_index(i);
            }

            if !success {
                // Allocate a new spillslot.
                let spillslot = SpillSlotIndex::new(self.ctx.spillslots.len());
                self.ctx.spillslots.push(SpillSlotData {
                    ranges: self.ctx.scratch_spillset_pool.pop().unwrap_or_default(),
                    alloc: Allocation::none(),
                    slots: self.func.spillslot_size(self.ctx.spillsets[spillset].class) as u32,
                });
                self.ctx.slots_by_class[class].slots.push(spillslot);
                self.ctx.slots_by_class[class].probe_start =
                    self.ctx.slots_by_class[class].slots.len() - 1;

                self.allocate_spillset_to_spillslot(spillset, spillslot);
            }
        }

        // Assign actual slot indices to spillslots.
        for i in 0..self.ctx.spillslots.len() {
            self.ctx.spillslots[i].alloc = self.allocate_spillslot(self.ctx.spillslots[i].slots);
        }

        trace!("spillslot allocator done");
    }

    pub fn allocate_spillslot(&mut self, size: u32) -> Allocation {
        let mut offset = self.ctx.output.num_spillslots as u32;
        // Align up to `size`.
        debug_assert!(size.is_power_of_two());
        offset = (offset + size - 1) & !(size - 1);
        let slot = if self.func.multi_spillslot_named_by_last_slot() {
            offset + size - 1
        } else {
            offset
        };
        offset += size;
        self.ctx.output.num_spillslots = offset as _;
        Allocation::stack(SpillSlot::new(slot as usize))
    }
}
