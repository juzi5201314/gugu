use super::*;
use std::collections::BTreeMap;

#[derive(Default)]
pub(super) struct Harness {
    pub failures_remaining: u32,
    pub interference: Option<u64>,
    pub loads: u32,
    pub stores: u32,
    pub failures: u32,
    pub clears: u32,
    pub trace: Vec<&'static str>,
    pub monitor: Option<(u64, u64)>,
}

pub(super) fn word(memory: &BTreeMap<u64, u8>, at: u64, bytes: u64) -> u64 {
    (0..bytes).fold(0, |v, i| {
        v | (u64::from(*memory.get(&(at + i)).unwrap_or(&0)) << (i * 8))
    })
}
pub(super) fn store(memory: &mut BTreeMap<u64, u8>, at: u64, bytes: u64, value: u64) {
    for i in 0..bytes {
        memory.insert(at + i, (value >> (i * 8)) as u8);
    }
}

impl Harness {
    pub fn clear_monitor(&mut self) {
        self.monitor = None;
    }
    pub fn execute(
        &mut self,
        inst: &inst::Inst,
        regs: &mut [u64; 32],
        memory: &mut BTreeMap<u64, u8>,
    ) -> bool {
        let key = table::get(inst.form).unwrap().key;
        let op = &inst.operands;
        if key == "dmb" {
            self.trace.push(if op[0] == inst::Operand::Imm(9) {
                "ishld"
            } else {
                assert_eq!(op[0], inst::Operand::Imm(11));
                "ish"
            });
            return true;
        }
        if key == "clrex" {
            self.clear_monitor();
            self.clears += 1;
            self.trace.push("clear");
            return true;
        }
        let exclusive_load = key.starts_with("ldxr.") || key.starts_with("ldaxr.");
        let exclusive_store = key.starts_with("stxr.") || key.starts_with("stlxr.");
        let load = exclusive_load || key.starts_with("ldar.");
        if !load && !exclusive_store && !key.starts_with("stlr.") {
            return false;
        }
        let inst::Operand::Address(address) = op.last().unwrap() else {
            panic!()
        };
        let at = super::vm::get(address.base, regs);
        let bytes = match key.rsplit('.').next().unwrap() {
            "B" => 1,
            "H" => 2,
            "W" => 4,
            _ => 8,
        };
        assert_eq!(at % bytes, 0, "atomic alignment");
        if load {
            super::vm::put(super::vm::rr(&op[0]), word(memory, at, bytes), regs);
            self.trace
                .push(if key.starts_with("ldaxr") || key.starts_with("ldar") {
                    "acquire"
                } else {
                    "read"
                });
            if exclusive_load {
                self.monitor = Some((at, bytes));
                self.loads += 1;
            }
        } else {
            let value_index = usize::from(exclusive_store);
            let value = super::vm::get(super::vm::rr(&op[value_index]), regs);
            let mut success = true;
            if exclusive_store {
                assert_ne!(super::vm::rr(&op[0]).kind, address.base.kind);
                assert_ne!(super::vm::rr(&op[0]).kind, super::vm::rr(&op[1]).kind);
                success = self.monitor.take() == Some((at, bytes));
                if self.failures_remaining != 0 {
                    self.failures_remaining -= 1;
                    success = false;
                    if let Some(value) = self.interference.take() {
                        store(memory, at, bytes, value);
                    }
                }
                super::vm::put(super::vm::rr(&op[0]), u64::from(!success), regs);
                if success {
                    self.stores += 1;
                } else {
                    self.failures += 1;
                }
            } else {
                self.clear_monitor();
            }
            self.trace.push(if !success {
                "retry"
            } else if key.starts_with("stlr.") || key.starts_with("stlxr") {
                "release"
            } else {
                "write"
            });
            if success {
                store(memory, at, bytes, value);
            }
        }
        true
    }
}
