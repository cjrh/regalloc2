# Split bundles do not follow their spill bundle's register

This note describes a problem in the `ion` allocator. A value that is split
before a loop and used inside it can change register on every iteration,
although one register is free for the whole of its live range. The note
covers the root cause, a fix on branch `issue-939-loop-splits`, how the fix
was checked, the alternatives that were measured and rejected, and what
remains.

All line references are to base commit `bb8bf74` (branch `fork`).

## Summary

```rust
let len = buffer_len(b);      // call
let rows = buffer_rows(b);    // call
let mut i = 0;
loop {
    if i >= len { trap }
    *rows.add(i) = byte;
    i += 1;
    if i >= n { break }
}
```

`n` and `byte` are parameters, live across both calls. `len` is the first
call's result, live across the second. On x86-64 the loop is 11 instructions
at base: 6 for the work, 4 register moves and an unconditional jump. With
the fix it is 6.

Each of the three values is split at a call. Splitting trims the piece
that holds the uses to those uses; the rest of the live range goes to the
value's spill bundle. The pieces are allocated first and take caller-saved
registers. The spill bundles are allocated last; they cross the calls, so
they take callee-saved registers. Nothing then brings the two together. The
value sits in the spill bundle's register on entry to each iteration, moves
to the piece's register before its first use and moves back after its last.

The fix is a pass after the spill bundles have their registers: each other
bundle of the spillset moves into the spill bundle's register when that
register is free for all of the bundle. It takes free space only, so no
other bundle's allocation changes.

## Reproduction

[`spill-bundle-registers/fill.clif`](spill-bundle-registers/fill.clif),
compiled with the harness of the loop-exit note
([`cold-exit-block-splits/harness/`](cold-exit-block-splits/harness/)), whose
`Cargo.toml` patches `regalloc2` to the checkout that contains it:

```sh
cd docs
cargo run --release --manifest-path cold-exit-block-splits/harness/Cargo.toml -- \
    spill-bundle-registers/fill.clif > out.txt
python3 cold-exit-block-splits/harness/loops.py < out.txt
```

To measure the base, copy `docs/` into a worktree of `bb8bf74` and run the
same command there.

```text
function %fill(i64, i64, i64) -> i64 system_v {
    sig0 = (i64) -> i64 system_v
    fn0 = colocated u0:1 sig0
    fn1 = colocated u0:2 sig0
block0(v0: i64, v1: i64, v2: i64):
    ; v0 = b, v1 = n, v2 = byte
    v3 = call fn0(v0)
    v4 = call fn1(v0)
    v5 = iconst.i64 0
    jump block1(v5)
block1(v6: i64):
    v7 = icmp ult v6, v3
    brif v7, block2, block3
block3 cold:
    trap user1
block2:
    v8 = iadd v4, v6
    v11 = ireduce.i8 v2
    store.i8 notrap aligned v11, v8
    v12 = iconst.i64 1
    v9 = iadd v6, v12
    v10 = icmp slt v9, v1
    brif v10, block1(v9), block4
block4:
    return v9
}
```

Base `bb8bf74`, loop `0x3d`–`0x66`, 11 instructions:

```text
    1b:  movq %rdi, %rbx
    1e:  movq %rdx, %r12          ; byte: spill bundle in r12
    21:  movq %rsi, %r13          ; n: spill bundle in r13
    24:  callq 0x29
    29:  movq %rbx, %rdi
    2c:  movq %rax, %r14          ; len: spill bundle in r14
    2f:  callq 0x34
    34:  movq %rax, %rdx
    37:  xorq %rax, %rax
    3a:  movq %r14, %rcx          ; len's piece is in rcx
    3d:  cmpq %rcx, %rax
    40:  jae 0x87
    46:  movq %r12, %rsi          ; byte to its piece's register
    49:  movb %sil, (%rdx, %rax)
    4d:  addq $1, %rax
    54:  movq %r13, %rdi          ; n to its piece's register
    57:  cmpq %rdi, %rax
    5a:  jge 0x6b
    60:  movq %rsi, %r12          ; and back
    63:  movq %rdi, %r13
    66:  jmp 0x3d
```

With the fix, loop `0x3a`–`0x51`, 6 instructions:

```text
    1b:  movq %rdi, %rbx
    1e:  movq %rdx, %r12
    21:  movq %rsi, %r13
    24:  callq 0x29
    29:  movq %rbx, %rdi
    2c:  movq %rax, %r14
    2f:  callq 0x34
    34:  movq %rax, %rdx
    37:  xorq %rax, %rax
    3a:  cmpq %r14, %rax
    3d:  jae 0x73
    43:  movb %r12b, (%rdx, %rax)
    47:  addq $1, %rax
    4e:  cmpq %r13, %rax
    51:  jl 0x3a
```

## Root cause

1. `byte` (`v2`) arrives in `rdx`, a fixed-register def, and both calls
   clobber `rdx`. Its bundle cannot be allocated whole and is split at the
   conflict (`split_and_requeue_bundle`, `src/ion/process.rs` 412).
