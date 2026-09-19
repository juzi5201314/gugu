//! trace descriptor 的运行时解释器：按 Bitmap 或 Program 表示扫描对象的 managed pointer word。
//!
//! 解释器只消费已验证的 descriptor 字节（`gc_metadata_schema::boot_verify` 的同一套 tiling
//! 与位图不变量），不依赖调用方重复校验。它是**唯一**的解释内核：递归执行改为最多
//! `TRACE_MAX_DEPTH + 1` 个显式帧的可暂停游标，因此一个巨大的 descriptor 也不能绕过工作预算。
//!
//! REPEAT/REPEAT_FIELD 的 base 与 stride 以 8 字节 word 计；SWITCH 的 tag 以 payload 字节计，
//! 选择分支后必须回到**整个 SWITCH**（含 default 编码）之后；`ARENA_SLOTS` 不携带自己的扫描
//! 位，由调用方按 backing 的 initialized 位图与元素 descriptor 展开，因此这里只标记该语义。

use super::gc_metadata_schema::{TraceKind, TraceOp, decode_uleb};
use super::gc_metadata_section::GcRuntimeMetadata;
use super::slab::RawInvariant;

/// trace program 允许的最大嵌套深度；与验证器共用同一上界。
pub(crate) const TRACE_MAX_DEPTH: u8 = 32;

/// 显式解释帧数量：根帧加 `TRACE_MAX_DEPTH` 层嵌套。
pub(crate) const TRACE_MAX_FRAMES: usize = TRACE_MAX_DEPTH as usize + 1;

/// 一次 descriptor 扫描的结果。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct TraceScan {
    /// 扫描到的 managed pointer word 数。
    pub pointers: u32,
    /// descriptor 是否含 `ARENA_SLOTS`，需要按 backing initialized 位图展开。
    pub arena_slots: bool,
}

/// 访问一个 managed pointer word 的字节视图；`address` 是该 word 的 payload 地址。
pub(crate) type TraceVisitor<'a> = dyn FnMut(&mut [u8; 8], u64) -> Result<(), RawInvariant> + 'a;

/// 一次扫描的工作预算。
///
/// 每个 opcode、case、bitmap word 与指针访问各扣一个单位；额度用尽时游标返回 `Pending`，
/// 下一次调用从原状态继续，绝不从头重扫已处理的前缀。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct WorkBudget {
    remaining: u32,
    spent: u32,
}

impl WorkBudget {
    /// 以给定额度创建预算。
    pub(crate) const fn new(remaining: u32) -> Self {
        Self {
            remaining,
            spent: 0,
        }
    }

    /// 返回尚未使用的额度。
    pub(crate) const fn remaining(&self) -> u32 {
        self.remaining
    }

    /// 返回已经消费的额度。
    pub(crate) const fn spent(&self) -> u32 {
        self.spent
    }

    /// 扣除一个工作单位；额度耗尽返回 false。
    fn charge(&mut self) -> bool {
        if self.remaining == 0 {
            return false;
        }
        self.remaining -= 1;
        self.spent += 1;
        true
    }
}

/// 一次游标推进的结果。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TraceProgress {
    /// 预算用尽、游标已保存；调用方必须再次调用同一游标。
    Pending,
    /// 该 descriptor 已扫描完。
    Complete,
}

/// 一枚游标内部的单步结果。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StepOutcome {
    /// 本步完成，还有工作可做。
    Continue,
    /// 该 descriptor 已到 END。
    Complete,
}

/// 一个显式解释帧的状态。
///
/// `Body` 记录本帧的 program 位置与 REPEAT 累加位移；`Words`/`Repeat`/`Switch` 是执行完这段
/// 结构后回到 `resume`（或 `body_end`）的临时状态，各自的子 body 作为真正的子帧压栈。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FrameState {
    /// 根 bitmap：按 descriptor 字节推进，`bits` 是该字节内尚未访问的位。
    Bitmap {
        byte_index: usize,
        bits: u32,
        bitmap_bytes: usize,
    },
    /// 一个 program body 的执行位置。
    Body { pc: usize, shift_words: u64 },
    /// DIRECT/INTERIOR 的连续 word 区间。
    Words {
        next_word: u64,
        remaining: u64,
        shift_words: u64,
        resume: usize,
    },
    /// REPEAT/REPEAT_FIELD 的重复项推进。
    Repeat {
        item: u64,
        count: u64,
        base: u64,
        stride: u64,
        body_start: usize,
        body_end: usize,
        shift_words: u64,
    },
    /// SWITCH 的 case 扫描；扫完后本帧变成 `Body` 并压入所选分支。
    Switch {
        case_pc: usize,
        case_index: u64,
        case_count: u64,
        tag: u64,
        chosen: Option<(usize, usize)>,
        shift_words: u64,
    },
}

