//! `RawWorld` 上的 TurnRegion 接入：私有 bump 区、export summary 门禁与 `RegionTransfer`。
//!
//! 接入把参照实现（`runtime::region`）的判据连到真实 owner 事实，因此没有一条判据是摆设：
//!
//! 1. **region 容量进入账本**：`open` 时把整个容量 class commit 给 owner 并从 cache 扣除，
//!    即「这些字节已被 live 记录占用」；`reset` 时 `release` 归还它们，`promote` 时保留。
//! 2. **资源与 FFI 地址限制**：resource lease 与 pin/foreign 地址在 region 内出现时写观察位，
//!    门禁因此拒绝 reset 并转向 promote；越界本身进入 `RuntimeInvariant`。
//! 3. **transfer 走既有 producer 路径**：消息写入同一个 node pool、同一个 staging chain 与
//!    同一套 flush 触发器；接收方在 `service` 循环里按消息族分派，确认后两边的账本一起移动。
//! 4. **credit 观测**：在途 transfer 字节进入 credit 快照的 pending 项，因此「已移交但未采纳」
//!    的字节不会被任何一侧当成空闲。

use super::super::inbox::ShardIndex;
use super::super::message::{ProducerStaging, RegionTransferBatch, stage_region_transfer};
use super::super::region::{
    PromoteReason, RegionId, RegionPlane, RegionState, ResetOutcome, ResetRefusal,
};
use super::super::region_schema::{REGION_EXPORT_ALL, RegionExport, TurnRegionRuntimeContract};
use super::super::slab::RawInvariant;
use super::RawWorld;

/// 一次 region 结束动作的结果。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RegionEnd {
    /// 门禁通过，整区回收。
    Reset { bytes: u32, objects: u32 },
    /// summary 未闭合或门禁拒绝，整区保留。
    Promoted { bytes: u32, reason: PromoteReason },
}

/// `RegionTransfer` 接收侧的返回值。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RegionAdoption {
    /// 接收方 registry 内的新编号。
    pub(crate) region: RegionId,
    /// 采纳的 payload 字节数。
    pub(crate) bytes: u32,
    /// 来源 owner 身份。
    pub(crate) source: super::super::slab::OwnerId,
}

impl RawWorld {
    /// 用已构建的契约配置 region plane。
    ///
    /// 契约是容量阶梯、对象上界与状态名的唯一来源；没有配置时任何 region 入口都返回
    /// `RuntimeInvariant`，而不是悄悄用一个平行默认值。
    pub(crate) fn configure_regions(
        &mut self,
        contract: &TurnRegionRuntimeContract,
    ) -> Result<(), RawInvariant> {
        let owners: Vec<_> = (0..self.owners.len())
            .map(|index| self.owners[index].token())
            .collect();
        self.regions = Some(RegionPlane::new(&owners, contract));
        Ok(())
    }

    /// 返回 region plane；未配置时失败。
    pub(crate) fn region_plane(&self) -> Result<&RegionPlane, RawInvariant> {
        self.regions
            .as_ref()
            .ok_or_else(|| RawInvariant::new("region plane 未按契约配置"))
    }

    /// 返回 region plane（可变）；未配置时失败。
    fn region_plane_mut(&mut self) -> Result<&mut RegionPlane, RawInvariant> {
        self.regions
            .as_mut()
            .ok_or_else(|| RawInvariant::new("region plane 未按契约配置"))
    }

    /// 返回 owner 的活跃 region 数。
    pub(crate) fn region_active(&self, owner: u32) -> Result<u32, RawInvariant> {
        Ok(self.region_plane()?.active(owner))
    }

    /// 返回 owner 的 region 计数器。
    pub(crate) fn region_counters(
        &self,
        owner: u32,
    ) -> Result<super::super::region::RegionCounters, RawInvariant> {
        Ok(*self.region_plane()?.registry(owner).counters())
    }

    /// 返回在途 `RegionTransfer` 消息数。
    pub(crate) fn region_pending_transfers(&self) -> Result<usize, RawInvariant> {
        Ok(self.region_plane()?.pending())
    }

    /// 返回最早排队的一条 `RegionTransfer` 消息副本。
    pub(crate) fn region_pending_batch(&self) -> Result<RegionTransferBatch, RawInvariant> {
        self.region_plane()?
            .front()
            .ok_or_else(|| RawInvariant::new("没有在途 region transfer"))
    }