2. The split passes `trim_ends_into_spill_bundle` (`process.rs` 645–760).
   Empty space after the first half's last use and before the second half's
   first use becomes use-free live ranges in the spillset's spill bundle.
   The half with the loop's use shrinks to that use. The spill bundle now
   holds the range across the calls, the loop header up to the use, and the
   body from the use to the back edge.

   The split point itself is hoisted out of a loop (`process.rs`
   1320–1335), but trimming moves the bundle's start back in to the first
   use, so the hoist does not decide where the value changes location.
3. The queue allocates the trimmed bundles. The one in the loop does not
   cross a call; it takes a caller-saved register (`rsi`).
4. `try_allocating_regs_for_spilled_bundles` (`src/ion/spill.rs` 22) runs
   after the queue is empty. The only tie to the trimmed bundles is
   `spillsets[..].hint`, the register of the bundle allocated last. The
   spill bundle crosses the calls, so the hinted register conflicts and the
   traversal gives it the first one that is free: callee-saved `r12`.
5. The value is now in `r12` in the header and on the back edge and in `rsi`
   at the use. Move insertion connects them on every iteration: `r12` to
   `rsi` before the use, `rsi` to `r12` after it. The jump at the end of
   the loop is the block those back-edge moves need.

`r12` was free over the trimmed bundle's range all along: the spill bundle
holds it on both sides, and the value is the same one. The allocator never
looks at that pairing. A trimmed bundle is allocated before its spill
bundle, so it cannot ask; the spill bundle is allocated by probing registers
for itself only.

