//! aarch64 对 LIR [`TargetOracle`] 的实现。
//!
//! legalization 复用与 codegen 完全相同的 [`super::lower::lower`] 分派，在合成的最小
//! body 上试降指令：合成的 body 只承担 lower 读取的 `args`/`values`/`target`/`signature`
//! 上下文，判定路径单一，不存在平行 lowering 表。与 x64 的差异在于 x64 在此运行
//! baseline instruction verifier；a64 的 form 合法面在编码时校验，oracle 回答
//! 「这条指令有没有可发出的机器序列」。

use super::lower::{self, Builder};
use super::machine;
use crate::gir::body::SourceInfo;
use crate::lir::body::{
    Body, Definition, Instruction, Op, Origin, Signature, Type, Value, ValueId, ValueType,
};
use crate::lir::target::{LegalizeError, TargetOracle};

/// 按目标名回答 LIR 机器事实的 aarch64 后端。
#[derive(Clone, Copy, Debug)]
pub struct A64Oracle {
    target: &'static str,
}

impl A64Oracle {
    /// `target` 是目标名（如 `aarch64-linux`）：它同时是 LIR body 的平台字符串，
    /// foreign call lowering 经 [`super::foreign_packet::platform`] 据此选择 ABI。
    pub const fn new(target: &'static str) -> Self {
        Self { target }
    }
}

impl TargetOracle for A64Oracle {
    fn poll_cost(&self, _op: &Op) -> u32 {
        // a64 尚无逐 form 的成本表；统一取 1 与 x64 probe 失败的回退值一致。
        // 代价是 poll 插入偏密（更保守），不会漏 safepoint。
        1
    }

    fn legalize(
        &self,
        op: &Op,
        operands: &[ValueId],
        results: &[ValueId],
        kind: &dyn Fn(ValueId) -> ValueType,
        source: &SourceInfo,
    ) -> Result<(), LegalizeError> {
        // 与 `select::select_mode` 的 runtime=true 分派同源：StackCheck/SafepointPoll 是
        // 结构性条目（入口检查与 Item::Poll），runtime 域 op 的 catalog 绑定在 codegen
        // 检查（`handles` 是同一判定谓词），其余落 `lower::lower`。
        match op {
            Op::StackCheck | Op::SafepointPoll { .. } => return Ok(()),
            _ if lower::runtime::handles(op) => return Ok(()),
            _ => {}
        }
        let (body, instruction) = probe(self.target, op, operands, results, kind, source);
        if let Op::Call(call) | Op::ForeignCall(call) = &instruction.op {
            // CallTarget::Runtime 经 runtime_catalog 绑定目标名——与 handles() 域同理，
            // 绑定合法性由 codegen 的 catalog 检查，oracle 这里只有 op 形状。
            if matches!(call.target, crate::lir::body::CallTarget::Runtime(_)) {
                return Ok(());
            }
            // 与 select 的 runtime 分支同源：Call 走 `Mode::Runtime`（checked entry /
            // 栈图上下文由 codegen 真实装配提供）。
            let mut function = machine::Function {
                name: "a64-oracle-probe".to_string(),
                entry: 0,
                checked_entry: false,
                types: Vec::new(),
                blocks: Vec::new(),
            };
            let mut builder = Builder {
                runtime_catalog: None,
                universe: None,
                symbols: None,
                function: &mut function,
                items: Vec::new(),
                context: "legalize".to_string(),
            };
            return lower::call::lower_mode(
                &body,
                call,
                body.args(&instruction.arguments),
                &body.results(&instruction).collect::<Vec<_>>(),
                lower::call::CallTail {
                    tail: false,
                    mode: lower::call::Mode::Runtime(None),
                    landing: None,
                },
                &mut builder,
            )
            .map_err(|error| LegalizeError::MissingSequence {
                op: op.domain(),
                detail: error.0,
            });
        }
        let mut function = machine::Function {
            name: "a64-oracle-probe".to_string(),
            entry: 0,
            checked_entry: false,
            types: Vec::new(),
            blocks: Vec::new(),
        };
        // `select_image` 给 codegen 的是镜像符号表（SymbolAddr 经它绑定镜像名，
        // CoverageCounter 要求它存在）；probe 用被测 op 自己的符号作绑定行。
        let bound = match op {
            Op::SymbolAddr(symbol) => vec![(symbol.clone(), "a64-oracle-probe".to_string())],
            _ => vec![(
                crate::lir::body::Symbol::Instance([0; 32]),
                "a64-oracle-probe".to_string(),
            )],
        };
        let mut builder = Builder {
            runtime_catalog: None,
            universe: None,
            symbols: Some(&bound),
            function: &mut function,
            items: Vec::new(),
            context: "legalize".to_string(),
        };
        lower::lower(&body, &instruction, &mut builder).map_err(|error| {
            LegalizeError::MissingSequence {
                op: op.domain(),
                detail: error.0,
            }
        })
    }
}

