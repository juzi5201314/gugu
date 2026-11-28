//! 验证 scoped callback 的同步调用闭包，以及 view 参数不逃逸。

use crate::body::{
    Body, Call, CallTarget, Conversion, Definition, Instruction, Op, Origin, Provenance,
    RuntimeCall, SlotId, Terminator, ValueId,
};
use crate::gir::body::CallKind;
use crate::{Diagnostic, invalid};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

pub fn verify(bodies: &[Body]) -> Result<(), Diagnostic> {
    let seeds: Vec<_> = bodies
        .iter()
        .flat_map(calls)
        .filter(|(call, _)| !call.scoped_arguments.is_empty())
        .collect();
    if seeds.is_empty() {
        return Ok(());
    }
    let mut verifier = Verifier {
        bodies,
        pending: VecDeque::new(),
        parameters: BTreeMap::new(),
        checked: BTreeMap::new(),
        synchronous: BTreeSet::new(),
    };
    for (call, arguments) in seeds {
        let CallTarget::Instance(target) = call.target else {
            return Err(error("入口目标不是可证明的静态回调"));
        };
        let formals = verifier.formals(call, arguments.len())?;
        let mut previous = None;
        for index in &call.scoped_arguments {
            if previous.is_some_and(|last| last >= *index) {
                return Err(error("入口借用参数位置未严格递增"));
            }
            previous = Some(*index);
            let formal = formals
                .get(*index as usize + 1)
                .ok_or_else(|| error("入口借用参数位置超出目标签名"))?;
            verifier.add_parameter(target, formal.value.0);
        }
    }
    verifier.drain()
}

fn calls(body: &Body) -> impl Iterator<Item = (&Call, &[ValueId])> {
    let instructions = body.instructions.iter().filter_map(|instruction| {
        if let Op::Call(call) | Op::ForeignCall(call) = &instruction.op {
            Some((call, body.args(&instruction.arguments)))
        } else {
            None
        }
    });
    let terminators = body.blocks.iter().filter_map(|block| {
        if let Terminator::Invoke {
            call, arguments, ..
        }
        | Terminator::TailCall {
            call, arguments, ..
        } = &block.terminator
        {
            Some((call, body.args(arguments)))
        } else {
            None
        }
    });
    instructions.chain(terminators)
}

struct Verifier<'a> {
    bodies: &'a [Body],
    pending: VecDeque<[u8; 32]>,
    /// 参数集合单调增加；递归和互递归在有限的实例与 SSA 值集合上终止。
    parameters: BTreeMap<[u8; 32], BTreeSet<u32>>,
    checked: BTreeMap<[u8; 32], BTreeSet<u32>>,
    synchronous: BTreeSet<[u8; 32]>,
}

impl<'a> Verifier<'a> {
    fn drain(&mut self) -> Result<(), Diagnostic> {
        while let Some(key) = self.pending.pop_front() {
            let body = self.body(key)?;
            let parameters = self.parameters.get(&key).cloned().unwrap_or_default();
            if self.synchronous.insert(key) {
                self.check_synchronous(body)?;
            }
            if self.checked.get(&key) == Some(&parameters) || parameters.is_empty() {
                continue;
            }
            self.checked.insert(key, parameters.clone());
            self.check_borrows(body, &parameters)
                .map_err(|diagnostic| {
                    invalid(&format!("{}：{}", body.name, diagnostic.message()))
                })?;
        }
        Ok(())
    }

    fn check_synchronous(&mut self, body: &Body) -> Result<(), Diagnostic> {
        for instruction in &body.instructions {
            match instruction.op {
                Op::CoroutineSwitch => return Err(error("回调可达路径包含 suspend")),
                Op::ForeignCall(_) | Op::PlatformCall(_) | Op::RuntimeMachineCall(_) => {
                    return Err(error("回调可达路径包含 foreign frame"));
                }
                Op::InlineAsm(_) => return Err(error("回调可达路径包含 inline asm")),
                _ => {}
            }
        }
        for (call, _) in calls(body) {
            if call.kind != CallKind::Managed || call.may_suspend {
                return Err(error("回调可达调用可能挂起或进入 foreign frame"));
            }
            match call.target {
                CallTarget::Instance(key) => self.enqueue(key),
                CallTarget::Runtime(RuntimeCall::SelectCommit { .. }) => {
                    return Err(error("回调可达路径包含 select"));
                }
                CallTarget::Runtime(_) => {}
                _ => return Err(error("回调可达调用无法静态证明同步")),
            }
        }
        Ok(())
    }