The same happens when the spill bundle gets no register. The value then
lives in its stack slot around the loop and is reloaded before the first
use and stored back after the last one on every iteration. The fix below
does not cover that case; see [Not fixed](#not-fixed).

## The change

`move_bundles_to_spill_bundle_regs` (`src/ion/spill.rs`), called at the end
of `try_allocating_regs_for_spilled_bundles`. For each bundle that

* has a register,
* belongs to a spillset whose spill bundle has a different, non-stack
  register,
* has no register limit and has requirement `Register` (no fixed register,
  no stack-only use),

it removes the bundle's ranges from its register's allocation map and tries
the spill bundle's register with `try_to_allocate_bundle_to_reg`. On a
conflict it puts the bundle back where it was. One pass over the bundles,
one B-tree probe sequence per candidate.

Properties:

* No other bundle changes allocation. A register is taken only where it is
  free.
* The rule does not mention loops, calls or block kinds. It removes a
  move wherever a split left a value's bundles in two registers and the
  spill bundle's register can hold both.
* A bundle that stays (conflict in the spill bundle's register) is
  byte-for-byte as before.

Test: `ion::tests::value_live_across_calls_keeps_one_register_in_the_loop`
(`src/ion/tests.rs`), the function above written against the `Function`
trait, with the checker run on the output. It fails at `bb8bf74` with
`moves inside the loop body: [(progpoint7-pre, Move { from: p4i, to: p2i })]`.

## Verification

### regalloc2's own tests

| Command | Result |
|---|---|
| `cargo test` | 11 passed |
| `cargo test --features fuzzing` (includes the 1 s ion checker smoke test) | 15 passed |
| `cargo fmt --check` | clean |
| `cargo clippy` | no new warnings |

### Ion checker fuzzing

The `arbtest` driver described in
[the loop-exit note](cold-exit-block-splits.md#ion-checker-fuzzing), release
mode with `debug-assertions` and `overflow-checks`, one core per process:

| Allocator | Duration | Cases | Result |
|---|---|---|---|
| this change | 4 × 300 s | 944,833 | all passed |

The generator exercises the pass: in a 30 s run with a temporary counter,
25,800 functions had 37,742 candidate bundles, of which 14,324 moved.

The first version of the pass failed the checker within seconds
(`AllocationIsNotReg`): a spill bundle's "register" can be a stack-backed
`PReg` in the fuzzer's environment. The pass now skips those.

### Downstream: the Plenty compiler

Plenty (Cranelift 0.137.0, x86-64 Linux) built against this checkout,
`--release`, user-mode instructions from `perf stat`, one run pinned to a
physical core. "Base" is the same compiler with `bb8bf74`.

| Program | Base | This change | |
|---|---|---|---|
| binary-trees 16 | 7,299,422,672 | 7,194,693,169 | −1.43% |
| fannkuch-redux 10 | 1,680,419,828 | 1,680,419,700 | 0.00% |
| fannkuch-redux 10, parallel | 1,495,736,134 | 1,495,739,420 | 0.00% |
| fasta 2,500,000 | 1,431,468,485 | 1,384,801,633 | −3.26% |
| k-nucleotide 250,000 | 6,327,763,969 | 6,292,180,983 | −0.56% |
| k-nucleotide 250,000, parallel | 6,326,365,162 | 6,283,222,075 | −0.68% |
| mandelbrot 4000 | 8,333,462,897 | 8,333,450,626 | 0.00% |
| n-body 5,000,000 | 3,225,592,393 | 3,225,592,141 | 0.00% |
| n-body 5,000,000, C-layout lists | 12,340,574,973 | 12,340,574,699 | 0.00% |
| n-body 5,000,000, unchecked | 5,755,575,658 | 5,755,575,373 | 0.00% |
| regex-redux 50,000 | 560,963,897 | 560,563,227 | −0.07% |
| reverse-complement 2,500,000 | 552,470,790 | 488,928,960 | −11.50% |
| spectral-norm 2000 | 2,724,644,768 | 2,724,524,665 | 0.00% |

k-nucleotide varies by about 0.2% between runs (hash seeds); the others
repeat to within a few thousand instructions.

All 114 Plenty programs in that repository's `experiments/`, `examples/`
and `benchmarks/` trees were compiled with both allocators and every
function's machine code compared. 100 compile; the other 14 are rejected identically by both. 96 change. Static instructions
fall from 370,444 to 363,925. 85 innermost loops are shorter, none is
longer, and no program grew.

Plenty's test suite (`just test`: `cargo test --locked --workspace
--all-features`, compiler at `e3f69b4f` built against this change through a
path override): all pass, 6,582 integration tests among them, including the
artifact reproducibility tests.

Compile cost, Plenty's `compile_bench 1000 10 --workload aot` (1,000
generated units, 40,003 lines, compiled and linked 10 times after a warm-up,
8 cores), user-mode instructions for the whole run, three runs each:

| Allocator | Instructions |
|---|---|
| base | 125,183,041,758; 125,337,940,303; 125,333,858,317 |
| this change | 125,287,452,277; 125,293,396,299; 125,330,452,721 |

No difference outside run-to-run variation (0.1%). Wall-clock medians
overlapped (base 577–785 ms, this change 599–713 ms over seven runs each)
on a machine with other work running; they do not resolve a difference
either way.

## Alternatives measured

Three ways of keeping the trimmed bundle from being cut inside a loop were
built first. Each lets a split piece cover a whole loop instead of only its
uses, so each also helps when the spill bundle gets a stack slot. Each made
other programs worse, because a longer bundle holds its register over more
code and other values are then split or spilled instead. Same measurement
as above, change from base:

| Variant | fasta | reverse-complement | n-body | n-body, C-layout lists | n-body, unchecked | k-nucleotide | fannkuch, parallel |
|---|---|---|---|---|---|---|---|
| A: trim stops at the outermost loop that holds the use and not the bundle's other end | −4.25% | −13.80% | **+2.79%** | −5.31% | −3.30% | +0.41% | +0.73% |
| B: as A, innermost loop | | | **+2.79%** | | | | |
| C: as A, but the extension has no spill weight and is trimmed again on a conflict or eviction | −1.08% | −13.80% | **+1.24%** | −2.96% | −2.61% | +0.20% | +0.24% |
| This change | −3.26% | −11.50% | 0.00% | 0.00% | 0.00% | −0.56% | 0.00% |

With A, n-body's main loop (645 instructions) gains 18 instructions per
step: the step counter moves to a stack
slot and 17 more `xmm` stores appear, because integer and float values that
were trimmed before now hold registers across the loop.

A and C are saved as patches outside the repository; neither is on a
branch.

## Not fixed

**Spill bundle in a stack slot.** When the spill bundle gets no register,
the pass has nothing to move into. A loop then reloads the value before its
first use and stores it back after its last. In Plenty's fasta the
per-symbol loop of `random_fasta` does this for two values (two reloads and
two stores per symbol) while two caller-saved registers are unused in that
loop. The values are live across a call in the enclosing loop, where every
callee-saved register is taken.

Variants A and C remove these and account for their extra gain on fasta
and the n-body list variants. A rule in the style of this change would be:
after allocation, extend a bundle over the gaps of its enclosing loop when
its register is free there, taking those ranges from the spill bundle. It
also takes free space only. It was not built: it must split the spill
bundle's ranges after allocation, and it trades a reload inside the loop
for one on every entry to it, which is a loss for a loop whose use is on a
cold path.

**The store back.** Independently, the store after the last use writes a
value the slot already holds. The redundant-move eliminator (`src/ion/moves.rs` 787) does not see it:
its state is cleared at every block boundary.

## Risks

* **A move where there was none.** A bundle that moves leaves the register
  its neighbours expected. If it was adjacent to another bundle of the same
  value in the old register, or fed a block parameter there, a move appears
  at that boundary. The pass does not weigh this against the moves it
  removes. Not observed: no loop grew in the 100 programs compared.
* **Callee-saved registers held longer.** A spill bundle's register is
  often callee-saved. Moving a bundle there does not add a save (the spill
  bundle already uses the register), but the caller-saved register it frees
  is not reused, since allocation is over.
* **Compile time.** One pass over all bundles; work only for bundles of a
  spillset whose spill bundle has a register.
* **Untested:** non-x86 targets; `allow_multiple_vreg_defs`. `fastalloc` is
  unaffected (the change is in `ion`).

## Not verified

* Cranelift filetests and `clif-util`; Sightglass; wasmtime's test suite.
  `precise-output` expectations will change wherever a function has a value
  split around a call.
* `cargo fuzz` targets under libFuzzer. Only the `arbtest` driver was run.
