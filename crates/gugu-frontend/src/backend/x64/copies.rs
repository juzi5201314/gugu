//! 块参数与调用边界的并行拷贝：并行 pairs 与虚拟层串行渲染。
//!
//! 语义是「所有读发生在所有写之前」：一串边参数拷贝必须作为一个整体执行，任何一条 move
//! 都不能覆盖还没被读走的源。这里采用轮转法把并行拷贝摊平成串行序列：
//!
//! 1. 先发射目标不再作为任何剩余拷贝源的边（这些目标已经不会被读到）；
//! 2. 只剩环时选一条边，把它的**目标**值存进同类型的临时虚拟寄存器，把仍然读该目标的
//!    源改指临时寄存器，再发射这条边——此时该目标已被保存，环被打破。
//!
//! 这里的产物只是**虚拟层渲染**：选指阶段必须把可编码的机器序列写进站点，同一批
//! `pairs` 同时登记为 [`CopyGroup`]。寄存器分配阶段丢弃渲染结果、按 `pairs` 用物理位置
//! 重新解析（`r11` 或 16 字节栈 scratch 打断环），因此临时虚拟寄存器不会进入分配。
//! 渲染仍必须自洽：harness 与站点文本断言都读它。

use super::lower::LoweringError;
use super::reg::{FRAME_SLOT_BASE, Reg};
use crate::lir::body::{Body, EdgeId, Type};

/// 一条待发射的拷贝。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Copy {
    /// 源寄存器。
    pub src: Reg,
    /// 目标寄存器。
    pub dest: Reg,
    /// 机器类型；决定 move 的宽度与 XMM 形式。
    pub ty: Type,
}

/// 一段并行拷贝：站点序列里的机器指令范围与并行语义的原始配对。
///
/// `instructions` 相对站点序列起点；分配阶段按 `pairs` 重发这段指令范围，渲染结果
/// （含可能出现的临时虚拟寄存器）被整体替换。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CopyGroup {
    pub(crate) instructions: std::ops::Range<u32>,
    pub(crate) pairs: Vec<Copy>,
}

/// 函数内单调递增的虚拟临时编号；溢出是内部不变量失败，绝不回绕复用。
#[derive(Clone, Copy, Debug)]
pub(crate) struct Temps {
    next: u32,
}

impl Temps {
    pub(crate) fn new(base: u32) -> Self {
        Self { next: base }
    }

    fn take(&mut self) -> Result<Reg, LoweringError> {
        let value = self.next;
        // 临时编号与 `StackAddr` 的 frame 占位编号必须留在同一上界之下。
        self.next = value
            .checked_add(1)
            .filter(|next| *next < FRAME_SLOT_BASE)
            .ok_or(LoweringError::InvalidOperands)?;
        Ok(Reg::Virtual(value))
    }
}

/// 一条终结符边的块参数拷贝；源与目标都还是虚拟寄存器。
///
/// 只返回**并行 pairs**：调度与渲染由调用方在站点序列里完成，分配阶段消费同一批 pairs。
pub(crate) fn edge_copies(body: &Body, edge: EdgeId) -> Result<Vec<Copy>, LoweringError> {
    let edge = &body.edges[edge.index()];
    let args = body.args(&edge.arguments);
    let params = body.params(edge.to);
    if args.len() != params.len() {
        return Err(LoweringError::InvalidOperands);
    }
    Ok(args
        .iter()
        .zip(params)
        .filter_map(|(argument, parameter)| {
            let src = Reg::Virtual(argument.0);
            let dest = Reg::Virtual(parameter.value.0);
            (src != dest).then_some(Copy {
                src,
                dest,
                ty: body.values[parameter.value.index()].kind.ty,
            })
        })
        .collect())
}

/// 把并行拷贝摊平成可顺序发射的 move 列表。
pub(crate) fn schedule(pairs: &[Copy], temps: &mut Temps) -> Result<Vec<Copy>, LoweringError> {
    let mut pending = pairs.to_vec();
    let mut schedule = Vec::with_capacity(pending.len());
    while !pending.is_empty() {
        let ready = pending
            .iter()
            .position(|candidate| !pending.iter().any(|other| other.src == candidate.dest));
        match ready {
            Some(index) => schedule.push(pending.remove(index)),
            None => {
                // 环：保存第一条边的目标值，让仍读它的源改读临时寄存器，再发射这条边。
                let saved = pending[0];
                let temp = temps.take()?;
                schedule.push(Copy {
                    src: saved.dest,
                    dest: temp,
                    ty: saved.ty,
                });
                for copy in &mut pending {
                    if copy.src == saved.dest {
                        copy.src = temp;
                    }
                }
                schedule.push(pending.remove(0));
            }
        }
    }
    Ok(schedule)
}
