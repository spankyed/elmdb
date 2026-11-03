"""What a Callgrind file says of a case: its allocations and its functions' own instructions.

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
    """Each function's own instructions, and the calls made to each"""
    names, own, calls = {}, Counter(), Counter()
    function = callee = None
    next_is_a_call = False

    def named(text):
        found = re.fullmatch(r"\((\d+)\)(?: (.*))?", text)
        if not found:
            return text
        if found[2]:
            names[found[1]] = found[2]
        return names[found[1]]

    for line in path.read_text(errors="replace").splitlines():
        if line.startswith("fn="):
            function = named(line[3:])
        elif line.startswith("cfn="):
            callee = named(line[4:])
        elif line.startswith("calls="):
            calls[callee] += int(line[6:].split()[0])
            next_is_a_call = True
        elif function and re.match(r"[+\-*\d]", line):
            if next_is_a_call:
                next_is_a_call = False
            else:
                fields = line.split()
                if len(fields) > 1:
                    own[function] += int(fields[1])
    return own, calls


def allocations(directory, suffix=""):
    """How many allocations the case made, across its threads"""
    total = 0
    for path in _files(directory, suffix):
        total += sum(count for name, count in _read(path)[1].items() if ALLOCATOR in name)
    return total


def grew_most(directory, baseline, top=5):
    """The functions whose own instructions moved most between the baseline and the run, as a line for a report"""
    before, after = Counter(), Counter()
    for path in _files(directory, f"base@{baseline}"):
        before.update(_read(path)[0])
    for path in _files(directory, ""):
        after.update(_read(path)[0])
    moved = sorted(((after[name] - before[name], name) for name in before.keys() | after.keys()),
                   key=lambda item: -abs(item[0]))
    return "; ".join(f"`{name}` {change:+,}" for change, name in moved[:top] if change) or "no function moved"
