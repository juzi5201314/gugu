//! Windows 运行时入口。退出和内存都走 IAT，不写 syscall 号。

use super::super::elf::BootOffsets;
use super::super::elf::asm::Asm;

const RAX: u8 = 0;
const RCX: u8 = 1;
const RDX: u8 = 2;
const RBX: u8 = 3;
const RSP: u8 = 4;
const RSI: u8 = 6;
const RDI: u8 = 7;
const R8: u8 = 8;
const R9: u8 = 9;
const R10: u8 = 10;
const R11: u8 = 11;
const R14: u8 = 14;
const R15: u8 = 15;

const EXIT: &str = "ntdll.dll!RtlExitUserProcess";
const ALLOC: &str = "kernel32.dll!VirtualAlloc";
const FREE: &str = "kernel32.dll!VirtualFree";
const PROTECT: &str = "kernel32.dll!VirtualProtect";
const STDOUT: &str = "kernel32.dll!GetStdHandle";
const WRITE: &str = "kernel32.dll!WriteFile";

const MEM_COMMIT_RESERVE: u32 = 0x3000;
const MEM_RELEASE: u32 = 0x8000;
const PAGE_NOACCESS: u32 = 0x01;
const PAGE_READWRITE: u32 = 0x04;
const GROWTH: i32 = 64 * 1024;

#[derive(Clone)]
pub(super) struct ImportUse {
    pub(super) offset: u32,
    pub(super) symbol: &'static str,
}

#[derive(Clone)]
pub(super) struct CallSite {
    pub(super) offset: u32,
    pub(super) symbol: String,
}

pub(super) struct Routine {
    pub(super) name: &'static str,
    pub(super) bytes: Vec<u8>,
    pub(super) imports: Vec<ImportUse>,
    pub(super) calls: Vec<CallSite>,
    pub(super) runtime: bool,
}

pub(super) struct Gen {
    pub(super) asm: Asm,
    imports: Vec<ImportUse>,
    calls: Vec<CallSite>,
}

impl Gen {
    pub(super) fn new() -> Self {
        Self {
            asm: Asm::new(),
            imports: Vec::new(),
            calls: Vec::new(),
        }
    }

    pub(super) fn call_import(&mut self, symbol: &'static str) {
        self.asm.emit(&[0xff, 0x15]);
        let offset = u32::try_from(self.asm.offset()).expect("导入位移");
        self.asm.emit(&[0, 0, 0, 0]);
        self.imports.push(ImportUse { offset, symbol });
    }

    fn call_entry(&mut self, symbol: &str) {
        self.asm.emit(&[0xe8]);
        let offset = u32::try_from(self.asm.offset()).expect("入口位移");
        self.asm.emit(&[0, 0, 0, 0]);
        self.calls.push(CallSite {
            offset,
            symbol: symbol.to_owned(),
        });
    }

    pub(super) fn alloc_reg(&mut self, size: u8, protect: u32, fail: u32) {
        self.asm.sub_imm32(RSP, 40);
        self.asm.xor_self32(RCX);
        self.asm.mov_reg(RDX, size);
        self.asm.mov_imm32(R8, MEM_COMMIT_RESERVE);
        self.asm.mov_imm32(R9, protect);
        self.call_import(ALLOC);
        self.asm.add_imm32(RSP, 40);
        self.asm.test_self(RAX);
        self.asm.je(fail);
    }

    pub(super) fn exit(&mut self, code: u32) {
        self.asm.sub_imm32(RSP, 40);
        self.asm.mov_imm32(RCX, code);
        self.call_import(EXIT);
        self.asm.ud2();
    }

    pub(super) fn alloc(&mut self, size: u32, protect: u32, fail: u32) {
        self.asm.sub_imm32(RSP, 40);
        self.asm.xor_self32(RCX);
        self.asm.mov_imm32(RDX, size);
        self.asm.mov_imm32(R8, MEM_COMMIT_RESERVE);
        self.asm.mov_imm32(R9, protect);
        self.call_import(ALLOC);
        self.asm.add_imm32(RSP, 40);
        self.asm.test_self(RAX);
        self.asm.je(fail);
    }

