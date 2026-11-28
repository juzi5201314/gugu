#!/usr/bin/env python3
"""在原生宿主上编译并执行 Gugu fixtures，校验退出码与标准输出。"""

import argparse
import difflib
import os
from pathlib import Path
import subprocess
import sys
import tempfile

FIXTURES = Path(__file__).resolve().parent / "fixtures"
TARGETS = ("x86_64-linux", "x86_64-windows", "aarch64-linux", "aarch64-macos")


def run_case(gugu, target, scratch, name, expected_status, inputs=((),)):
    print(f"编译 {name} ({target})", flush=True)
    subprocess.run(
        [gugu, "build", "--target", target, "--target-dir", scratch,
         "--cache-dir", scratch / "cache", FIXTURES / f"{name}.gg"],
        check=True,
        timeout=180,
    )
    suffix = ".exe" if target == "x86_64-windows" else ""
    executable = scratch / f"{name}{suffix}"
    if os.name != "nt":
        executable.chmod(0o755)

    print(f"执行 {name}", flush=True)
    output = bytearray()
    for args in inputs:
        result = subprocess.run(
            [executable, *args], stdout=subprocess.PIPE, timeout=30, check=False,
        )
        if result.returncode != expected_status:
            sys.stderr.buffer.write(result.stdout)
            raise SystemExit(
                f"{name}: 预期退出码 {expected_status}，实际为 {result.returncode}"
            )
        output.extend(result.stdout)

    expected = (FIXTURES / f"{name}.stdout").read_bytes()
    if output != expected:
        diff = difflib.diff_bytes(
            difflib.unified_diff, expected.splitlines(keepends=True),
            bytes(output).splitlines(keepends=True),
            fromfile=b"expected", tofile=b"actual",
        )
        sys.stderr.buffer.writelines(diff)
        raise SystemExit(f"{name}: 标准输出不一致")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    suffix = ".exe" if os.name == "nt" else ""
    parser.add_argument("gugu", nargs="?", type=Path, default=Path(f"target/debug/gugu{suffix}"))
    parser.add_argument("--target", required=True, choices=TARGETS)
    args = parser.parse_args()
    gugu = args.gugu.resolve()
    with tempfile.TemporaryDirectory(prefix="gugu-e2e-") as directory:
        scratch = Path(directory)
        for name, status in (
            ("hello", 0),
            ("sum", 7),
            ("collections", 0),
            ("formatting", 0),
            ("channel_negative_capacity", 1),
            ("channel_double_close", 1),
        ):
            run_case(gugu, args.target, scratch, name, status)
        # 每个分片单独执行，限制单进程分配量，同时覆盖全部 UAX #15 数据。
        inputs = [(path,) for path in sorted(FIXTURES.glob("NormalizationTest.*.utf8"))]
        run_case(gugu, args.target, scratch, "normalization", 0, inputs)


if __name__ == "__main__":
    main()