    fn check_borrows(&mut self, body: &Body, parameters: &BTreeSet<u32>) -> Result<(), Diagnostic> {
        let mut state = BorrowState::new(body, parameters)?;
        loop {
            let mut changed = state.storage_addresses(body);
            for instruction in &body.instructions {
                changed |= state.instruction(body, instruction)?;
            }
            changed |= state.edges(body);
            for (call, arguments) in calls(body) {
                self.propagate_call(call, arguments, &state)?;
            }
            for block in &body.blocks {
                if let Terminator::Return { values, .. } = &block.terminator
                    && body.args(values).iter().any(|value| state.borrowed(*value))
                {
                    return Err(error("借用作为函数返回值逃逸"));
                }
            }
            if !changed {
                return Ok(());
            }
        }
    }

    fn propagate_call(
        &mut self,
        call: &Call,
        arguments: &[ValueId],
        state: &BorrowState,
    ) -> Result<(), Diagnostic> {
        if !arguments.iter().any(|value| state.borrowed(*value)) {
            return Ok(());
        }
        if arguments
            .iter()
            .any(|value| state.containing[value.index()])
        {
            return Err(error("含借用的局部槽地址传入 helper"));
        }
        if let CallTarget::Runtime(runtime) = call.target {
            // 资源值按值复制只增加控制页租约，不保存 view 地址或改写它的槽。
            if matches!(
                runtime,
                RuntimeCall::ResourceAcquire | RuntimeCall::ValueCopy
            ) && arguments.len() == 2
                && state.tainted[arguments[0].index()]
                && !state.borrowed(arguments[1])
            {
                return Ok(());
            }
            // 两种原语按 (destination, source, descriptor) 复制 T 的语义值。
            if matches!(
                runtime,
                RuntimeCall::ValueTransfer | RuntimeCall::CowSnapshot
            ) && arguments.len() == 3
                && arguments
                    .get(1)
                    .is_some_and(|value| state.tainted[value.index()])
                && arguments
                    .first()
                    .is_some_and(|value| !state.tainted[value.index()])
            {
                return Ok(());
            }
            return Err(error("借用传入未登记 no-escape 的 runtime 调用"));
        }
        let CallTarget::Instance(target) = call.target else {
            return Err(error("借用传入无法静态证明 no-escape 的调用"));
        };
        let formals = self.formals(call, arguments.len())?;
        for (argument, formal) in arguments.iter().zip(formals.iter().skip(1)) {
            if state.tainted[argument.index()] {
                self.add_parameter(target, formal.value.0);
            }
        }
        Ok(())
    }

    fn formals(
        &self,
        call: &Call,
        arguments: usize,
    ) -> Result<&'a [crate::body::Parameter], Diagnostic> {
        if call.kind != CallKind::Managed || call.may_suspend {
            return Err(error("借用传入可能挂起或 foreign call"));
        }
        let CallTarget::Instance(target) = call.target else {
            return Err(error("借用调用目标不是静态实例"));
        };
        let body = self.body(target)?;
        let formals = body.params(body.entry);
        if arguments != call.parameters.len()
            || arguments != body.signature.parameters.len()
            || formals.len() != arguments + 1
        {
            return Err(error("借用 helper 参数与静态目标签名不一致"));
        }
        Ok(formals)
    }

    fn add_parameter(&mut self, key: [u8; 32], value: u32) {
        if self.parameters.entry(key).or_default().insert(value) && !self.pending.contains(&key) {
            self.pending.push_back(key);
        }
    }

    fn enqueue(&mut self, key: [u8; 32]) {
        if !self.synchronous.contains(&key) && !self.pending.contains(&key) {
            self.pending.push_back(key);
        }
    }

    fn body(&self, key: [u8; 32]) -> Result<&'a Body, Diagnostic> {
        self.bodies
            .binary_search_by_key(&key, |body| body.instance)
            .ok()
            .map(|index| &self.bodies[index])
            .ok_or_else(|| error("静态回调目标缺少 LIR body"))
    }
}

struct BorrowState {
    /// SSA 值编号稠密，逐值位表覆盖 view 本身与含 view 的存储地址。
    tainted: Vec<bool>,
    containing: Vec<bool>,
    /// 栈槽和私有临时对象按字段偏移追踪，避免污染同一聚合中的普通值字段。
    stack: BTreeSet<(Storage, i64)>,
    unknown_offsets: BTreeSet<Storage>,
}

#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
enum Storage {
    Stack(SlotId),
    Allocation(u32),
}

