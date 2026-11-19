use super::*;
use crate::{
    NativeImport,
    macho::LIB_SYSTEM,
    runtime::process_schema::{ProcessRequest, SpawnPacket},
};

const ARG: usize = offset_of!(ProcessRequest, arguments);
const VALUE: usize = offset_of!(ProcessRequest, value);
const ERROR: usize = offset_of!(ProcessRequest, error);
const REQ: Reg = Reg::x(19);
const X0: Reg = Reg::x(0);
const XZR: Reg = Reg::zr(Width::X);

fn check_calls(fragment: &Fragment, names: &[&str]) {
    assert_eq!(fragment.relocations.len(), names.len());
    for (call, name) in fragment.relocations.iter().zip(names) {
        assert_eq!(call.relocation.kind, RelocKind::Call26);
        assert_eq!(call.relocation.target, format!("__gugu_darwin_c_{name}"));
    }
}

fn dispatch_sites(code: &[Inst], mut compare: u32, count: usize) -> Vec<u32> {
    let op = Reg::x(9);
    (0..count)
        .map(|index| {
            assert_eq!(form(at(code, compare)), "subs.X.imm");
            assert_eq!(
                at(code, compare).operands,
                [reg(XZR), reg(op), add_imm(index)]
            );
            assert_eq!(at(code, compare + 4).operands[1], condition(1));
            let body = compare + 8;
            compare = target(code, compare + 4);
            assert!(compare > body);
            body
        })
        .collect()
}

#[test]
fn darwin_process_call_encodes_wait_status_and_pipe_results() {
    let fragment = darwin_process_call();
    check_calls(
        &fragment,
        &[
            "waitpid", "__error", "kill", "__error", "close", "__error", "read", "__error",
            "write", "__error",
        ],
    );
    let code = instructions(&fragment);
    assert_eq!(code[5].operands, [reg(REQ), reg(Reg::x(1)), add_imm(0)]);
    assert!(memory(&code[6..8], "str.X.Unsigned", XZR, REQ, VALUE));
    assert!(memory(&code[6..8], "str.X.Unsigned", XZR, REQ, ERROR));
    let bodies = dispatch_sites(&code, 36, 5);
    let wait = fragment.relocations[0].offset;
    assert_eq!(wait, bodies[0] + 16);
    assert!(memory(
        &code[(bodies[0] / 4) as usize..(wait / 4) as usize],
        "ldr.X.Unsigned",
        X0,
        REQ,
        ARG
    ));
    assert_eq!(
        at(&code, wait - 8).operands,
        [reg(Reg::x(1)), reg(REQ), add_imm(ARG + 8)]
    );
    assert_eq!(at(&code, wait - 4).operands, [reg(Reg::x(2)), movz(1)]);
    assert_eq!(form(at(&code, wait + 4)), "cbz.W");
    assert_eq!(target(&code, wait + 16), fragment.relocations[1].offset);
    assert_eq!(
        at(&code, wait + 24).operands,
        [reg(Reg::x(10)), reg(Reg::x(9)), imm(0), imm(6)]
    );
    assert_eq!(
        at(&code, wait + 32).operands,
        [reg(Reg::x(10)), reg(Reg::x(9)), imm(8), imm(15)]
    );
    assert_eq!(at(&code, wait + 36).operands, [reg(X0), movz(1)]);
    assert_eq!(
        at(&code, target(&code, wait + 28)).operands,
        [reg(X0), movz(2)]
    );
    let kill = fragment.relocations[2].offset;
    assert_eq!(at(&code, kill - 4).operands, [reg(Reg::x(1)), movz(9)]);
    for index in [6, 8] {
        let site = fragment.relocations[index].offset;
        assert_eq!(form(at(&code, site + 4)), "adds.X.imm");
        assert_eq!(
            target(&code, site + 8),
            fragment.relocations[index + 1].offset
        );
    }
}

#[test]
fn darwin_process_worker_encodes_spawn_cleanup_and_variadic_fcntl() {
    let imports = [
        "waitpid",
        "kill",
        "close",
        "read",
        "write",
        "__error",
        "pipe",
        "open",
        "fcntl",
        "posix_spawn_file_actions_init",
        "posix_spawn_file_actions_adddup2",
        "posix_spawn_file_actions_addchdir_np",
        "posix_spawn",
        "posix_spawn_file_actions_destroy",
    ];
    assert_eq!(
        darwin_process_imports(),
        imports.map(|name| NativeImport::function(LIB_SYSTEM, name))
    );
    let fragment = darwin_process_worker();
    check_calls(
        &fragment,
        &[
            "posix_spawn_file_actions_init",
            "posix_spawn_file_actions_adddup2",
            "posix_spawn_file_actions_adddup2",
            "posix_spawn_file_actions_adddup2",
            "posix_spawn_file_actions_addchdir_np",
            "posix_spawn",
            "posix_spawn_file_actions_destroy",
            "pipe",
            "fcntl",
            "fcntl",
            "fcntl",
            "fcntl",
            "__error",
            "close",
            "close",
            "__error",
            "open",
            "__error",
            "fcntl",
            "__error",
        ],
    );
    let code = instructions(&fragment);
    assert_eq!(form(&code[0]), "adr");
    assert_eq!(target(&code, 0), 8);
    assert_eq!(form(&code[1]), "ret");
    assert_eq!(code[2].operands, [reg(SP), reg(SP), add_imm(64)]);
    let bodies = dispatch_sites(&code, 52, 4);
    let calls = &fragment.relocations;
    let mut offsets = calls.iter().map(|call| call.offset);
    let init = offsets.next().unwrap();
    assert_eq!(init, bodies[0] + 8);
    let destroy = calls[6].offset;
    let cleanup = destroy - 8;
    for site in &calls[1..6] {
        assert_eq!(target(&code, site.offset + 4), cleanup);
    }
    assert!(target(&code, init + 4) > destroy);
    assert!(memory(
        &code[(cleanup / 4) as usize..=(cleanup / 4) as usize],
        "str.W.Unsigned",
        Reg::w(0),
        REQ,
        ERROR
    ));
    let spawn = calls[5].offset;
    let code_before_spawn = &code[(spawn / 4 - 6) as usize..(spawn / 4) as usize];
    let packet = Reg::x(20);
    for (arg, member) in [
        (1, offset_of!(SpawnPacket, executable)),
        (4, offset_of!(SpawnPacket, argv)),
        (5, offset_of!(SpawnPacket, environment)),
    ] {
        assert!(memory(
            code_before_spawn,
            "ldr.X.Unsigned",
            Reg::x(arg),
            packet,
            member
        ));
    }
    for (index, (command, value)) in
        [8, 9, 10, 11, 18]
            .into_iter()
            .zip([(2, 1), (2, 1), (4, 4), (73, 1), (67, 3)])
    {
        let site = calls[index].offset;
        assert_eq!(
            at(&code, site - 12).operands,
            [reg(Reg::x(1)), movz(command)]
        );
        assert_eq!(at(&code, site - 8).operands, [reg(Reg::x(9)), movz(value)]);
        assert_eq!(
            at(&code, site - 4).operands,
            [reg(Reg::w(9)), mem(SP, AddressMode::Unsigned(0))]
        );
        let error = calls[if index == 18 { 19 } else { 12 }].offset;
        assert_eq!(target(&code, site + 12), error);
    }
    assert_eq!(calls[7].offset, bodies[1] + 4);
    assert_eq!(form(at(&code, bodies[2])), "movz.X");
    assert_eq!(calls[18].offset, bodies[3] + 16);
}