/// 根表示种类：`None` 直接完成，Bitmap 与 Program 各有一条推进路径。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RootKind {
    None,
    Bitmap,
    Program,
}
/// Bitmap 扫描的上下文快照；由 `step_bitmap` 消费以解构 `step_once` 的参数。
#[derive(Clone, Copy)]
struct BitmapContext<'a> {
    descriptor: &'a [u8],
    byte_index: usize,
    bits: u32,
    bitmap_bytes: usize,
}

/// SWITCH case 扫描的上下文快照；由 `step_switch` 消费以解构 `step_once` 的参数。
#[derive(Clone, Copy)]
struct SwitchContext<'a> {
    program: &'a [u8],
    case_pc: usize,
    case_index: u64,
    case_count: u64,
    tag: u64,
    chosen: Option<(usize, usize)>,
}

/// 可暂停的 trace 解释游标。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TraceCursor {
    frames: [Option<FrameState>; TRACE_MAX_FRAMES],
    depth: usize,
    root: RootKind,
    pointers: u32,
    arena_slots: bool,
    complete: bool,
}

impl Default for TraceCursor {
    fn default() -> Self {
        Self::new()
    }
}

impl TraceCursor {
    /// 创建一个尚未开始的空游标。
    pub(crate) const fn new() -> Self {
        Self {
            frames: [None; TRACE_MAX_FRAMES],
            depth: 0,
            root: RootKind::None,
            pointers: 0,
            arena_slots: false,
            complete: false,
        }
    }

    /// 返回扫描到的 managed pointer word 数。
    pub(crate) const fn pointers(&self) -> u32 {
        self.pointers
    }

    /// 返回 descriptor 是否含 `ARENA_SLOTS`。
    pub(crate) const fn arena_slots(&self) -> bool {
        self.arena_slots
    }

    /// 返回扫描结果。
    pub(crate) const fn scan(&self) -> TraceScan {
        TraceScan {
            pointers: self.pointers,
            arena_slots: self.arena_slots,
        }
    }

    /// 推进游标至多 `budget` 个工作单位。
    ///
    /// `type_index` 与 `types` 每步都要给出：游标只保存下标与状态，不跨调用借用类型表，也不
    /// 复制 descriptor。`payload` 由 owner 在扫描时取出视图，跨批不保存裸地址。
    pub(crate) fn step(
        &mut self,
        type_index: u32,
        types: &GcRuntimeMetadata,
        payload: &mut [u8],
        payload_base: u64,
        budget: &mut WorkBudget,
        visit: &mut TraceVisitor<'_>,
    ) -> Result<TraceProgress, RawInvariant> {
        if self.complete {
            return Ok(TraceProgress::Complete);
        }
        let descriptor = trace_of(types, type_index)?;
        if self.frames[0].is_none() {
            self.open(descriptor)?;
            if self.complete {
                return Ok(TraceProgress::Complete);
            }
        }
        let program = program_of(descriptor)?;
        loop {
            if !budget.charge() {
                return Ok(TraceProgress::Pending);
            }
            match self.step_once(descriptor, program, payload, payload_base, visit)? {
                StepOutcome::Complete => {
                    self.complete = true;
                    return Ok(TraceProgress::Complete);
                }
                StepOutcome::Continue => {}
            }
        }
    }

    /// 初始化根帧。
    fn open(&mut self, descriptor: &[u8]) -> Result<(), RawInvariant> {
        match descriptor.first().copied() {
            Some(kind) if kind == TraceKind::None as u8 => {
                self.root = RootKind::None;
                self.complete = true;
            }
            Some(kind) if kind == TraceKind::Bitmap as u8 => {
                let word_count = read_u32(descriptor, 4)?;
                let bitmap_bytes =
                    usize::try_from(word_count.div_ceil(8)).expect("bitmap 字节数适配宿主");
                // 两张位图都必须完整落在 descriptor 内：越界是真实损坏，而不是“没有指针”。
                descriptor
                    .get(8..8 + bitmap_bytes * 2)
                    .ok_or_else(|| RawInvariant::new("trace bitmap 越界"))?;
                self.root = RootKind::Bitmap;
                // 进入根帧时就把第一个有标记位的字节读进来：帧里的 `bits` 始终表示“该字节尚未
                // 访问的位”，因此位耗尽后只需推进到下一字节，绝不会重访同一位。
                let (byte_index, bits) = next_bitmap_byte(descriptor, 0, bitmap_bytes)?;
                if byte_index >= bitmap_bytes {
                    self.complete = true;
                    return Ok(());
                }
                self.frames[0] = Some(FrameState::Bitmap {
                    byte_index,
                    bits,
                    bitmap_bytes,
                });
            }
            Some(kind) if kind == TraceKind::Program as u8 => {
                let program = program_of(descriptor)?;
                if program.is_empty() {
                    return Err(RawInvariant::new("trace program 为空"));
                }
                self.root = RootKind::Program;
                self.frames[0] = Some(FrameState::Body {
                    pc: 0,
                    shift_words: 0,
                });
            }
            _ => return Err(RawInvariant::new("未知 trace descriptor kind")),
        }
        Ok(())
    }

