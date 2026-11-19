//! Deterministic substitutes for the already independently tested PR24 fragments.
use super::*;
use crate::runtime::{
    cage::CompressionPlane,
    compression_schema::{CompressionDemand, CompressionPolicyV1, CompressionRuntimeContract},
};
use crate::target::PointerCompression;
use std::collections::{BTreeMap, VecDeque};

pub(super) fn bindings() -> glue::RuntimeBindings {
    glue::RuntimeBindings {
        morestack_or_poll: "__gugu_runtime_morestack_or_poll".into(),
        safepoint_slow: "__gugu_runtime_safepoint_slow".into(),
    }
}

pub(super) struct Harness {
    pub expected_sp: u64,
    pub watermark: u64,
    pub flags: u32,
    pub growth: VecDeque<u64>,
    pub candidates: Vec<u64>,
    pub guard_loads: usize,
    pub polls: usize,
    pub gc_direct_delta: u64,
    pub repeat_polls: bool,
    pub(super) processor: u64,
    pub(super) arguments: [u64; 9],
}

impl Default for Harness {
    fn default() -> Self {
        Self {
            expected_sp: 1 << 40,
            watermark: 0,
            flags: 0,
            growth: VecDeque::new(),
            candidates: vec![],
            guard_loads: 0,
            polls: 0,
            gc_direct_delta: 0,
            repeat_polls: false,
            processor: 0x2000,
            arguments: [0; 9],
        }
    }
}

fn word(memory: &mut BTreeMap<u64, u8>, at: u64, value: u64, bytes: u64) {
    for byte in 0..bytes {
        memory.insert(at + byte, (value >> (8 * byte)) as u8);
    }
}

