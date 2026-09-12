//! 手工machine-intrinsic smoke：运行compiler实际产出的代码，往返独立arena stack。
//! 宿主VM与裸汇编仅存在于此验收二进制；compiler保持forbid(unsafe_code)。

use gugu_compiler::{CompileRequest, Compiler, CoroutineContext, TargetName};
use std::arch::naked_asm;
use std::mem::offset_of;

#[repr(C)]
struct State {
    system: CoroutineContext,
    child: CoroutineContext,
    switch: usize,
    restore: usize,
    hot: usize,
    processor: usize,
    observed_hot: usize,
    observed_processor: usize,
    step: u64,
    result: u64,
    failure: u64,
}

/// SysV宿主adapter保留r14/r15；Gugu内部stub不承担宿主C ABI。
///
/// # Safety
/// code必须是当前契约的可执行片段，save/resume必须有效且地址稳定；目标栈拥有足够空间且不展开。
#[unsafe(naked)]
unsafe extern "sysv64" fn enter(
    _code: usize,
    _save: *mut CoroutineContext,
    _resume: *const CoroutineContext,
    _hot: usize,
    _processor: usize,
) {
    naked_asm!(
        "push r14",
        "push r15",
        "sub rsp, 8",
        "mov rax, rdi",
        "mov rdi, rsi",
        "mov rsi, rdx",
        "mov rdx, rcx",
        "mov rcx, r8",
        "call rax",
        "add rsp, 8",
        "pop r15",
        "pop r14",
        "ret",
    );
}

/// 用裸入口避免宿主compiler改变哨兵寄存器；调用和finish都走被验收的实际代码片段。
///
/// # Safety
/// r12必须指向独占State，rsp必须位于已提交的独立stack，State中的code/context均已校验。
#[unsafe(naked)]
unsafe extern "sysv64" fn child_entry() {
    naked_asm!(
        "sub rsp, 24",
        "mov qword ptr [rsp], 1234567",
        "mov rbx, 1111", "mov rbp, 2222", "mov r13, 3333",
        "mov [r12 + {observed_hot}], r14", "mov [r12 + {observed_processor}], r15",
        "mov qword ptr [r12 + {step}], 1",
        "lea rdi, [r12 + {child}]", "lea rsi, [r12 + {system}]",
        "mov rdx, [r12 + {hot}]", "mov rcx, [r12 + {processor}]",
        "call qword ptr [r12 + {switch}]",
        "cmp rbx, 1111", "jne 2f", "cmp rbp, 2222", "jne 2f", "cmp r13, 3333", "jne 2f",
        "cmp qword ptr [rsp], 1234567", "jne 2f",
        "cmp r14, [r12 + {hot}]", "jne 2f", "cmp r15, [r12 + {processor}]", "jne 2f",
        "mov qword ptr [r12 + {result}], 42", "jmp 3f",
        "2:", "mov qword ptr [r12 + {failure}], 1",
        "3:", "mov qword ptr [r12 + {step}], 2",
        "mov qword ptr [r12 + {child_rip}], 0",
        "lea rsi, [r12 + {system}]", "mov rdx, [r12 + {hot}]", "mov rcx, [r12 + {processor}]",
        "jmp qword ptr [r12 + {restore}]",
        system = const offset_of!(State, system), child = const offset_of!(State, child),
        child_rip = const offset_of!(State, child) + offset_of!(CoroutineContext, rip),
        switch = const offset_of!(State, switch), restore = const offset_of!(State, restore),
        hot = const offset_of!(State, hot), processor = const offset_of!(State, processor),
        observed_hot = const offset_of!(State, observed_hot), observed_processor = const offset_of!(State, observed_processor),
        step = const offset_of!(State, step), result = const offset_of!(State, result), failure = const offset_of!(State, failure),
    );
}

struct Mapping {
    base: *mut u8,
    bytes: usize,
}

#[cfg(target_os = "linux")]
mod vm {
    use super::Mapping;
    unsafe extern "C" {
        fn mmap(
            address: *mut u8,
            bytes: usize,
            protection: i32,
            flags: i32,
            fd: i32,
            offset: i64,
        ) -> *mut u8;
        fn mprotect(address: *mut u8, bytes: usize, protection: i32) -> i32;
        fn munmap(address: *mut u8, bytes: usize) -> i32;
    }
    pub(super) fn reserve(bytes: usize) -> Mapping {
        // SAFETY: 匿名mapping没有外部文件与别名，失败哨兵在产生引用前检查。
        let base = unsafe { mmap(std::ptr::null_mut(), bytes, 0, 0x22, -1, 0) };
        assert_ne!(base.addr(), usize::MAX, "mmap失败");
        Mapping { base, bytes }
    }
    pub(super) fn protect(base: *mut u8, bytes: usize, executable: bool) {
        // SAFETY: 调用者只传入已拥有mapping内的完整页；代码页从RW单向转为RX。
        assert_eq!(
            unsafe { mprotect(base, bytes, if executable { 5 } else { 3 }) },
            0,
            "mprotect失败"
        );
    }
    pub(super) fn release(mapping: &Mapping) {
        // SAFETY: 所有context已回到system stack，mapping内已无活跃引用。
        assert_eq!(
            unsafe { munmap(mapping.base, mapping.bytes) },
            0,
            "munmap失败"
        );
    }
}