    /// 推进一个工作单位。
    fn step_once(
        &mut self,
        descriptor: &[u8],
        program: &[u8],
        payload: &mut [u8],
        payload_base: u64,
        visit: &mut TraceVisitor<'_>,
    ) -> Result<StepOutcome, RawInvariant> {
        let frame =
            self.frames[self.depth].ok_or_else(|| RawInvariant::new("trace 游标缺少当前帧"))?;
        match frame {
            FrameState::Bitmap {
                byte_index,
                bits,
                bitmap_bytes,
            } => {
                let ctx = BitmapContext {
                    descriptor,
                    byte_index,
                    bits,
                    bitmap_bytes,
                };
                self.step_bitmap(&ctx, payload, payload_base, visit)
            }
            FrameState::Words {
                next_word,
                remaining,
                shift_words,
                resume,
            } => {
                if remaining == 0 {
                    self.frames[self.depth] = Some(FrameState::Body {
                        pc: resume,
                        shift_words,
                    });
                    return Ok(StepOutcome::Continue);
                }
                let word = next_word
                    .checked_add(shift_words)
                    .ok_or_else(|| RawInvariant::new("trace word 下标溢出"))?;
                visit_word(payload, payload_base, word, visit, &mut self.pointers)?;
                let remainder = remaining - 1;
                self.frames[self.depth] = if remainder == 0 {
                    Some(FrameState::Body {
                        pc: resume,
                        shift_words,
                    })
                } else {
                    Some(FrameState::Words {
                        next_word: next_word + 1,
                        remaining: remainder,
                        shift_words,
                        resume,
                    })
                };
                Ok(StepOutcome::Continue)
            }
            FrameState::Repeat {
                item,
                count,
                base,
                stride,
                body_start,
                body_end,
                shift_words,
            } => {
                if item >= count {
                    self.frames[self.depth] = Some(FrameState::Body {
                        pc: body_end,
                        shift_words,
                    });
                    return Ok(StepOutcome::Continue);
                }
                let shift = base
                    .checked_add(item.saturating_mul(stride))
                    .and_then(|shift| shift.checked_add(shift_words))
                    .ok_or_else(|| RawInvariant::new("trace repeat 位移溢出"))?;
                self.frames[self.depth] = Some(FrameState::Repeat {
                    item: item + 1,
                    count,
                    base,
                    stride,
                    body_start,
                    body_end,
                    shift_words,
                });
                self.push(FrameState::Body {
                    pc: body_start,
                    shift_words: shift,
                })?;
                Ok(StepOutcome::Continue)
            }
            FrameState::Switch {
                case_pc,
                case_index,
                case_count,
                tag,
                chosen,
                shift_words,
            } => {
                let ctx = SwitchContext {
                    program,
                    case_pc,
                    case_index,
                    case_count,
                    tag,
                    chosen,
                };
                self.step_switch(&ctx, shift_words)
            }
            FrameState::Body { pc, shift_words } => {
                self.step_body(descriptor, program, payload, pc, shift_words)
            }
        }
    }

    /// 推进根 bitmap 的一位。
    fn step_bitmap(
        &mut self,
        ctx: &BitmapContext<'_>,
        payload: &mut [u8],
        payload_base: u64,
        visit: &mut TraceVisitor<'_>,
    ) -> Result<StepOutcome, RawInvariant> {
        if ctx.bits != 0 {
            let bit = ctx.bits.trailing_zeros();
            // 位号 0..=7 来自 direct、8..=15 来自 interior；两者都指向**同一个** payload word，
            // 因此 interior 不能额外再偏移八个 word。
            let word = u64::try_from(ctx.byte_index).expect("位图字节下标适配 u64") * 8
                + u64::from(bit % 8);
            visit_word(payload, payload_base, word, visit, &mut self.pointers)?;
            self.frames[0] = Some(FrameState::Bitmap {
                byte_index: ctx.byte_index,
                bits: ctx.bits & (ctx.bits - 1),
                bitmap_bytes: ctx.bitmap_bytes,
            });
            return Ok(StepOutcome::Continue);
        }
        let word_count = read_u32(ctx.descriptor, 4)?;
        // 当前字节的标记位已经访问完：从**下一个**字节继续，绝不重读同一位。
        let (next_index, next_bits) =
            next_bitmap_byte(ctx.descriptor, ctx.byte_index + 1, ctx.bitmap_bytes)?;
        if next_index >= ctx.bitmap_bytes {
            if u64::from(self.pointers) > u64::from(word_count) {
                return Err(RawInvariant::new("trace bitmap 扫描位数超过 word 数"));
            }
            self.complete = true;
            return Ok(StepOutcome::Complete);
        }
        self.frames[0] = Some(FrameState::Bitmap {
            byte_index: next_index,
            bits: next_bits,
            bitmap_bytes: ctx.bitmap_bytes,
        });
        Ok(StepOutcome::Continue)
    }

