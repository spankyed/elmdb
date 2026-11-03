"""What the instruction-count checks hold each operation to, for `diagnostics-overhead.py` and `instruction-determinism.py`."""

# Each case's allowed difference: this share of its count, or this many instructions if more. The floor is what one
# operation's spans cost with nothing collecting, which `small_write` shows at 172, a span and no more; what the gate is
# for grows with the rows, past it
SHARE = 0.0005
FLOOR = 300
GATED = {"key_encode", "value_rebuild", "export", "indexed_limit", "read_bytes", "small_write", "graph"}
# The tokenizer seeds its hash map per process, which moves a text rebuild's count by a fraction of a percent between
# runs of one build (`docs/evidence/instruction-counts.md`)
REPORTED = {"text_rebuild", "rank_drain", "rank_build"}


def allowed_growth(count):
    return max(FLOOR, int(count * SHARE))
