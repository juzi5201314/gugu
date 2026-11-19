use super::{Body, word_at};
use crate::lir::body::BlockId;
use crate::lir::stackmap::{LogicalRoot, LogicalSafepoint};
use std::{cell::RefCell, collections::BTreeMap, rc::Rc};

pub(super) type Frames = Rc<RefCell<BTreeMap<usize, Vec<u64>>>>;

#[derive(Debug)]
pub(super) struct InterpretedPanic;

pub(super) struct Context<'a> {
    pub(super) body: &'a Body,
    pub(super) slots: &'a [u64],
    pub(super) values: &'a [u64],
    pub(super) memory: &'a mut [u8],
    pub(super) root_bases: &'a [Option<u64>],
}

pub(super) struct RootTrace {
    frames: Frames,
    base: usize,
    bits: Vec<Vec<bool>>,
    anchors: Vec<LogicalSafepoint>,
}

impl RootTrace {
    pub(super) fn new(body: &Body, frames: Frames) -> Self {
        let base = frames
            .borrow()
            .last_key_value()
            .map_or(0, |(key, _)| key + 1);
        frames.borrow_mut().insert(base, Vec::new());
        Self {
            frames,
            base,
            bits: body
                .stack_slots
                .iter()
                .map(|slot| vec![false; slot.roots.len()])
                .collect(),
            anchors: crate::lir::stackmap::anchors(body).unwrap(),
        }
    }

    pub(super) fn at(&mut self, block: BlockId, position: u32, context: &mut Context<'_>) {
        let Context {
            body,
            slots,
            values,
            memory,
            root_bases,
        } = context;
        for event in &body.root_inits {
            if event.block == block && event.position == position {
                self.bits[event.slot.index()][event.bit as usize] = event.initialized;
                if let Some(base) = root_bases[event.slot.index()] {
                    memory[base as usize + event.bit as usize] = u8::from(event.initialized);
                }
            }
        }
        let instruction = body.blocks[block.index()].instructions.start + position;
        let Some(anchor) = self
            .anchors
            .iter()
            .find(|anchor| anchor.block == block.0 && anchor.instruction == instruction)
        else {
            return;
        };
        let mut roots = Vec::new();
        for (class, group) in [
            &anchor.roots.direct,
            &anchor.roots.interior,
            &anchor.roots.handle,
            &anchor.roots.compressed,
            &anchor.roots.stack,
        ]
        .into_iter()
        .enumerate()
        {
            for root in group {
                match root {
                    LogicalRoot::Value { value, .. } => roots.push(values[*value as usize]),
                    LogicalRoot::Slot { slot, offset } => {
                        let initialized = body.stack_slots[*slot as usize]
                            .roots
                            .iter()
                            .enumerate()
                            .any(|(bit, (at, kind))| {
                                at == offset
                                    && kind.root_kind() == Some(class as u32)
                                    && root_bases[*slot as usize]
                                        .map_or(self.bits[*slot as usize][bit], |base| {
                                            memory[base as usize + bit] != 0
                                        })
                            });
                        if initialized {
                            roots.push(word_at(memory, slots[*slot as usize] + offset));
                        }
                    }
                    LogicalRoot::Argument { .. } => {}
                }
            }
        }
        self.frames.borrow_mut().insert(self.base, roots);
    }
}

impl Drop for RootTrace {
    fn drop(&mut self) {
        self.frames.borrow_mut().remove(&self.base);
    }
}