    /// 扫描 SWITCH 的一个 case，或在其后压入所选分支。
    fn step_switch(
        &mut self,
        ctx: &SwitchContext<'_>,
        shift_words: u64,
    ) -> Result<StepOutcome, RawInvariant> {
        let mut pc = ctx.case_pc;
        if ctx.case_index < ctx.case_count {
            let case_tag = read_u64(ctx.program, pc)?;
            pc += 8;
            let body_len = read_u32(ctx.program, pc)?;
            pc += 4;
            let body_end = pc + body_len as usize;
            if body_end > ctx.program.len() {
                return Err(RawInvariant::new("trace switch case body 越界"));
            }
            let chosen = if case_tag == ctx.tag && ctx.chosen.is_none() {
                Some((pc, body_end))
            } else {
                ctx.chosen
            };
            self.frames[self.depth] = Some(FrameState::Switch {
                case_pc: body_end,
                case_index: ctx.case_index + 1,
                case_count: ctx.case_count,
                tag: ctx.tag,
                chosen,
                shift_words,
            });
            return Ok(StepOutcome::Continue);
        }
        let default_len = read_u32(ctx.program, pc)?;
        pc += 4;
        let default_end = pc + default_len as usize;
        if default_end > ctx.program.len() {
            return Err(RawInvariant::new("trace switch default body 越界"));
        }
        let (body_start, _) = ctx.chosen.unwrap_or((pc, default_end));
        // 选择分支后必须回到整个 SWITCH（含 default 编码）之后，否则 default body 会被当作
        // opcode 再解释一遍。
        self.frames[self.depth] = Some(FrameState::Body {
            pc: default_end,
            shift_words,
        });
        self.push(FrameState::Body {
            pc: body_start,
            shift_words,
        })?;
        Ok(StepOutcome::Continue)
    }

    /// 执行一个 opcode。
    fn step_body(
        &mut self,
        descriptor: &[u8],
        program: &[u8],
        payload: &[u8],
        pc: usize,
        shift_words: u64,
    ) -> Result<StepOutcome, RawInvariant> {
        let op = *program
            .get(pc)
            .ok_or_else(|| RawInvariant::new("trace program 缺少 END"))?;
        let mut index = pc + 1;
        match op {
            x if x == TraceOp::End as u8 => {
                if self.depth == 0 {
                    if index != program.len() {
                        return Err(RawInvariant::new("trace program 未恰好在 END 处结束"));
                    }
                    self.complete = true;
                    return Ok(StepOutcome::Complete);
                }
                self.frames[self.depth] = None;
                self.depth -= 1;
                Ok(StepOutcome::Continue)
            }
            x if x == TraceOp::Direct as u8 || x == TraceOp::Interior as u8 => {
                let base = uleb(program, &mut index)?;
                let count = uleb(program, &mut index)?;
                self.frames[self.depth] = Some(FrameState::Words {
                    next_word: base,
                    remaining: count,
                    shift_words,
                    resume: index,
                });
                Ok(StepOutcome::Continue)
            }
            x if x == TraceOp::Repeat as u8 || x == TraceOp::RepeatField as u8 => {
                let repeat_field = x == TraceOp::RepeatField as u8;
                let base = uleb(program, &mut index)?;
                let count = if repeat_field {
                    let count_offset = uleb(program, &mut index)?;
                    let width = uleb(program, &mut index)?;
                    let count = read_field(payload, count_offset, width)?;
                    let stride = uleb(program, &mut index)?;
                    (count, stride)
                } else {
                    let count = uleb(program, &mut index)?;
                    let stride = uleb(program, &mut index)?;
                    (count, stride)
                };
                let body_len = read_u32(program, index)?;
                index += 4;
                let body_end = index + body_len as usize;
                if body_end > program.len() {
                    return Err(RawInvariant::new("trace repeat body 越界"));
                }
                self.frames[self.depth] = Some(FrameState::Repeat {
                    item: 0,
                    count: count.0,
                    base,
                    stride: count.1,
                    body_start: index,
                    body_end,
                    shift_words,
                });
                Ok(StepOutcome::Continue)
            }
            x if x == TraceOp::Switch as u8 => {
                let tag_offset = uleb(program, &mut index)?;
                let width = uleb(program, &mut index)?;
                let case_count = uleb(program, &mut index)?;
                let tag = read_field(payload, tag_offset, width)?;
                self.frames[self.depth] = Some(FrameState::Switch {
                    case_pc: index,
                    case_index: 0,
                    case_count,
                    tag,
                    chosen: None,
                    shift_words,
                });
                Ok(StepOutcome::Continue)
            }
            x if x == TraceOp::ArenaSlots as u8 => {
                self.arena_slots = true;
                self.frames[self.depth] = Some(FrameState::Body {
                    pc: index,
                    shift_words,
                });
                Ok(StepOutcome::Continue)
            }
            _ => {
                let _ = descriptor;
                Err(RawInvariant::new("未知 trace op"))
            }
        }
    }