/// 合成只够 `lower` 读取的最小 body：`operands` 表只装参数 id，`results`
/// 是 `values` 表上新增的一段连续区间（与 build 的 emit 同构），其余下标填哨兵标量。
fn probe(
    target: &str,
    op: &Op,
    operands: &[ValueId],
    results: &[ValueId],
    kind: &dyn Fn(ValueId) -> ValueType,
    source: &SourceInfo,
) -> (Body, Instruction) {
    let sentinel = ValueType::scalar(Type::I64);
    let operand_top = operands.iter().map(|id| id.index() + 1).max().unwrap_or(0);
    // 值定义依赖型约束（如 Integer div 要求静态非零除数）不在 oracle API 面上：
    // legality 按「类型组合有没有序列」回答，操作数呈现为 IConst 定义即可走真实路径；
    // 变量除数等真实缺口在 codegen 以同一错误拒绝。
    let sentinel_definition = |id: ValueId| {
        if operands.contains(&id) {
            Definition::Instruction {
                instruction: crate::lir::body::InstId(0),
                result: 0,
            }
        } else {
            Definition::Parameter {
                block: crate::lir::body::BlockId(0),
                index: id.0,
            }
        }
    };
    let mut values: Vec<Value> = (0..operand_top)
        .map(|index| {
            let id = ValueId(crate::lir::body::id(index));
            Value {
                definition: sentinel_definition(id),
                kind: if operands.contains(&id) {
                    kind(id)
                } else {
                    sentinel
                },
                origin: Origin::None,
                uses: 0..0,
            }
        })
        .collect();
    let results_start = crate::lir::body::id(values.len());
    values.extend(results.iter().enumerate().map(|(index, id)| Value {
        definition: Definition::Instruction {
            instruction: crate::lir::body::InstId(1),
            result: crate::lir::body::id(index),
        },
        kind: kind(*id),
        origin: Origin::None,
        uses: 0..0,
    }));
    let arguments = 0..crate::lir::body::id(operands.len());
    let instruction = Instruction {
        op: op.clone(),
        arguments,
        results: results_start..results_start + crate::lir::body::id(results.len()),
        // sequencing token：atomic lowering 只要求 token 存在（select 同样不查值）。
        memory: Some(crate::lir::body::Memory {
            input: ValueId(0),
            output: ValueId(0),
        }),
        safepoint: None,
        source: source.clone(),
    };
    let signature = Signature {
        parameters: operands.iter().map(|id| kind(*id)).collect(),
        results: results.iter().map(|id| kind(*id)).collect(),
        sret: None,
        by_value: Vec::new(),
    };
    // StackAddr/RootInitAddr 按 SlotId 下标查 `stack_slots`；补齐到引用下标即可。
    let slots = match op {
        Op::StackAddr(slot) | Op::RootInitAddr(slot) => slot.index() + 1,
        _ => 0,
    };
    let stack_slots = (0..slots)
        .map(|_| crate::lir::body::StackSlot {
            local: 0,
            bytes: 8,
            align: 8,
            descriptor: [0; 32],
            roots: Vec::new(),
        })
        .collect();
    let body = Body {
        revision: 0,
        instance: [0; 32],
        owner: [0; 32],
        name: "a64-oracle-probe".to_string(),
        target: target.to_string(),
        signature,
        values,
        blocks: Vec::new(),
        // 槽 0 是被引用的操作数定义：一个 IConst(1)（见 sentinel_definition）。
        instructions: vec![Instruction {
            op: Op::IConst(1),
            arguments: 0..0,
            results: 0..0,
            memory: None,
            safepoint: None,
            source: source.clone(),
        }],
        operands: operands.to_vec(),
        parameters: Vec::new(),
        edges: Vec::new(),
        predecessors: Vec::new(),
        switch_cases: Vec::new(),
        stack_slots,
        lifetimes: Vec::new(),
        root_inits: Vec::new(),
        safepoints: Vec::new(),
        barrier_permits: Vec::new(),
        no_safepoint_regions: Vec::new(),
        source_scopes: Vec::new(),
        uses: Vec::new(),
        data: Vec::new(),
        assembly: Vec::new(),
        environments: Vec::new(),
        entry: crate::lir::body::BlockId(0),
        poll_summary: crate::lir::body::PollSummary {
            entry_stack_check: false,
            poll_free_cost: 0,
            has_poll_free_cycle: false,
        },
        poll_interface: crate::lir::body::PollInterface {
            summary: crate::lir::body::PollSummary {
                entry_stack_check: false,
                poll_free_cost: 0,
                has_poll_free_cycle: false,
            },
            leaf: true,
        },
    };
    (body, instruction)
}
