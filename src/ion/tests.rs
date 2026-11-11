/*
 * Released under the terms of the Apache 2.0 license with LLVM
 * exception. See `LICENSE` for details.
 */

use crate::checker::Checker;
use crate::{
    run, Algorithm, Block, Function, Inst, InstRange, MachineEnv, Operand, OperandConstraint,
    OperandKind, OperandPos, PReg, PRegSet, RegClass, RegallocOptions, VReg,
};
use alloc::vec;
use alloc::vec::Vec;

/// A value that enters in a register clobbered by calls, stays live across
/// two calls before a loop, and is used inside the loop must not change
/// register inside the loop.
///
/// The value is split after the calls. The piece with the uses is trimmed
/// to them and the rest of its live range, including the rest of the loop
/// body, joins the spill bundle. The piece is allocated first and takes the
/// register hinted by the entry, which the spill bundle cannot have because
/// it crosses the calls. Unless the piece then follows the spill bundle into
/// its register, the value changes register inside the loop on every
/// iteration.
#[test]
fn value_live_across_calls_keeps_one_register_in_the_loop() {
    let call_clobbers = PRegSet::empty().with(p(1));
    let f = TestFunction::new(vec![
        // block0: v0 and v1 arrive in p0 and p1; two calls return v2 and v3.
        block(
            &[],
            vec![
                inst(vec![def(0, fixed(0)), def(1, fixed(1))]),
                inst(vec![use_(0, fixed(0)), def(2, fixed(0))]).clobbers(call_clobbers),
                inst(vec![use_(0, fixed(0)), def(3, fixed(0))]).clobbers(call_clobbers),
                inst(vec![def(4, reg())]),
                inst(vec![]).branch(&[(1, &[4])]),
            ],
        ),
        // block1: loop header; leaves to the trap in block4 when v5 >= v2.
        block(
            &[5],
            vec![
                inst(vec![use_(5, reg()), use_(2, reg())]),
                inst(vec![]).branch(&[(2, &[]), (4, &[])]),
            ],
        ),
        // block2: body; stores to v3[v5], steps v5, compares with v1.
        block(
            &[],
            vec![
                inst(vec![use_(3, reg()), use_(5, reg())]),
                inst(vec![use_(5, reg()), def(6, reg())]),
                inst(vec![use_(6, reg()), use_(1, reg())]),
                inst(vec![]).branch(&[(3, &[]), (5, &[])]),
            ],
        ),
        // block3: back edge.
        block(&[], vec![inst(vec![]).branch(&[(1, &[6])])]),
        // block4: trap.
        block(&[], vec![inst(vec![]).ret()]),
        // block5: return v6.
        block(&[], vec![inst(vec![use_(6, fixed(0))]).ret()]),
    ]);
    let env = MachineEnv {
        preferred_regs_by_class: [(0..2).map(p).collect(), PRegSet::empty(), PRegSet::empty()],
        non_preferred_regs_by_class: [(2..6).map(p).collect(), PRegSet::empty(), PRegSet::empty()],
        scratch_by_class: [None, None, None],
        fixed_stack_slots: vec![],
    };
    let mut options = RegallocOptions::default();
    options.validate_ssa = true;
    options.algorithm = Algorithm::Ion;
    let output = run(&f, &env, &options).unwrap();

    let mut checker = Checker::new(&f, &env);
    checker.prepare(&output);
    checker.run().unwrap();

    let loop_insts = f.block_insns(Block::new(1)).first().index()
        ..f.block_insns(Block::new(3)).last().index() + 1;
    let in_loop: Vec<_> = output
        .edits
        .iter()
        .filter(|(point, _)| loop_insts.contains(&point.inst().index()))
        .collect();
    // The back edge passes v6 to v5; that move is not at issue here.
    let back_edge = f.block_insns(Block::new(3)).first().index();
    let moved_in_body: Vec<_> = in_loop
        .iter()
        .filter(|(point, _)| point.inst().index() != back_edge)
        .collect();
    assert!(
        moved_in_body.is_empty(),
        "moves inside the loop body: {:?}",
        moved_in_body
    );
}