    /// 建立一个私有 region 并把容量 commit 给 owner。
    pub(crate) fn region_open(&mut self, owner: u32, bytes: u64) -> Result<RegionId, RawInvariant> {
        let owner_id = self.owners[owner as usize].token().owner_id;
        let (region, capacity) = {
            let plane = self.region_plane_mut()?;
            let region = plane.registry_mut(owner).open(bytes)?;
            let descriptor = plane.registry(owner).descriptor(region)?;
            (region, descriptor.capacity_bytes)
        };
        let accounting = self
            .directory
            .accounting_mut(owner_id)
            .ok_or_else(|| RawInvariant::new("region 缺少 owner 账本"))?;
        accounting.commit(u64::from(capacity));
        accounting.take_from_cache(u64::from(capacity));
        Ok(region)
    }

    /// 在一个 region 上 bump 出一个对象。
    pub(crate) fn region_bump(
        &mut self,
        owner: u32,
        region: RegionId,
        bytes: u32,
        objects: u32,
    ) -> Result<u32, RawInvariant> {
        self.region_plane_mut()?
            .registry_mut(owner)
            .bump(region, bytes, objects)
    }

    /// 登记 export summary。
    pub(crate) fn region_publish(
        &mut self,
        owner: u32,
        region: RegionId,
        export: u8,
    ) -> Result<(), RawInvariant> {
        if export & !REGION_EXPORT_ALL != 0 {
            return Err(RawInvariant::new("region export summary 含未登记位"));
        }
        self.region_plane_mut()?
            .registry_mut(owner)
            .publish(region, export)
    }

    /// 记录一个 owner-local 事实造成的 export summary 位。
    ///
    /// 这是「只有无外部 alias、无 resource lease、无 FFI 地址、无 pending transfer 且无 live
    /// root 时才 reset」的运行时入口：事实记下之后，同一次 publish 生命周期里的 reset 一定
    /// 被门禁拒绝，并转向 promote。
    pub(crate) fn region_confine(
        &mut self,
        owner: u32,
        region: RegionId,
        export: RegionExport,
    ) -> Result<(), RawInvariant> {
        self.region_plane_mut()?
            .registry_mut(owner)
            .observe(region, export.bit())
    }

    /// 清除一个 owner-local 事实位。
    pub(crate) fn region_release_confine(
        &mut self,
        owner: u32,
        region: RegionId,
        export: RegionExport,
    ) -> Result<(), RawInvariant> {
        self.region_plane_mut()?
            .registry_mut(owner)
            .clear_observation(region, export.bit())
    }

    /// 按门禁结束 region：闭合就 reset，未闭合或拒绝就 promote。
    pub(crate) fn region_end(
        &mut self,
        owner: u32,
        region: RegionId,
    ) -> Result<RegionEnd, RawInvariant> {
        let owner_id = self.owners[owner as usize].token().owner_id;
        let descriptor = self.region_plane()?.registry(owner).descriptor(region)?;
        // 编译器声明的 summary 未闭合时直接整区保留：这正是 lowering 里
        // `RegionPublish { export != 0 }` 后跟 `PromoteManaged` 的运行时对应动作。运行时在
        // publish 之后新观察到的位则不由这里短路，而是走门禁的拒绝路径。
        if descriptor.declared != 0 {
            let bytes = self
                .region_plane_mut()?
                .registry_mut(owner)
                .promote(region, PromoteReason::Summary)?;
            return Ok(RegionEnd::Promoted {
                bytes,
                reason: PromoteReason::Summary,
            });
        }
        let outcome = self.region_plane_mut()?.registry_mut(owner).reset(region)?;
        let (end, capacity) = match outcome {
            ResetOutcome::Reset { bytes, objects } => (
                RegionEnd::Reset { bytes, objects },
                descriptor.capacity_bytes,
            ),
            ResetOutcome::Refused(refusal) => {
                let bytes = self
                    .region_plane_mut()?
                    .registry_mut(owner)
                    .promote(region, PromoteReason::Refused(refusal))?;
                (
                    RegionEnd::Promoted {
                        bytes,
                        reason: PromoteReason::Refused(refusal),
                    },
                    0,
                )
            }
        };
        if capacity != 0 {
            let accounting = self
                .directory
                .accounting_mut(owner_id)
                .ok_or_else(|| RawInvariant::new("region 缺少 owner 账本"))?;
            accounting.release(u64::from(capacity));
        }
        Ok(end)
    }

