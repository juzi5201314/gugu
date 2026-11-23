use super::*;
use crate::a64::{inst::Operand, reg::RegKind};
use std::collections::BTreeMap;

#[derive(Default)]
struct Vm {
    gpr: [u64; 32],
    vector: [[u64; 2]; 32],
    memory: BTreeMap<u64, u64>,
    code: BTreeMap<u64, Inst>,
}

impl Vm {
    fn load(&mut self, base: u64, fragment: &Fragment, symbols: &[(&str, u64)]) {
        for (index, instruction) in instructions(fragment).into_iter().enumerate() {
            self.code.insert(base + index as u64 * 4, instruction);
        }
        for fixup in &fragment.relocations {
            let address = base + u64::from(fixup.offset);
            let target = symbols
                .iter()
                .find(|(name, _)| *name == fixup.relocation.target)
                .unwrap()
                .1;
            let instruction = self.code.get_mut(&address).unwrap();
            instruction.operands = vec![Operand::Target(Target::Offset(
                target as i64 - address as i64 + fixup.relocation.addend,
            ))];
        }
    }
    fn get(&self, operand: &Operand) -> u64 {
        match operand {
            Operand::Reg(Reg {
                kind: RegKind::Gpr(index),
                ..
            }) => self.gpr[*index as usize],
            Operand::Reg(Reg {
                kind: RegKind::Sp, ..
            }) => self.gpr[31],
            Operand::Reg(Reg {
                kind: RegKind::Zero,
                ..
            }) => 0,
            _ => panic!("not a GPR"),
        }
    }
    fn set(&mut self, operand: &Operand, value: u64) {
        let index = match operand {
            Operand::Reg(Reg {
                kind: RegKind::Gpr(index),
                ..
            }) => *index as usize,
            Operand::Reg(Reg {
                kind: RegKind::Sp, ..
            }) => 31,
            _ => panic!("not a writable GPR"),
        };
        self.gpr[index] = value;
    }
    fn words(&self, address: u64, count: usize) -> Vec<u64> {
        (0..count)
            .map(|i| *self.memory.get(&(address + i as u64 * 8)).unwrap_or(&0))
            .collect()
    }
    fn store(&mut self, address: u64, words: &[u64]) {
        for (index, word) in words.iter().enumerate() {
            self.memory.insert(address + index as u64 * 8, *word);
        }
    }
    fn context(&mut self, address: u64, context: Context) {
        let words = [context.sp, context.pc]
            .into_iter()
            .chain(context.x19_to_x26)
            .chain([context.x29])
            .chain(context.d8_to_d15)
            .collect::<Vec<_>>();
        self.store(address, &words);
    }
    fn saved(&self, address: u64) -> Context {
        let words = self.words(address, 19);
        Context {
            sp: words[0],
            pc: words[1],
            x19_to_x26: words[2..10].try_into().unwrap(),
            x29: words[10],
            d8_to_d15: words[11..19].try_into().unwrap(),
        }
    }
    fn run(&mut self, mut pc: u64) -> u64 {
        for _ in 0..256 {
            let Some(instruction) = self.code.get(&pc).cloned() else {
                return pc;
            };
            let args = &instruction.operands;
            let mut next = pc + 4;
            match form(&instruction) {
                "add.X.imm" => {
                    let Operand::AddImm { imm12, shift12 } = args[2] else {
                        panic!("immediate")
                    };
                    self.set(
                        &args[0],
                        self.get(&args[1]) + (u64::from(imm12) << if shift12 { 12 } else { 0 }),
                    );
                }
                "sub.X.shift" => self.set(
                    &args[0],
                    self.get(&args[1]).wrapping_sub(self.get(&args[2])),
                ),
                "b" | "bl" => {
                    let Operand::Target(Target::Offset(displacement)) = args[0] else {
                        panic!("branch")
                    };
                    if form(&instruction) == "bl" {
                        self.gpr[30] = next;
                    }
                    next = (pc as i64 + displacement) as u64;
                }
                "ret" => next = self.get(&args[0]),
                key if key.starts_with("str.") || key.starts_with("ldr.") => {
                    self.transfer(key, args)
                }
                key => panic!("unsupported context fixture instruction {key}"),
            }
            pc = next;
        }
        panic!("context fragment did not finish");
    }
    fn transfer(&mut self, key: &str, args: &[Operand]) {
        let Operand::Address(Address {
            base,
            mode: AddressMode::Unsigned(offset),
        }) = args[1]
        else {
            panic!("address")
        };
        let address = self.get(&reg(base)) + offset as u64;
        let load = key.starts_with("ldr.");
        if key.contains(".X.") {
            if load {
                self.set(&args[0], self.words(address, 1)[0]);
            } else {
                self.store(address, &[self.get(&args[0])]);
            }
        } else {
            let Operand::Reg(Reg {
                kind: RegKind::Vector(index),
                ..
            }) = args[0]
            else {
                panic!("vector")
            };
            let count = if key.contains(".Q.") { 2 } else { 1 };
            if load {
                let words = self.words(address, count);
                self.vector[index as usize] = [words[0], if count == 2 { words[1] } else { 0 }];
            } else {
                let words = self.vector[index as usize];
                self.store(address, &words[..count]);
            }
        }
    }
}

