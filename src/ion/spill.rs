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

use super::{
    AllocRegResult, Env, LiveBundleIndex, LiveBundleVec, LiveRangeKey, PRegIndex, RegTraversalIter,
    Requirement, SpillSetIndex, SpillSlotData, SpillSlotIndex,
};
use crate::{Allocation, Function, SpillSlot};

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