    /// 压入一个子帧并推进深度。
    fn push(&mut self, state: FrameState) -> Result<(), RawInvariant> {
        if self.depth + 1 >= TRACE_MAX_FRAMES {
            return Err(RawInvariant::new("trace program 解释嵌套过深"));
        }
        self.depth += 1;
        self.frames[self.depth] = Some(state);
        Ok(())
    }
}

/// `ARENA_SLOTS` backing 的动态 payload 头字节数：`{slot_count, records_offset, data_offset, capacity}`。
pub(crate) const ARENA_HEADER_BYTES: u64 = 32;
/// backing 头内各字段的字节偏移。
const ARENA_HEADER_SLOT_COUNT: u64 = 0;
const ARENA_HEADER_RECORDS_OFFSET: u64 = 8;
const ARENA_HEADER_DATA_OFFSET: u64 = 16;
const ARENA_HEADER_CAPACITY: u64 = 24;
/// 一条 slot 记录的字节数：`{value_offset: u64, type_id: u32, flags: u32}`。
pub(crate) const ARENA_SLOT_RECORD_BYTES: u64 = 16;
/// slot 记录的 `INITIALIZED` 位；其余位必须为 0。
const ARENA_SLOT_INITIALIZED: u32 = 1;

/// ARENA_SLOTS 的 slot 展开游标。
///
/// 只解释 initialized slot：未初始化槽不是 root，也不建立可达性。每个 initialized slot 用
/// 同一套访问内核解释其 inline descriptor，因此 inline 值的字段偏移与一次性扫描完全一致。
///
/// 编译器只把 `ARENA_SLOTS` 作为内建 backing program 的第一条有效指令并紧接 `END` 发出：该
/// 指令本身不访问任何 word，backing 的可达性全部来自随后的 slot 展开。运行时先完整执行验证过的
/// program（`ARENA_SLOTS` 只置位标记），再在同一预算下展开 initialized slot，展开不重跑 program。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ArenaSlotCursor {
    slot_count: u64,
    records_offset: u64,
    data_offset: u64,
    capacity: u64,
    slot_index: u64,
    /// 已经结束的 inline 扫描计到的 managed pointer word 数。
    pointers: u32,
    /// 正在解释的 inline 值游标；`None` 表示当前槽未初始化或已解释完。
    inline: Option<Box<TraceCursor>>,
}

impl ArenaSlotCursor {
    /// 从 backing payload 读取头部并创建一个 slot 游标。
    fn open(payload: &[u8]) -> Result<Self, RawInvariant> {
        let head = payload
            .get(..usize::try_from(ARENA_HEADER_BYTES).expect("头长度适配宿主"))
            .ok_or_else(|| RawInvariant::new("arena backing 头部越过 payload"))?;
        let field = |offset: u64| -> Result<u64, RawInvariant> {
            read_u64(head, usize::try_from(offset).expect("头字段偏移适配宿主"))
        };
        let slot_count = field(ARENA_HEADER_SLOT_COUNT)?;
        let records_offset = field(ARENA_HEADER_RECORDS_OFFSET)?;
        let data_offset = field(ARENA_HEADER_DATA_OFFSET)?;
        let capacity = field(ARENA_HEADER_CAPACITY)?;
        let payload_len = u64::try_from(payload.len()).expect("payload 长度适配 u64");
        if data_offset > payload_len || capacity > payload_len - data_offset {
            return Err(RawInvariant::new("arena backing 的 data 区越过 payload"));
        }
        // 记录区必须完整落在 payload 内：越界记录不能靠“读不到就跳过”掩盖。
        let records_end = slot_count
            .checked_mul(ARENA_SLOT_RECORD_BYTES)
            .and_then(|bytes| records_offset.checked_add(bytes))
            .ok_or_else(|| RawInvariant::new("arena backing 的记录区范围溢出"))?;
        if records_offset > payload_len || records_end > payload_len {
            return Err(RawInvariant::new("arena backing 的记录区越过 payload"));
        }
        Ok(Self {
            slot_count,
            records_offset,
            data_offset,
            capacity,
            slot_index: 0,
            pointers: 0,
            inline: None,
        })
    }

    /// 返回 slot 展开计到的 managed pointer word 数。
    const fn pointers(&self) -> u32 {
        match self.inline.as_ref() {
            Some(inline) => self.pointers.saturating_add(inline.pointers()),
            None => self.pointers,
        }
    }