#[test]
fn emitted_switch_and_restore_transfer_exact_context_and_supplied_runtime_registers() {
    let code = context_switch();
    let mut vm = Vm::default();
    vm.load(0x10000, &code.fragment, &[]);
    let restore = Context {
        sp: 0x9000,
        pc: 0x5004,
        x19_to_x26: [77; 8],
        x29: 0x9040,
        d8_to_d15: [88; 8],
    };
    vm.context(0x30000, restore);
    vm.gpr[0] = 0x20000;
    vm.gpr[1] = 0x30000;
    vm.gpr[2] = 0x40000;
    vm.gpr[3] = 0x50000;
    vm.gpr[31] = 0x1180;
    vm.gpr[30] = 0x6004;
    vm.gpr[29] = 0x11c0;
    vm.gpr[19..27].copy_from_slice(&[11; 8]);
    vm.gpr[27] = 0xaaaa;
    vm.gpr[28] = 0xbbbb;
    for (index, lane) in vm.vector.iter_mut().enumerate().take(16).skip(8) {
        *lane = [index as u64, u64::MAX];
    }
    assert_eq!(vm.run(0x10000), restore.pc);
    let saved = vm.saved(0x20000);
    assert_eq!(
        saved,
        Context {
            sp: 0x1180,
            pc: 0x6004,
            x19_to_x26: [11; 8],
            x29: 0x11c0,
            d8_to_d15: std::array::from_fn(|index| index as u64 + 8)
        }
    );
    assert_eq!(vm.gpr[19..27], restore.x19_to_x26);
    assert_eq!(
        (vm.gpr[27], vm.gpr[28], vm.gpr[29], vm.gpr[31]),
        (0x40000, 0x50000, restore.x29, restore.sp)
    );
    for lane in vm.vector.iter().take(16).skip(8) {
        assert_eq!(*lane, [88, 0]);
    }
    vm.gpr[0] = 0x20000;
    vm.gpr[1] = 0x70000;
    vm.gpr[2] = 0x80000;
    assert_eq!(vm.run(0x10000 + u64::from(code.restore_offset)), saved.pc);
    assert_eq!(vm.gpr[19..27], saved.x19_to_x26);
    assert_eq!(
        (vm.gpr[27], vm.gpr[28], vm.gpr[29], vm.gpr[31]),
        (0x70000, 0x80000, saved.x29, saved.sp)
    );
    assert_eq!(std::mem::size_of::<Context>(), 152);
}

