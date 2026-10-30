"""Validate the actual allocation/codec smoke reports; timings are observations, never budgets."""
import argparse
import json
from pathlib import Path

parser = argparse.ArgumentParser()
parser.add_argument("--allocations", type=Path, required=True)
parser.add_argument("--codecs", type=Path, required=True)
args = parser.parse_args()
expected = {"field", "page", "page_projected", "all_rows"}
for path in (args.allocations, args.codecs):
    report = json.loads(path.read_text())
    assert report["schemaVersion"] == 1
    rows = report["workloads"]
    assert len(rows) == 4 and {row["workload"] for row in rows} == expected
    assert report["fixture"]["rows"] == 10000
    for row in rows:
        assert row["answerBytes"] > 0
        if path == args.allocations:
            assert row["iterations"] > 0
            assert row["totalAllocations"] > 0 and row["totalAllocatedBytes"] > 0 and row["peakHeapBytes"] > 0
        else:
            assert len(row["readSyncNs"]) == len(row["decodeNs"]) >= 1
            assert all(type(n) is int and n >= 0 for n in row["readSyncNs"] + row["decodeNs"])
            assert len(row["answerSha256"]) == 64
    if path == args.codecs:
        assert len(report["binary"]["sha256"]) == 64
        assert len(report["source"]["commit"]) == 40
        assert type(report["source"]["dirty"]) is bool
    print(f"Validated four read measurements: {path}")