    /// 推进 slot 展开，直至全部 initialized slot 解释完或预算用尽。
    fn step(
        &mut self,
        types: &GcRuntimeMetadata,
        payload: &mut [u8],
        payload_base: u64,
        budget: &mut WorkBudget,
        visit: &mut TraceVisitor<'_>,
    ) -> Result<TraceProgress, RawInvariant> {
        loop {
            if self.inline.is_some() {
                // 先按记录读出身份：identity 只借 `self`，随后才能可变借用 inline 游标。
                let (type_id, base) = self.inline_identity(payload)?;
                let start = usize::try_from(base).expect("inline base 适配宿主");
                let slice = payload
                    .get_mut(start..)
                    .ok_or_else(|| RawInvariant::new("arena slot 的 inline 值越过 payload"))?;
                let inline_base = payload_base
                    .checked_add(base)
                    .ok_or_else(|| RawInvariant::new("arena slot 的 inline 地址溢出"))?;
                let inline = self.inline.as_mut().expect("已确认存在 inline 游标");
                match inline.step(type_id, types, slice, inline_base, budget, visit)? {
                    TraceProgress::Pending => return Ok(TraceProgress::Pending),
                    TraceProgress::Complete => {
                        // inline 值完全解释完才算入统计：暂停时它的指针已经记在自身游标里。
                        self.pointers = self
                            .pointers
                            .checked_add(inline.pointers())
                            .ok_or_else(|| RawInvariant::new("arena slot 指针计数溢出"))?;
                        self.inline = None;
                        continue;
                    }
                }
            }
            if self.slot_index >= self.slot_count {
                return Ok(TraceProgress::Complete);
            }
            let record_offset = self
                .records_offset
                .checked_add(
                    self.slot_index
                        .checked_mul(ARENA_SLOT_RECORD_BYTES)
                        .ok_or_else(|| RawInvariant::new("arena slot 记录偏移溢出"))?,
                )
                .ok_or_else(|| RawInvariant::new("arena slot 记录偏移溢出"))?;
            let record_offset = usize::try_from(record_offset).expect("slot 记录偏移适配宿主");
            let record = payload
                .get(record_offset..record_offset + 16)
                .ok_or_else(|| RawInvariant::new("arena slot 记录越过 payload"))?;
            let value_offset = read_u64(record, 0)?;
            let type_id = read_u32(record, 8)?;
            let flags = read_u32(record, 12)?;
            self.slot_index += 1;
            if flags & !ARENA_SLOT_INITIALIZED != 0 {
                return Err(RawInvariant::new("arena slot 记录了未知 flags"));
            }
            if flags & ARENA_SLOT_INITIALIZED == 0 {
                // 未初始化槽不是 root：跳过，不解释任何内容。
                continue;
            }
            // 记录本身也扣一个工作单位：一个巨大的 backing 不能靠“全部未初始化”绕过预算。
            if !budget.charge() {
                self.slot_index -= 1;
                return Ok(TraceProgress::Pending);
            }
            self.validate_inline_slot(value_offset, type_id, types)?;
            self.inline = Some(Box::new(TraceCursor::new()));
        }
    }

    /// 返回当前 inline 槽的 `(TypeId, payload 内 base 偏移)`。
    ///
    /// base 是 `data_offset + value_offset`：记录里的 `value_offset` 相对 data 区，而 inline
    /// descriptor 的字段偏移相对整个 backing payload，两者必须在这里合成一次。
    fn inline_identity(&self, payload: &[u8]) -> Result<(u32, u64), RawInvariant> {
        let record_index = self.slot_index.saturating_sub(1);
        let record_offset = self
            .records_offset
            .checked_add(
                record_index
                    .checked_mul(ARENA_SLOT_RECORD_BYTES)
                    .ok_or_else(|| RawInvariant::new("arena slot 记录偏移溢出"))?,
            )
            .ok_or_else(|| RawInvariant::new("arena slot 记录偏移溢出"))?;
        let record_offset = usize::try_from(record_offset).expect("slot 记录偏移适配宿主");
        let record = payload
            .get(record_offset..record_offset + 16)
            .ok_or_else(|| RawInvariant::new("arena slot 记录越过 payload"))?;
        let value_offset = read_u64(record, 0)?;
        let type_id = read_u32(record, 8)?;
        let base = self
            .data_offset
            .checked_add(value_offset)
            .ok_or_else(|| RawInvariant::new("arena slot 的值地址溢出"))?;
        Ok((type_id, base))
    }

    /// 校验一个 initialized 槽：记录指向 data 区、类型存在、对齐与容量都成立。
    fn validate_inline_slot(
        &self,
        value_offset: u64,
        type_id: u32,
        types: &GcRuntimeMetadata,
    ) -> Result<(), RawInvariant> {
        let entry = types
            .types()
            .get(usize::try_from(type_id).expect("类型下标适配宿主"))
            .ok_or_else(|| RawInvariant::new("arena slot 引用了不存在的类型"))?;
        // `flags` bit 3 = `HAS_RESOURCE`（`gc-metadata.md` 类型旗标表）：含 resource 的类型在
        // arena allocation 前已被拒绝，这里出现即元数据与分配不一致。
        if entry.flags & 0b1000 != 0 {
            return Err(RawInvariant::new("arena slot 的类型含 resource"));
        }
        if value_offset >= self.capacity {
            return Err(RawInvariant::new("arena slot 的值偏移越过 backing 容量"));
        }
        if entry.align != 0 && !value_offset.is_multiple_of(entry.align) {
            return Err(RawInvariant::new("arena slot 的值没有满足类型对齐"));
        }
        if entry.size > self.capacity - value_offset {
            return Err(RawInvariant::new("arena slot 的值越过 backing 容量"));
        }
        Ok(())
    }
}