impl BorrowState {
    fn new(body: &Body, parameters: &BTreeSet<u32>) -> Result<Self, Diagnostic> {
        let mut tainted = vec![false; body.values.len()];
        for value in parameters {
            let tainted = tainted
                .get_mut(*value as usize)
                .ok_or_else(|| error("回调参数值超出 LIR 值表"))?;
            *tainted = true;
        }
        Ok(Self {
            tainted,
            containing: vec![false; body.values.len()],
            stack: BTreeSet::new(),
            unknown_offsets: BTreeSet::new(),
        })
    }

    fn borrowed(&self, value: ValueId) -> bool {
        self.tainted[value.index()] || self.containing[value.index()]
    }

    fn has_slot(&self, slot: Storage) -> bool {
        self.unknown_offsets.contains(&slot)
            || self.stack.iter().any(|(candidate, _)| *candidate == slot)
    }

    fn storage_addresses(&mut self, body: &Body) -> bool {
        let mut changed = false;
        for index in 0..self.containing.len() {
            let value = ValueId(index as u32);
            if local_storage(body, value).is_some_and(|slot| self.has_slot(slot)) {
                changed |= mark(&mut self.containing, value);
            }
        }
        changed
    }

    fn edges(&mut self, body: &Body) -> bool {
        let mut changed = false;
        for edge in &body.edges {
            for (parameter, argument) in body.params(edge.to).iter().zip(body.args(&edge.arguments))
            {
                if self.tainted[argument.index()] {
                    changed |= mark(&mut self.tainted, parameter.value);
                }
                if self.containing[argument.index()] {
                    changed |= mark(&mut self.containing, parameter.value);
                }
            }
        }
        changed
    }

    fn instruction(&mut self, body: &Body, instruction: &Instruction) -> Result<bool, Diagnostic> {
        let arguments = body.args(&instruction.arguments);
        let mut changed = false;
        if matches!(instruction.op, Op::PtrOffset | Op::Select | Op::Convert(_))
            && arguments.iter().any(|value| self.containing[value.index()])
        {
            for result in body.results(instruction) {
                changed |= mark(&mut self.containing, result);
            }
        }
        changed |= match &instruction.op {
            Op::Store(_) => self.store(body, arguments)?,
            Op::Load(_) => self.load(body, instruction)?,
            Op::Memcpy { bytes } | Op::Memmove { bytes } => self.copy(body, arguments, *bytes)?,
            Op::Memset { .. } if self.tainted[arguments[0].index()] => {
                return Err(error("借用引用的目标被清空"));
            }
            Op::Atomic { .. } if arguments.iter().any(|value| self.borrowed(*value)) => {
                return Err(error("借用进入原子存储或原子操作"));
            }
            _ => self.derived(body, instruction)?,
        };
        Ok(changed)
    }

    fn store(&mut self, body: &Body, arguments: &[ValueId]) -> Result<bool, Diagnostic> {
        let (destination, value) = (arguments[0], arguments[1]);
        if self.tainted[destination.index()] {
            return Err(error("借用引用的目标被写入"));
        }
        if self.containing[value.index()] {
            return Err(error("含借用的局部槽地址被存储"));
        }
        if !self.tainted[value.index()] {
            return Ok(false);
        }
        let Some(slot) = local_storage(body, destination) else {
            return Err(error("借用被存入函数以外的槽"));
        };
        Ok(
            if let Some((_, offset)) = local_address(body, destination) {
                self.stack.insert((slot, offset))
            } else {
                self.unknown_offsets.insert(slot)
            },
        )
    }

    fn load(&mut self, body: &Body, instruction: &Instruction) -> Result<bool, Diagnostic> {
        let address = body.args(&instruction.arguments)[0];
        let borrowed = if let Some(slot) = local_storage(body, address) {
            self.unknown_offsets.contains(&slot)
                || local_address(body, address).map_or_else(
                    || self.has_slot(slot),
                    |(_, offset)| self.stack.contains(&(slot, offset)),
                )
        } else {
            self.containing[address.index()]
        };
        if !borrowed {
            return Ok(false);
        }
        let mut changed = false;
        for result in body.results(instruction) {
            if !body.values[result.index()]
                .kind
                .provenance
                .is_some_and(Provenance::managed)
            {
                return Err(error("借用通过 transmute 转为 raw pointer 或整数"));
            }
            changed |= mark(&mut self.tainted, result);
        }
        Ok(changed)
    }

