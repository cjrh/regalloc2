#!/usr/bin/env python3
"""Count the hot-path instructions of each innermost loop in the harness output.

A loop is a backward branch and its target. Within it, a conditional branch
whose target lies outside the loop is an exit; code reached only through
such an exit (the out-of-line trap and call paths) is not counted. Reads
stdin, prints one line per loop.
"""
import re
import sys

LINE = re.compile(r"^\s*([0-9a-f]+):\s+([a-z][\w.]*)\s*(.*)$")
UNCONDITIONAL = ("jmp", "ret", "retq", "ud2")


def main():
    funcs, func = {}, None
    for line in sys.stdin:
        if line.startswith("==== "):
            func = line.split()[1]
            funcs[func] = []
            continue
        m = LINE.match(line)
        if m and func:
            funcs[func].append((int(m.group(1), 16), m.group(2), m.group(3)))
    for name, insns in funcs.items():
        addrs = [a for a, _, _ in insns]
        for i, (addr, op, arg) in enumerate(insns):
            if not op.startswith("j"):
                continue
            t = re.match(r"^(?:0x)?([0-9a-f]+)$", arg.strip())
            if not t or int(t.group(1), 16) > addr:
                continue
            start = int(t.group(1), 16)
            # Walk the fall-through path from the loop head to the backedge,
            # following unconditional jumps that stay inside the loop.
            count, pc, seen = 0, addrs.index(start), set()
            while pc not in seen:
                seen.add(pc)
                a, o, x = insns[pc]
                count += 1
                if a == addr:
                    break
                if o == "jmp":
                    pc = addrs.index(int(x.strip(), 16))
                    continue
                pc += 1
            print(f"{name} loop {start:#x}-{addr:#x}: {count} instructions")


main()