    pub(super) fn finish(self, name: &'static str, runtime: bool) -> Routine {
        Routine {
            name,
            bytes: self.asm.finish(),
            imports: self.imports,
            calls: self.calls,
            runtime,
        }
    }
}

pub(super) fn routines(boot: BootOffsets, entry: &str) -> Vec<Routine> {
    let mut out = vec![
        rt0(boot, entry),
        morestack(boot),
        panic_exit(),
        alloc_payload(64 * 1024, "gc_alloc_slow"),
        alloc_payload(64 * 1024, "gc_region_slow"),
        barrier(boot),
        channel_new(),
        channel_send(),
        channel_receive(),
        spawn_task(),
        virtual_free_header(),
        memset_body(),
        memmove_body(),
    ];
    let fence = fence_ret();
    for name in [
        "gc_region_publish",
        "gc_region_reset",
        "gc_shared_access_begin",
        "gc_shared_access_end",
        "gc_region_promote",
        "gc_region_transfer",
        "gc_mark_ticket_batch",
        "gc_edge_delta_batch",
        "gc_shared_field_barrier",
        "gc_forward_shared_handle",
        "value_publish",
        "value_drop",
        "value_forget",
        "pin",
        "unpin",
        "resource_transfer",
        "platform_huge_page_hint",
        "platform_set_dump_policy",
    ] {
        out.push(clone_named(name, &fence));
    }
    let poll = clear_poll(boot);
    for name in ["safepoint_slow", "yield", "sched_park", "sched_ready"] {
        out.push(clone_named(name, &poll));
    }
    out.push(identity("gc_resolve_shared_handle"));
    out.push(identity("dynamic_erase"));
    out.push(super::algo::empty_pair("format"));
    out.push(select_commit());
    out.push(channel_close());
    out.push(resource_acquire(boot));
    out.push(resource_release(boot, "resource_release"));
    out.push(resource_release(boot, "resource_finalize"));
    out.push(type_is());
    out.push(wide_div());
    out.push(coroutine_switch());
    out.push(memmove_body_named("memcpy"));
    out.extend(super::algo::routines());
    out.extend(super::platform::routines());
    out
}

fn clone_named(name: &'static str, source: &Routine) -> Routine {
    Routine {
        name,
        bytes: source.bytes.clone(),
        imports: source.imports.clone(),
        calls: Vec::new(),
        runtime: true,
    }
}

fn fence_ret() -> Routine {
    let mut code = Gen::new();
    code.asm.mfence();
    code.asm.ret();
    code.finish("fence", true)
}

fn identity(name: &'static str) -> Routine {
    let mut code = Gen::new();
    code.asm.ret();
    code.finish(name, true)
}

fn clear_poll(boot: BootOffsets) -> Routine {
    let mut code = Gen::new();
    let poll = i32::try_from(boot.poll).expect("poll 偏移");
    code.asm.xor_self32(RAX);
    code.asm.mov_store32(R15, poll, RAX);
    code.asm.ret();
    code.finish("poll", true)
}

fn rt0(boot: BootOffsets, entry: &str) -> Routine {
    let mut code = Gen::new();
    let fail = code.asm.label();
    code.asm.and_imm32(RSP, -16);
    code.asm.sub_imm32(RSP, 8);
    code.alloc(8 * 1024 * 1024, PAGE_NOACCESS, fail);
    code.asm.mov_reg(RBX, RAX);
    protect_tail(&mut code, fail);
    code.alloc(4096, PAGE_READWRITE, fail);
    code.asm.mov_reg(R14, RAX);
    store_stack(&mut code, boot);
    code.alloc(8192, PAGE_READWRITE, fail);
    code.asm.mov_reg(R15, RAX);
    code.alloc(64 * 1024, PAGE_READWRITE, fail);
    store_span(&mut code, boot.tlab_cursor, boot.tlab_limit);
    code.alloc(64 * 1024, PAGE_READWRITE, fail);
    store_span(&mut code, boot.turn_cursor, boot.turn_limit);
    let check = i32::try_from(boot.stack_check).expect("栈检查偏移");
    code.asm.mov_load(RAX, R14, check);
    code.asm.mov_reg(RSP, RAX);
    code.asm.add_imm32(RSP, -8);
    code.call_entry(entry);
    code.exit(0);
    code.asm.bind(fail);
    code.exit(127);
    code.finish("_start", true)
}

