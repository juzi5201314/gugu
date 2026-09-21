//! 外部调用桥接契约：额度池、processor lease、错误捕获与线程边界。
//!
//! 这是确定性参照，不创建操作系统线程。普通 bridge、dirty 与 leaf 的调度差异都在这张
//! 状态机上可回放。等待中的调用不进入 native，opaque native 栈不记入栈图。外部线程不能
//! 改协程或 GC metadata。阻塞 poller 与普通 bridge 共用同一套 admission。

use serde::{Deserialize, Serialize};

use super::model::RawModelError;

/// 桥接契约段 schema。
pub(crate) const BRIDGE_SCHEMA: u32 = 1;
/// 同时执行阻塞外部调用的 worker 上限，也是 `BridgeCredit` 的容量。
pub(crate) const MAX_BLOCKING_WORKERS: u32 = 8;
/// dirty 调用可占用的执行槽。managed worker 至少保留一个，不在这里计数。
pub(crate) const DIRTY_SLOTS: u32 = 1;

/// 调用点可证明的外部效应。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BridgeMode {
    /// 取得额度后可以短暂保留 processor lease。
    Normal,
    /// 立即释放 processor，并占用一个 dirty 槽。
    Dirty,
    /// 留在当前 processor 上，不允许回调。
    Leaf,
}

/// 固定的桥接契约段。参数不随目标变化。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct BridgeContract {
    schema: u32,
    max_blocking: u32,
    dirty_slots: u32,
    fingerprint: [u8; 32],
}

impl BridgeContract {
    /// 返回当前登记的桥接契约。
    pub(crate) fn fixed() -> Self {
        let mut contract = Self {
            schema: BRIDGE_SCHEMA,
            max_blocking: MAX_BLOCKING_WORKERS,
            dirty_slots: DIRTY_SLOTS,
            fingerprint: [0; 32],
        };
        contract.fingerprint = fingerprint(&contract.canonical_bytes());
        contract
    }

    /// 校验 schema 与登记常量。
    pub(crate) fn verify(&self) -> Result<(), RawModelError> {
        let schema_ok = self.schema == BRIDGE_SCHEMA && self.max_blocking == MAX_BLOCKING_WORKERS;
        let slots_ok = self.dirty_slots == DIRTY_SLOTS;
        let fingerprint_ok = self.fingerprint == fingerprint(&self.canonical_bytes());
        if schema_ok && slots_ok && fingerprint_ok {
            Ok(())
        } else {
            Err(RawModelError::new("桥接契约 schema 不匹配"))
        }
    }