fn maps(rva: u64, entry: bool) -> crate::runtime::stack::a64::FrameMetadata {
    use crate::runtime::{
        stackmap_codec::{self, CodeLayout, SafepointLayout, a64_unwind},
        stackmap_schema::StackMapArchitecture,
    };
    let code = CodeLayout {
        code_rva: rva,
        code_size: 64,
        frame_size: 64,
        unwind_index: 0,
        runtime_bridge: false,
        panic_landing: false,
        has_stack_interior: true,
    };
    let mut point = SafepointLayout::<u32> {
        pc_offset: 8,
        kind: if entry { 4 } else { 0 },
        dirty: false,
        copy_allowed: !entry,
        scan_allowed: true,
        slots: std::array::from_fn(|_| vec![0]),
        registers: [0; 5],
        typed: vec![],
        function: 0,
        slot_count: if entry { 0 } else { 8 },
    };
    if entry {
        point.registers[4] = 1;
    } else {
        point.slots[4][0] = 4;
    }
    let maps =
        stackmap_codec::encode_for(StackMapArchitecture::Aarch64, &[code], &[(0, point)]).unwrap();
    let unwind = a64_unwind::encode(
        code,
        0,
        &[a64_unwind::Site {
            pc_offset: 8,
            entry,
        }],
        &[],
    )
    .unwrap();
    crate::runtime::stack::a64::FrameMetadata::decode(&maps, &unwind).unwrap()
}

fn mailbox(vm: &Vm, address: u64) -> MorestackMailbox {
    let words = vm.words(address, 9);
    MorestackMailbox {
        hot: words[0],
        entry_sp: words[1],
        candidate: words[2],
        required: words[3],
        scratch: words[4],
        mutator_context: words[5],
        system_context: words[6],
        return_value: words[7],
        processor: words[8],
    }
}

fn scratch(vm: &Vm, address: u64) -> MorestackScratch {
    let words = vm.words(address, 27);
    MorestackScratch {
        return_pc: words[0],
        caller_lr: words[1],
        vector: std::array::from_fn(|lane| {
            let mut bytes = [0; 16];
            bytes[..8].copy_from_slice(&words[2 + lane * 2].to_le_bytes());
            bytes[8..].copy_from_slice(&words[3 + lane * 2].to_le_bytes());
            bytes
        }),
        gpr: words[18..27].try_into().unwrap(),
    }
}

