//! radix routing profile 的世界接线。
//!
//! 把 `RoutingPlane` 的路由决策接到真实世界事实上：发布路径按契约模式分流，
//! route step 的动作由 world 执行（node 改写、integrity 重算与 inbox 发布），
//! maintenance 相位序列由 world 完成每一步的真实工作（冲刷 staging、排空
//! bucket、恢复 target cache）。direct 模式的既有路径零改动。

use super::super::inbox::ShardIndex;
use super::super::message::{FlushTrigger, IntegrityTag, MessageState, StagedChain};
use super::super::model::RawModelError;
use super::super::routing::{
    MaintenancePhase, RouteAction, RouteResolution, RoutingPlane, RoutingStats,
};
use super::super::routing_schema::{
    MAX_RADIX_LEVELS, RADIX_BUCKET_LOG2, RADIX_BUCKETS, RADIX_HOP_LIMIT, RouteMode,
    RoutingRuntimeContract,
};
use super::super::slab::{OwnerToken, RawInvariant, Resolution};
use super::RawWorld;

/// 配置 routing 平面允许的最大迭代数；正常 radix 深度远小于该界。
const ROUTE_STEP_LIMIT: u32 = 4096;

impl RawWorld {
    /// 按契约配置 routing 平面；要求世界尚未产生任何在飞 return。
    pub(crate) fn configure_routing(
        &mut self,
        routing: &RoutingRuntimeContract,
    ) -> Result<(), RawModelError> {
        if self
            .return_stagings
            .iter()
            .any(|staging| staging.count() > 0)
        {
            return Err(RawModelError::new(
                "routing 平面配置前必须排空全部 return staging",
            ));
        }
        if self.routing.pending_batches() != 0 {
            return Err(RawModelError::new("routing 平面配置前不得存在在飞 batch"));
        }
        self.routing = RoutingPlane::new(
            routing.mode(),
            routing.max_levels(),
            routing.bucket_count(),
            routing.bucket_log2(),
            routing.hop_limit(),
        );
        Ok(())
    }

    /// 返回路由平面统计快照。
    pub(crate) fn routing_stats(&self) -> RoutingStats {
        self.routing.stats()
    }

    /// 返回 radix staging 与转发记录中的在飞字节数。
    pub(crate) fn routing_pending_bytes(&self) -> u64 {
        self.routing.pending_bytes()
    }

    /// 返回 radix staging 与转发记录中的在飞 batch 数。
    pub(crate) fn routing_pending_batches(&self) -> u64 {
        self.routing.pending_batches()
    }

    /// 返回当前 maintenance 相位。
    pub(crate) fn routing_maintenance(&self) -> MaintenancePhase {
        self.routing.maintenance()
    }

    /// 返回 bucket 槽位总量；direct 模式为 0。
    pub(crate) fn routing_bucket_capacity(&self) -> usize {
        self.routing.bucket_capacity()
    }

    /// radix 发布入口：把 producer staging 的已关闭 chain 放进 radix staging。
    ///
    /// 触发条件由调用方决定（item/byte 上限、forced 触发或 maintenance service）；
    /// batch 在 plane 内按原始 target 的 route key 逐层前进，不经 owner inbox。
    pub(crate) fn enqueue_routing_chain(&mut self, chain: StagedChain) -> Result<(), RawInvariant> {
        self.routing.enqueue(chain)
    }

    /// 推进一跳并执行全部动作；返回本次发布的 batch 数。
    fn route_once(&mut self) -> Result<u32, RawInvariant> {
        let actions = {
            let domain_owner = self.domain_owner;
            let directory = &self.directory;
            let mut resolve = |target: OwnerToken| match directory.resolve(&target) {
                Resolution::Match => Ok(RouteResolution::Deliver),
                Resolution::Forward(forward_target) => Ok(RouteResolution::Forward(forward_target)),
                Resolution::Retired => Ok(RouteResolution::Inject(domain_owner)),
                Resolution::Unknown => Err(RawInvariant::new(
                    "radix 终点解析遇到未知 owner 或伪造 route key",
                )),
            };
            self.routing.route_step(&mut resolve)?
        };
        let mut published = 0;
        for action in actions {
            match action {
                RouteAction::Deliver(chain) => {
                    self.publish_chain_direct(chain)?;
                    published += 1;
                }
                RouteAction::Retarget(chain) => {
                    self.publish_retargeted_chain(chain)?;
                    published += 1;
                }
            }
        }
        Ok(published)
    }

    /// 排空 radix staging：反复推进直到没有在飞 batch；迭代数有固定上界。
    pub(crate) fn drain_routing(&mut self) -> Result<u32, RawInvariant> {
        let mut published = 0;
        for _ in 0..ROUTE_STEP_LIMIT {
            if self.routing.pending_batches() == 0 {
                return Ok(published);
            }
            published += self.route_once()?;
        }
        Err(RawInvariant::new("radix 排空超过固定迭代上界"))
    }