    /// 返回规范化字节。指纹字段不参与编码。
    pub(crate) fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&self.schema.to_le_bytes());
        bytes.extend_from_slice(&self.max_blocking.to_le_bytes());
        bytes.extend_from_slice(&self.dirty_slots.to_le_bytes());
        bytes.extend_from_slice(b"normal\0dirty\0leaf\0");
        bytes
    }

    /// 返回契约 schema。
    pub(crate) fn schema(&self) -> u32 {
        self.schema
    }

    /// 返回契约指纹。
    pub(crate) fn fingerprint(&self) -> [u8; 32] {
        self.fingerprint
    }

    /// 返回阻塞 worker 上限。
    pub(crate) fn max_blocking(&self) -> u32 {
        self.max_blocking
    }

    /// 返回 dirty 槽数。
    pub(crate) fn dirty_slots(&self) -> u32 {
        self.dirty_slots
    }

    /// 返回一份满额度的状态机。
    pub(crate) fn machine(&self) -> BridgeMachine {
        BridgeMachine {
            credit: self.max_blocking,
            dirty_free: self.dirty_slots,
            calls: Vec::new(),
            next_id: 0,
            waiting_normal: 0,
            waiting_dirty: 0,
            rejected_has_processor: false,
            external: false,
        }
    }

    /// 返回 dump 行。
    pub(crate) fn dump(&self) -> String {
        format!(
            "bridge schema={} blocking={} dirty={} fingerprint={}\n",
            self.schema,
            self.max_blocking,
            self.dirty_slots,
            hex(&self.fingerprint)
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Call {
    id: u32,
    mode: BridgeMode,
    processor: bool,
    lease: bool,
    roots: bool,
    captured: bool,
    errno: i32,
    last_error: u32,
    pinned: bool,
}

/// 进程内的桥接额度池。一次 `admit` 是一条调用，不是整池的互斥锁。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct BridgeMachine {
    credit: u32,
    dirty_free: u32,
    calls: Vec<Call>,
    next_id: u32,
    waiting_normal: u32,
    waiting_dirty: u32,
    rejected_has_processor: bool,
    external: bool,
}

impl BridgeMachine {
    /// 按模式进入外部调用。没有额度时进入等待，且不执行 native。
    ///
    /// 已有等待者时，新的同类调用排到队尾，不插到等待者前面。
    pub(crate) fn admit(&mut self, mode: BridgeMode) -> Result<u32, RawModelError> {
        match mode {
            BridgeMode::Leaf => Ok(self.start(mode)),
            BridgeMode::Dirty => self.admit_dirty(),
            BridgeMode::Normal => self.admit_normal(),
        }
    }

    /// 在额度归还后让队首等待者进入 native。没有等待者或仍无额度时失败。
    pub(crate) fn resume_waiter(&mut self, mode: BridgeMode) -> Result<u32, RawModelError> {
        match mode {
            BridgeMode::Normal => self.resume_normal(),
            BridgeMode::Dirty => self.resume_dirty(),
            BridgeMode::Leaf => Err(RawModelError::new("leaf 不进入等待队列")),
        }
    }

    /// 取回普通 bridge 的 processor lease。调用方转入不持有 processor 的状态。
    pub(crate) fn retake_lease(&mut self, id: u32) -> Result<(), RawModelError> {
        let call = self.call_mut(id)?;
        if call.mode != BridgeMode::Normal || !call.lease {
            return Err(RawModelError::new("没有可取回的 processor lease"));
        }
        call.lease = false;
        call.processor = false;
        Ok(())
    }

    /// native 返回后、重新入队前捕获 errno 与 Windows last-error。
    pub(crate) fn capture(
        &mut self,
        id: u32,
        errno: i32,
        last_error: u32,
    ) -> Result<(), RawModelError> {
        let call = self.call_mut(id)?;
        if call.captured {
            return Err(RawModelError::new("外部错误已经捕获"));
        }
        call.errno = errno;
        call.last_error = last_error;
        call.captured = true;
        Ok(())
    }

    /// 读取本次调用已经捕获的错误。未捕获时不能回读线程局部状态。
    pub(crate) fn read_error(&self, id: u32) -> Result<(i32, u32), RawModelError> {
        let call = self.call(id)?;
        if !call.captured {
            return Err(RawModelError::new("外部错误尚未捕获"));
        }
        Ok((call.errno, call.last_error))
    }

    /// 结束调用并把额度还给池。普通 bridge 与 dirty 必须已经捕获错误。
    pub(crate) fn finish(&mut self, id: u32) -> Result<(), RawModelError> {
        let mode = self.finish_mode(id)?;
        self.calls.retain(|call| call.id != id);
        match mode {
            BridgeMode::Normal => self.credit += 1,
            BridgeMode::Dirty => self.dirty_free += 1,
            BridgeMode::Leaf => {}
        }
        Ok(())
    }

    /// 普通 bridge 可以回调；dirty 与 leaf 不可以。
    pub(crate) fn callback(&self, id: u32) -> Result<(), RawModelError> {
        let call = self.call(id)?;
        if call.mode == BridgeMode::Normal && call.roots {
            Ok(())
        } else {
            Err(RawModelError::new("该外部模式不能回调 Gugu"))
        }
    }

    /// 把该调用暴露的对象钉住。
    pub(crate) fn pin(&mut self, id: u32) -> Result<(), RawModelError> {
        self.call_mut(id)?.pinned = true;
        Ok(())
    }

    /// 未 pin 的指针不能跨越外部调用。
    pub(crate) fn pass_pointer(&self, id: u32) -> Result<(), RawModelError> {
        if self.call(id)?.pinned {
            Ok(())
        } else {
            Err(RawModelError::new("外部指针缺少 pin"))
        }
    }

    /// 登记一条外部线程。登记后仍不能直接操作协程或 GC metadata。
    pub(crate) fn register_external(&mut self) {
        self.external = true;
    }

    /// 外部线程触碰协程或 GC metadata。
    pub(crate) fn touch_managed(&self) -> Result<(), RawModelError> {
        if self.external {
            Err(RawModelError::new("外部线程不能操作协程或 GC metadata"))
        } else {
            Ok(())
        }
    }

    /// native 栈是否被记入栈图。opaque native 必须为 false。
    pub(crate) fn native_stack_mapped(&self, id: u32) -> Result<bool, RawModelError> {
        let _ = self.call(id)?;
        Ok(false)
    }

    /// 展开是否扫描 native 栈。桥帧根在 Gugu 栈上，native 段没有栈图。
    pub(crate) fn scans_native_unwind(&self, id: u32) -> Result<bool, RawModelError> {
        let _ = self.call(id)?;
        Ok(false)
    }

    /// 该调用是否登记了桥帧上的精确根。
    pub(crate) fn bridge_roots(&self, id: u32) -> Result<bool, RawModelError> {
        Ok(self.call(id)?.roots)
    }

    /// 该调用当前是否持有 processor。
    pub(crate) fn holds_processor(&self, id: u32) -> Result<bool, RawModelError> {
        Ok(self.call(id)?.processor)
    }

    /// 最近一次被拒绝的 admission 是否持有 processor。
    pub(crate) fn rejected_holds_processor(&self) -> bool {
        self.rejected_has_processor
    }

    /// 是否有尚未进入 native 的额度等待。
    pub(crate) fn is_waiting(&self) -> bool {
        self.waiting_normal > 0 || self.waiting_dirty > 0
    }

    /// 已经进入 native 的调用数。等待者不计入。
    pub(crate) fn active_native(&self) -> usize {
        self.calls.len()
    }

    fn admit_normal(&mut self) -> Result<u32, RawModelError> {
        if self.waiting_normal > 0 || self.credit == 0 {
            self.waiting_normal += 1;
            self.rejected_has_processor = false;
            return Err(RawModelError::new("BridgeCredit 不足"));
        }
        self.credit -= 1;
        Ok(self.start(BridgeMode::Normal))
    }

    fn admit_dirty(&mut self) -> Result<u32, RawModelError> {
        if self.waiting_dirty > 0 || self.dirty_free == 0 {
            self.waiting_dirty += 1;
            self.rejected_has_processor = false;
            return Err(RawModelError::new("dirty 额度不足"));
        }
        self.dirty_free -= 1;
        Ok(self.start(BridgeMode::Dirty))
    }

    fn resume_normal(&mut self) -> Result<u32, RawModelError> {
        if self.waiting_normal == 0 {
            return Err(RawModelError::new("没有等待中的普通 bridge"));
        }
        if self.credit == 0 {
            return Err(RawModelError::new("BridgeCredit 不足"));
        }
        self.waiting_normal -= 1;
        self.credit -= 1;
        Ok(self.start(BridgeMode::Normal))
    }

    fn resume_dirty(&mut self) -> Result<u32, RawModelError> {
        if self.waiting_dirty == 0 {
            return Err(RawModelError::new("没有等待中的 dirty 调用"));
        }
        if self.dirty_free == 0 {
            return Err(RawModelError::new("dirty 额度不足"));
        }
        self.waiting_dirty -= 1;
        self.dirty_free -= 1;
        Ok(self.start(BridgeMode::Dirty))
    }

    fn start(&mut self, mode: BridgeMode) -> u32 {
        self.next_id += 1;
        let (processor, lease, roots) = match mode {
            BridgeMode::Leaf => (true, false, false),
            BridgeMode::Dirty => (false, false, true),
            BridgeMode::Normal => (true, true, true),
        };
        let id = self.next_id;
        self.calls.push(Call {
            id,
            mode,
            processor,
            lease,
            roots,
            captured: false,
            errno: 0,
            last_error: 0,
            pinned: false,
        });
        id
    }

    fn finish_mode(&mut self, id: u32) -> Result<BridgeMode, RawModelError> {
        let call = self.call(id)?;
        if call.mode != BridgeMode::Leaf && !call.captured {
            return Err(RawModelError::new("外部错误尚未捕获"));
        }
        Ok(call.mode)
    }

    fn call(&self, id: u32) -> Result<&Call, RawModelError> {
        self.calls
            .iter()
            .find(|call| call.id == id)
            .ok_or_else(|| RawModelError::new("没有进行中的外部调用"))
    }

    fn call_mut(&mut self, id: u32) -> Result<&mut Call, RawModelError> {
        self.calls
            .iter_mut()
            .find(|call| call.id == id)
            .ok_or_else(|| RawModelError::new("没有进行中的外部调用"))
    }
}

fn fingerprint(bytes: &[u8]) -> [u8; 32] {
    *blake3::Hasher::new_derive_key("gugu-bridge-v1")
        .update(bytes)
        .finalize()
        .as_bytes()
}

fn hex(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::{BRIDGE_SCHEMA, BridgeContract, BridgeMode, DIRTY_SLOTS, MAX_BLOCKING_WORKERS};

    #[test]
    fn contract_fingerprint_is_stable() {
        let first = BridgeContract::fixed();
        let second = BridgeContract::fixed();
        assert_eq!(first.schema(), BRIDGE_SCHEMA);
        assert_eq!(first.max_blocking(), MAX_BLOCKING_WORKERS);
        assert_eq!(first.dirty_slots(), DIRTY_SLOTS);
        assert_eq!(first.canonical_bytes(), second.canonical_bytes());
        assert_eq!(first.fingerprint(), second.fingerprint());
        first.verify().expect("契约自洽");
        assert!(first.dump().contains("bridge schema=1"));
    }

    #[test]
    fn leaf_keeps_processor_and_forbids_callback() {
        let mut machine = BridgeContract::fixed().machine();
        let id = machine.admit(BridgeMode::Leaf).expect("leaf");
        assert!(machine.holds_processor(id).expect("processor"));
        assert!(!machine.bridge_roots(id).expect("roots"));
        assert!(machine.callback(id).is_err());
        assert!(!machine.native_stack_mapped(id).expect("map"));
        assert!(!machine.scans_native_unwind(id).expect("unwind"));
        assert!(machine.retake_lease(id).is_err());
        machine.finish(id).expect("leaf 结束不要求捕获");
        assert_eq!(machine.active_native(), 0);
    }

    #[test]
    fn dirty_second_call_waits_without_native() {
        let mut machine = BridgeContract::fixed().machine();
        let id = machine.admit(BridgeMode::Dirty).expect("dirty");
        assert!(!machine.holds_processor(id).expect("processor"));
        assert!(machine.bridge_roots(id).expect("roots"));
        assert!(machine.callback(id).is_err());
        assert!(!machine.native_stack_mapped(id).expect("map"));
        let rejected = machine.admit(BridgeMode::Dirty);
        assert!(rejected.is_err());
        assert!(machine.is_waiting());
        assert!(!machine.rejected_holds_processor());
        assert_eq!(machine.active_native(), 1);
    }

    #[test]
    fn normal_ninth_waits_until_credit_returns() {
        let mut machine = BridgeContract::fixed().machine();
        let mut ids = Vec::new();
        for _ in 0..MAX_BLOCKING_WORKERS {
            let id = machine.admit(BridgeMode::Normal).expect("credit");
            assert!(machine.holds_processor(id).expect("processor"));
            assert!(machine.callback(id).is_ok());
            ids.push(id);
        }
        assert!(machine.admit(BridgeMode::Normal).is_err());
        assert!(machine.is_waiting());
        assert!(!machine.rejected_holds_processor());
        assert_eq!(machine.active_native(), 8);
        let first = ids[0];
        assert!(machine.finish(first).is_err());
        machine.capture(first, 2, 5).expect("capture");
        assert_eq!(machine.read_error(first).expect("error"), (2, 5));
        machine.retake_lease(first).expect("retake");
        assert!(!machine.holds_processor(first).expect("processor"));
        machine.finish(first).expect("finish");
        assert!(machine.is_waiting());
        let resumed = machine.resume_waiter(BridgeMode::Normal).expect("resume");
        assert!(machine.holds_processor(resumed).expect("processor"));
        assert!(!machine.is_waiting());
        assert_eq!(machine.active_native(), 8);
    }

    #[test]
    fn error_stays_unreadable_until_capture() {
        let mut machine = BridgeContract::fixed().machine();
        let id = machine.admit(BridgeMode::Normal).expect("bridge");
        assert!(machine.read_error(id).is_err());
        machine.capture(id, 1, 0).expect("errno");
        assert!(machine.capture(id, 2, 0).is_err());
        assert_eq!(machine.read_error(id).expect("saved"), (1, 0));
    }

    #[test]
    fn pointer_requires_pin() {
        let mut machine = BridgeContract::fixed().machine();
        let id = machine.admit(BridgeMode::Normal).expect("bridge");
        assert!(machine.pass_pointer(id).is_err());
        machine.pin(id).expect("pin");
        machine.pass_pointer(id).expect("pinned");
    }

    #[test]
    fn external_thread_cannot_touch_managed_state() {
        let mut machine = BridgeContract::fixed().machine();
        machine.touch_managed().expect("登记前是 managed worker");
        machine.register_external();
        assert!(machine.touch_managed().is_err());
    }
}