    fn copy(&mut self, body: &Body, arguments: &[ValueId], bytes: u64) -> Result<bool, Diagnostic> {
        let (destination, source) = (arguments[0], arguments[1]);
        if self.tainted[destination.index()] {
            return Err(error("借用引用的目标被复制写入"));
        }
        // 从 view 读取 T 的字节是语义复制；这不复制 view 本身。
        if self.tainted[source.index()] && !self.containing[source.index()] {
            return Ok(false);
        }
        let Some((source_slot, source_offset)) = local_address(body, source) else {
            if self.containing[source.index()] {
                return Err(error("含借用的局部槽被复制到无法证明的存储"));
            }
            return Ok(false);
        };
        if self.unknown_offsets.contains(&source_slot) {
            return Err(error("含借用的栈槽复制缺少字段偏移证明"));
        }
        let offsets: Vec<_> = self
            .stack
            .iter()
            .filter_map(|(slot, offset)| {
                (*slot == source_slot
                    && *offset >= source_offset
                    && u64::try_from(*offset - source_offset).is_ok_and(|delta| delta < bytes))
                .then_some(*offset - source_offset)
            })
            .collect();
        if offsets.is_empty() {
            return Ok(false);
        }
        let Some((slot, offset)) = local_address(body, destination) else {
            return Err(error("含借用的局部槽被复制到外部存储"));
        };
        let mut changed = false;
        for delta in offsets {
            changed |= self.stack.insert((slot, offset + delta));
        }
        Ok(changed)
    }

    fn derived(&mut self, body: &Body, instruction: &Instruction) -> Result<bool, Diagnostic> {
        let arguments = body.args(&instruction.arguments);
        if !arguments.iter().any(|value| self.tainted[value.index()]) {
            return Ok(false);
        }
        match &instruction.op {
            Op::Convert(conversion) => {
                if matches!(
                    conversion,
                    Conversion::PointerToInt
                        | Conversion::IntToPointer
                        | Conversion::PointerCast
                        | Conversion::RawToReference
                ) || body.results(instruction).any(|value| {
                    !body.values[value.index()]
                        .kind
                        .provenance
                        .is_some_and(Provenance::managed)
                }) {
                    return Err(error("借用引用被转换为 raw pointer 或整数"));
                }
            }
            Op::PtrOffset | Op::Select | Op::DecodeCompressedRef | Op::ForwardSharedHandle => {}
            Op::RuntimeMachineCall(_) | Op::PlatformCall(_) | Op::InlineAsm(_) => {
                return Err(error("借用进入无法验证的 intrinsic"));
            }
            _ => return Ok(false),
        }
        let mut changed = false;
        for result in body.results(instruction) {
            changed |= mark(&mut self.tainted, result);
        }
        Ok(changed)
    }
}

fn mark(values: &mut [bool], value: ValueId) -> bool {
    !std::mem::replace(&mut values[value.index()], true)
}

fn local_storage(body: &Body, mut value: ValueId) -> Option<Storage> {
    for _ in 0..body.values.len() {
        match body.values[value.index()].origin {
            Origin::Stack(slot) => return Some(Storage::Stack(slot)),
            Origin::Allocation(allocation) => return Some(Storage::Allocation(allocation)),
            Origin::Derived(base) => value = base,
            _ => return None,
        }
    }
    None
}

fn local_address(body: &Body, mut value: ValueId) -> Option<(Storage, i64)> {
    let mut offset = 0i64;
    for _ in 0..body.values.len() {
        match body.values[value.index()].origin {
            Origin::Stack(slot) => return Some((Storage::Stack(slot), offset)),
            Origin::Allocation(allocation) => {
                return Some((Storage::Allocation(allocation), offset));
            }
            Origin::Derived(base) => {
                if let Definition::Instruction { instruction, .. } =
                    body.values[value.index()].definition
                    && matches!(body.instructions[instruction.index()].op, Op::PtrOffset)
                {
                    let arguments = body.args(&body.instructions[instruction.index()].arguments);
                    let constant = arguments.get(1)?;
                    let Definition::Instruction { instruction, .. } =
                        body.values[constant.index()].definition
                    else {
                        return None;
                    };
                    let Op::IConst(value) = body.instructions[instruction.index()].op else {
                        return None;
                    };
                    offset = offset.checked_add(i64::try_from(value).ok()?)?;
                }
                value = base;
            }
            _ => return None,
        }
    }
    None
}

fn error(message: &str) -> Diagnostic {
    invalid(&format!("scoped callback {message}"))
}
