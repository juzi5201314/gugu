//! raw link provenance 与 release 安全 profile 的确定性参照实现。
//!
//! `ProvenancePlane` 是 provenance 契约段的对偶：
//!
//! - per-domain secret 由 world secret 经 blake3 派生键逐 domain 推导，全零 secret 视为
//!   初始化失败（进入 runtime fatal 路径），secret 只存在于 non-moving 平面；
//! - 每个 domain 持有独立的 `LinkCodec`，encoded link 因此是 domain 绑定的，跨 domain
//!   伪造 link 在 tag/checksum 层直接失败；
//! - 释放拒绝按契约登记的稳定分类记账，检查失败不静默丢弃消息；
//! - debug profile 在返还时写入 poison 标记、用标记识别重复返还、并在全链走查后计数；
//! - security profile 用独立 runtime seed 随机化 raw slot 的 reuse order，并提供
//!   ForeignBridge 接缝的 checked copy 与 guard region 检查；
//! - 随机复用、guard region 与 checked copy 都不改变 managed trace 语义，只改变 raw
//!   slot 的复用顺序与额外记账。

use super::message::{ChainWalkError, LinkCodec};
use super::provenance_schema::{
    PROVENANCE_REJECTIONS, PROVENANCE_STATISTICS, ProvenanceRuntimeContract, SafetyProfile,
};
use super::slab::{MemoryDomainId, RawInvariant, RuntimeSeed, SlabDescriptorId, SlabTable};

/// debug poison 的稳定标记字；ASCII "gugupois"。这是调试标记，不是安全 secret。
pub(crate) const POISON_WORD: u64 = 0x6775_6775_706F_6973;

/// security profile 随机复用的深度上界：每次弹出在前 `REUSE_DEPTH_LIMIT` 个 slot 中挑选，
/// 深度越过链长时取链尾；release/debug 固定为 0（LIFO）。
pub(crate) const REUSE_DEPTH_LIMIT: u32 = 8;

/// per-domain secret 的 blake3 派生键。
pub(crate) const DOMAIN_SECRET_DERIVE_KEY: &str = "gugu-provenance-domain-secret-v1";

/// 释放拒绝的稳定分类；顺序与契约的 `PROVENANCE_REJECTIONS` 一致。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ReleaseRejection {
    /// 链字被破坏或链上出现环。
    ChainCorruption,
    /// link 校验能过但归属、范围或状态不符，属于伪造。
    ForgedLink,
    /// 同一 slot 被返还多次。
    DoubleReturn,
    /// 消息投递到非目标 owner。
    CrossOwner,
    /// 释放请求的 bytes 与 class stride 不一致。
    CrossClass,
    /// 释放请求引用过期 generation。
    StaleGeneration,
    /// 释放或拷贝窗口与 guard region 重叠。
    GuardRegion,
}

impl ReleaseRejection {
    /// 返回登记目录中的分类名。
    pub(crate) const fn name(self) -> &'static str {
        PROVENANCE_REJECTIONS[self.index()]
    }

    /// 返回统计数组的下标。
    pub(crate) const fn index(self) -> usize {
        match self {
            Self::ChainCorruption => 0,
            Self::ForgedLink => 1,
            Self::DoubleReturn => 2,
            Self::CrossOwner => 3,
            Self::CrossClass => 4,
            Self::StaleGeneration => 5,
            Self::GuardRegion => 6,
        }
    }
}

/// 把 free 链走查失败映射到稳定拒绝分类。
pub(crate) fn classify_chain_error(error: &ChainWalkError) -> ReleaseRejection {
    match error {
        ChainWalkError::Link(super::message::LinkError::Null)
        | ChainWalkError::Link(super::message::LinkError::Checksum) => {
            ReleaseRejection::ChainCorruption
        }
        ChainWalkError::Link(super::message::LinkError::Foreign { .. })
        | ChainWalkError::Link(super::message::LinkError::OutOfRange { .. })
        | ChainWalkError::Link(super::message::LinkError::Alignment { .. }) => {
            ReleaseRejection::ForgedLink
        }
        ChainWalkError::Link(super::message::LinkError::Generation { .. }) => {
            ReleaseRejection::StaleGeneration
        }
        ChainWalkError::OutsideSpan => ReleaseRejection::ForgedLink,
        ChainWalkError::Overflow => ReleaseRejection::ChainCorruption,
    }
}

