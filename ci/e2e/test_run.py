"""用进程替身验证 E2E 执行器的判定，不启动编译器或生成程序。"""

from contextlib import redirect_stdout
import io
from pathlib import Path
import subprocess
import unittest
from unittest.mock import Mock, patch

import run


class RunCaseTests(unittest.TestCase):
    def setUp(self):
        self.process = self.enterContext(patch.object(run.subprocess, "run"))
        self.enterContext(patch.object(Path, "chmod"))
        self.expected = self.enterContext(patch.object(Path, "read_bytes", return_value=b"ok\n"))
        self.enterContext(redirect_stdout(io.StringIO()))
        self.enterContext(patch.object(run.sys, "stderr", Mock(buffer=io.BytesIO())))
        self.process.side_effect = [
            subprocess.CompletedProcess([], 0),
            subprocess.CompletedProcess([], 0, stdout=b"ok\n"),
        ]

    def run_case(self, target="x86_64-linux", status=0, inputs=((),)):
        run.run_case(Path("gugu"), target, Path("scratch with spaces"), "hello", status, inputs)

    def test_all_targets_select_their_executable(self):
        for target in run.TARGETS:
            with self.subTest(target=target):
                self.process.side_effect = [
                    subprocess.CompletedProcess([], 0),
                    subprocess.CompletedProcess([], 7, stdout=b"ok\n"),
                ]
                self.run_case(target, status=7)
                build, execute = self.process.call_args_list[-2:]
                self.assertEqual(build.args[0][2:4], ["--target", target])
                suffix = ".exe" if target == "x86_64-windows" else ""
                self.assertEqual(execute.args[0], [Path(f"scratch with spaces/hello{suffix}")])
                self.assertEqual(execute.kwargs["timeout"], 30)

    def test_compile_failure_stops_execution(self):
        self.process.side_effect = subprocess.CalledProcessError(2, "gugu")
        with self.assertRaises(subprocess.CalledProcessError):
            self.run_case()
        self.assertEqual(self.process.call_count, 1)
        self.assertTrue(self.process.call_args.kwargs["check"])

    def test_unexpected_exit_status_fails(self):
        with self.assertRaisesRegex(SystemExit, "退出码"):
            self.run_case(status=7)

    def test_output_comparison_preserves_line_endings(self):
        self.expected.return_value = b"ok\r\n"
        with self.assertRaisesRegex(SystemExit, "标准输出"):
            self.run_case()

    def test_timeout_fails(self):
        self.process.side_effect = [
            subprocess.CompletedProcess([], 0),
            subprocess.TimeoutExpired("hello", 30),
        ]
        with self.assertRaises(subprocess.TimeoutExpired):
            self.run_case()

    def test_shards_run_separately_and_combine_output(self):
        self.process.side_effect = [
            subprocess.CompletedProcess([], 0),
            subprocess.CompletedProcess([], 0, stdout=b"o"),
            subprocess.CompletedProcess([], 0, stdout=b"k\n"),
        ]
        inputs = [(Path("first shard.utf8"),), (Path("second shard.utf8"),)]
        self.run_case(inputs=inputs)
        executions = self.process.call_args_list[1:]
        self.assertEqual([call.args[0][1:] for call in executions], [list(args) for args in inputs])
