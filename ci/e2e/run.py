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


def build_case(gugu, target, scratch, name, **options):
    print(f"编译 {name} ({target})", flush=True)
    return subprocess.run(
        [gugu, "build", "--target", target, "--target-dir", scratch,
         "--cache-dir", scratch / "cache", FIXTURES / f"{name}.gg"],
        timeout=180,
        **options,
    )


def run_compile_failure(gugu, target, scratch, name, diagnostic):
    result = build_case(
        gugu, target, scratch, name, check=False,
        stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
    )
    if result.returncode != 1 or diagnostic.encode("utf-8") not in result.stdout:
        sys.stderr.buffer.write(result.stdout)
        raise SystemExit(f"{name}: 预期编译失败并报告 {diagnostic}")
    suffix = ".exe" if target == "x86_64-windows" else ""
    if (scratch / f"{name}{suffix}").exists():
        raise SystemExit(f"{name}: 编译失败不应生成可执行镜像")
    print(f"通过 {name}: 编译期拒绝，未生成镜像", flush=True)


def run_case(gugu, target, scratch, name, expected_status, inputs=((),)):
    build_case(gugu, target, scratch, name, check=True)
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
    print(f"通过 {name}: 退出码 {expected_status}，标准输出匹配", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    suffix = ".exe" if os.name == "nt" else ""
    parser.add_argument("gugu", nargs="?", type=Path, default=Path(f"target/debug/gugu{suffix}"))
    parser.add_argument("--target", required=True, choices=TARGETS)
    args = parser.parse_args()
    gugu = args.gugu.resolve()
    with tempfile.TemporaryDirectory(prefix="gugu-e2e-") as directory:
        scratch = Path(directory)
        run_compile_failure(
            gugu, args.target, scratch, "uint_conversion_out_of_range",
            "整数字面量超出目标类型范围",
        )
        for name, status in (
            ("hello", 0),
            ("float_constants_copy", 0),
            ("float_constants_add", 0),
            ("float_constants_mul", 0),
            ("float_constants_compare", 0),
            ("uint_max", 0),
            ("uint_high_bit", 0),
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
