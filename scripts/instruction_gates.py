"""What the instruction-count checks hold each operation to, for `diagnostics-overhead.py` and `instruction-determinism.py`."""

# Each case's allowed difference: this share of its count, or this many instructions if more
SHARE = 0.0005
FLOOR = 100
GATED = {"key_encode", "value_rebuild", "export", "indexed_limit", "read_bytes", "small_write", "graph"}
# The tokenizer seeds its hash map per process, which moves a text rebuild's count by a fraction of a percent between
# runs of one build (`docs/evidence/instruction-counts.md`)
REPORTED = {"text_rebuild", "rank_drain", "rank_build"}


def allowed_growth(count):
    return max(FLOOR, int(count * SHARE))