/// 一个对象的完整扫描状态：程序游标加 ARENA_SLOTS 的 slot 展开。
///
/// backing descriptor 的 `ARENA_SLOTS` 只是“含 arena backing 语义”的标记：真正的可达性来自
/// 逐个 initialized slot 的 inline descriptor，因此两者必须在同一预算下一起推进。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ObjectTraceCursor {
    program: TraceCursor,
    slots: Option<ArenaSlotCursor>,
}

impl ObjectTraceCursor {
    /// 创建一个尚未开始的对象扫描。
    pub(crate) const fn new() -> Self {
        Self {
            program: TraceCursor::new(),
            slots: None,
        }
    }

    /// 返回扫描到的 managed pointer word 数。
    pub(crate) const fn pointers(&self) -> u32 {
        let program = self.program.pointers();
        match self.slots.as_ref() {
            Some(slots) => program.saturating_add(slots.pointers()),
            None => program,
        }
    }

    /// 返回扫描结果。
    pub(crate) fn scan(&self) -> TraceScan {
        TraceScan {
            pointers: self.pointers(),
            arena_slots: self.program.arena_slots(),
        }
    }

    /// 推进对象扫描；程序部分完成后自动转入 slot 展开。
    pub(crate) fn step(
        &mut self,
        type_index: u32,
        types: &GcRuntimeMetadata,
        payload: &mut [u8],
        payload_base: u64,
        budget: &mut WorkBudget,
        visit: &mut TraceVisitor<'_>,
    ) -> Result<TraceProgress, RawInvariant> {
        if let Some(slots) = self.slots.as_mut() {
            return slots.step(types, payload, payload_base, budget, visit);
        }
        match self
            .program
            .step(type_index, types, payload, payload_base, budget, visit)?
        {
            TraceProgress::Pending => Ok(TraceProgress::Pending),
            TraceProgress::Complete => {
                if !self.program.arena_slots() {
                    return Ok(TraceProgress::Complete);
                }
                // backing program 的 ARENA_SLOTS 只标记语义：展开 initialize slot 才算完成。
                self.slots = Some(ArenaSlotCursor::open(payload)?);
                self.step(type_index, types, payload, payload_base, budget, visit)
            }
        }
    }
}

/// 按 descriptor 扫描一个对象的 payload。
///
/// 这是既有的一次性入口：它驱动同一个游标直至完成，因此一次性扫描与批量扫描访问完全相同的
/// 真实偏移。`payload_base` 是 `payload[0]` 的地址。
///
/// 总工作量有明确上界（descriptor 长度 × 帧数 + payload word 数）：合法 descriptor 的访问次数
/// 远低于它，而损坏的 descriptor 不能靠无限预算把驱动变成不终止的循环或无限增长的访问列表。
pub(crate) fn walk_descriptor(
    type_index: u32,
    types: &GcRuntimeMetadata,
    payload: &mut [u8],
    payload_base: u64,
    visit: &mut TraceVisitor<'_>,
) -> Result<TraceScan, RawInvariant> {
    let descriptor =
        u64::try_from(trace_of(types, type_index)?.len()).expect("descriptor 长度适配 u64");
    let payload_words = u64::try_from(payload.len()).expect("payload 长度适配 u64") / 8;
    let frames = u64::from(TRACE_MAX_DEPTH) + 1;
    let ceiling = descriptor
        .saturating_mul(frames)
        .saturating_add(payload_words)
        .saturating_add(1024)
        .min(u64::from(u32::MAX));
    let mut budget = WorkBudget::new(u32::try_from(ceiling).expect("上界已收敛到 u32"));
    let mut cursor = ObjectTraceCursor::new();
    match cursor.step(type_index, types, payload, payload_base, &mut budget, visit)? {
        TraceProgress::Complete => Ok(cursor.scan()),
        // 额度是本函数的证明上界：耗尽说明 descriptor 描述的访问次数超出合法范围。
        TraceProgress::Pending => Err(RawInvariant::new("trace 扫描超过工作量上界")),
    }
}

/// 返回一个类型的 trace descriptor；类型表缺失下标是真实损坏。
pub(crate) fn trace_of(types: &GcRuntimeMetadata, type_index: u32) -> Result<&[u8], RawInvariant> {
    types
        .types()
        .get(usize::try_from(type_index).expect("类型下标适配宿主"))
        .map(|entry| entry.trace.as_slice())
        .ok_or_else(|| RawInvariant::new("trace 引用了不存在的类型"))
}

