use super::linux_vm::Vm;
use super::*;
use crate::runtime::callback as q;

const CONTROL: u64 = 0x2_0000;
const PROCESSOR: u64 = 0x3_0000;

fn queue_vm(action: i32, node: u64) -> Vm {
    let mut vm = Vm::new(0, [0; 6]);
    vm.registers[0] = CONTROL;
    vm.registers[1] = node;
    vm.registers[2] = u64::from(action as u32);
    vm
}

/// OPEN 返回 OK 并登记 PROCESSOR/ACCEPTING；之前漏写 x0 会把入参 control 当状态返回。
#[test]
fn open_registers_processor_and_returns_ok() {
    let code = instructions(&callback_queue());
    let mut vm = queue_vm(q::OPEN, PROCESSOR);
    assert_eq!(vm.run_calls(&code, |_, _| panic!("no calls")), q::OK as u64);
    assert_eq!(vm.read(CONTROL + q::PROCESSOR as u64, 8), PROCESSOR);
    assert_eq!(vm.read(CONTROL + q::ACCEPTING as u64, 8), 1);
}

#[test]
fn open_twice_is_invalid() {
    let code = instructions(&callback_queue());
    let mut vm = queue_vm(q::OPEN, PROCESSOR);
    assert_eq!(vm.run_calls(&code, |_, _| panic!("no calls")), q::OK as u64);
    let mut second = queue_vm(q::OPEN, PROCESSOR);
    second.put(CONTROL + q::REGISTERED as u64, 1, 8);
    assert_eq!(
        second.run_calls(&code, |_, _| panic!("no calls")),
        q::INVALID as u64
    );
}
