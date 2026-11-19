"""The Callgrind reading the instruction-count gate rests on, against a file in the format gungraun writes."""
from pathlib import Path
import tempfile

from callgrind import allocator_instructions, grew_most, linked_for_valgrind, nothing_counted

FILE = """# callgrind format
events: Ir Dr Dw
summary: 1000 0 0

ob=/bin/bench
fl=???
fn=read'2
0 100 1 1
cfn=instructions::bump::bump_allocation
calls=3 0
0 90 0 0
0 5

fn=read
0 10

fn=instructions::bump::bump_allocation
0 90
"""

with tempfile.TemporaryDirectory() as scratch:
    case = Path(scratch)
    # The cost after `calls=` is the callee's, already its own under `fn=`, and depths of a recursion are one function
    (case / "callgrind.c.t1.p1.out").write_text(FILE)
    assert allocator_instructions(case) == 90
    # Threads add up, and neither another run's file nor the baseline's is read as this one's
    (case / "callgrind.c.t2.p1.out").write_text("fn=instructions::bump::bump_allocation\n0 10\n")
    (case / "callgrind.c.t1.p1.out.old").write_text("fn=instructions::bump::bump_allocation\n0 7\n")
    assert allocator_instructions(case) == 100
    assert allocator_instructions(case, "base@off") == 0
    (case / "callgrind.c.t1.p1.out.base@off").write_text("fn=instructions::bump::bump_allocation\n0 40\nfn=read\n0 500\n")
    assert allocator_instructions(case, "base@off") == 40
    assert grew_most(case, "off", top=2) == "`read` -385; `instructions::bump::bump_allocation` +60", grew_most(case, "off", top=2)

with tempfile.TemporaryDirectory() as scratch:
    # What the scripts refuse: nothing found reads as 0, and a report says there was nothing to read
    assert allocator_instructions(scratch) == 0
    assert "no Callgrind files" in grew_most(scratch, "off")
print("ok")

# The link Valgrind reads follows whatever flags the environment holds, and leaves the rest of it as it is
assert linked_for_valgrind({"A": "b"}) == {"A": "b", "RUSTFLAGS": "-C link-arg=-Wl,-z,norelro"}
assert linked_for_valgrind({"RUSTFLAGS": "--cfg x"})["RUSTFLAGS"] == "--cfg x -C link-arg=-Wl,-z,norelro"

# A run that counted nothing is every case at 0: one case at 0 is that case's, and no case is no run
assert nothing_counted([0, 0, 0])
assert not nothing_counted([0, 783, 0])
assert not nothing_counted([])