fn protect_tail(code: &mut Gen, fail: u32) {
    code.asm.mov_reg(RDI, RBX);
    code.asm.add_imm32(RDI, 8 * 1024 * 1024 - 4096);
    code.asm.sub_imm32(RSP, 40);
    code.asm.mov_reg(RCX, RDI);
    code.asm.mov_imm32(RDX, 4096);
    code.asm.mov_imm32(R8, PAGE_READWRITE);
    lea_rsp(&mut code.asm, R9, 32);
    code.call_import(PROTECT);
    code.asm.add_imm32(RSP, 40);
    code.asm.test_self(RAX);
    code.asm.je(fail);
}

fn store_stack(code: &mut Gen, boot: BootOffsets) {
    let check = i32::try_from(boot.stack_check).expect("栈检查偏移");
    let base = i32::try_from(boot.map_base).expect("映射基址偏移");
    code.asm.mov_reg(RAX, RBX);
    code.asm.add_imm32(RAX, 8 * 1024 * 1024);
    code.asm.mov_store(R14, check, RAX);
    code.asm.mov_store(R14, base, RBX);
}

fn store_span(code: &mut Gen, cursor: u32, limit: u32) {
    let cursor = i32::try_from(cursor).expect("cursor 偏移");
    let limit = i32::try_from(limit).expect("limit 偏移");
    code.asm.push(RAX);
    code.asm.add_imm32(RAX, 32);
    code.asm.mov_store(R15, cursor, RAX);
    code.asm.pop(RAX);
    code.asm.add_imm32(RAX, 64 * 1024);
    code.asm.mov_store(R15, limit, RAX);
}

fn morestack(boot: BootOffsets) -> Routine {
    let mut code = Gen::new();
    let fail = code.asm.label();
    let ok = code.asm.label();
    for reg in [RAX, RBX, RCX, RDX, RSI, RDI, R8, R9, 10, R11] {
        code.asm.push(reg);
    }
    let check = i32::try_from(boot.stack_check).expect("栈检查偏移");
    let base = i32::try_from(boot.map_base).expect("映射基址偏移");
    code.asm.mov_load(RAX, R14, check);
    code.asm.sub_imm32(RAX, GROWTH);
    code.asm.cmp_mem(RAX, R14, base);
    code.asm.jb(fail);
    code.asm.mov_reg(RDI, RAX);
    code.asm.sub_imm32(RSP, 40);
    code.asm.mov_reg(RCX, RDI);
    code.asm.mov_imm32(RDX, GROWTH as u32);
    code.asm.mov_imm32(R8, PAGE_READWRITE);
    lea_rsp(&mut code.asm, R9, 32);
    code.call_import(PROTECT);
    code.asm.add_imm32(RSP, 40);
    code.asm.test_self(RAX);
    code.asm.je(fail);
    code.asm.mov_load(RAX, R14, check);
    code.asm.sub_imm32(RAX, GROWTH);
    code.asm.mov_store(R14, check, RAX);
    let poll = i32::try_from(boot.poll).expect("poll 偏移");
    code.asm.xor_self32(RAX);
    code.asm.mov_store32(R15, poll, RAX);
    code.asm.jmp(ok);
    code.asm.bind(fail);
    code.exit(127);
    code.asm.bind(ok);
    for reg in [R11, 10, R9, R8, RDI, RSI, RDX, RCX, RBX, RAX] {
        code.asm.pop(reg);
    }
    code.asm.ret();
    code.finish("morestack_or_poll", true)
}