#[cfg(target_os = "windows")]
mod vm {
    use super::Mapping;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn VirtualAlloc(
            address: *mut u8,
            bytes: usize,
            allocation: u32,
            protection: u32,
        ) -> *mut u8;
        fn VirtualProtect(address: *mut u8, bytes: usize, protection: u32, old: *mut u32) -> i32;
        fn VirtualFree(address: *mut u8, bytes: usize, operation: u32) -> i32;
    }
    pub(super) fn reserve(bytes: usize) -> Mapping {
        // SAFETY: 只取得PAGE_NOACCESS的独立reservation。
        let base = unsafe { VirtualAlloc(std::ptr::null_mut(), bytes, 0x2000, 1) };
        assert!(!base.is_null(), "VirtualAlloc reserve失败");
        Mapping { base, bytes }
    }
    pub(super) fn protect(base: *mut u8, bytes: usize, executable: bool) {
        if executable {
            let mut old = 0;
            // SAFETY: 代码已经完整写入，随后不再修改RX页。
            assert_ne!(
                unsafe { VirtualProtect(base, bytes, 0x20, &mut old) },
                0,
                "VirtualProtect失败"
            );
        } else {
            // SAFETY: 只提交reservation中的payload，不触碰两端guard。
            assert_eq!(
                unsafe { VirtualAlloc(base, bytes, 0x1000, 4) },
                base,
                "VirtualAlloc commit失败"
            );
        }
    }
    pub(super) fn release(mapping: &Mapping) {
        // SAFETY: 所有context已离开mapping。
        assert_ne!(
            unsafe { VirtualFree(mapping.base, 0, 0x8000) },
            0,
            "VirtualFree失败"
        );
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        vm::release(self);
    }
}

fn main() {
    let target = if cfg!(target_os = "windows") {
        TargetName::X86_64Windows
    } else {
        TargetName::X86_64Linux
    };
    let compilation = Compiler::new().compile(CompileRequest::single_file(
        "main.gg",
        "fn main() { let child = async { 42 }\n _ = child }",
        target,
    ));
    assert!(
        compilation.is_success(),
        "{:?}",
        compilation.diagnostics().items()
    );
    let contract = compilation
        .image_plan()
        .expect("镜像计划")
        .coroutine_runtime();
    let code = vm::reserve(4096);
    vm::protect(code.base, code.bytes, false);
    // SAFETY: 独占可写mapping足够容纳固定片段，随后设置RX再执行。
    unsafe {
        std::ptr::copy_nonoverlapping(
            contract.context.bytes.as_ptr(),
            code.base,
            contract.context.bytes.len(),
        );
    }
    vm::protect(code.base, code.bytes, true);
    let payload = usize::try_from(contract.stack.arena_bytes).expect("arena长度");
    let arena = vm::reserve(payload + 8192);
    let low = arena.base.wrapping_add(4096);
    vm::protect(low, payload, false);
    let high = low.addr() + 65536;
    let mut state = Box::new(State {
        system: CoroutineContext::default(),
        child: CoroutineContext {
            rsp: high - 8,
            rip: (child_entry as *const ()).addr(),
            ..CoroutineContext::default()
        },
        switch: code.base.addr(),
        restore: code.base.addr()
            + usize::try_from(contract.context.restore_offset).expect("入口偏移"),
        hot: 0x123400,
        processor: 0x567800,
        observed_hot: 0,
        observed_processor: 0,
        step: 0,
        result: 0,
        failure: 0,
    });
    state.child.r12 = std::ptr::from_mut(state.as_mut()).addr();
    // SAFETY: context与State地址稳定；独立栈为RW、代码为RX，裸入口不展开且必定返回system context。
    unsafe {
        enter(
            state.switch,
            &mut state.system,
            &state.child,
            state.hot,
            state.processor,
        );
    }
    assert_eq!(state.step, 1);
    assert_eq!(
        (state.observed_hot, state.observed_processor),
        (state.hot, state.processor)
    );
    assert!((low.addr()..high).contains(&state.child.rsp));
    state.processor = 0x987600;
    // SAFETY: 继续已保存的context，恢复新的processor寄存器，完成后单向返回system stack。
    unsafe {
        enter(
            state.switch,
            &mut state.system,
            &state.child,
            state.hot,
            state.processor,
        );
    }
    assert_eq!(
        (state.step, state.result, state.failure, state.child.rip),
        (2, 42, 0, 0)
    );
    drop(arena);
    assert_eq!(state.result, 42, "归还旧stack后结果仍在cold侧存活");
    println!("context-switch: 往返恢复与寄存器哨兵通过；finish单向返回；结果42；arena已归还");
}