    /// 把 chain 原样发布到其 target owner 的 inbox。
    fn publish_chain_direct(&self, chain: StagedChain) -> Result<(), RawInvariant> {
        let target = chain
            .target
            .ok_or_else(|| RawInvariant::new("radix 交付的 chain 缺少 target"))?;
        let inbox = self.inbox_for(&target)?;
        inbox.publish_batch(&chain, &self.pool)
    }

    /// 把转发/注入 chain 改写到新终点后发布：node 内容按新 target 重写、integrity
    /// 重算，旧 node 进入 grace。与 `forward_message` 的语义一致，只是整链执行。
    fn publish_retargeted_chain(&mut self, chain: StagedChain) -> Result<(), RawInvariant> {
        let target = chain
            .target
            .ok_or_else(|| RawInvariant::new("radix 转发 chain 缺少终点 token"))?;
        let slot = self.owner_slot(&target)?;
        let shard =
            ShardIndex::from_raw((slot % crate::runtime::OWNER_INBOX_SHARDS as usize) as u32)
                .ok_or_else(|| RawInvariant::new("转发 chain 的 shard 编号越界"))?;
        let mut first = None;
        let mut last = None;
        let mut count = 0_u32;
        let mut bytes = 0_u64;
        let mut cursor = Some(chain.first);
        while let Some(node) = cursor {
            cursor = self.pool.next(node);
            let mut message = self.load_return_message(node)?;
            message.target = target;
            message.state = MessageState::Forwarded;
            message.integrity.owner_id = target.owner_id;
            message.integrity.route_key = target.route_key;
            message.integrity.checksum = IntegrityTag::compute(&self.integrity_secret, &message);
            let rebuilt = self.pool.allocate()?;
            self.pool
                .store(rebuilt, &message, message.integrity.checksum);
            self.pool.link(rebuilt, None);
            if let Some(previous) = last {
                self.pool.link(previous, Some(rebuilt));
            }
            first = first.or(Some(rebuilt));
            last = Some(rebuilt);
            count += 1;
            bytes += u64::from(message.bytes);
            self.graced_nodes.push(node);
        }
        let Some(first) = first else {
            return Ok(());
        };
        let rebuilt_chain = StagedChain {
            first,
            last: last.expect("非空 chain 必有尾节点"),
            count,
            bytes,
            target: Some(target),
            shard: Some(shard),
        };
        self.publish_chain_direct(rebuilt_chain)
    }

    /// 切换路由模式：驱动完整 maintenance 相位序列并完成每一步的真实工作。
    ///
    /// 返回 maintenance epoch；切换期间发布被冻结，pending bytes 仍在 pressure
    /// 账本口径内可见。
    pub(crate) fn switch_routing_mode(&mut self, target: RouteMode) -> Result<u64, RawModelError> {
        let epoch = self.epoch.next().raw() as u64;
        self.routing
            .begin_maintenance(target, epoch)
            .map_err(|error| RawModelError::new(error.message().to_owned()))?;
        for _ in 0..ROUTE_STEP_LIMIT {
            match self.routing.maintenance() {
                MaintenancePhase::FreezeTargetCache | MaintenancePhase::RestoreTargetCache => {
                    // 冻结/恢复由 publish 门禁承担；相位本身没有额外的世界工作。
                    self.advance_routing_maintenance()?;
                }
                MaintenancePhase::FlushStaging => {
                    // 冲刷全部 partial batch：mode 仍是 direct，chain 直达目标 inbox。
                    for owner in 0..self.return_stagings.len() {
                        let owner = owner as u32;
                        self.flush_return_staging(owner, FlushTrigger::Maintenance)?;
                    }
                    self.advance_routing_maintenance()?;
                }
                MaintenancePhase::DrainBuckets | MaintenancePhase::DrainForwarding => {
                    self.drain_routing()
                        .map_err(|error| RawModelError::new(error.message().to_owned()))?;
                    self.advance_routing_maintenance()?;
                }
                MaintenancePhase::PublishMode => {
                    self.advance_routing_maintenance()?;
                }
                MaintenancePhase::Idle => return Ok(self.routing.maintenance_epoch()),
            }
        }
        Err(RawModelError::new("routing maintenance 超过固定相位上界"))
    }

    fn advance_routing_maintenance(&mut self) -> Result<(), RawModelError> {
        self.routing
            .advance_maintenance()
            .map(|_| ())
            .map_err(|error| RawModelError::new(error.message().to_owned()))
    }

    /// 记录 owner directory 的 topology epoch 变化；retire 路径在 epoch 前进后调用。
    pub(crate) fn observe_routing_topology(&mut self) -> Result<(), RawInvariant> {
        self.routing.observe_topology(self.directory.epoch())
    }
}

/// `RawWorld::new` 使用的默认平面装配；direct 模式不分配 bucket 表，
/// 结构参数与契约 verifier 固定的常量同源。
pub(crate) fn default_plane() -> RoutingPlane {
    RoutingPlane::new(
        RouteMode::Direct,
        MAX_RADIX_LEVELS,
        RADIX_BUCKETS,
        RADIX_BUCKET_LOG2,
        RADIX_HOP_LIMIT,
    )
}