/// 由 world secret 与 domain 稠密编号派生 per-domain secret。
fn derive_domain_secret(world_secret: &[u8; 32], domain: MemoryDomainId) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new_derive_key(DOMAIN_SECRET_DERIVE_KEY);
    hasher.update(world_secret);
    hasher.update(&[domain.raw()]);
    *hasher.finalize().as_bytes()
}

/// 校验单个 per-domain secret：全零 secret 按初始化失败拒绝，不允许回落到公开常量。
fn validate_domain_secret(secret: &[u8; 32]) -> Result<(), RawInvariant> {
    if *secret == [0; 32] {
        return Err(RawInvariant::new(
            "per-domain secret 初始化失败：派生结果为全零",
        ));
    }
    Ok(())
}

/// 为全部登记 domain 派生 secret；任一派生结果为全零都按初始化失败拒绝。
fn derive_domain_secrets(world_secret: &[u8; 32]) -> Result<Vec<[u8; 32]>, RawInvariant> {
    let mut secrets = Vec::with_capacity(MemoryDomainId::ALL.len());
    for domain in MemoryDomainId::ALL {
        let secret = derive_domain_secret(world_secret, domain);
        validate_domain_secret(&secret)?;
        secrets.push(secret);
    }
    Ok(secrets)
}

/// checked copy 使用的受限缓冲区；只在 provenance 平面内登记与访问。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GuardedBuffer {
    /// 缓冲区的稳定基址（foreign descriptor 内偏移，不是 managed 裸地址）。
    base: u64,
    readable: bool,
    writable: bool,
    bytes: Vec<u8>,
}

impl GuardedBuffer {
    /// 创建一个受限缓冲区。
    pub(crate) fn new(base: u64, readable: bool, writable: bool, bytes: Vec<u8>) -> Self {
        Self {
            base,
            readable,
            writable,
            bytes,
        }
    }

    /// 返回缓冲区长度。
    pub(crate) const fn len(&self) -> u64 {
        self.bytes.len() as u64
    }

    /// 返回缓冲区内容快照。
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct GuardRegion {
    base: u64,
    bytes: u64,
}

impl GuardRegion {
    const fn overlaps(self, start: u64, end: u64) -> bool {
        let region_end = self.base.saturating_add(self.bytes);
        start < region_end && self.base < end
    }
}

/// raw link provenance 平面：per-domain secret、释放拒绝分类、debug poison 与 security
/// 随机复用的确定性对偶。
#[derive(Clone, Debug)]
pub(crate) struct ProvenancePlane {
    mode: SafetyProfile,
    secrets: Vec<[u8; 32]>,
    codecs: Vec<LinkCodec>,
    rng: RuntimeSeed,
    stats: [u64; PROVENANCE_STATISTICS.len()],
    rejections: [u64; PROVENANCE_REJECTIONS.len()],
    guard_regions: Vec<GuardRegion>,
}

impl ProvenancePlane {
    /// 由 world secret 构建平面；任一 per-domain secret 为全零都按初始化失败拒绝。
    pub(crate) fn new(
        world_secret: [u8; 32],
        mode: SafetyProfile,
        rng_seed: u64,
    ) -> Result<Self, RawInvariant> {
        let secrets = derive_domain_secrets(&world_secret)?;
        let mut codecs = Vec::with_capacity(secrets.len());
        for secret in &secrets {
            codecs.push(LinkCodec::new(*secret));
        }
        Ok(Self {
            mode,
            secrets,
            codecs,
            rng: RuntimeSeed::new(rng_seed),
            stats: [0; PROVENANCE_STATISTICS.len()],
            rejections: [0; PROVENANCE_REJECTIONS.len()],
            guard_regions: Vec::new(),
        })
    }

    /// 返回当前安全 profile。
    pub(crate) const fn mode(&self) -> SafetyProfile {
        self.mode
    }

    /// 按已验证契约段配置安全 profile；其余契约字段由编译侧持有，平面只消费 mode。
    pub(crate) fn configure(&mut self, contract: &ProvenanceRuntimeContract) {
        self.mode = contract.mode();
    }

    /// 返回某个 domain 的 secret；越界 domain 是不变量。
    pub(crate) fn secret_for(&self, domain: MemoryDomainId) -> &[u8; 32] {
        self.secrets
            .get(usize::from(domain.raw()))
            .expect("domain 稠密编号与登记目录一致")
    }

    /// 返回某个 domain 的 link 编码器。
    pub(crate) fn codec_for(&self, domain: MemoryDomainId) -> &LinkCodec {
        self.codecs
            .get(usize::from(domain.raw()))
            .expect("domain 稠密编号与登记目录一致")
    }