/// A value kept on the stack around a loop is reloaded before the loop, not
/// on each of its iterations.
///
/// The loop holds an exit block whose call clobbers every register while
/// the value is live, and another such call follows the loop, so the
/// value's spill bundle gets a stack slot. The piece holding the loop's use
/// is trimmed to that use. Unless it is extended over the loop afterwards,
/// the value is loaded from its slot before the use on every iteration.
#[test]
fn value_spilled_around_a_loop_is_reloaded_at_loop_entry() {
    let call_clobbers = PRegSet::empty().with(p(1)).with(p(2)).with(p(3));
    let f = TestFunction::new(vec![
        // block0: v0 is the value; a call returns v1; v2 starts the count.
        block(
            &[],
            vec![
                inst(vec![def(0, reg())]),
                inst(vec![def(1, fixed(0))]).clobbers(call_clobbers),
                inst(vec![def(2, reg())]),
                inst(vec![]).branch(&[(1, &[2])]),
            ],
        ),
        // block1: loop header; compares the count v3 with v0.
        block(
            &[3],
            vec![
                inst(vec![use_(3, reg()), use_(0, reg())]),
                inst(vec![]).branch(&[(2, &[]), (3, &[])]),
            ],
        ),
        // block2: exit inside the loop's span; a call, then v0.
        block(
            &[],
            vec![
                inst(vec![def(5, fixed(0))]).clobbers(call_clobbers),
                inst(vec![use_(0, reg()), use_(5, reg())]).ret(),
            ],
        ),
        // block3: body; leaves when done.
        block(
            &[],
            vec![
                inst(vec![use_(3, reg()), def(4, reg())]),
                inst(vec![]).branch(&[(4, &[]), (5, &[])]),
            ],
        ),
        // block4: back edge.
        block(&[], vec![inst(vec![]).branch(&[(1, &[4])])]),
        // block5: a call, then v0 and v1.
        block(
            &[],
            vec![
                inst(vec![def(6, fixed(0))]).clobbers(call_clobbers),
                inst(vec![use_(0, reg()), use_(1, reg()), use_(6, fixed(0))]).ret(),
            ],
        ),
    ]);
    let env = MachineEnv {
        preferred_regs_by_class: [(0..4).map(p).collect(), PRegSet::empty(), PRegSet::empty()],
        non_preferred_regs_by_class: [PRegSet::empty(), PRegSet::empty(), PRegSet::empty()],
        scratch_by_class: [None, None, None],
        fixed_stack_slots: vec![],
    };
    let mut options = RegallocOptions::default();
    options.validate_ssa = true;
    options.algorithm = Algorithm::Ion;
    let output = run(&f, &env, &options).unwrap();

    let mut checker = Checker::new(&f, &env);
    checker.prepare(&output);
    checker.run().unwrap();

    // The back edge passes v4 to v3; that move is not at issue here.
    let hot = [f.block_insns(Block::new(1)), f.block_insns(Block::new(3))];
    let in_loop: Vec<_> = output
        .edits
        .iter()
        .filter(|(point, _)| {
            hot.iter().any(|insts| {
                (insts.first().index()..=insts.last().index()).contains(&point.inst().index())
            })
        })
        .collect();
    assert!(in_loop.is_empty(), "moves inside the loop: {:?}", in_loop);
}

fn p(hw_enc: usize) -> PReg {
    PReg::new(hw_enc, RegClass::Int)
}

fn fixed(hw_enc: usize) -> OperandConstraint {
    OperandConstraint::FixedReg(p(hw_enc))
}

fn reg() -> OperandConstraint {
    OperandConstraint::Reg
}

fn use_(vreg: usize, constraint: OperandConstraint) -> Operand {
    Operand::new(
        VReg::new(vreg, RegClass::Int),
        constraint,
        OperandKind::Use,
        OperandPos::Early,
    )
}

fn def(vreg: usize, constraint: OperandConstraint) -> Operand {
    Operand::new(
        VReg::new(vreg, RegClass::Int),
        constraint,
        OperandKind::Def,
        OperandPos::Late,
    )
}