fn panic_exit() -> Routine {
    let mut code = Gen::new();
    code.asm.mov_load(RBX, RAX, 0);
    code.asm.mov_load(RSI, RAX, 8);
    code.asm.sub_imm32(RSP, 56);
    code.asm.mov_imm32(RCX, -11_i32 as u32);
    code.call_import(STDOUT);
    code.asm.mov_reg(RCX, RAX);
    code.asm.mov_reg(RDX, RBX);
    code.asm.mov_reg(R8, RSI);
    lea_rsp(&mut code.asm, R9, 40);
    code.asm.xor_self32(R11);
    code.asm.mov_store(RSP, 32, R11);
    code.call_import(WRITE);
    code.asm.add_imm32(RSP, 56);
    code.exit(101);
    code.finish("panic", true)
}

fn alloc_payload(size: u32, name: &'static str) -> Routine {
    let mut code = Gen::new();
    let fail = code.asm.label();
    code.alloc(size, PAGE_READWRITE, fail);
    code.asm.add_imm32(RAX, 32);
    code.asm.ret();
    code.asm.bind(fail);
    code.exit(127);
    code.finish(name, true)
}

fn barrier(boot: BootOffsets) -> Routine {
    let mut code = Gen::new();
    let slot = i32::try_from(boot.barrier).expect("屏障槽偏移");
    code.asm.inc_mem64(R15, slot);
    code.asm.mov_store(R15, slot + 8, RAX);
    code.asm.ret();
    code.finish("gc_write_barrier", true)
}

fn channel_new() -> Routine {
    let mut code = Gen::new();
    let fail = code.asm.label();
    let bad = code.asm.label();
    code.asm.push(RAX);
    code.alloc(4096, PAGE_READWRITE, fail);
    code.asm.pop(RDX);
    code.asm.cmp_imm32(RDX, 0);
    code.asm.jle(bad);
    code.asm.cmp_imm32(RDX, 500);
    code.asm.ja(bad);
    code.asm.mov_store(RAX, 0, RDX);
    code.asm.xor_self32(RDX);
    code.asm.mov_store(RAX, 8, RDX);
    code.asm.mov_store(RAX, 16, RDX);
    code.asm.ret();
    code.asm.bind(fail);
    code.exit(127);
    code.asm.bind(bad);
    code.exit(127);
    code.finish("channel_new", true)
}

fn channel_send() -> Routine {
    let mut code = Gen::new();
    let full = code.asm.label();
    let closed = code.asm.label();
    code.asm.mov_reg(R11, RAX);
    code.asm.movzx8(RCX, R11, 4088);
    code.asm.test_self(RCX);
    code.asm.jne(closed);
    code.asm.mov_load(RCX, R11, 8);
    code.asm.cmp_mem(RCX, R11, 0);
    code.asm.jae(full);
    enqueue(&mut code);
    code.asm.mov_imm32(RAX, 1);
    code.asm.ret();
    code.asm.bind(full);
    code.asm.xor_self32(RAX);
    code.asm.ret();
    code.asm.bind(closed);
    code.exit(101);
    code.finish("channel_send", true)
}

fn channel_receive() -> Routine {
    let mut code = Gen::new();
    let empty = code.asm.label();
    code.asm.mov_reg(R11, RAX);
    code.asm.mov_load(RCX, R11, 8);
    code.asm.test_self(RCX);
    code.asm.je(empty);
    code.asm.mov_load(RDX, R11, 16);
    code.asm.lea_disp(RAX, R11, RDX, 8, 24);
    code.asm.mov_load(RAX, RAX, 0);
    code.asm.push(RAX);
    code.asm.mov_reg(RAX, RDX);
    code.asm.add_imm32(RAX, 1);
    code.asm.xor_self32(RDX);
    code.asm.mov_load(RCX, R11, 0);
    code.asm.div_r64(RCX);
    code.asm.mov_store(R11, 16, RDX);
    code.asm.dec_mem64(R11, 8);
    code.asm.pop(RAX);
    code.asm.ret();
    code.asm.bind(empty);
    code.exit(127);
    code.finish("channel_receive", true)
}

