"""What a Callgrind file says of a case: its functions' own instructions, which cover what the case counts alone.

A file's call counts include the setup that ran before counting began, so nothing here reads them. gungraun writes names
and positions uncompressed; were that to change, `allocator_instructions` would find no allocator, which the scripts
refuse. Each case's directory holds one file a thread, `callgrind.<case>.t<n>.p1.out`, and with `.base@<name>` the baseline's.
"""
from collections import Counter
from pathlib import Path
import re

# The benchmark's allocator (`benches/instructions.rs`), named so that it can be counted
ALLOCATOR = "bump_allocation"


def _files(directory, suffix):
    ending = f".out.{suffix}" if suffix else ".out"
    return [path for path in Path(directory).glob("callgrind.*.out*") if path.name.endswith(ending)]


def _read(path):
    """Each function's own instructions, a recursive function's depths (`f'2`) added together"""
    own = Counter()
    function = None
    next_is_a_call = False
    for line in path.read_text(errors="replace").splitlines():
        if line.startswith("fn="):
            function = re.sub(r"'\d+$", "", line[3:])
        elif line.startswith("calls="):
            next_is_a_call = True
        elif function and re.match(r"[+\-*\d]", line):
            if next_is_a_call:
                next_is_a_call = False
            else:
                fields = line.split()
                if len(fields) > 1:
                    own[function] += int(fields[1])
    return own


def allocator_instructions(directory, suffix=""):
    """The allocator's own instructions across the case's threads, a fixed number an allocation. 0 is no answer: every
    case the scripts gate allocates, so it means the allocator wasn't found under `ALLOCATOR` or there were no files"""
    total = 0
    for path in _files(directory, suffix):
        total += sum(count for name, count in _read(path).items() if ALLOCATOR in name)
    return total


def grew_most(directory, baseline, top=5):
    """The functions whose own instructions moved most between the baseline and the run, as a line for a report"""
    before, after = Counter(), Counter()
    if not _files(directory, f"base@{baseline}") or not _files(directory, ""):
        return f"no Callgrind files to compare under {directory}"
    for path in _files(directory, f"base@{baseline}"):
        before.update(_read(path))
    for path in _files(directory, ""):
        after.update(_read(path))
    moved = sorted(((after[name] - before[name], name) for name in before.keys() | after.keys()),
                   key=lambda item: -abs(item[0]))
    return "; ".join(f"`{name}` {change:+,}" for change, name in moved[:top] if change) or "no function moved"
