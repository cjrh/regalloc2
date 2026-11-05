# Fixed-register uses in loop-exit blocks constrain the loop

This note describes a problem in the `ion` allocator. A loop with a
bounds check whose failure path calls a function before trapping gets extra
moves on every iteration. The note covers the root cause, a fix on branch
`cold-block-splits`, how the fix was checked, and what remains unchecked.
It ends with a draft issue and pull request description.

All line references are to base commit `4a23344` ("ion: reject a fixed def
that conflicts with a clobber (#266)") unless marked as being in the fix
commit `a4042a1`.

## Summary

A bounds-checked loop commonly lowers to the shape below. The failure path
calls a function with the index and length, then traps:

```rust
while lo < hi {
    if lo >= len { trap_index_oob(lo, len) }   // cold, never returns
    if hi >= len { trap_index_oob(hi, len) }   // cold, never returns
    s.swap(lo, hi);
    lo += 1;
    hi -= 1;
}
```

The x86-64 loop body is 12 instructions when the failure path only traps.
With the call, it is 19 instructions. The 7 extra instructions are register
moves that run on every iteration, although the call never runs on a
successful pass.

There are two causes:

1. Cranelift keeps cold blocks in IR order when it hands the function to
   regalloc2. It moves them to the end only at emission. The cold block
   therefore sits inside the loop's span of the linear block order. regalloc2
   gives it the loop's depth and hot spill weights.
2. A value that is live in the loop and used by the call in the cold block
   has one contiguous live range and one bundle covering both. The call's
   fixed-register use (`rdi` for the first argument) becomes the requirement
   of the whole bundle. The index then occupies `rdi` across the loop, and
   the values that `rdi` displaces are shuffled on every iteration.

The fix gives blocks with no successors loop depth 0. For each such block
entered from a deeper loop, a vreg's live ranges in that block get a bundle
of their own. The move into the fixed register then lands on the exit
edge, inside the cold block. With the fix, the loop body is 12 instructions
with the call and 12 without it.

## Reproductions

The files are in [`cold-exit-block-splits/`](cold-exit-block-splits/):

| File | Functions |
|---|---|
| `reverse.clif` | `%reverse_call` (the problem), `%reverse_trap` (control: trap only), `%reverse_shared_block` and `%reverse_shared_block_site` (a shared cold block with block parameters) |
| `dot.clif` | `%dot_call` (the problem), `%dot_trap` (control) |
| `early_return.clif` | `%find`: a return inside a loop, to check that ordinary code does not get worse |

Each file starts with Cranelift filetest headers:

```text
test compile precise-output
set opt_level=speed
set preserve_frame_pointers=true
target x86_64
```

The external callee is a test-case name, `%trap_index_oob`, with signature
`(i64, i64) system_v`.

### Commands that were run

`clif-util` was not available for this work (see
[Not verified](#not-verified)). The disassembly below comes from a small
harness, [`cold-exit-block-splits/harness/`](cold-exit-block-splits/harness/).
It parses each file with `cranelift_reader::parse_test`, builds the ISA from
the file's own `set` and `target` lines, compiles with
`cranelift-codegen` 0.137.0, and disassembles with Capstone. Its
`Cargo.toml` patches `regalloc2` to the checkout that contains it
(`path = "../../.."`), so the same command measures whichever commit is
checked out:

```sh
cd docs/cold-exit-block-splits
cargo run --release --manifest-path harness/Cargo.toml -- \
    reverse.clif dot.clif early_return.clif > out.txt
python3 harness/loops.py < out.txt
```

The docs directory does not exist at `4a23344`. To measure the base, copy
`docs/cold-exit-block-splits` into a worktree of `4a23344` and run the same
command there. Both runs were made this way, with exit status 0.

`loops.py` counts the instructions of one iteration of each innermost loop
along the fall-through path, from the backward branch's target to the
branch. It skips out-of-line code reached only through a conditional
exit.

### Equivalent `clif-util` and filetest commands (not run)

```sh
# In a wasmtime checkout whose cranelift-codegen is 0.137 (wasmtime v50.0.0),
# with regalloc2 patched to this checkout:
cargo run -p cranelift-tools --bin clif-util \
    --config 'patch.crates-io.regalloc2.path="/path/to/regalloc2"' \
    -- compile --target x86_64 -D --set opt_level=speed \
    --set preserve_frame_pointers=true reverse.clif

# As a filetest: copy the file into cranelift/filetests/filetests/isa/x64/,
# then fill in the expected output once and compare across commits.
CRANELIFT_TEST_BLESS=1 cargo run -p cranelift-tools --bin clif-util \
    --config 'patch.crates-io.regalloc2.path="/path/to/regalloc2"' \
    -- test cranelift/filetests/filetests/isa/x64/reverse.clif
```

### `%reverse_call`

```text
function %reverse_call(i64, i64, i64, i64) system_v {
    sig0 = (i64, i64) system_v
    fn0 = %trap_index_oob sig0
block0(v0: i64, v1: i64, v2: i64, v3: i64):
    ; v0 = base, v1 = len, v2 = lo, v3 = hi
    v100 = iconst.i64 3
    v101 = iconst.i64 1
    v4 = icmp slt v2, v3
    brif v4, block1(v2, v3), block6

block1(v5: i64, v6: i64):
    v7 = icmp ult v5, v1
    brif v7, block3, block2

block2 cold:
    call fn0(v5, v1)
    trap user1

block3:
    v8 = ishl v5, v100
    v9 = iadd v0, v8
    v10 = load.i64 notrap aligned v9
    v11 = icmp ult v6, v1
    brif v11, block5, block4

block4 cold:
    call fn0(v6, v1)
    trap user1

block5:
    v12 = ishl v6, v100
    v13 = iadd v0, v12
    v14 = load.i64 notrap aligned v13
    store notrap aligned v14, v9
    store notrap aligned v10, v13
    v15 = iadd v5, v101
    v16 = isub v6, v101
    v17 = icmp slt v15, v16
    brif v17, block1(v15, v16), block6

block6:
    return
}
```

Base `4a23344`, loop `0x13`–`0x5b`, 19 instructions:

```text
     0:  pushq %rbp
     1:  movq %rsp, %rbp
     4:  movq %rdi, %rax          ; base moved out of rdi
     7:  cmpq %rcx, %rdx
     a:  jge 0x60
    10:  movq %rdx, %rdi          ; lo lives in rdi, the call's first argument
    13:  cmpq %rsi, %rdi
    16:  jae 0x65
    1c:  movq (%rax, %rdi, 8), %rdx
    20:  movq %rcx, %r8           ; swap lo and hi so hi is in rdi
    23:  movq %rdi, %rcx
    26:  movq %r8, %rdi
    29:  cmpq %rsi, %rdi
    2c:  jae 0x73
    32:  movq (%rax, %rdi, 8), %r8
    36:  movq %r8, (%rax, %rcx, 8)
    3a:  movq %rdx, (%rax, %rdi, 8)
    3e:  addq $1, %rcx
    45:  subq $1, %rdi
    49:  cmpq %rdi, %rcx
    4c:  jge 0x60
    52:  movq %rcx, %rdx          ; and back, so lo is in rdi again
    55:  movq %rdi, %rcx
    58:  movq %rdx, %rdi
    5b:  jmp 0x13
    60:  movq %rbp, %rsp
    63:  popq %rbp
    64:  retq
    65:  movabsq $0, %rax
    6f:  callq *%rax
    71:  ud2
    73:  movabsq $0, %rax
    7d:  callq *%rax
    7f:  ud2
```

With the fix (`a4042a1`), loop `0xd`–`0x3d`, 12 instructions. This equals
`%reverse_trap` at either commit. Each cold path has one move:

```text
     0:  pushq %rbp
     1:  movq %rsp, %rbp
     4:  cmpq %rcx, %rdx
     7:  jge 0x43
     d:  cmpq %rsi, %rdx
    10:  jae 0x48
    16:  movq (%rdi, %rdx, 8), %rax
    1a:  cmpq %rsi, %rcx
    1d:  jae 0x59
    23:  movq (%rdi, %rcx, 8), %r8
    27:  movq %r8, (%rdi, %rdx, 8)
    2b:  movq %rax, (%rdi, %rcx, 8)
    2f:  addq $1, %rdx
    36:  subq $1, %rcx
    3a:  cmpq %rcx, %rdx
    3d:  jl 0xd
    43:  movq %rbp, %rsp
    46:  popq %rbp
    47:  retq
    48:  movq %rdx, %rdi          ; on the exit path only
    4b:  movabsq $0, %rax
    55:  callq *%rax
    57:  ud2
    59:  movq %rcx, %rdi          ; on the exit path only
    5c:  movabsq $0, %rax
    66:  callq *%rax
    68:  ud2
```

### `%dot_call`

Here `xs` is known to hold `n` elements, and `ys[i]` is checked against
`ys_len`:

```rust
for i in 0..n {
    if i >= ys_len { trap_index_oob(i, ys_len) }
    t = t.wrapping_add(xs[i] * ys[i]);
}
```

```text
function %dot_call(i64, i64, i64, i64) -> i64 system_v {
    sig0 = (i64, i64) system_v
    fn0 = %trap_index_oob sig0
block0(v0: i64, v1: i64, v2: i64, v3: i64):
    ; v0 = xs, v1 = ys, v2 = n, v3 = ys_len
    v100 = iconst.i64 3
    v101 = iconst.i64 1
    v4 = iconst.i64 0
    v5 = icmp sgt v2, v4
    brif v5, block1(v4, v4), block4(v4)

block1(v6: i64, v7: i64):
    v8 = icmp ult v6, v3
    brif v8, block3, block2

block2 cold:
    call fn0(v6, v3)
    trap user1

block3:
    v9 = ishl v6, v100
    v10 = iadd v0, v9
    v11 = load.i64 notrap aligned v10
    v12 = iadd v1, v9
    v13 = load.i64 notrap aligned v12
    v14 = imul v11, v13
    v15 = iadd v7, v14
    v16 = iadd v6, v101
    v17 = icmp slt v16, v2
    brif v17, block1(v16, v15), block4(v15)

block4(v18: i64):
    return v18
}
```

Base: the loop is 16 instructions. `i` takes `rdi`, so `xs`, which
arrives in `rdi`, is swapped between `rcx` and `rsi` twice per iteration:

```text
    26:  cmpq %rcx, %rdi
    29:  jae 0x6a
    2f:  movq %rcx, %r9
    32:  movq %rsi, %rcx
    35:  movq %r9, %rsi
    38:  movq (%rcx, %rdi, 8), %r9
    3c:  movq (%r8, %rdi, 8), %r10
    40:  addq $1, %rdi
    47:  imulq %r10, %r9
    4b:  addq %r9, %rax
    4e:  cmpq %rdx, %rdi
    51:  jge 0x65
    57:  movq %rcx, %r9
    5a:  movq %rsi, %rcx
    5d:  movq %r9, %rsi
    60:  jmp 0x26
```

Fixed: the loop is 9 instructions, the same as `%dot_trap`:

```text
    23:  cmpq %rcx, %rsi
    26:  jae 0x50
    2c:  movq (%rdi, %rsi, 8), %r9
    30:  movq (%r8, %rsi, 8), %r10
    34:  addq $1, %rsi
    3b:  imulq %r10, %r9
    3f:  addq %r9, %rax
    42:  cmpq %rdx, %rsi
    45:  jl 0x23
    ...
    50:  movq %rsi, %rdi          ; exit path
    53:  movq %rcx, %rsi          ; exit path
    56:  movabsq $0, %rax
    60:  callq *%rax
    62:  ud2
```

## Root cause

These are the steps, traced with `regalloc2/trace-log` on `%dot_call` at
base. VCode block 4 is the cold call block. It sits between the loop check
(block 3) and the loop body (block 5).

1. **Cold blocks stay inside the loop span.** Cranelift lowers blocks in
   their layout order and moves cold blocks to the end only when emitting
   code (`cranelift-codegen` 0.137.0, `src/machinst/vcode.rs` ~759, via
   `BlockLoweringOrder::is_cold`). regalloc2's `Function` trait carries no
   coldness, so the allocator sees the cold block in the loop's span.
   Cranelift also reports a block ending in a trap as a return
   (`vcode.rs` ~1574–1581), so the block has no successors.

2. **Such blocks get the loop's depth.** `CFGInfo::init` computes
   `approx_loop_depth` from backedges in linear order and pushes the current
   depth for every block (`src/cfg.rs:128`–`137`). A cold block between a
   loop header and its backedge gets depth 1. `spill_weight_from_constraint`
   (`src/ion/liveranges.rs:36`–`51`) then adds a hot bonus of
   `1000 * 4^depth` (line 44) to the cold block's uses, the same as in-loop
   uses. In the trace, the call's use of `i` has weight 35703, equal to
   `i`'s uses in the loop.

3. **One contiguous range covers the loop and the cold block.**
   `add_liverange_to_vreg` (`src/ion/liveranges.rs:148`) adds ranges in
   reverse order. It merges a new range into the vreg's last range whenever
   they touch (lines 190–197). The index is live from the loop check
   through the cold block into the body, so its ranges join:
   `LiveRangeIndex(13)` covers `progpoint7`–`progpoint10`, from the check
   through the call.

4. **One bundle per vreg.** `merge_vreg_bundles` (`src/ion/merge.rs:259`)
   starts by putting all of a vreg's ranges in one bundle. Block-parameter
   merges and reuse merges (lines 343–420) then join further vregs into it.

5. **The fixed use becomes the whole bundle's requirement.**
   `compute_requirement` (`src/ion/requirement.rs:132`) merges the
   requirements of all uses in the bundle. In the trace, bundle 4 (`v200`,
   which is `i`) gets `FixedReg(p7)` (`rdi`) from the call's use at
   `progpoint10`. Its in-loop uses at `progpoint12`–`14` then report
   `req FixedReg(PReg(hw = 7 ...))`, giving `final: FixedReg(rdi)`.

6. **The loop pays for it.** `process_bundle` (`src/ion/process.rs:975`)
   allocates `rdi` to the bundle. `xs` arrives in `rdi` (`v192`, a fixed
   def of `p7` in block 0) and conflicts with it. Splitting
   (`split_and_requeue_bundle`, `src/ion/process.rs:412`, and the
   fixed-register handling around lines 1046 and 1222) lands the splits in
   the loop, not at the cold block's entry, because the cold block lies in
   the middle of the contiguous range. The resulting moves run on every
   iteration.

An existing mechanism handles one form of conflicting fixed uses:
`MultiFixedRegFixup` with `FixedRegFixupLevel::Initial`
(`src/ion/liveranges.rs:539`–`545`). It rewrites a conflicting Early fixed
use to `Any` and inserts a move at the use. It applies only when the same
instruction has conflicting constraints, or the preg is clobbered or
fixed-defined there. A single fixed use in a cold block does not trigger
it.

## The change

Commit `a4042a1` on branch `cold-block-splits`. It touches 3 files, with
165 insertions and 61 deletions.

### `src/cfg.rs`

* **Depth 0 for blocks without successors** (`a4042a1` `src/cfg.rs:144`–`155`).
  A block with no successors cannot reach a backedge. It runs at most once
  per call, wherever the block order puts it. Its uses are no longer
  weighted as hot. On its own this only changes spill weights, and it does
  not fix the reproductions. The bundle in step 5 still carries the fixed
  requirement.
* **`CFGInfo::loop_exit_block`** (`a4042a1` `src/cfg.rs:46`, computed at
  `:167`–`177`). This flag marks a block that has no successors and has a
  predecessor at a greater loop depth: an exit from a loop. The flag is
  computed once and used by the two parts below.

### `src/ion/liveranges.rs`

* **`starts_loop_exit_block`** (`a4042a1` `:148`), used in
  `add_liverange_to_vreg` (`:208`). Ranges are not coalesced across the
  entry of a loop-exit block, so the exit block's part of a vreg's
  liveness is its own `LiveRange`. Without this, step 3 leaves a single
  range, and no bundle boundary could fall at the exit block.

### `src/ion/merge.rs`

* **Separate bundles in `merge_vreg_bundles`** (`a4042a1` `:330`). Each vreg
  gets one main bundle for its ranges outside loop-exit blocks, plus one
  bundle per loop-exit block containing its ranges there. All of a vreg's
  bundles share one spill set, as split bundles do. Bundle properties
  (cached fixed and fixed-def flags, stack constraint, `Limit`) are computed
  per bundle by the new `init_vreg_bundle_props` (`:269`). This step moves
  the cold block's fixed use out of the loop bundle's requirement.
  `loop_exit_block_of` (`:264`) maps a range to its exit block.
* **Reuse merges look up the bundle at the instruction** (`vreg_bundle_at`,
  `:316`). A reuse constraint in an exit block must merge the exit-block
  bundles of the two vregs, not their main bundles. Otherwise the exit
  block's constraints would join the loop bundle again through the merge.
* **No block-parameter merge into a loop-exit block** (`:427`). Merging a
  branch argument with an exit block's parameter would join the loop
  value's bundle to the exit block's again. The edge moves for those
  parameters are inserted on the exit edge, as for any unmerged block
  parameter.

The allocator already connects split bundles of one vreg with moves at
block boundaries. For an exit block with a single predecessor that has two
successors, that move goes at the start of the exit block, so it is off the
loop's path. No new move-insertion code is needed.

### An earlier attempt that was not enough

The first attempt, branch `cold-block-splits-stage1` (`3212505`), kept one
bundle per vreg. It reused the existing fixup mechanism instead. It made
the depth-0 change. For a fixed Early use in a loop-exit block of a vreg
live into that block, it rewrote the use to `Any` and recorded a
`MultiFixedRegFixup` at `FixedRegFixupLevel::Initial` to insert the move.

It fixed some shapes and made others worse:

| Shape | Base | Stage 1 |
|---|---|---|
| Call passing the index only | 20 | 12 |
| Call passing the index in the first check only | 15 | 12 |
| Call passing the length only | 19 | 16 |
| Operands stored to a stack slot, call with a constant then the slot address | 15 | 18 |
| One shared cold block with block parameters | 17 | 19 |
| A cold block per check, index as block parameter | 20 | 21 |

The vreg's range still ran contiguously through the exit block, and the
exit block's other constraints still applied to the loop bundle. These
included the reserved preg at the fixup and the call's clobbers inside the
range. The allocator therefore still split inside the loop. The
regressions were not analyzed further. Separate bundles remove the exit
block from the loop bundle entirely, and they gave the results below.

## Verification

### regalloc2's own tests

All runs are at `a4042a1`:

| Command | Result |
|---|---|
| `cargo test` | 10 passed |
| `cargo test --features fuzzing` (includes the 1 s ion checker smoke test) | 14 passed |
| `cargo clippy` | no new warnings (5 existing warnings in unchanged code) |

### Ion checker fuzzing

The fuzz targets under `fuzz/` need `cargo fuzz` and nightly, and were not
run. Instead, a small driver calls `regalloc2::fuzzing::ion::check` in a
loop through `arbtest`, for a fixed time per process. It is built in
release mode with `debug-assertions` and `overflow-checks`. Each process is
pinned to one core.

The existing generator does not exercise the change. In
`Func::arbitrary_with_options` (`src/fuzzing/func.rs` ~446–449), every
side-exit block gets an edge back into the CFG, and only the last block
returns. So no generated function has a block without successors inside a
loop.

Branch `cold-block-splits-fuzzgen` (`425fe0a`, 8 lines on top of `a4042a1`)
adds `Options::exit_blocks`. With it, a side-exit block may end in a return
instead. The ion fuzzer enables it. Over 5 s of generation, 3,563 of 9,333
functions (38%) contained at least one return block inside a backedge's
span, 11,373 such blocks in all.

| Allocator | Generator | Duration | Cases | Result |
|---|---|---|---|---|
| `a4042a1` | existing | 4 × 300 s | not counted | all passed |
| `a4042a1` | with `exit_blocks` | 4 × 300 s | 786,534 | all passed |
| `4a23344` | with `exit_blocks` (checks the generator) | 1 × 120 s | 103,043 | all passed |
| `2b063fd` (refinement below) | with `exit_blocks` | 4 × 180 s | 489,979 | all passed |

Driver (`main.rs`):

```rust
use arbitrary::Arbitrary;
use regalloc2::fuzzing::ion;

fn main() {
    let ms: u64 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(60_000);
    let mut cases = 0u64;
    arbtest::arbtest(|u| {
        let t = ion::TestCase::arbitrary(u)?;
        ion::check(t);
        cases += 1;
        Ok(())
    })
    .budget_ms(ms);
    println!("ok, {cases} cases");
}
```

```toml
[dependencies]
regalloc2 = { path = "/path/to/regalloc2", features = ["fuzzing"] }
arbtest = "0.3.2"
arbitrary = "1"

[profile.release]
debug-assertions = true
overflow-checks = true
```

```sh
for i in 0 1 2 3; do taskset -c $i ./target/release/driver 300000 & done; wait
```

### Per-iteration instruction counts

Each count is for one iteration of the innermost loop, measured as described
in [Commands that were run](#commands-that-were-run).

The reproductions in this directory:

| Function | Base `4a23344` | Fix `a4042a1` |
|---|---|---|
| `%reverse_call` | 19 | 12 |
| `%reverse_trap` (control) | 12 | 12 (identical code) |
| `%reverse_shared_block` | 12 | 12 (identical code) |
| `%reverse_shared_block_site` | 14 | 14 |
| `%dot_call` | 16 | 9 |
| `%dot_trap` (control) | 9 | 9 (identical code) |
| `%find` (return inside the loop) | 6 | 6, but the return path gains one move (see [Risks](#risks)) |

A wider set of variants of the same reversal loop was measured with the
same Cranelift version and flags (`opt_level=speed`, `is_pic`,
`preserve_frame_pointers`, `tail` calling convention for the function). It
took the slice as an `i128` pair, and each variant changes what the cold
block does:

| Cold-block shape | Base | Fix |
|---|---|---|
| Trap only; call with no arguments; store index or index and length to a stack slot, then trap; stack slot, then call with one constant | 12 | 12 |
| Call with the length only | 19 | 12 |
| Call with the index only | 20 | 12 |
| Call with the index, first check only | 15 | 12 |
| Store operands to a global, then call with one constant | 15 | 12 |
| Store operands to a stack slot, call with the slot address and a constant | 15 | 12 |
| Store operands to a global, then trap (no call) | 15 | 12 |
| Loop as a frontend emits it unrotated, call with index and length | 22 | 13–14 |
| Loop rotated, call with index and length | 20 | 12 |
| A cold block per check with the index as block parameter | 20 | 12 |
| Unrotated, stack-slot record | 15 | 13 |
| Stack-slot record, call with a constant then the slot address | 15 | 12 |
| One shared cold block, index and a per-site constant as block parameters | 17 | 17 |
| `trapz` checks; check hoisted before the loop; unchecked loop top | 12 / 8 / 9 | unchanged |

The "store to a global, then trap" variant has no fixed-register use in
the cold block. Base still adds a per-iteration move of the base pointer
there, and the fix removes it. I did not analyze this case.

### Downstream results

These results come from a compiler frontend that emits bounds-checked loops
through Cranelift 0.137 with this regalloc2 patched in. It is x86-64,
measured with `perf stat -e instructions:u` and pinned to 8 cores.

* That frontend had worked around the problem by storing the index and
  length to a stack slot and passing its address. With the fix, passing them
  as call arguments gives the same loop code as the workaround, for each
  kernel measured.
* A dot-product loop with one checked index fell from 19 to 17 instructions
  per iteration. The 2 remaining moves come from x86-64's one-operand
  `imul`, used for overflow-checked multiply, which clobbers `rdx`. They are
  unrelated to this change.
* fannkuch-redux, n=10, multithreaded: 3 runs each, and the output was
  identical for all builds:

  | Build | Instructions |
  |---|---|
  | base | 2.01947e9 |
  | fix, with the stack-slot workaround still in place | 2.01948e9 (+0.0007%) |
  | fix, operands passed as arguments | 2.00701e9 (−0.6%) |

* Compile time of a large program in that frontend, with its function cache
  disabled, 3 runs each: base 1.9893–1.9909e9 instructions, fix
  1.9896–1.9904e9. The ranges overlap, so the difference is within noise.
* The frontend's test suite (5,462 integration tests, many of which compile and
  run generated code) passed with the fix.

## Risks

* **Extra moves on exit paths.** Each loop-exit block that uses a loop value
  gets an extra bundle and usually a move at its entry. Those paths run at
  most once per call.
* **Exit blocks outside the loop span.** Cranelift already lays out some
  in-loop returns after the loop. `%find` is an example: its found-path
  return is the last VCode block, so base already gave it depth 0. The
  fix still marks it as a loop exit, because its predecessor is deeper.
  The separate bundle then loses a free choice. At base the counter stayed
  in `rax`, the return register, at no cost. With the fix, the loop is
  unchanged at 6 instructions, but the return path gains
  `movq %r8, %rax`.

  Branch `cold-block-splits-refine` (`2b063fd`, on top of
  `cold-block-splits-fuzzgen`) marks only exit blocks inside a loop's linear
  span. That restores base's code for `%find`, and every other count in
  the tables above is unchanged. `cargo test --features fuzzing` passes:
  14 tests. Its fuzz results are listed in the fuzzing table. It is a
  candidate to fold into the main change.
* **Late uses at the last instruction of an exit block.** The no-coalescing
  check is at the exit block's entry only. A range whose use is Late on the
  exit block's last instruction ends at the next block's entry. If the vreg
  is live in that next block, the two ranges would coalesce, and
  `loop_exit_block_of` would assign the merged range to the exit bundle.
  The result is still a valid allocation, because bundles of one vreg may
  partition its ranges in any way. It may not be the intended one.
  Cranelift's return and trap operands are Early uses, so this was not
  observed.
* **Spill weights in ordinary returns.** Depth 0 applies to every block
  without successors, including returns that the block order places inside
  a loop's span. Those blocks do run at most once per call. Still, this
  changes spill weights and therefore allocation in code without a cold
  call.
* **Compile time.** The extra bundles and ranges are proportional to the
  number of vregs live into loop-exit blocks. `vreg_bundle_at` scans a
  vreg's ranges, but only for instructions in loop-exit blocks. No change in
  compile time was measured in the downstream build. Sightglass was not
  run.
* **Untested interactions:** `allow_multiple_vreg_defs`; non-x86 targets;
  the `fastalloc` allocator, which is unaffected, since the change is in
  `ion` and in `cfg.rs` depth only (`fastalloc` does not use
  `approx_loop_depth`; not verified).

## Not verified

* **Cranelift filetests and `clif-util`.** Building a wasmtime checkout was
  not permitted in the environment used for this work. The commands are
  under
  [Equivalent `clif-util` and filetest commands](#equivalent-clif-util-and-filetest-commands-not-run).
  The full x64, aarch64, riscv64 and s390x filetest suites
  (`cargo run -p cranelift-tools --bin clif-util -- test cranelift/filetests/filetests`,
  with regalloc2 patched) need to be run. Many `precise-output` expectations
  are likely to change, since depth 0 for returns alters spill weights.
  Each change should be reviewed.
* **Sightglass** and wasmtime's own test suite.
* **`cargo fuzz`** targets (`fuzz/fuzz_targets/ion.rs`,
  `ion_checker`-style targets) under libFuzzer. Only the `arbtest` driver
  above was run.
* **The `clif-util` header form.** The `test compile precise-output` and
  `target x86_64` headers parse with `cranelift_reader::parse_test`. I did
  not check them with `clif-util test`, which would also require the
  expected-output comments.

## Draft upstream issue

> **Title:** ion: fixed-register uses in a loop's trap/return blocks force
> that register across the loop
>
> A bounds-checked loop whose failure path calls a reporting function
> before trapping gets per-iteration register moves, even though the call
> never runs on a successful pass.
>
> Reproduction (x86-64, `opt_level=speed`): `%reverse_call` below compiles
> to a 19-instruction loop body. The same loop with `trap` instead of
> `call fn0(...); trap` compiles to 12. The extra 7 instructions are moves
> that keep the loop index in `rdi`, the call's first argument register.
> A dot product with one checked index goes from 9 to 16 instructions.
>
> *(paste `%reverse_call` from `reverse.clif`)*
>
> Cause: Cranelift keeps cold blocks in layout order for the allocator, so
> the cold block lies inside the loop's linear span. `approx_loop_depth`
> gives it the loop's depth (cfg.rs:128–137). The index's live range is
> coalesced through the cold block (`add_liverange_to_vreg`), and
> `merge_vreg_bundles` puts it in one bundle. `compute_requirement` then
> makes the call's `FixedReg(rdi)` the requirement of the whole bundle,
> including its in-loop uses, so conflicting values split and move inside
> the loop.
>
> Frontends can work around this by passing the operands through memory,
> but that costs a stack frame and makes the failure call harder to write.

## Draft pull request

> **Title:** ion: keep loop-exit blocks from constraining a value's in-loop
> allocation
>
> A block with no successors (a return, or a trap after a call) entered
> from inside a loop runs at most once per call. Because it sits inside the
> loop's span in linear order, it was given the loop's depth. Its
> fixed-register uses also joined the value's single bundle, so the whole
> in-loop range required that register.
>
> This change:
>
> * gives blocks without successors loop depth 0 (`cfg.rs`);
> * adds `CFGInfo::loop_exit_block` for such blocks entered from a deeper
>   loop, and does not coalesce live ranges across their entry
>   (`liveranges.rs`);
> * gives each vreg a separate bundle, sharing its spill set, for its ranges
>   in each loop-exit block, and resolves reuse and block-parameter merges
>   accordingly (`merge.rs`).
>
> The move into the exit block's fixed register is then inserted at the
> start of the exit block, off the loop's path. No `Function` API change is
> needed.
>
> Results on x86-64 (one loop iteration):
>
> | | Before | After |
> |---|---|---|
> | Reversal, call(index, len) on failure | 19 | 12 |
> | Dot product, call(index, len) on failure | 16 | 9 |
> | Same loops with trap only | 12 / 9 | 12 / 9 (identical code) |
>
> Testing: `cargo test`, `cargo test --features fuzzing`, and about 790k ion
> checker cases. The cases come from a generator extended to put return
> blocks inside loops (`Options::exit_blocks`, included), since the
> existing generator never produces them. Filetest expectations that
> change are updated in the accompanying wasmtime PR.
>
> Known costs: one move on each exit path that uses a loop value. Possible
> spill-weight changes for returns placed inside a loop's span.
>
> Possible follow-up: a `Function::block_is_cold` hint, which Cranelift
> could implement from `BlockLoweringOrder::is_cold`. It would extend this
> to cold blocks that rejoin the loop. It would also cover a cold block
> shared by several checks, whose block-parameter moves still run on the
> in-loop edges.
