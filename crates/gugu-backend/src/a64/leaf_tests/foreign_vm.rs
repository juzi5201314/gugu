//! Deterministic admission/copy-back substitute; no native threads or OS calls.
use super::*;
use crate::runtime::foreign_packet as packet;
use std::collections::BTreeMap;

#[derive(Default)]
pub(super) struct Harness {
    pub common_results: [u64; 5],
    pub outputs: BTreeMap<u32, Vec<u8>>,
    pub packets: Vec<Vec<u8>>,
    pub inputs: BTreeMap<u32, Vec<u8>>,
    pub move_stack: u64,
    pub dirty: bool,
}

pub(super) fn read(memory: &BTreeMap<u64, u8>, at: u64) -> u64 {
    (0..8).fold(0, |value, byte| {
        value | (u64::from(*memory.get(&(at + byte)).unwrap_or(&0)) << (byte * 8))
    })
}
fn write(memory: &mut BTreeMap<u64, u8>, at: u64, value: u64) {
    for byte in 0..8 {
        memory.insert(at + byte, (value >> (byte * 8)) as u8);
    }
}

pub(super) fn bindings() -> glue::RuntimeBindings {
    let mut bindings = super::runtime_vm::bindings();
    bindings.foreign = Some(glue::ForeignBindings {
        ordinary_dispatch: "ffi_dispatch".into(),
        dirty_dispatch: "ffi_dirty_dispatch".into(),
    });
    bindings
}

impl Harness {
    pub fn dispatch(
        &mut self,
        fragment: &codegen::LeafFragment,
        pc: usize,
        registers: &mut [u64; 32],
        memory: &mut BTreeMap<u64, u8>,
    ) -> u64 {
        let site = sites::lookup(&fragment.runtime_sites, (pc + 4) as u32).unwrap();
        assert_eq!(
            site.kind,
            sites::SiteKind::ForeignBridge { dirty: self.dirty }
        );
        let maps =
            crate::runtime::decode_tables(&fragment.metadata.as_ref().unwrap().stackmaps).unwrap();
        let point = maps
            .safepoints
            .iter()
            .find(|point| point.pc_offset == site.pc_offset)
            .unwrap();
        let map = &maps.maps[point.map_index as usize];
        assert_eq!(point.kind, 3);
        assert_eq!(point.flags & 8 != 0, self.dirty);
        assert_eq!(map.registers, [0; 5]);
        let high = read(memory, registers[27] + 80);
        let base = high - registers[0];
        assert_eq!(base, registers[31]);
        let mut native = (0..registers[1])
            .map(|offset| *memory.get(&(base + offset)).unwrap_or(&0))
            .collect::<Vec<_>>();
        let mut outputs = Vec::new();
        for index in 0..registers[3] {
            let record = base + registers[2] + index * 32;
            let slot = read(memory, record) as u32;
            let depth = read(memory, record + 8);
            let bytes = read(memory, record + 16);
            let kind = read(memory, record + 24);
            if kind == packet::FIXUP_POINTER {
                assert!(!(base..=high).contains(&depth));
                continue;
            }
            let data = (0..bytes)
                .map(|offset| *memory.get(&(high - depth + offset)).unwrap_or(&0))
                .collect::<Vec<_>>();
            if kind == packet::FIXUP_INPUT {
                self.inputs.insert(slot, data);
            } else {
                assert_eq!(kind, packet::FIXUP_OUTPUT);
                outputs.push((
                    depth,
                    self.outputs
                        .get(&slot)
                        .cloned()
                        .unwrap_or_else(|| vec![0; bytes as usize]),
                ));
                assert_eq!(outputs.last().unwrap().1.len(), bytes as usize);
            }
            let raw = 0x10000 + index * 0x1000;
            native[slot as usize..slot as usize + 8].copy_from_slice(&raw.to_le_bytes());
            native[(record - base + 8) as usize..(record - base + 16) as usize].fill(0);
        }
        self.packets.push(native);
        let delta = self.move_stack;
        if delta != 0 {
            let bytes = (base..high)
                .map(|at| *memory.get(&at).unwrap_or(&0))
                .collect::<Vec<_>>();
            for (offset, byte) in bytes.into_iter().enumerate() {
                memory.insert(base + delta + offset as u64, byte);
            }
            for slot in &map.slots[4] {
                let at = base + delta + u64::from(*slot) * 8;
                let value = read(memory, at);
                if (base..high).contains(&value) {
                    write(memory, at, value + delta);
                }
            }
            registers[31] += delta;
            if (base..high).contains(&registers[29]) {
                registers[29] += delta;
            }
            write(memory, registers[27] + 80, high + delta);
        }
        for (index, value) in self.common_results.iter().enumerate() {
            write(memory, base + delta + 128 + index as u64 * 8, *value);
        }
        for (depth, bytes) in outputs {
            for (offset, byte) in bytes.into_iter().enumerate() {
                memory.insert(high + delta - depth + offset as u64, byte);
            }
        }
        for (index, value) in registers.iter_mut().enumerate().take(18) {
            *value = 0xdead0000 + index as u64;
        }
        delta
    }
}