    /// 返回统计快照；顺序与契约的统计目录一致。
    pub(crate) const fn stats(&self) -> &[u64; PROVENANCE_STATISTICS.len()] {
        &self.stats
    }

    /// 返回某个拒绝分类的累计计数。
    pub(crate) const fn rejection_count(&self, rejection: ReleaseRejection) -> u64 {
        self.rejections[rejection.index()]
    }

    /// 记录一次释放拒绝：分类计数与 release-rejections 总计同时推进。
    pub(crate) fn record_rejection(&mut self, rejection: ReleaseRejection) {
        self.rejections[rejection.index()] += 1;
        self.stats[0] += 1;
    }

    /// debug profile：返还路径写入 poison 标记后记账。
    pub(crate) fn note_poison_write(&mut self) {
        self.stats[1] += 1;
    }

    /// debug profile：识别到「对已返还 slot 的再次返还」标记。
    pub(crate) fn note_double_return_marker(&mut self) {
        self.stats[2] += 1;
    }

    /// debug profile：完成一次全链走查。
    pub(crate) fn note_full_chain_verification(&mut self) {
        self.stats[3] += 1;
    }

    /// security profile：一次非零深度的随机弹出。
    pub(crate) fn note_randomized_pop(&mut self) {
        self.stats[4] += 1;
    }

    /// security profile：一次通过校验的 checked copy。
    pub(crate) fn note_checked_copy(&mut self) {
        self.stats[5] += 1;
    }

    /// 返回本次分配使用的 free list 弹出深度：security profile 用独立 seed 随机挑选，
    /// 其余 profile 固定 LIFO。
    pub(crate) fn next_reuse_depth(&mut self) -> u32 {
        if self.mode != SafetyProfile::Security {
            return 0;
        }
        let depth = u32::try_from(self.rng.next() % u64::from(REUSE_DEPTH_LIMIT))
            .expect("深度上界适配 u32");
        if depth > 0 {
            self.note_randomized_pop();
        }
        depth
    }

    /// debug profile：分配时校验 freed slot 的 poison 标记未被改写，然后清除。
    pub(crate) fn check_freed_slot_stamp(&mut self, stamp: u64) -> Result<(), RawInvariant> {
        if stamp != 0 && stamp != POISON_WORD {
            self.record_rejection(ReleaseRejection::ChainCorruption);
            return Err(RawInvariant::new(
                "freed slot 的 poison 标记被改写：slot 在 free 期间被写入",
            ));
        }
        Ok(())
    }

    /// 按 provenance 契约执行全链走查：失败先按稳定分类记账，再以原不变量文本返回。
    pub(crate) fn verify_full_chain(&mut self, table: &SlabTable) -> Result<(), RawInvariant> {
        for (index, descriptor) in table.descriptors().iter().enumerate() {
            if !descriptor.link_usable {
                continue;
            }
            let id = SlabDescriptorId::from_raw(u32::try_from(index).expect("描述符下标适配 u32"));
            if let Err(error) =
                table.verify_free_chain_checked(id, self.codec_for(descriptor.domain))
            {
                self.record_rejection(classify_chain_error(&error));
                return Err(RawInvariant::from(error));
            }
        }
        table.verify()?;
        if self.mode == SafetyProfile::Debug {
            self.note_full_chain_verification();
        }
        Ok(())
    }

    /// security profile：登记一段不可作为 payload 释放或拷贝的 guard region。
    pub(crate) fn register_guard_region(&mut self, base: u64, bytes: u64) {
        self.guard_regions.push(GuardRegion { base, bytes });
    }