fn channel_close() -> Routine {
    let mut code = Gen::new();
    let fail = code.asm.label();
    code.asm.movzx8(RCX, RAX, 4088);
    code.asm.test_self(RCX);
    code.asm.jne(fail);
    code.asm.mov_imm32(RCX, 1);
    code.asm.mov_store(RAX, 4088, RCX);
    code.asm.ret();
    code.asm.bind(fail);
    code.exit(101);
    code.finish("channel_close", true)
}

fn enqueue(code: &mut Gen) {
    code.asm.mov_load(RDX, R11, 16);
    code.asm.add_reg(RDX, RCX);
    code.asm.mov_load(RCX, R11, 0);
    code.asm.mov_reg(RAX, RDX);
    code.asm.xor_self32(RDX);
    code.asm.div_r64(RCX);
    code.asm.lea_disp(RAX, R11, RDX, 8, 24);
    code.asm.mov_store(RAX, 0, RBX);
    code.asm.inc_mem64(R11, 8);
}

fn spawn_task() -> Routine {
    let mut code = Gen::new();
    let fail = code.asm.label();
    code.asm.push(RAX);
    code.asm.push(RBX);
    code.alloc(4096, PAGE_READWRITE, fail);
    code.asm.pop(RBX);
    code.asm.pop(RCX);
    code.asm.mov_store(RAX, 0, RCX);
    code.asm.mov_store(RAX, 8, RBX);
    code.asm.ret();
    code.asm.bind(fail);
    code.exit(127);
    code.finish("spawn", true)
}

fn select_commit() -> Routine {
    let mut code = Gen::new();
    code.asm.xor_self32(RAX);
    code.asm.xor_self32(RBX);
    code.asm.ret();
    code.finish("select_commit", true)
}

fn resource_acquire(boot: BootOffsets) -> Routine {
    let mut code = Gen::new();
    code.asm.inc_mem64(R15, lease(boot));
    code.asm.ret();
    code.finish("resource_acquire", true)
}

fn resource_release(boot: BootOffsets, name: &'static str) -> Routine {
    let mut code = Gen::new();
    let fail = code.asm.label();
    let slot = lease(boot);
    code.asm.mov_load(RAX, R15, slot);
    code.asm.test_self(RAX);
    code.asm.je(fail);
    code.asm.dec_mem64(R15, slot);
    code.asm.ret();
    code.asm.bind(fail);
    code.exit(127);
    code.finish(name, true)
}

fn lease(boot: BootOffsets) -> i32 {
    i32::try_from(boot.barrier + 16).expect("租约槽")
}

fn type_is() -> Routine {
    let mut code = Gen::new();
    let same = code.asm.label();
    code.asm.mov_imm32(RCX, 1);
    code.asm.cmp_mem(RBX, RAX, 0);
    code.asm.je(same);
    code.asm.xor_self32(RCX);
    code.asm.bind(same);
    code.asm.mov_reg(RAX, RCX);
    code.asm.ret();
    code.finish("type_is", true)
}

fn wide_div() -> Routine {
    let mut code = Gen::new();
    let slow = code.asm.label();
    let fail = code.asm.label();
    code.asm.test_self(RDX);
    code.asm.jne(slow);
    fast_div(&mut code.asm, fail);
    code.asm.ret();
    code.asm.bind(slow);
    wide_long(&mut code.asm);
    code.asm.ret();
    code.asm.bind(fail);
    code.exit(127);
    code.finish("wide_div", true)
}

fn fast_div(asm: &mut Asm, fail: u32) {
    asm.test_self(RCX);
    asm.je(fail);
    asm.push(RAX);
    asm.mov_reg(RAX, RBX);
    asm.xor_self32(RDX);
    asm.div_r64(RCX);
    asm.mov_reg(RBX, RAX);
    asm.pop(RAX);
    asm.div_r64(RCX);
}

