use super::*;
use crate::runtime::RuntimeMachineOp;
const ENV: &str = "std/runtime/environment.gg";

fn os(h: &mut Harness<'_>, m: &mut [u8], value: &[u8]) -> u64 {
    let raw = h.allocate(m, value.len());
    let at = usize::try_from(raw).unwrap();
    m[at..at + value.len()].copy_from_slice(value);
    let out = h.allocate(m, 16);
    store(m, out, &[raw, u64::try_from(value.len()).unwrap()]);
    out
}

fn machine(_: &mut Harness<'_>, op: RuntimeMachineOp, args: &[u64], _: &mut Vec<u8>) -> Vec<u64> {
    match op {
        RuntimeMachineOp::EnvironmentSymbol => vec![100 + args[0]],
        RuntimeMachineOp::ProcessWorker => vec![200],
        _ => panic!("未登记机器入口 {op:?}"),
    }
}

#[test]
fn linux_environment_empty_value_remove_and_case_sensitive_names() {
    let output = protocol(TargetName::X86_64Linux);
    let mut h = output.harness();
    let mut m = vec![0; 1048576];
    h.machine_hook = Some(machine);
    h.bridge_hook = Some(|h, args, m| {
        let packet = args[1];
        let symbol = word(m, packet);
        h.calls
            .push((symbol, vec![word(m, packet + 16), word(m, packet + 24)]));
        let (expected, result, error) = h.script.pop_front().unwrap();
        assert_eq!(symbol, expected);
        put(m, packet + 128, result);
        put(m, packet + 160, error);
        vec![0, 0]
    });
    h.script
        .extend([(101, 0, 0), (103, 2048, 0), (102, 0, 0), (103, 0, 0)]);
    let name = os(&mut h, &mut m, b"A");
    let empty = os(&mut h, &mut m, b"");
    assert_eq!(h.invoke(ENV, "set", &[name, empty], &mut m)[0], 0);
    let some = h.invoke(ENV, "get", &[name], &mut m);
    assert_eq!(&some[..2], &[0, 0]);
    assert_eq!(some[3], 0, "空值是 Some，不是删除");
    assert_eq!(h.invoke(ENV, "remove", &[name], &mut m)[0], 0);
    assert_eq!(&h.invoke(ENV, "get", &[name], &mut m)[..2], &[0, 1]);
    let lower = os(&mut h, &mut m, b"a");
    assert_eq!(
        h.invoke(ENV, "compare", &[name, lower], &mut m)[..2],
        [0, u64::MAX]
    );
    assert!(h.allocations.is_empty());
    assert!(h.script.is_empty());
}

#[test]
fn windows_invalid_environment_block_is_freed_before_error_returns() {
    let output = protocol(TargetName::X86_64Windows);
    let mut h = output.harness();
    let mut m = vec![0; 1048576];
    h.machine_hook = Some(machine);
    h.bridge_hook = Some(|h, args, m| {
        let packet = args[1];
        let symbol = word(m, packet);
        h.calls.push((symbol, vec![word(m, packet + 16)]));
        let (expected, result, _) = h.script.pop_front().unwrap();
        assert_eq!(symbol, expected);
        if symbol == 101 {
            assert_eq!(word(m, packet + 16), 2048);
        }
        put(m, packet + 128, result);
        vec![0, 0]
    });
    let bytes: Vec<_> = [65u16, 61, 0xd800, 0, 0]
        .into_iter()
        .flat_map(u16::to_le_bytes)
        .collect();
    m[2048..2048 + bytes.len()].copy_from_slice(&bytes);
    h.script.extend([(100, 2048, 0), (101, 1, 0)]);
    assert_eq!(&h.invoke(ENV, "entries_locked", &[0], &mut m)[..2], &[1, 4]);
    assert!(
        h.script.is_empty(),
        "非法 UTF-16 仍须调用 FreeEnvironmentStringsW"
    );
    assert!(h.allocations.is_empty());
}

#[test]
fn stdio_pipe_and_null_failures_release_every_acquired_handle() {
    let output = protocol(TargetName::X86_64Linux);
    for (mode, script, closed) in [
        (2, vec![(1, 0, 0), (3, 0, 13)], vec![72, 71]),
        (1, vec![(2, 71, 0), (3, 0, 13)], vec![71]),
        (1, vec![(2, 0, 13)], vec![]),
        (0, vec![(3, 0, 13)], vec![]),
        (2, vec![(1, 0, 13)], vec![]),
    ] {
        let mut h = output.harness();
        let mut m = vec![0; 1048576];
        h.machine_hook = Some(machine);
        h.script.extend(script);
        h.script.extend(closed.iter().map(|_| (2, 0, 0)));
        h.bridge_hook = Some(|h, args, m| {
            let raw = word(m, args[1] + 16);
            let (op, result, error) = h.script.pop_front().unwrap();
            assert_eq!(word(m, raw), op);
            put(m, raw + 72, if op == 1 { 71 } else { result });
            put(m, raw + 24, 72);
            put(m, raw + 80, error);
            vec![0, 0]
        });
        store(&mut m, 512, &[mode, 1, 0]);
        assert_eq!(
            &h.invoke("std/runtime/process_spawn.gg", "stdio", &[512, 0], &mut m)[..2],
            &[1, 1]
        );
        assert_eq!(
            h.calls.iter().map(|(_, args)| args[1]).collect::<Vec<_>>(),
            closed
        );
        assert!(h.allocations.is_empty());
        assert!(h.script.is_empty());
    }
}
