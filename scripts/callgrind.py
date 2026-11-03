"""What a Callgrind file says of a case: its functions' own instructions, which cover what the case counts alone.

A file's call counts include the setup that ran before counting began, so nothing here reads them.

Each case's directory holds one file a thread, `callgrind.<case>.t<n>.p1.out`, and with `.base@<name>` the baseline's.
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
    names, own = {}, Counter()
    function = None
    next_is_a_call = False

    def named(text):
        found = re.fullmatch(r"\((\d+)\)(?: (.*))?", text)
        if not found:
            return re.sub(r"'\d+$", "", text)
        if found[2]:
            names[found[1]] = found[2]
        return re.sub(r"'\d+$", "", names[found[1]])

    for line in path.read_text(errors="replace").splitlines():
        if line.startswith("fn="):
            function = named(line[3:])
        elif line.startswith("cfn="):
            named(line[4:])
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
    """The allocator's own instructions across the case's threads: a fixed number an allocation, so they count them"""
    total = 0
    for path in _files(directory, suffix):
        total += sum(count for name, count in _read(path).items() if ALLOCATOR in name)
    return total


def grew_most(directory, baseline, top=5):
    """The functions whose own instructions moved most between the baseline and the run, as a line for a report"""
    before, after = Counter(), Counter()
    for path in _files(directory, f"base@{baseline}"):
        before.update(_read(path))
    for path in _files(directory, ""):
        after.update(_read(path))
    moved = sorted(((after[name] - before[name], name) for name in before.keys() | after.keys()),
                   key=lambda item: -abs(item[0]))
    return "; ".join(f"`{name}` {change:+,}" for change, name in moved[:top] if change) or "no function moved"