/// 访问一个 payload word，并把计数记进扫描统计。
fn visit_word(
    payload: &mut [u8],
    payload_base: u64,
    word: u64,
    visit: &mut TraceVisitor<'_>,
    pointers: &mut u32,
) -> Result<(), RawInvariant> {
    let offset = word
        .checked_mul(8)
        .ok_or_else(|| RawInvariant::new("trace word 字节偏移溢出"))?;
    let offset =
        usize::try_from(offset).map_err(|_| RawInvariant::new("trace word 偏移超出宿主"))?;
    let end = offset
        .checked_add(8)
        .ok_or_else(|| RawInvariant::new("trace word 范围溢出"))?;
    let address = payload_base
        .checked_add(u64::try_from(offset).expect("偏移适配 u64"))
        .ok_or_else(|| RawInvariant::new("trace word 地址溢出"))?;
    let word_bytes: &mut [u8; 8] = payload
        .get_mut(offset..end)
        .ok_or_else(|| RawInvariant::new("trace word 越过 payload"))?
        .try_into()
        .expect("word 视图宽度固定");
    visit(word_bytes, address)?;
    *pointers = pointers
        .checked_add(1)
        .ok_or_else(|| RawInvariant::new("trace 指针计数溢出"))?;
    Ok(())
}

/// 从 `start` 起查找下一个含标记位的位图字节；返回 `(字节下标, 剩余位)`。
///
/// 找不到时返回 `(bitmap_bytes, 0)`，调用方据此判断扫描结束。direct 与 interior 的同位号
/// 位都指向同一个 payload word，因此这里把两者合成同一份“剩余位”。
fn next_bitmap_byte(
    descriptor: &[u8],
    start: usize,
    bitmap_bytes: usize,
) -> Result<(usize, u32), RawInvariant> {
    let direct = descriptor
        .get(8..8 + bitmap_bytes)
        .ok_or_else(|| RawInvariant::new("trace bitmap 越界"))?;
    let interior = descriptor
        .get(8 + bitmap_bytes..8 + bitmap_bytes * 2)
        .ok_or_else(|| RawInvariant::new("trace bitmap 越界"))?;
    let mut index = start;
    while index < bitmap_bytes {
        let bits = u32::from(direct[index]) | (u32::from(interior[index]) << 8);
        if bits != 0 {
            return Ok((index, bits));
        }
        index += 1;
    }
    Ok((bitmap_bytes, 0))
}

/// 按 `width` 从 payload 的小端字段读取计数或判别值。
fn read_field(payload: &[u8], offset: u64, width: u64) -> Result<u64, RawInvariant> {
    if !matches!(width, 1 | 2 | 4 | 8) {
        return Err(RawInvariant::new("trace 字段宽度必须是 1、2、4 或 8"));
    }
    let offset =
        usize::try_from(offset).map_err(|_| RawInvariant::new("trace 字段偏移超出宿主"))?;
    let end = offset
        .checked_add(width as usize)
        .ok_or_else(|| RawInvariant::new("trace 字段范围溢出"))?;
    let bytes = payload
        .get(offset..end)
        .ok_or_else(|| RawInvariant::new("trace 字段越过 payload"))?;
    let mut value = 0u64;
    for (shift, byte) in bytes.iter().enumerate() {
        value |= u64::from(*byte) << (shift * 8);
    }
    Ok(value)
}

/// 解码 ULEB 操作数；验证器错误转换成平面不变量。
fn uleb(program: &[u8], index: &mut usize) -> Result<u64, RawInvariant> {
    decode_uleb(program, index).map_err(|error| RawInvariant::new(error.message().to_owned()))
}

/// 返回 descriptor 中的 program 视图；非 program 表示返回空切片。
fn program_of(descriptor: &[u8]) -> Result<&[u8], RawInvariant> {
    if descriptor.first().copied() != Some(TraceKind::Program as u8) {
        return Ok(&[]);
    }
    let length = read_u32(descriptor, 1)?;
    descriptor
        .get(5..5 + length as usize)
        .ok_or_else(|| RawInvariant::new("trace program 越界"))
}

fn read_u32(bytes: &[u8], start: usize) -> Result<u32, RawInvariant> {
    let end = start
        .checked_add(4)
        .ok_or_else(|| RawInvariant::new("u32 范围溢出"))?;
    let slice: [u8; 4] = bytes
        .get(start..end)
        .ok_or_else(|| RawInvariant::new("u32 越界"))?
        .try_into()
        .expect("u32 宽度固定");
    Ok(u32::from_le_bytes(slice))
}

fn read_u64(bytes: &[u8], start: usize) -> Result<u64, RawInvariant> {
    let end = start
        .checked_add(8)
        .ok_or_else(|| RawInvariant::new("u64 范围溢出"))?;
    let slice: [u8; 8] = bytes
        .get(start..end)
        .ok_or_else(|| RawInvariant::new("u64 越界"))?
        .try_into()
        .expect("u64 宽度固定");
    Ok(u64::from_le_bytes(slice))
}