impl Harness {
    pub fn setup(&self, registers: &mut [u64; 32]) {
        registers[31] = self.expected_sp;
        registers[27] = 0x1000;
        registers[28] = self.processor;
        registers[8] = 0x8888;
    }
    pub fn initialize(&mut self, registers: &[u64; 32], memory: &mut BTreeMap<u64, u8>) {
        self.arguments.copy_from_slice(&registers[..9]);
        word(
            memory,
            registers[27] + u64::from(crate::runtime::coroutine_schema::stack_check_offset()),
            self.watermark,
            8,
        );
        word(
            memory,
            registers[28] + u64::from(crate::runtime::processor::poll_flags_offset()),
            u64::from(self.flags),
            4,
        );
    }
    pub fn load(
        &mut self,
        fragment: &codegen::LeafFragment,
        at: u64,
        registers: &[u64; 32],
        memory: &BTreeMap<u64, u8>,
        entry: Option<&[u64; 32]>,
    ) {
        if at == 0x1000 + u64::from(crate::runtime::coroutine_schema::stack_check_offset()) {
            self.guard_loads += 1;
            let expected_sp = entry.map_or(self.expected_sp, |entry| entry[31]);
            assert_eq!(registers[31], expected_sp, "SP changed before entry check");
            let candidate = expected_sp.wrapping_sub(u64::from(fragment.frame.required_frame));
            assert_eq!(registers[11], candidate);
            assert!(
                !memory
                    .keys()
                    .any(|at| *at >= candidate && *at < expected_sp),
                "frame store before entry check"
            );
            assert_eq!(
                &registers[..9],
                entry.map_or(&self.arguments[..], |entry| &entry[..9])
            );
        }
    }
    pub fn call(
        &mut self,
        fragment: &codegen::LeafFragment,
        pc: usize,
        registers: &mut [u64; 32],
        memory: &mut BTreeMap<u64, u8>,
        args: usize,
    ) -> bool {
        let Some(fixup) = fragment.relocations.iter().find(|r| r.offset == pc as u64) else {
            return false;
        };
        let binding = bindings();
        if fixup.relocation.target != binding.morestack_or_poll
            && fixup.relocation.target != binding.safepoint_slow
        {
            return false;
        }
        registers[30] = pc as u64 + 4;
        let site = sites::lookup(&fragment.runtime_sites, registers[30] as u32).unwrap();
        if fixup.relocation.target == binding.morestack_or_poll {
            assert_eq!(site.kind, sites::SiteKind::MorestackEntry);
            assert_eq!(registers[31], self.expected_sp);
            assert_eq!(registers[10], u64::MAX);
            assert_eq!(&registers[..9], &self.arguments);
            self.candidates.push(registers[11]);
            let delta = self.growth.pop_front().expect("unexpected morestack retry");
            let old_sp = self.expected_sp;
            self.expected_sp += delta;
            for offset in 0..args.saturating_sub(8) as u64 * 8 {
                let value = *memory.get(&(old_sp + offset)).unwrap_or(&0);
                memory.insert(self.expected_sp + offset, value);
            }
            registers[31] = self.expected_sp;
            let watermark = if self.growth.is_empty() {
                self.expected_sp - u64::from(fragment.frame.required_frame)
            } else {
                crate::runtime::coroutine::POLL_SENTINEL as u64
            };
            word(
                memory,
                0x1000 + u64::from(crate::runtime::coroutine_schema::stack_check_offset()),
                watermark,
                8,
            );
            for (id, register) in registers.iter_mut().enumerate().take(15).skip(9) {
                if id != 10 {
                    *register = 0xbad0 + id as u64;
                }
            }
        } else if fixup.relocation.target == binding.safepoint_slow {
            assert_eq!(site.kind, sites::SiteKind::PollResume);
            self.polls += 1;
            if self.gc_direct_delta != 0 {
                let metadata = fragment.metadata.as_ref().unwrap();
                let maps = crate::runtime::decode_tables(&metadata.stackmaps).unwrap();
                let point = maps
                    .safepoints
                    .iter()
                    .find(|p| p.pc_offset == site.pc_offset)
                    .unwrap();
                let words = (0..fragment.frame.size / 8)
                    .map(|offset| {
                        (0..8).fold(0, |value, byte| {
                            value
                                | (u64::from(
                                    *memory
                                        .get(&(registers[31] + u64::from(offset) * 8 + byte))
                                        .unwrap_or(&0),
                                ) << (byte * 8))
                        })
                    })
                    .collect::<Vec<_>>();
                let roots = crate::runtime::stackmap::scan_frame(
                    &maps,
                    u64::from(point.pc_offset),
                    &words,
                    registers,
                    &[],
                    &mut CompressionPlane::new(
                        &CompressionRuntimeContract::build(
                            CompressionDemand::default(),
                            CompressionPolicyV1::disabled(),
                            PointerCompression::aarch64(),
                        )
                        .unwrap(),
                    ),
                )
                .unwrap();
                for root in roots.slots {
                    let crate::runtime::stackmap::ScannedRoot::Direct(offset) = root else {
                        continue;
                    };
                    let at = registers[31] + u64::from(offset) * 8;
                    let pointer = (0..8).fold(0, |value, byte| {
                        value | (u64::from(*memory.get(&(at + byte)).unwrap_or(&0)) << (byte * 8))
                    });
                    if pointer != 0 {
                        word(memory, at, pointer + self.gc_direct_delta, 8);
                    }
                }
            }
            for (id, reg) in registers.iter_mut().enumerate().take(27) {
                *reg = 0xdead0000 + id as u64;
            }
            registers[29] = 0xbad29;
            self.processor = 0x3000;
            registers[28] = self.processor;
            word(
                memory,
                self.processor + u64::from(crate::runtime::processor::poll_flags_offset()),
                u64::from(self.repeat_polls),
                4,
            );
        } else {
            return false;
        }
        true
    }
}

pub(super) fn run(
    fragment: &codegen::LeafFragment,
    args: &[u64],
    harness: &mut Harness,
) -> [u64; 2] {
    run_linked(fragment, &[], args, harness)
}

pub(super) fn run_linked(
    fragment: &codegen::LeafFragment,
    callees: &[&codegen::LeafFragment],
    args: &[u64],
    harness: &mut Harness,
) -> [u64; 2] {
    let stubs = stubs();
    let mut linked = callees.to_vec();
    linked.extend(stubs.iter());
    vm::execute_inner(fragment, &linked, args, Some(harness)).0
}

pub(super) fn stubs() -> [codegen::LeafFragment; 2] {
    let mut morestack = Fixture::new(&[], &[]).compile();
    let mut poll = morestack.clone();
    morestack.symbol = bindings().morestack_or_poll;
    poll.symbol = bindings().safepoint_slow;
    [morestack, poll]
}