fn wide_long(asm: &mut Asm) {
    let step = asm.label();
    let subtract = asm.label();
    let next = asm.label();
    asm.mov_reg(R8, RAX);
    asm.mov_reg(R9, RBX);
    asm.mov_reg(R10, RCX);
    asm.mov_reg(R11, RDX);
    asm.xor_self32(RAX);
    asm.xor_self32(RBX);
    asm.xor_self32(RCX);
    asm.xor_self32(RDX);
    asm.mov_imm32(RSI, 128);
    asm.bind(step);
    shift_pair(asm);
    asm.cmp_reg(RDX, R11);
    asm.jb(next);
    asm.ja(subtract);
    asm.cmp_reg(RCX, R10);
    asm.jb(next);
    asm.bind(subtract);
    asm.sub_reg(RCX, R10);
    asm.sbb_reg(RDX, R11);
    asm.or_imm8(RAX, 1);
    asm.bind(next);
    asm.dec_reg(RSI);
    asm.jne(step);
}

fn shift_pair(asm: &mut Asm) {
    asm.shl1(RAX);
    asm.rcl1(RBX);
    asm.shl1(R8);
    asm.rcl1(R9);
    asm.rcl1(RCX);
    asm.rcl1(RDX);
}

fn coroutine_switch() -> Routine {
    Routine {
        name: "coroutine_switch",
        bytes: crate::runtime::ContextSwitchCode::fixed().bytes,
        imports: Vec::new(),
        calls: Vec::new(),
        runtime: true,
    }
}

fn memset_body() -> Routine {
    let mut code = Gen::new();
    let done = code.asm.label();
    let fill = code.asm.label();
    code.asm.mov_reg(R11, RAX);
    code.asm.test_self(RCX);
    code.asm.je(done);
    code.asm.bind(fill);
    code.asm.store_bl();
    code.asm.add_imm32(RAX, 1);
    code.asm.dec_reg(RCX);
    code.asm.jne(fill);
    code.asm.bind(done);
    code.asm.mov_reg(RAX, R11);
    code.asm.ret();
    code.finish("memset", false)
}

fn memmove_body() -> Routine {
    memmove_body_named("memmove")
}

fn memmove_body_named(name: &'static str) -> Routine {
    let mut code = Gen::new();
    let done = code.asm.label();
    let forward = code.asm.label();
    let back = code.asm.label();
    code.asm.mov_reg(R11, RAX);
    code.asm.cmp_reg(RAX, RBX);
    code.asm.je(done);
    code.asm.jb(forward);
    code.asm.add_reg(RAX, RCX);
    code.asm.add_reg(RBX, RCX);
    code.asm.bind(back);
    code.asm.test_self(RCX);
    code.asm.je(done);
    code.asm.add_imm32(RAX, -1);
    code.asm.add_imm32(RBX, -1);
    code.asm.copy_byte();
    code.asm.dec_reg(RCX);
    code.asm.jmp(back);
    code.asm.bind(forward);
    code.asm.test_self(RCX);
    code.asm.je(done);
    code.asm.copy_byte();
    code.asm.add_imm32(RAX, 1);
    code.asm.add_imm32(RBX, 1);
    code.asm.dec_reg(RCX);
    code.asm.jmp(forward);
    code.asm.bind(done);
    code.asm.mov_reg(RAX, R11);
    code.asm.ret();
    code.finish(name, false)
}

fn virtual_free_header() -> Routine {
    let mut code = Gen::new();
    let fail = code.asm.label();
    code.asm.sub_imm32(RSP, 40);
    code.asm.mov_reg(RCX, RAX);
    code.asm.xor_self32(RDX);
    code.asm.mov_imm32(R8, MEM_RELEASE);
    code.call_import(FREE);
    code.asm.add_imm32(RSP, 40);
    code.asm.test_self(RAX);
    code.asm.je(fail);
    code.asm.ret();
    code.asm.bind(fail);
    code.exit(127);
    code.finish("platform_release", true)
}

pub(super) fn lea_rsp(asm: &mut Asm, dest: u8, disp: i8) {
    let mut rex = 0x48;
    if dest >= 8 {
        rex |= 0x04;
    }
    let modrm = 0x40 | ((dest & 7) << 3) | 0x04;
    asm.emit(&[rex, 0x8d, modrm, 0x24, disp as u8]);
}
