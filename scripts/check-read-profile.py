"""Validate allocation/codec reports and their common fixture; observations are never timing budgets."""
import argparse
import json
from pathlib import Path
import re

WORKLOADS = {"field", "page", "page_projected", "all_rows"}


def integer(value, minimum=0):
    assert type(value) is int and value >= minimum, f"expected integer >= {minimum}, got {value!r}"


def digest(value, length):
    assert isinstance(value, str) and re.fullmatch(rf"[a-f0-9]{{{length}}}", value), "invalid SHA digest"


def validate_reports(allocations, codecs):
    by_kind = []
    for report, kind in [(allocations, "rust-read-allocations"), (codecs, "node-read-and-decode")]:
        assert type(report["schemaVersion"]) is int and report["schemaVersion"] == 1
        assert report["measurement"] == kind, "wrong measurement kind"
        integer(report["fixture"]["version"], 1)
        integer(report["fixture"]["rows"], 1)
        assert report["fixture"]["version"] == 2 and report["fixture"]["rows"] == 10000
        rows = report["workloads"]
        assert len(rows) == 4 and {row["workload"] for row in rows} == WORKLOADS
        for row in rows:
            integer(row["answerBytes"], 1)
            digest(row["answerSha256"], 64)
            if kind == "rust-read-allocations":
                for name in ["iterations", "totalAllocations", "totalAllocatedBytes", "peakHeapBytes"]:
                    integer(row[name], 1)
            else:
                assert isinstance(row["readSyncNs"], list) and isinstance(row["decodeNs"], list)
                assert len(row["readSyncNs"]) == len(row["decodeNs"]) >= 1
                for sample in row["readSyncNs"] + row["decodeNs"]:
                    integer(sample)
        by_kind.append({row["workload"]: row for row in rows})
    assert type(allocations["debugAssertions"]) is bool
    assert codecs["fixture"]["perChannel"] == 100 and codecs["fixture"]["syncs"] is False
    digest(codecs["binary"]["sha256"], 64)
    digest(codecs["source"]["commit"], 40)
    assert type(codecs["source"]["dirty"]) is bool
    assert codecs["source"]["provenance"] in {"git", "caller-provided snapshot"}
    for name in ["node", "napi", "v8", "platform", "arch"]:
        assert isinstance(codecs["runtime"][name], str) and codecs["runtime"][name]
    for name in WORKLOADS:
        for field in ["answerBytes", "answerSha256"]:
            assert by_kind[0][name][field] == by_kind[1][name][field], f"{name}: Rust/Node fixture mismatch ({field})"


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--allocations", type=Path, required=True)
    parser.add_argument("--codecs", type=Path, required=True)
    args = parser.parse_args()
    validate_reports(json.loads(args.allocations.read_text()), json.loads(args.codecs.read_text()))
    print("Validated four byte-identical Rust/Node read measurements")


if __name__ == "__main__":
    main()
