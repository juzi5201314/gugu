//! 内核加载写出的 Linux static PIE，确认 rt0 能进入入口并退出。
//!
//! 不进默认测试套件。`cargo bench -p gugu-compiler --bench linux_pie --profile dev`。

use gugu_compiler::{CompileRequest, Compiler, EMBEDDED_ARTIFACTS, TargetName};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

fn main() {
    let programs = [
        ("hello", "fn main() {}", 0, "", ""),
        (
            "addr-of-place",
            "static PAIRS: [[uint; 2]; 4] = [[11, 12], [21, 22], [31, 32], [41, 42]]\nfn base_address() uint { unsafe { std.mem.transmute[*uint, uint](std.ptr.addr_of(PAIRS[0][0])) } }\nfn load_at(address: uint) uint { unsafe { std.ptr.read_unaligned[uint](std.mem.transmute[uint, *uint](address)) } }\nfn write_at(address: uint, value: uint) { unsafe { std.ptr.write_unaligned[uint](std.mem.transmute[uint, *uint](address), value) } }\nfn main() {\n let base = base_address()\n let p = unsafe { std.ptr.addr_of(PAIRS[0][1]) }\n let q = unsafe { std.mem.transmute[*uint, uint](p) }\n if q - base != uint(8) { panic(\"q-base\") }\n if load_at(base) != uint(11) { panic(\"first\") }\n if load_at(q) != uint(12) { panic(\"second\") }\n write_at(q, uint(77))\n if PAIRS[0][1] != uint(77) { panic(\"write-through\") }\n if PAIRS[0][0] != uint(11) { panic(\"neighbor\") }\n}\n",
            0,
            "",
            "",
        ),
        (
            "panic",
            "fn main() {\n    panic(\"boom\")\n}\n",
            1,
            "",
            "{\"schema\":\"gugu-runtime-report-v1\",\"event\":\"panic\",\"class\":\"program-failure\",\"reason\":\"unhandled-panic\",\"message\":\"boom\",\"location\":{\"file\":\"main.gg\",\"line\":2,\"column\":5},\"backtrace\":[],\"exit_code\":1}\n{\"schema\":\"gugu-runtime-report-v1\",\"event\":\"termination\",\"class\":\"program-failure\",\"reason\":\"unhandled-panic\",\"message\":null,\"location\":null,\"backtrace\":[],\"exit_code\":1}\n",
        ),
        (
            "grow",
            "fn depth(n: uint) uint { let frame: [uint; 16] = [n, n, n, n, n, n, n, n, n, n, n, n, n, n, n, n]\n if n == 0 { return frame[0] }\n frame[int(n % uint(16))] + depth(n - uint(1)) }\nfn main() { if depth(uint(5000)) != uint(12502500) { panic(\"wrong result\") } }\n",
            0,
            "",
            "",
        ),
        (
            "spawn",
            "fn child() int { 42 }\nfn main() {\n let handle = async child()\n match handle.wait() {\n Ok(value) => { if value != 42 { panic(\"spawn\") } _ = value }\n Err(_) => panic(\"join\")\n }\n}\n",
            0,
            "",
            "",
        ),
        (
            "closure",
            "fn make(n: int) fn(int) int { fn(x: int) int = n + x }\nfn overwrite(n: int) int { let cells = [n, n, n, n, n, n, n, n]\n if n == 0 { return cells[0] }\n cells[int(uint(n) % uint(8))] + overwrite(n - 1) }\nfn main() { let first = make(40)\n let second = make(90)\n if overwrite(40) != 820 { panic(\"overwrite\") }\n if first(2) != 42 { panic(\"first capture\") }\n if second(3) != 93 { panic(\"second capture\") }\n if first(7) != 47 { panic(\"capture identity\") } }\n",
            0,
            "",
            "",
        ),
        (
            // 参数包必须在入队前复制到子协程自持的内存：launch 返回后它的帧即可被复用，
            // burn 的深递归会覆盖那片栈；子协程此时才第一次运行，读父帧就是悬垂读。
            "spawn-reuse",
            "fn make(n: int) fn(int) int { fn(x: int) int = n + x }\nfn burn(n: int) int { let cells: [int; 8] = [n, n, n, n, n, n, n, n]\n if n == 0 { return cells[0] }\n cells[int(uint(n) % uint(8))] + burn(n - 1) }\nfn child(f: fn(int) int, ch: chan[int]) { yield\n ch.send(f(1) + f(2)) }\nfn launch(ch: chan[int]) { let captured = make(40)\n let handle = async child(captured, ch)\n _ = handle }\nfn main() { let ch = chan[int](0)\n launch(ch)\n if burn(40) != 820 { panic(\"burn\") }\n match ch.recv() {\n Ok(value) => { if value != 83 { panic(\"pack reuse\") } _ = value }\n Err(_) => panic(\"closed\")\n }\n}\n",
            0,
            "",
            "",
        ),
        (
            "function-handle",
            "fn add(x: int) int = x + 2\nfn apply(f: fn(int) int, x: int) int = f(x)\nfn main() { let f: fn(int) int = add\n if apply(f, 40) != 42 { panic(\"function handle\") } }\n",
            0,
            "",
            "",
        ),
        (
            "function-abi",
            "struct Triple { a: int, b: int, c: int }\nfn identity(v: Triple) Triple = v\nfn apply(f: fn(Triple) Triple, v: Triple) Triple = f(v)\nfn sum(a: int, b: int, c: int, d: int, e: int, f: int, g: int, h: int, i: int, j: int) int = a + b * 2 + c * 3 + d * 4 + e * 5 + f * 6 + g * 7 + h * 8 + i * 9 + j * 10\nfn main() { let f: fn(Triple) Triple = identity\n let result = apply(f, Triple { a: 11, b: 22, c: 33 })\n if result.a != 11 || result.b != 22 || result.c != 33 { panic(\"aggregate adapter\") }\n let many: fn(int, int, int, int, int, int, int, int, int, int) int = sum\n if many(1, 2, 3, 4, 5, 6, 7, 8, 9, 10) != 385 { panic(\"stack adapter\") } }\n",
            0,
            "",
            "",
        ),
        (
            "large-mapping",
            "fn main() { let values: [uint; 5000] = [7; 5000]\n if values[0] != uint(7) || values[4999] != uint(7) { panic(\"large mapping\") } }\n",
            0,
            "",
            "",
        ),
        (
            "yield",
            "fn child(ch: chan[int]) { ch.send(1)\n yield\n ch.send(2)\n }\nfn main() {\n let ch = chan[int](0)\n let handle = async child(ch)\n match ch.recv() {\n Ok(value) => { if value != 1 { panic(\"yield\") } _ = value }\n Err(_) => panic(\"closed\")\n }\n match ch.recv() {\n Ok(value) => { if value != 2 { panic(\"yield2\") } _ = value }\n Err(_) => panic(\"closed2\")\n }\n _ = handle.wait()\n}\n",
            0,
            "",
            "",
        ),
        (
            "close",
            "fn main() {\n let ch = chan[int](1)\n ch.send(7)\n ch.close()\n match ch.recv() {\n Ok(value) => { if value != 7 { panic(\"lost\") } _ = value }\n Err(_) => panic(\"early\")\n }\n match ch.recv() {\n Ok(_) => panic(\"extra\")\n Err(_) => { _ = 0 }\n }\n}\n",
            0,
            "",
            "",
        ),
        (
            "close-send",
            "fn main() {\n let ch = chan[int](1)\n ch.close()\n ch.send(1)\n}\n",
            1,
            "",
            "{\"schema\":\"gugu-runtime-report-v1\",\"event\":\"panic\",\"class\":\"program-failure\",\"reason\":\"unhandled-panic\",\"message\":\"send on closed channel\",\"location\":{\"file\":\"main.gg\",\"line\":4,\"column\":2},\"backtrace\":[],\"exit_code\":1}\n{\"schema\":\"gugu-runtime-report-v1\",\"event\":\"termination\",\"class\":\"program-failure\",\"reason\":\"unhandled-panic\",\"message\":null,\"location\":null,\"backtrace\":[],\"exit_code\":1}\n",
        ),
        (
            "select",
            "fn main() {\n let first = chan[int](1)\n let second = chan[int](1)\n first.send(1)\n second.send(2)\n select {\n let value = first.recv() => { _ = value }\n let value = second.recv() => { panic(\"second\") }\n }\n match second.recv() {\n Ok(value) => { if value != 2 { panic(\"select\") } }\n Err(_) => panic(\"empty\")\n }\n}\n",
            0,
            "",
            "",
        ),
        (
            "lock",
            "use std.sync.{Mutex}\nfn child(gate: &Mutex[int], ch: chan[int]) {\n gate.lock()\n ch.send(1)\n gate.unlock()\n}\nfn main() {\n let gate = Mutex[int]::new(0)\n let ch = chan[int](0)\n gate.lock()\n let handle = async child(&gate, ch)\n yield\n gate.unlock()\n match ch.recv() {\n Ok(value) => { if value != 1 { panic(\"lock\") } _ = value }\n Err(_) => panic(\"closed\")\n }\n _ = handle.wait()\n}\n",
            0,
            "",
            "",
        ),
        (
            "wait-children",
            "fn child() { let n = 0\n while n < 10 { n = n + 1 }\n }\nfn main() {\n let handle = async child()\n _ = handle\n}\n",
            0,
            "",
            "",
        ),
        (
            "static-init",
            "static MODULE: int = 41\nfn take() int {\n static LOCAL: int = 9\n LOCAL + MODULE\n}\nfn main() {\n if take() != 50 { panic(\"static\") }\n if take() != 50 { panic(\"static-again\") }\n}\n",
            0,
            "",
            "",
        ),
        (
            "atomic-return",
            "fn probe() int {\n let bytes: [byte; 3] = [90, 18, 165]\n let words: [uint; 3] = [81985529216486895, 9223372036854775826, 18364758544493064720]\n let expected: uint = 9223372036854775826\n let neighbor: uint = 81985529216486895\n unsafe {\n let p = std.ptr.addr_of(bytes[1])\n if std.ptr.ptr_read[byte](p) != byte(18) { return 1 }\n if bytes[0] != byte(90) { return 2 }\n if bytes[2] != byte(165) { return 3 }\n let q = std.ptr.addr_of(words[1])\n if std.ptr.ptr_read[uint](q) != expected { return 4 }\n if words[0] != neighbor { return 5 }\n _ = 0\n }\n 0\n}\nfn main() {\n let failure = probe()\n if failure != 0 { panic(\"atomic-return\") }\n}\n",
            0,
            "",
            "",
        ),
        (
            "unsafe-return",
            "fn probe(seed: uint) uint {\n let values: [uint; 4] = [3, 5, 7, 11]\n unsafe {\n let p = std.ptr.addr_of(values[1])\n let base = std.mem.transmute[*uint, uint](p)\n let scaled = base / uint(8) * uint(8)\n let offset = base - scaled\n let first = std.ptr.ptr_read[uint](std.mem.transmute[uint, *uint](base - offset + uint(8)))\n if first != uint(7) { return 41 }\n let second = std.ptr.ptr_read[uint](std.mem.transmute[uint, *uint](base - uint(8)))\n if second != uint(3) { return 42 }\n if offset != uint(0) { return 43 }\n _ = 0\n }\n seed % uint(2)\n}\nfn main() {\n let failure = probe(uint(4))\n if failure == uint(41) { panic(\"code-41\") }\n if failure == uint(42) { panic(\"code-42\") }\n if failure == uint(43) { panic(\"code-43\") }\n if failure != 0 { panic(\"code-other\") }\n}\n",
            0,
            "",
            "",
        ),
        (
            "tuple-returns",
            "fn pick(flag: int) (int, int) {\n if flag == 1 { return (7, 8) }\n if flag == 2 { return (9, 10) }\n (11, 12)\n}\nfn main() {\n let (first, second) = pick(1)\n if first != 7 { panic(\"first\") }\n if second != 8 { panic(\"second\") }\n let (third, fourth) = pick(3)\n if third != 11 { panic(\"third\") }\n if fourth != 12 { panic(\"fourth\") }\n yield\n}\n",
            0,
            "",
            "",
        ),
        (
            "wait-managed",
            "fn child() fn(int) int {\n let base: int = 11\n return fn(add: int) int = base + add\n}\nfn main() {\n let handle = async child()\n yield\n match handle.wait() {\n Ok(carried) => {\n if carried(1) != 12 { panic(\"wait-managed\") }\n _ = carried\n }\n Err(_) => panic(\"wait-err\")\n }\n}\n",
            0,
            "",
            "",
        ),
        (
            // 分配量必须超过 nursery 的 2 MiB arena：每个闭包环境持有一个 512 项 int 数组
            // （4 KiB），600 次创建累计 2.4 MiB。无回收时 `gc_refill_line` 必然失败并 fatal，
            // 因此“跑完且哨兵仍可调用”就是镜像内真实发生回收且保留可达对象的证据。
            "closure-pressure",
            "fn total(block: [int; 512]) int { let sum = 0\n let index = 0\n while index < 512 { sum = sum + block[index]\n index = index + 1 }\n sum }\nfn make(n: int) fn(int) int { let block: [int; 512] = [n; 512]\n fn(x: int) int = total(block) + x }\nfn main() { let sentinel = make(40)\n let index = 0\n while index < 600 { let f = make(index)\n if f(1) != index * 512 + 1 { panic(\"reclaim-live\") }\n _ = f\n index = index + 1 }\n if sentinel(2) != 40 * 512 + 2 { panic(\"reclaim-sentinel\") }\n}\n",
            0,
            "",
            "",
        ),
        (
            // 父栈复用：`make` 返回后其帧被 `churn` 复用，闭包捕获的对象必须仍然正确。
            "stack-reuse",
            "fn make(n: int) fn(int) int { let block: [int; 512] = [n; 512]\n fn(x: int) int = block[0] + x }\nfn churn(n: int) [int; 512] { [n; 512] }\nfn main() { let carried = make(11)\n let scratch = churn(5)\n if scratch[0] != 5 { panic(\"scratch\") }\n if carried(1) != 12 { panic(\"stack-reuse\") }\n}\n",
            0,
            "",
            "",
        ),
        (
            // 跨调用活跃引用：哨兵闭包在整个分配循环中始终活跃，每次分配后都要重新读取。
            "live-across-calls",
            "fn total(block: [int; 512]) int { let sum = 0\n let index = 0\n while index < 512 { sum = sum + block[index]\n index = index + 1 }\n sum }\nfn make(n: int) fn(int) int { let block: [int; 512] = [n; 512]\n fn(x: int) int = total(block) + x }\nfn main() { let sentinel = make(7)\n let index = 0\n while index < 600 { let f = make(index)\n if f(1) != index * 512 + 1 { panic(\"churn-live\") }\n if sentinel(1) != 7 * 512 + 1 { panic(\"across-calls\") }\n _ = f\n index = index + 1 }\n}\n",
            0,
            "",
            "",
        ),
        (
            // 全局静态：静态数组在镜像数据段，读取不得被当成受管指针。
            "statics",
            "static TABLE: [int; 8] = [1, 2, 3, 4, 5, 6, 7, 8]\nfn main() { let index = 0\n let sum = 0\n while index < 8 { sum = sum + TABLE[index]\n index = index + 1 }\n if sum != 36 { panic(\"statics\") }\n}\n",
            0,
            "",
            "",
        ),
        (
            // 切片跨调用存活：`view` 是落在捕获对象内部的切片，整个分配循环中保持活跃。
            "slice-across-calls",
            "fn total(block: [int; 512]) int { let sum = 0\n let index = 0\n while index < 512 { sum = sum + block[index]\n index = index + 1 }\n sum }\nfn make(n: int) fn() &[int] { let block: [int; 512] = [n; 512]\n fn() &[int] = block[0..512] }\nfn churn(n: int) fn(int) int { let block: [int; 512] = [n; 512]\n fn(x: int) int = total(block) + x }\nfn main() { let maker = make(41)\n let view = maker()\n let index = 0\n while index < 600 { let f = churn(index)\n if f(1) != index * 512 + 1 { panic(\"churn\") }\n if view[index % 8] != 41 { panic(\"slice-live\") }\n _ = f\n index = index + 1 }\n}\n",
            0,
            "",
            "",
        ),
    ];
    for (name, source, code, stdout, stderr) in programs {
        if std::env::var("GUGU_PIE_CASE").is_ok_and(|filter| filter != name) {
            continue;
        }
        launch(name, source, code, stdout, stderr);
    }
    const GC_CASES: &[(&str, &str)] = &[
        ("gc-alloc", "gc:gc-alloc:ok\n"),
        ("gc-refill", "gc:gc-refill:ok\n"),
        ("gc-minor", "gc:gc-minor:ok\n"),
        ("gc-interior", "gc:gc-interior:ok\n"),
        ("gc-root-map", "gc:gc-root-map:ok\n"),
        ("gc-raw-not-root", "gc:gc-raw-not-root:ok\n"),
        ("gc-static", "gc:gc-static:ok\n"),
        ("gc-dynamic-roots", "gc:gc-dynamic-roots:ok\n"),
        ("gc-shared-capture", "gc:gc-shared-capture:ok\n"),
        ("gc-barrier", "gc:gc-barrier:ok\n"),
        ("gc-stop-flush", "gc:gc-stop-flush:ok\n"),
        ("gc-major", "gc:gc-major:ok\n"),
        ("gc-mark-ticket", "gc:gc-mark-ticket:ok\n"),
        ("gc-trace", "gc:gc-trace:ok\n"),
        ("gc-region", "gc:gc-region:ok\n"),
        ("gc-candidate", "gc:gc-candidate:ok\n"),
        ("gc-block-return", "gc:gc-block-return:ok\n"),
        ("gc-compressed", "gc:gc-compressed:ok\n"),
        ("gc-pacing", "gc:gc-pacing:ok\n"),
        ("gc-oom", "gc:gc-oom:ok\n"),
        ("gc-corrupt", "gc:gc-corrupt:ok\n"),
    ];
    let case_filter = std::env::var("GUGU_PIE_CASE").ok();
    for &(case, stdout) in GC_CASES {
        if case_filter
            .as_deref()
            .is_some_and(|filter| filter != "all" && filter != case)
        {
            continue;
        }
        launch_request(
            case,
            CompileRequest::runtime_gc_probe(case, TargetName::X86_64Linux),
            0,
            stdout,
            "",
        );
    }
    #[cfg(feature = "runtime-gc-probes")]
    if case_filter.is_some_and(|filter| filter == "all") {
        launch_request(
            "gc-atomics",
            CompileRequest::runtime_gc_probe("gc-atomics", TargetName::X86_64Linux),
            0,
            "gc:gc-atomics:ok\n",
            "",
        );
    }
}

