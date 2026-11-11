# A value spilled around a loop is reloaded on every iteration

Follows `spill-bundle-registers.md`, which left this case open under "Not
fixed". The change is in `src/ion/spill.rs` (`extend_bundles_over_loops`),
with loop spans added to `src/cfg.rs`.

## Summary

Splitting trims each piece of a value to its uses and gives the rest of the
live range, including loop headers and back edges, to the value's spill
bundle. When the spill bundle gets a stack slot, a loop that uses the value
loads it from the slot before the use and, at the back edge, hands it back
to the slot, on every iteration, even when a register is free for the whole
loop.

After allocation, a new pass gives the piece's register to the spill
bundle's ranges inside the loop when the register is free there. The load
then happens once on entry to the loop and the store once on each exit.

## Reproduction

`ion::tests::value_spilled_around_a_loop_is_reloaded_at_loop_entry`: a loop
whose span holds an exit block with a call that clobbers every register
while the value is live. Without the pass the loop holds
`Move { from: p2i, to: stack0 }`; with it, no edit.

In Plenty's fasta, the per-symbol loop of `random_fasta` held, per symbol,

```text
mov  0xA8(%rsp),%r12      ; reload
...
mov  0xC0(%rsp),%rax      ; reload
...
mov  %r12,0xA8(%rsp)      ; store back, value unchanged
mov  %rax,0xC0(%rsp)      ; store back, value unchanged
```

and now holds none of them: both values are loaded before the loop, the
second into `r10`, which nothing else in the loop uses.

## The rule

For each bundle B that has a register `r` and whose spill bundle has ranges
elsewhere:

1. **Which loop.** The innermost loop L with a use of B in a block that
   dominates every source of a backedge to L's header. A parent loop is
   tried afterwards under the same test.
2. **What is taken.** Every range of B's spillset inside L's span of the
   block order, except in blocks without successors. The spill bundle's
   ranges are split at the span's edges; another register bundle of the
   spillset that lies wholly inside is moved.
3. **All or nothing.** `r` must be free for all of it. Otherwise each other
   register is tried that is free for all of it and for B itself, B moving
   with it; a non-preferred register that nothing in the function uses is
   not tried, since using it can cost a save and restore. If none fits,
   nothing changes.

Only free space is taken, so no other value's allocation changes, and B is
not moved unless something is gained.

### Uses on cold paths

The extension trades moves inside the loop for one load on each entry and
one store on each exit. Rule 1 is what makes that a gain: a use in a block
that dominates all backedge sources runs on every iteration that repeats, so
each such iteration saves at least one load, and a loop that runs `n`
iterations saves at least `n - 1` for the one it pays on entry. A use on a
conditional path inside the loop, or in a trap or return block inside its
span (those have loop depth 0), does not select the loop. Such a value
keeps its slot and is loaded only where it is used.

### Self-overlap

When a value has been split down to single uses
(`split_into_minimal_bundles`), its spill range covers those uses' own
ranges. Taking it into one register would put the value there twice. The
pass rejects any extension whose parts overlap each other.

## Verification

* `cargo test`: 12 passed (one new). `cargo test --features fuzzing`: 16
  passed. `cargo fmt --check` clean; `cargo clippy` no new warnings.
* Ion checker through an `arbtest` driver with debug assertions, 4 × 300 s
  on this source: 852,268 cases, no failure. Two earlier forms of the pass
  failed this driver within a minute (a spill range whose `vreg` was never
  recorded, fixed in `process.rs`; and the self-overlap above); both seeds
  pass now.

Downstream numbers (Plenty's benchmark table, kernels, static loop lengths
and compile cost) are in the Plenty PR that pins this change.

## Risks

* **Entry and exit moves.** A loop entered many times and iterated once
  each time gains nothing and pays the entry load where it previously paid
  the load at the use; it is not worse. A loop left through many exits
  stores the value at each.
* **Register held across the loop.** Allocation is over, so the register
  taken was free; no later decision can want it.
* **A move where there was none.** When B changes register, a neighbour
  that shared its old register by hint now needs a move at that boundary.
  Not observed: no loop grew in the 100 Plenty programs compared.
* **Compile time.** One pass over the bundles and their uses. Switched on
  and off inside one binary it costs 0.3% of Plenty's compile instructions;
  two separately built compilers differ by 0.9%, the rest being different
  inlining in unrelated code.
* **Loop shape.** Loops are spans of the block order between a backedge's
  target and its last source, as for `approx_loop_depth`. A block placed in
  the span that is not part of the loop is treated as part of it.
* **Untested:** non-x86 targets; `allow_multiple_vreg_defs`. `fastalloc` is
  unaffected.

## Not verified

Cranelift filetests, Sightglass, wasmtime's suite, and `cargo fuzz` under
libFuzzer, as for the previous change.
