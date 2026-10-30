"""Report acceptance must reject mismatched fixtures and malformed measurements without timing gates."""
import copy
import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location("read_profile", Path(__file__).with_name("check-read-profile.py"))
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


def reports():
    common = {"schemaVersion": 1, "fixture": {"version": 2, "rows": 10000}, "workloads": [
        {"workload": name, "answerBytes": 40, "answerSha256": "a" * 64}
        for name in sorted(module.WORKLOADS)
    ]}
    allocations, codecs = copy.deepcopy(common), copy.deepcopy(common)
    allocations.update(measurement="rust-read-allocations", debugAssertions=False)
    codecs.update(measurement="node-read-and-decode", binary={"sha256": "b" * 64},
                  source={"commit": "c" * 40, "dirty": False, "provenance": "git"},
                  runtime={name: "test" for name in ["node", "napi", "v8", "platform", "arch"]})
    codecs["fixture"].update(perChannel=100, syncs=False)
    for row in allocations["workloads"]:
        row.update(iterations=2, totalAllocations=4, totalAllocatedBytes=80, peakHeapBytes=40)
    for row in codecs["workloads"]:
        row.update(readSyncNs=[1, 2], decodeNs=[1, 2])
    return allocations, codecs


class Reports(unittest.TestCase):
    def test_valid_matching_reports(self):
        module.validate_reports(*reports())

    def test_refuses_incorrect_evidence(self):
        cases = [
            ("different fixture bytes", lambda a, c: c["workloads"][0].update(answerSha256="d" * 64)),
            ("different fixture size", lambda a, c: c["workloads"][0].update(answerBytes=41)),
            ("old fixture", lambda a, c: a["fixture"].update(version=1)),
            ("wrong kind", lambda a, c: a.update(measurement="node-read-and-decode")),
            ("duplicate workload", lambda a, c: a["workloads"][0].update(workload=a["workloads"][1]["workload"])),
            ("invalid digest", lambda a, c: c["binary"].update(sha256="z" * 64)),
            ("invalid source SHA", lambda a, c: c["source"].update(commit="z" * 40)),
            ("boolean allocation count", lambda a, c: a["workloads"][0].update(totalAllocations=True)),
            ("negative time", lambda a, c: c["workloads"][0].update(readSyncNs=[-1, 2])),
            ("noninteger time", lambda a, c: c["workloads"][0].update(readSyncNs=[1.5, 2])),
            ("empty samples", lambda a, c: c["workloads"][0].update(readSyncNs=[], decodeNs=[])),
            ("missing decode sample", lambda a, c: c["workloads"][0].update(decodeNs=[1])),
        ]
        for name, alter in cases:
            with self.subTest(name=name):
                a, c = reports()
                alter(a, c)
                with self.assertRaises(AssertionError):
                    module.validate_reports(a, c)


if __name__ == "__main__":
    unittest.main()