    /// 把 region 的所有权移交给另一个 owner：发布消息并登记在途字节。
    pub(crate) fn region_transfer(
        &mut self,
        owner: u32,
        region: RegionId,
        target: u32,
        type_summary: u32,
    ) -> Result<(), RawInvariant> {
        let target_token = self.token(target);
        let secret = self.integrity_secret;
        let epoch = self.epoch.raw() as u64;
        let batch = self.region_plane_mut()?.registry_mut(owner).transfer(
            region,
            target_token,
            type_summary,
            epoch,
            &secret,
        )?;
        let inbox = self.inbox(target);
        let shard = ShardIndex::from_raw(target % super::super::OWNER_INBOX_SHARDS)
            .ok_or_else(|| RawInvariant::new("region transfer shard 越界"))?;
        let mut staging = ProducerStaging::new(self.limits);
        stage_region_transfer(&self.pool, Some(&inbox), &mut staging, &batch, shard, None)?;
        self.region_plane_mut()?.enqueue(batch)
    }

    /// 接收方采纳一条 `RegionTransfer`：两边的账本一起移动。
    pub(crate) fn service_region_transfer(
        &mut self,
        owner: u32,
        batch: &RegionTransferBatch,
    ) -> Result<RegionAdoption, RawInvariant> {
        let secret = self.integrity_secret;
        let (region, capacity_bytes) = {
            let plane = self.region_plane_mut()?;
            let region = plane.registry_mut(owner).receive(batch, &secret)?;
            let descriptor = plane.registry(owner).descriptor(region)?;
            (region, descriptor.capacity_bytes)
        };
        if self.region_plane()?.registry(owner).owner().owner_id != batch.target.owner_id {
            return Err(RawInvariant::new("region transfer 投递到错误 owner"));
        }
        let source_id = batch.source;
        let target_id = batch.target.owner_id;
        let bytes = u64::from(capacity_bytes);
        let source = self
            .directory
            .accounting_mut(source_id)
            .ok_or_else(|| RawInvariant::new("region transfer 缺少来源 owner 账本"))?;
        source.release(bytes);
        let target = self
            .directory
            .accounting_mut(target_id)
            .ok_or_else(|| RawInvariant::new("region transfer 缺少目标 owner 账本"))?;
        target.commit(bytes);
        target.take_from_cache(bytes);
        self.region_plane_mut()?.take(batch.target);
        Ok(RegionAdoption {
            region,
            bytes: batch.bytes,
            source: source_id,
        })
    }

    /// 发送方确认接收方已采纳：释放 transfer lease 与在途字节。
    pub(crate) fn region_confirm(
        &mut self,
        owner: u32,
        region: RegionId,
    ) -> Result<(), RawInvariant> {
        self.region_plane_mut()?.registry_mut(owner).confirm(region)
    }

    /// 接收方在自己的 turn 结束时回收已采纳的 region。
    pub(crate) fn region_receive_end(
        &mut self,
        owner: u32,
        region: RegionId,
    ) -> Result<RegionEnd, RawInvariant> {
        let owner_id = self.owners[owner as usize].token().owner_id;
        let descriptor = self.region_plane()?.registry(owner).descriptor(region)?;
        let outcome = self
            .region_plane_mut()?
            .registry_mut(owner)
            .receive_reset(region)?;
        let accounting = self
            .directory
            .accounting_mut(owner_id)
            .ok_or_else(|| RawInvariant::new("region 缺少 owner 账本"))?;
        accounting.release(u64::from(descriptor.capacity_bytes));
        let ResetOutcome::Reset { bytes, objects } = outcome else {
            return Err(RawInvariant::new("已采纳 region 只能整区回收"));
        };
        Ok(RegionEnd::Reset { bytes, objects })
    }

    /// 返回 owner 的全部 descriptor 状态名与 export summary，用于报告与 dump。
    pub(crate) fn region_states(
        &self,
        owner: u32,
    ) -> Result<Vec<(u32, &'static str, u8)>, RawInvariant> {
        let registry = self.region_plane()?.registry(owner);
        let mut states = Vec::new();
        for index in 0..registry.slots() {
            let region = RegionId(index);
            let Ok(descriptor) = registry.descriptor(region) else {
                continue;
            };
            states.push((index, descriptor.state.name(), descriptor.export()));
        }
        Ok(states)
    }

    /// 判定 region 是否仍处于可结束状态；测试与报告用它区分 `publishing` 与已结束。
    pub(crate) fn region_is_open(
        &self,
        owner: u32,
        region: RegionId,
    ) -> Result<bool, RawInvariant> {
        let descriptor = self.region_plane()?.registry(owner).descriptor(region)?;
        Ok(matches!(
            descriptor.state,
            RegionState::Private | RegionState::Publishing | RegionState::Received
        ))
    }

    /// 返回 `ResetRefusal` 的稳定名，用于报告。
    pub(crate) fn region_refusal_name(refusal: ResetRefusal) -> &'static str {
        refusal.name()
    }
}