    /// ForeignBridge 接缝的 checked copy：校验读/写权限、双方越界与 guard region 重叠，
    /// 全部通过后执行拷贝。任何失败都不产生部分写入。
    pub(crate) fn checked_copy(
        &mut self,
        source: &GuardedBuffer,
        target: &mut GuardedBuffer,
        source_offset: u64,
        target_offset: u64,
        bytes: u64,
    ) -> Result<(), RawInvariant> {
        if !source.readable {
            self.record_rejection(ReleaseRejection::GuardRegion);
            return Err(RawInvariant::new("checked copy 的源缓冲区不可读"));
        }
        if !target.writable {
            self.record_rejection(ReleaseRejection::GuardRegion);
            return Err(RawInvariant::new("checked copy 的目标缓冲区不可写"));
        }
        if source_offset.saturating_add(bytes) > source.len()
            || target_offset.saturating_add(bytes) > target.len()
        {
            self.record_rejection(ReleaseRejection::GuardRegion);
            return Err(RawInvariant::new("checked copy 越过缓冲区边界"));
        }
        let source_window = (
            source.base + source_offset,
            source.base + source_offset + bytes,
        );
        let target_window = (
            target.base + target_offset,
            target.base + target_offset + bytes,
        );
        for region in &self.guard_regions {
            if region.overlaps(source_window.0, source_window.1)
                || region.overlaps(target_window.0, target_window.1)
            {
                self.record_rejection(ReleaseRejection::GuardRegion);
                return Err(RawInvariant::new("checked copy 的窗口与 guard region 重叠"));
            }
        }
        let source_start = usize::try_from(source_offset).expect("偏移适配 usize");
        let target_start = usize::try_from(target_offset).expect("偏移适配 usize");
        let length = usize::try_from(bytes).expect("长度适配 usize");
        target.bytes[target_start..target_start + length]
            .copy_from_slice(&source.bytes[source_start..source_start + length]);
        self.note_checked_copy();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plane(mode: SafetyProfile) -> ProvenancePlane {
        ProvenancePlane::new([7; 32], mode, 0x1234_5678).expect("平面可构建")
    }

    #[test]
    fn domain_secrets_are_distinct_and_nonzero() {
        let plane = plane(SafetyProfile::Release);
        for domain in MemoryDomainId::ALL {
            assert_ne!(plane.secret_for(domain), &[0; 32]);
        }
        for (index, domain) in MemoryDomainId::ALL.iter().enumerate() {
            for other in &MemoryDomainId::ALL[index + 1..] {
                assert_ne!(plane.secret_for(*domain), plane.secret_for(*other));
            }
        }
    }

    #[test]
    fn zero_derived_secret_is_an_initialization_failure() {
        // 全零 secret 必须按 init-failure-fatal 规则拒绝，不允许回落到公开常量；
        // 真实 world secret（即使是全零种子）经 blake3 派生后仍产生非零且互异的 secret。
        assert!(validate_domain_secret(&[0; 32]).is_err());
        assert!(validate_domain_secret(&[1; 32]).is_ok());
        let plane = ProvenancePlane::new([0; 32], SafetyProfile::Release, 0)
            .expect("全零 world secret 的派生结果仍非全零");
        for domain in MemoryDomainId::ALL {
            assert_ne!(plane.secret_for(domain), &[0; 32]);
        }
    }

    #[test]
    fn same_seed_reproduces_reuse_depths_and_release_mode_stays_lifo() {
        let mut release = plane(SafetyProfile::Release);
        let mut security = plane(SafetyProfile::Security);
        for _ in 0..32 {
            assert_eq!(release.next_reuse_depth(), 0);
        }
        let first: Vec<u32> = (0..32).map(|_| security.next_reuse_depth()).collect();
        assert!(first.iter().any(|depth| *depth > 0));
        assert_eq!(
            security.stats()[4],
            first.iter().filter(|d| **d > 0).count() as u64
        );
        let mut replay = plane(SafetyProfile::Security);
        for depth in first {
            assert_eq!(replay.next_reuse_depth(), depth);
        }
    }

    #[test]
    fn checked_copy_enforces_permissions_bounds_and_guard_regions() {
        let mut plane = plane(SafetyProfile::Security);
        let source = GuardedBuffer::new(0x1000, true, false, vec![1, 2, 3, 4]);
        let mut target = GuardedBuffer::new(0x2000, false, true, vec![0; 4]);
        plane.checked_copy(&source, &mut target, 0, 0, 4).unwrap();
        assert_eq!(target.bytes(), &[1, 2, 3, 4]);
        assert_eq!(plane.stats()[5], 1);

        let mut readonly_target = GuardedBuffer::new(0x3000, false, false, vec![0; 4]);
        assert!(
            plane
                .checked_copy(&source, &mut readonly_target, 0, 0, 4)
                .is_err()
        );
        assert!(plane.checked_copy(&source, &mut target, 1, 0, 4).is_err());
        assert_eq!(target.bytes(), &[1, 2, 3, 4], "失败的拷贝不得产生部分写入");

        plane.register_guard_region(0x2000, 4);
        let mut overwrite = GuardedBuffer::new(0x2000, false, true, vec![0; 4]);
        assert!(
            plane
                .checked_copy(&source, &mut overwrite, 0, 0, 4)
                .is_err()
        );
        assert_eq!(plane.rejection_count(ReleaseRejection::GuardRegion), 3);
        assert_eq!(ReleaseRejection::GuardRegion.name(), "guard-region");
    }
}
