"""Disposable-copy negative controls proving core verification tests fail loudly when invariants break."""
import io
import os
from pathlib import Path
import subprocess
import tarfile
import tempfile


def run_test(root: Path, env: dict[str, str], pkg: str, test_bin: str, test_name: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        ["cargo", "test", "--offline", "-p", pkg, "--test", test_bin, "--", "--exact", test_name, "--nocapture"],
        cwd=root,
        env=env,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
    )


def main() -> None:
    # 1. Positive control on current tree: Grouped durable pin DST test passes.
    positive = run_test(
        Path.cwd(),
        dict(os.environ),
        "elmdb-sim",
        "dst_durability",
        "dst_pr268_grouped_durable_pin_prevents_freelist_reuse_corruption_and_guards_unacked_reboot",
    )
    if positive.returncode != 0:
        print(positive.stdout)
        raise SystemExit("positive control failed before mutation")
    print("Positive control passed: Grouped durable pin DST test")

    # 2. Disposable-copy negative control: disable Grouped durable pin installation in `durability.rs`
    #    and prove the DST freelist-reuse crash test fails loudly!
    scratch_target = subprocess.check_output(["cargo", "xtask", "scratch-target"], cwd=Path.cwd(), text=True).strip()
    with tempfile.TemporaryDirectory(prefix="elmdb-neg-control-") as temporary:
        root = Path(temporary)
        with tarfile.open(fileobj=io.BytesIO(subprocess.check_output(["git", "archive", "HEAD"]))) as archive:
            archive.extractall(root)

        durability = root / "crates/elmdb-storage/src/durability.rs"
        code = durability.read_text()
        target = "self.store.install_durable_pin(tok);"
        assert code.count(target) == 2, f"expected 2 install_durable_pin call sites, found {code.count(target)}"
        durability.write_text(code.replace(target, "drop(tok); // deliberate negative-control mutation"))

        negative = run_test(
            root,
            {**os.environ, "CARGO_TARGET_DIR": scratch_target},
            "elmdb-sim",
            "dst_durability",
            "dst_pr268_grouped_durable_pin_prevents_freelist_reuse_corruption_and_guards_unacked_reboot",
        )
        if not (negative.returncode != 0 and "running 1 test" in negative.stdout and "FAILED" in negative.stdout):
            print(negative.stdout)
            raise SystemExit("negative control did not fire when Grouped durable pin installation was removed")
        print("Negative control fired as expected: removing Grouped durable pin broke DST freelist-reuse test")


if __name__ == "__main__":
    main()
