//! raw link provenance 与 release 安全 profile 的 world 接线。
//!
//! 平面在 `RawWorld::new` 中以 release profile 建立：per-domain secret 与释放拒绝分类
//! 总是启用，release 路径零额外成本。`configure_provenance` 按已验证契约段把安全
//! profile 对齐到 raw policy 的选择；debug 的 poison/全链 verifier 与 security 的随机
//! 复用/checked copy 只在对应 profile 激活，且都不改变 managed trace 语义。

use super::super::provenance::{GuardedBuffer, ProvenancePlane, ReleaseRejection};
use super::super::provenance_schema::{PROVENANCE_STATISTICS, ProvenanceRuntimeContract};
use super::super::slab::RawInvariant;
use super::RawWorld;

impl RawWorld {
    /// 按已验证的 provenance 契约段配置安全 profile；secret 与拒绝分类已在 `new` 建立。
    pub(crate) fn configure_provenance(&mut self, contract: &ProvenanceRuntimeContract) {
        self.provenance.configure(contract);
    }

    /// 返回 provenance 平面。
    pub(crate) fn provenance(&self) -> &ProvenancePlane {
        &self.provenance
    }

    /// 返回统计快照；顺序与契约的统计目录一致。
    pub(crate) fn provenance_stats(&self) -> &[u64; PROVENANCE_STATISTICS.len()] {
        self.provenance.stats()
    }

    /// 返回某个拒绝分类的累计计数。
    pub(crate) fn provenance_rejection_count(&self, rejection: ReleaseRejection) -> u64 {
        self.provenance.rejection_count(rejection)
    }

    /// security profile：登记一段不可作为 payload 释放或拷贝的 guard region。
    pub(crate) fn register_guard_region(&mut self, base: u64, bytes: u64) {
        self.provenance.register_guard_region(base, bytes);
    }

    /// ForeignBridge 接缝的 checked copy：校验权限、越界与 guard region 后执行拷贝。
    pub(crate) fn checked_copy(
        &mut self,
        source: &GuardedBuffer,
        target: &mut GuardedBuffer,
        source_offset: u64,
        target_offset: u64,
        bytes: u64,
    ) -> Result<(), RawInvariant> {
        self.provenance
            .checked_copy(source, target, source_offset, target_offset, bytes)
    }
}