fn launch(name: &str, source: &str, code: i32, stdout: &str, stderr: &str) {
    launch_request(
        name,
        CompileRequest::single_file("main.gg", source, TargetName::X86_64Linux),
        code,
        stdout,
        stderr,
    );
}

fn launch_request(name: &str, request: CompileRequest, code: i32, stdout: &str, stderr: &str) {
    let compilation = Compiler::with_artifacts(&EMBEDDED_ARTIFACTS).compile(request);
    if !compilation.is_success() {
        eprintln!("{name} 编译失败: {:?}", compilation.diagnostics().items());
        std::process::exit(1);
    }
    if std::env::var_os("GUGU_PIE_DUMP_LIR").is_some() {
        fs::write(
            "/tmp/gugu-pie-debug-lir.txt",
            compilation.dump_lir().expect("LIR"),
        )
        .expect("调试 LIR");
    }
    let plan = compilation.image_plan().expect("镜像计划");
    if plan.linux_image_kind() != "static-pie" || !plan.linux_interpreter().is_empty() {
        eprintln!("{name} 不是无解释器的 static PIE");
        std::process::exit(1);
    }
    let path = std::env::temp_dir().join(format!("gugu-pie-{name}"));
    fs::write(&path, plan.linux_image()).expect("写出镜像");
    let mut permissions = fs::metadata(&path).expect("元数据").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&path, permissions).expect("权限");
    let output = Command::new(&path).output().unwrap_or_else(|error| {
        eprintln!("{name} 启动失败: {error}");
        std::process::exit(1);
    });
    let actual_code = output.status.code();
    println!("{name} status={:?}", output.status);
    if actual_code != Some(code) {
        // 退出码不符时打印编译期的 runtime 契约：绑定/兜底的 glue 条数直接指出哪个入口没有
        // 真实定义（兜底符号会落到 `__gugu_stub_exit`，表现为 exit 56）。
        if let Some(dump) = compilation.dump_runtime() {
            eprintln!("{name} runtime 契约:\n{dump}");
        }
        eprintln!(
            "{name} 退出码不符：预期 {code}，实际 {actual_code:?}; stdout={:?}; stderr={:?}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        std::process::exit(1);
    }
    if output.stdout != stdout.as_bytes() || output.stderr != stderr.as_bytes() {
        eprintln!(
            "{name} 输出不符：stdout={:?}，stderr={:?}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        std::process::exit(1);
    }
    if name == "panic" {
        corrupt_stackmap(plan.linux_image(), plan.x64_stackmap_section());
    }
}

/// 损坏栈图节头后，任何走安全点校验的路径都必须 fatal 退出（2）并报告同一条 fatal。
///
/// 定位只用节头整段前缀（魔数 + 版本字 + 头长度）：单独的魔数常量也以立即数形式活在机器码里
/// （`checked_safepoint` 的比较），只按魔数定位会翻转代码而不是节数据，运行期也就查不出来。
/// 运行的程序必须真的走到校验点：未捕获 panic 的 `panic_dispatch` 会校验节头，因此这里用
/// panic 程序，而不是纯递归程序（后者是否触发 morestack 取决于栈预留策略）。
fn corrupt_stackmap(image: &[u8], section: &[u8]) {
    let mut image = image.to_vec();
    let header = &section[..16.min(section.len())];
    assert!(header.starts_with(b"GUGUSM01"), "栈图节必须以魔数开头");
    let at = image
        .windows(header.len())
        .position(|bytes| bytes == header)
        .expect("栈图节头");
    image[at] ^= 1;
    let path = std::env::temp_dir().join("gugu-pie-invalid-stackmap");
    fs::write(&path, image).expect("写出损坏栈图镜像");
    let mut permissions = fs::metadata(&path).expect("元数据").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&path, permissions).expect("权限");
    let output = Command::new(&path).output().expect("加载损坏栈图镜像");
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        output.stderr,
        b"runtime fatal: invalid stack map or allocation\n"
    );
    assert!(output.stdout.is_empty());
}