struct TestInst {
    operands: Vec<Operand>,
    clobbers: PRegSet,
    /// Successors with the arguments passed to each; empty unless a branch.
    targets: Vec<(Block, Vec<VReg>)>,
    is_ret: bool,
}

fn inst(operands: Vec<Operand>) -> TestInst {
    TestInst {
        operands,
        clobbers: PRegSet::empty(),
        targets: vec![],
        is_ret: false,
    }
}

impl TestInst {
    fn clobbers(mut self, clobbers: PRegSet) -> Self {
        self.clobbers = clobbers;
        self
    }

    fn branch(mut self, targets: &[(usize, &[usize])]) -> Self {
        self.targets = targets
            .iter()
            .map(|(block, args)| {
                let args = args.iter().map(|&v| VReg::new(v, RegClass::Int)).collect();
                (Block::new(*block), args)
            })
            .collect();
        self
    }

    fn ret(mut self) -> Self {
        self.is_ret = true;
        self
    }
}

struct TestBlock {
    params: Vec<VReg>,
    insts: Vec<TestInst>,
}

fn block(params: &[usize], insts: Vec<TestInst>) -> TestBlock {
    TestBlock {
        params: params
            .iter()
            .map(|&v| VReg::new(v, RegClass::Int))
            .collect(),
        insts,
    }
}

/// A function given as blocks in layout order; each ends in a branch or a
/// return.
struct TestFunction {
    insts: Vec<TestInst>,
    params: Vec<Vec<VReg>>,
    ranges: Vec<InstRange>,
    succs: Vec<Vec<Block>>,
    preds: Vec<Vec<Block>>,
    num_vregs: usize,
}

impl TestFunction {
    fn new(blocks: Vec<TestBlock>) -> Self {
        let mut f = TestFunction {
            insts: vec![],
            params: vec![],
            ranges: vec![],
            succs: vec![],
            preds: vec![vec![]; blocks.len()],
            num_vregs: 0,
        };
        for (index, block) in blocks.into_iter().enumerate() {
            let start = f.insts.len();
            let last = block.insts.last().unwrap();
            f.succs
                .push(last.targets.iter().map(|(succ, _)| *succ).collect());
            for (succ, _) in &last.targets {
                f.preds[succ.index()].push(Block::new(index));
            }
            let vregs = block
                .insts
                .iter()
                .flat_map(|inst| inst.operands.iter().map(|op| op.vreg()))
                .chain(block.params.iter().copied());
            for vreg in vregs {
                f.num_vregs = f.num_vregs.max(vreg.vreg() + 1);
            }
            f.params.push(block.params);
            f.insts.extend(block.insts);
            f.ranges
                .push(InstRange::new(Inst::new(start), Inst::new(f.insts.len())));
        }
        f
    }
}

impl Function for TestFunction {
    fn num_insts(&self) -> usize {
        self.insts.len()
    }

    fn num_blocks(&self) -> usize {
        self.ranges.len()
    }

    fn entry_block(&self) -> Block {
        Block::new(0)
    }

    fn block_insns(&self, block: Block) -> InstRange {
        self.ranges[block.index()]
    }

    fn block_succs(&self, block: Block) -> &[Block] {
        &self.succs[block.index()]
    }

    fn block_preds(&self, block: Block) -> &[Block] {
        &self.preds[block.index()]
    }

    fn block_params(&self, block: Block) -> &[VReg] {
        &self.params[block.index()]
    }

    fn is_ret(&self, insn: Inst) -> bool {
        self.insts[insn.index()].is_ret
    }

    fn is_branch(&self, insn: Inst) -> bool {
        !self.insts[insn.index()].targets.is_empty()
    }

    fn branch_blockparams(&self, _block: Block, insn: Inst, succ_idx: usize) -> &[VReg] {
        &self.insts[insn.index()].targets[succ_idx].1
    }

    fn inst_operands(&self, insn: Inst) -> &[Operand] {
        &self.insts[insn.index()].operands
    }

    fn inst_clobbers(&self, insn: Inst) -> PRegSet {
        self.insts[insn.index()].clobbers
    }

    fn num_vregs(&self) -> usize {
        self.num_vregs
    }

    fn spillslot_size(&self, _regclass: RegClass) -> usize {
        1
    }
}
