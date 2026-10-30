//! 固定 x64 probe/profile/form 表的有界成本上下文。
//!
//! `lower::lower` 的探针固定使用 x86_64 Linux 与当前二进制内的 form 表；
//! 不依赖正在编译的目标或会话。只缓存完整 Op 身份，不混用类型、符号或汇编上下文。
use crate::lir::body::Op;
use std::cell::RefCell;

const CAPACITY: usize = 256;

#[derive(Default)]
struct CostCache {
    // Op 域通常很小，但参数化变体无界；256 项环形替换保证内存有界，命中不编码或分配。
    entries: Vec<(Op, u32)>,
    next: usize,
}

impl CostCache {
    fn cost(&mut self, op: &Op, compute: impl FnOnce(&Op) -> u32) -> u32 {
        if let Some((_, cost)) = self.entries.iter().find(|(key, _)| key == op) {
            return *cost;
        }
        let cost = compute(op);
        debug_assert!(self.entries.len() <= CAPACITY && self.next < CAPACITY);
        if self.entries.len() < CAPACITY {
            self.entries.push((op.clone(), cost));
        } else {
            self.entries[self.next] = (op.clone(), cost);
            self.next = (self.next + 1) % CAPACITY;
        }
        cost
    }
}

thread_local! {
    // 编译线程间不共享可变上下文，避免每条指令取得全局 Mutex。
    static CACHE: RefCell<CostCache> = RefCell::new(CostCache::default());
}

pub(super) fn cost(op: &Op) -> u32 {
    // numeric::iconst 按这三个值域选择 mov form；fconst 的 F64 probe 不分值域。
    // 只合并已证明使用同一 form 序列的立即数，其余参数化 Op 保留完整身份。
    let canonical = match op {
        Op::IConst(value) => Some(Op::IConst(if *value <= u64::from(u32::MAX) {
            0
        } else if i32::try_from(value.cast_signed()).is_ok() {
            u64::MAX
        } else {
            1_u64 << 32
        })),
        Op::FConst(_) => Some(Op::FConst(0)),
        _ => None,
    };
    let key = canonical.as_ref().unwrap_or(op);
    CACHE.with_borrow_mut(|cache| cache.cost(key, super::poll_cost_uncached))
}

#[cfg(test)]
mod tests {
    use super::{CAPACITY, CostCache, cost};
    use crate::lir::body::{Conversion, IntOp, Op, VectorOp};
    use std::cell::Cell;

    #[test]
    fn repeated_probe_is_cached_and_parameterized_ops_stay_distinct() {
        let probes = Cell::new(0);
        let mut cache = CostCache::default();
        let probe = |op: &Op| {
            probes.set(probes.get() + 1);
            super::super::poll_cost_uncached(op)
        };
        for op in [
            Op::Integer(IntOp::DivUnsigned),
            Op::Convert(Conversion::IntToFloat { signed: false }),
            Op::Vector(VectorOp::Extract(1)),
            Op::Vector(VectorOp::Extract(2)),
        ] {
            let expected = probe(&op);
            let before = probes.get();
            assert_eq!(cache.cost(&op, probe), expected);
            assert_eq!(cache.cost(&op, probe), expected);
            assert_eq!(probes.get() - before, 1);
        }
    }

    #[test]
    fn eviction_preserves_exact_cost_and_capacity() {
        let mut cache = CostCache::default();
        for value in 0..=CAPACITY {
            let op = Op::IConst(u64::try_from(value).expect("测试编号适配 u64"));
            assert_eq!(
                cache.cost(&op, super::super::poll_cost_uncached),
                super::super::poll_cost_uncached(&op)
            );
        }
        assert_eq!(cache.entries.len(), CAPACITY);
        assert_eq!(
            cache.cost(&Op::IConst(0), super::super::poll_cost_uncached),
            super::super::poll_cost_uncached(&Op::IConst(0))
        );
    }

    #[test]
    fn constant_classes_and_unsupported_ops_match_original_probe() {
        for op in [
            Op::IConst(0),
            Op::IConst(u64::from(u32::MAX)),
            Op::IConst(1_u64 << 32),
            Op::IConst(u64::MAX),
            Op::FConst(0),
            Op::FConst(f64::NAN.to_bits()),
            Op::Vector(VectorOp::Extract(u8::MAX)),
            Op::Vector(VectorOp::Shuffle([255; 16])),
        ] {
            assert_eq!(cost(&op), super::super::poll_cost_uncached(&op));
        }
        assert_eq!(cost(&Op::Vector(VectorOp::Shuffle([255; 16]))), 1);
    }
}