#[test]
fn emitted_morestack_switch_copy_and_restore_fix_roots_before_argument_reload() {
    use crate::runtime::{
        cage::CompressionPlane,
        compression_schema::{CompressionDemand, CompressionPolicyV1, CompressionRuntimeContract},
        coroutine::StackDescriptor,
        stack::a64::{Image, Snapshot},
    };
    use crate::target::PointerCompression;
    let switch = context_switch();
    let morestack = morestack_or_poll("switch");
    let mut vm = Vm::default();
    vm.load(0x60000, &morestack.fragment, &[("switch", 0x70000)]);
    vm.load(0x70000, &switch.fragment, &[]);
    let mut old = StackDescriptor::default();
    old.install(0x1000, 512).unwrap();
    vm.gpr[31] = 0x11c0;
    vm.gpr[29] = 0x11c0;
    vm.gpr[30] = 0x1008;
    vm.gpr[10] = 0x2008;
    vm.gpr[11] = 0xdc0;
    vm.gpr[27] = 0xc0000;
    vm.gpr[28] = 0x90000;
    vm.gpr[..9].copy_from_slice(&[0x11e0, 0x11e0, 2, 3, 4, 5, 6, 7, 8]);
    vm.gpr[19..27].fill(0x11e0);
    for (lane, slot) in vm.vector.iter_mut().enumerate().take(8) {
        *slot = [0x11e0 + lane as u64, u64::MAX];
    }
    let arguments: [u64; 9] = vm.gpr[..9].try_into().unwrap();
    let vectors = vm.vector;
    vm.store(
        0x90000 + processor::PROCESSOR_STATE_OFFSET as u64,
        &[0xa0000],
    );
    vm.store(
        0x90000 + processor::PROCESSOR_SYSTEM_SLOT_OFFSET as u64,
        &[0xd0000],
    );
    vm.store(0xa0000, &[0, 0, 0, 0, 0xb0000, 0x80000, 0x81000]);
    vm.context(
        0x81000,
        Context {
            sp: 0xe0000,
            pc: 0xf0004,
            ..Default::default()
        },
    );
    assert_eq!(vm.run(0x60000), 0xf0004);
    let context = vm.saved(0x80000);
    assert_eq!(context.pc, 0x60000 + u64::from(morestack.resume_offset));
    let mut mailbox = mailbox(&vm, 0xa0000);
    let mut scratch = scratch(&vm, 0xb0000);
    assert_eq!(
        (mailbox.entry_sp, mailbox.candidate, mailbox.required),
        (0x11c0, 0xdc0, 1024)
    );
    assert_eq!((scratch.return_pc, scratch.caller_lr), (0x1008, 0x2008));
    assert_eq!(scratch.gpr, arguments);
    let snapshot = Snapshot::morestack(
        context,
        &mailbox,
        &scratch,
        0x60000 + u64::from(morestack.resume_offset),
    )
    .unwrap();
    let mut words = [0_u64; 8];
    words[2] = 0x11e0;
    words[3] = 0x11e0;
    let bytes = words.into_iter().flat_map(u64::to_le_bytes).collect();
    let mut plane = CompressionPlane::new(
        &CompressionRuntimeContract::build(
            CompressionDemand::default(),
            CompressionPolicyV1::disabled(),
            PointerCompression::aarch64(),
        )
        .unwrap(),
    );
    let image = Image::capture(
        bytes,
        &snapshot,
        0,
        &[maps(0x1000, true), maps(0x2000, false)],
        &[],
        &mut plane,
    )
    .unwrap();
    let (image, context) = image
        .relocate_morestack(context, &mut mailbox, &mut scratch, &old, (0x8000, 2048))
        .unwrap();
    assert_eq!(
        u64::from_le_bytes(image.bytes()[16..24].try_into().unwrap()),
        0x87e0
    );
    assert_eq!(
        u64::from_le_bytes(image.bytes()[24..32].try_into().unwrap()),
        0x11e0
    );
    assert_eq!((mailbox.entry_sp, mailbox.candidate), (0x87c0, 0x83c0));
    assert_eq!(context.x19_to_x26, [0x11e0; 8]);
    vm.context(0x80000, context);
    vm.store(0xb0000 + 144, &scratch.gpr);
    vm.store(
        0x91000 + processor::PROCESSOR_STATE_OFFSET as u64,
        &[0xa1000],
    );
    vm.store(
        0xa1000,
        &[
            mailbox.hot,
            mailbox.entry_sp,
            mailbox.candidate,
            mailbox.required,
            mailbox.scratch,
            mailbox.mutator_context,
            mailbox.system_context,
        ],
    );
    vm.gpr[0] = 0x80000;
    vm.gpr[1] = 0xc1000;
    vm.gpr[2] = 0x91000;
    assert_eq!(vm.run(0x70000 + u64::from(switch.restore_offset)), 0x1008);
    assert_eq!(
        (vm.gpr[31], vm.gpr[29], vm.gpr[27], vm.gpr[28]),
        (0x87c0, 0x87c0, 0xc1000, 0x91000)
    );
    assert_eq!(vm.gpr[0], 0x87e0);
    assert_eq!(vm.gpr[1], arguments[1]);
    assert_eq!(vm.gpr[2..9], arguments[2..9]);
    assert_eq!(vm.gpr[10], 0x2008);
    assert_eq!(vm.vector[..8], vectors[..8]);
}
