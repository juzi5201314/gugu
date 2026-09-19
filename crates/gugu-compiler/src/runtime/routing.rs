//! temporal radix fan-out 的确定性参照实现。
//!
//! `RoutingPlane` 是 `routing_schema` 契约的对偶：direct 模式不分配任何 bucket 表；
//! radix 模式把 return 族 batch 放进固定 `2^k` bucket 的有限层级 staging，按原始
//! target 的 route key bits 逐层下降，终层经 owner directory 解析后交付、转发或
//! 注入 domain。每一跳计入 `remote-return-hops` 并受固定上限约束；模式切换只在
//! maintenance 相位序列内发生，旧 topology 的在飞 batch 沿转发记录排空后才释放。
//!
//! 平面只做路由决策与账本；node 内容改写、integrity 重算与 inbox 发布由 world 执行
//! （见 `world/routing_impl.rs`），与既有 `forward_message` 语义一致。

use super::message::StagedChain;
use super::routing_schema::RouteMode;
use super::slab::{Epoch, OwnerToken, RawInvariant, RouteKey};

/// maintenance 相位；名字与契约的 `ROUTING_MAINTENANCE_PHASES` 目录一致。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MaintenancePhase {
    /// 无切换进行中；publish 与 route step 的默认态。
    Idle,
    /// direct → radix：冻结 target cache，拒绝新的 direct staging。
    FreezeTargetCache,
    /// direct → radix：冲刷全部 partial batch。
    FlushStaging,
    /// radix → direct：排空 bucket。
    DrainBuckets,
    /// radix → direct：排空转发记录。
    DrainForwarding,
    /// 发布新 mode；唯一允许翻转模式的相位。
    PublishMode,
    /// radix → direct：恢复 target cache。
    RestoreTargetCache,
}

impl MaintenancePhase {
    /// 返回登记目录中的相位名。
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::FreezeTargetCache => "freeze-target-cache",
            Self::FlushStaging => "flush-staging",
            Self::DrainBuckets => "drain-buckets",
            Self::DrainForwarding => "drain-forwarding",
            Self::PublishMode => "publish-mode",
            Self::RestoreTargetCache => "restore-target-cache",
        }
    }
}

/// route step 对原始 target 的解析结果；由 world 用 owner directory 求值。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RouteResolution {
    /// 目标 owner 匹配：batch 原样发布到该 owner 的 inbox。
    Deliver,
    /// 旧 epoch 转发：沿转发记录交给新 token。
    Forward(OwnerToken),
    /// owner 不可达或已 retire：进入 domain injection 终点。
    Inject(OwnerToken),
}

/// 一次 route step 交给 world 执行的动作。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RouteAction {
    /// 原样发布：chain 内 node 的 target 与 integrity 已经匹配 chain 的 target。
    Deliver(StagedChain),
    /// 终点改写：world 按 chain 的 target 重写 node 内容与 integrity 后发布，
    /// 旧 node 进入 grace。用于旧 epoch 转发与 domain injection。
    Retarget(StagedChain),
}

/// radix staging 中的一个在飞 batch。
#[derive(Clone, Debug, Eq, PartialEq)]
struct RoutingEntry {
    chain: StagedChain,
    hops: u32,
}

/// 已选定终点的转发记录；在下一次 route step 交付。
#[derive(Clone, Debug, Eq, PartialEq)]
struct ForwardRecord {
    chain: StagedChain,
    original: OwnerToken,
    hops: u32,
}

/// 路由统计；字段顺序与契约的 `ROUTING_STATISTICS` 目录一致。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct RoutingStats {
    /// 累计转发跳数。
    pub(crate) remote_return_hops: u64,
    /// 进入 radix staging 的 batch 数。
    pub(crate) radix_batches: u64,
    /// 完成的模式切换次数。
    pub(crate) maintenance_switches: u64,
    /// 沿旧 topology 转发记录排空的 batch 数。
    pub(crate) old_topology_drained_batches: u64,
    /// 进入 domain injection 的 batch 数。
    pub(crate) injection_fallbacks: u64,
}

/// temporal radix fan-out 的执行平面。
#[derive(Debug)]
pub(crate) struct RoutingPlane {
    mode: RouteMode,
    maintenance: MaintenancePhase,
    maintenance_epoch: u64,
    observed_topology_epoch: Epoch,
    level_count: u32,
    bucket_count: u32,
    bucket_log2: u32,
    hop_limit: u32,
    /// radix 模式的 bucket 表；`level * bucket_count + slot` 稠密索引，direct 模式为空。
    buckets: Vec<Vec<RoutingEntry>>,
    forwardings: Vec<ForwardRecord>,
    stats: RoutingStats,
}

impl RoutingPlane {
    /// 由契约参数构建平面；direct 模式不分配 bucket 表。
    pub(crate) fn new(
        mode: RouteMode,
        level_count: u32,
        bucket_count: u32,
        bucket_log2: u32,
        hop_limit: u32,
    ) -> Self {
        let capacity = match mode {
            RouteMode::Direct => 0,
            RouteMode::Radix => level_count as usize * bucket_count as usize,
        };
        Self {
            mode,
            maintenance: MaintenancePhase::Idle,
            maintenance_epoch: 0,
            observed_topology_epoch: Epoch::default(),
            level_count,
            bucket_count,
            bucket_log2,
            hop_limit,
            buckets: vec![Vec::new(); capacity],
            forwardings: Vec::new(),
            stats: RoutingStats::default(),
        }
    }

    /// 返回当前路由模式。
    pub(crate) const fn mode(&self) -> RouteMode {
        self.mode
    }

    /// 返回当前 maintenance 相位。
    pub(crate) const fn maintenance(&self) -> MaintenancePhase {
        self.maintenance
    }

    /// 返回 maintenance epoch。
    pub(crate) const fn maintenance_epoch(&self) -> u64 {
        self.maintenance_epoch
    }

    /// 返回 bucket 槽位总量；direct 模式为 0，即不分配 producer×owner 队列矩阵。
    pub(crate) fn bucket_capacity(&self) -> usize {
        self.buckets.len()
    }

    /// 返回统计快照。
    pub(crate) const fn stats(&self) -> RoutingStats {
        self.stats
    }

    /// 返回在飞转发记录数。
    pub(crate) fn forwarding_count(&self) -> usize {
        self.forwardings.len()
    }

    /// 返回 radix staging 中未交付的字节数；direct 模式恒为 0。
    pub(crate) fn pending_bytes(&self) -> u64 {
        self.buckets
            .iter()
            .flat_map(|bucket| bucket.iter())
            .map(|entry| entry.chain.bytes)
            .chain(self.forwardings.iter().map(|record| record.chain.bytes))
            .sum()
    }

    /// 返回 radix staging 中的在飞 batch 数。
    pub(crate) fn pending_batches(&self) -> u64 {
        let bucket_batches = self
            .buckets
            .iter()
            .map(|bucket| bucket.len() as u64)
            .sum::<u64>();
        bucket_batches + self.forwardings.len() as u64
    }

    /// 返回发布是否被允许；maintenance 期间两种 mode 不得消费同一个 queue head。
    pub(crate) const fn publish_allowed(&self) -> bool {
        matches!(self.maintenance, MaintenancePhase::Idle)
    }

    /// 记录观测到的 owner directory topology epoch。
    pub(crate) fn observe_topology(&mut self, epoch: Epoch) -> Result<(), RawInvariant> {
        if epoch < self.observed_topology_epoch {
            return Err(RawInvariant::new("routing 观测的 topology epoch 回退"));
        }
        self.observed_topology_epoch = epoch;
        Ok(())
    }

    /// 返回 route key 在指定层选择的 bucket 槽位。
    fn bucket_slot(&self, level: u32, route_key: RouteKey) -> usize {
        let shift = level * self.bucket_log2;
        let mask = self.bucket_count - 1;
        ((route_key.raw() >> shift) & u64::from(mask)) as usize
    }

    /// 把一个 closed chain 放进 radix staging 的 level 0 bucket。
    pub(crate) fn enqueue(&mut self, chain: StagedChain) -> Result<(), RawInvariant> {
        if self.mode != RouteMode::Radix {
            return Err(RawInvariant::new("direct 模式不进入 radix staging"));
        }
        if !self.publish_allowed() {
            return Err(RawInvariant::new(
                "routing maintenance 期间不接受新的 radix batch",
            ));
        }
        let target = chain
            .target
            .ok_or_else(|| RawInvariant::new("进入 radix staging 的 chain 必须携带原始 target"))?;
        let slot = self.bucket_slot(0, target.route_key);
        self.buckets[slot].push(RoutingEntry { chain, hops: 0 });
        self.stats.radix_batches += 1;
        Ok(())
    }

    /// 推进恰好一跳：终层 batch 按 resolver 解析交付/转发/注入，非终层 batch 下降一层。
    ///
    /// 处理顺序固定：先交付在飞转发记录，再按 level 降序、bucket 槽位升序处理 bucket 内
    /// 的 FIFO 队列——终层先离开系统，随后才发生降层移动，因此每个 batch 每次调用恰好
    /// 前进一跳，同一输入得到同一份动作序列与统计。
    pub(crate) fn route_step(
        &mut self,
        resolve: &mut dyn FnMut(OwnerToken) -> Result<RouteResolution, RawInvariant>,
    ) -> Result<Vec<RouteAction>, RawInvariant> {
        if self.mode != RouteMode::Radix {
            return Err(RawInvariant::new("radix 路由步只在 radix 模式有效"));
        }
        match self.maintenance {
            MaintenancePhase::Idle
            | MaintenancePhase::DrainBuckets
            | MaintenancePhase::DrainForwarding => {}
            _ => {
                return Err(RawInvariant::new(
                    "routing maintenance 相位不允许消费 radix 队头",
                ));
            }
        }
        let mut actions = Vec::new();
        // 先交付已选定终点的转发记录：旧 topology 的在飞 batch 沿旧记录排空后才释放。
        let forwardings = std::mem::take(&mut self.forwardings);
        for record in forwardings {
            if record.hops + 1 > self.hop_limit {
                return Err(RawInvariant::new("radix 转发 hop 超过固定上限"));
            }
            self.stats.remote_return_hops += 1;
            self.stats.old_topology_drained_batches += 1;
            actions.push(RouteAction::Retarget(record.chain));
        }
        // 终层先处理：解析结果离开 radix staging；随后非终层才下降一层。
        for level in (0..self.level_count).rev() {
            for slot in 0..self.bucket_count {
                let index = level as usize * self.bucket_count as usize + slot as usize;
                let entries = std::mem::take(&mut self.buckets[index]);
                for entry in entries {
                    if entry.hops + 1 > self.hop_limit {
                        return Err(RawInvariant::new("radix 路由 hop 超过固定上限"));
                    }
                    self.stats.remote_return_hops += 1;
                    let hops = entry.hops + 1;
                    let target = entry.chain.target.ok_or_else(|| {
                        RawInvariant::new("radix staging 中的 chain 丢失原始 target")
                    })?;
                    if level + 1 != self.level_count {
                        let next = self.bucket_slot(level + 1, target.route_key);
                        let index = (level + 1) as usize * self.bucket_count as usize + next;
                        self.buckets[index].push(RoutingEntry {
                            chain: entry.chain,
                            hops,
                        });
                        continue;
                    }
                    match resolve(target)? {
                        RouteResolution::Deliver => {
                            actions.push(RouteAction::Deliver(entry.chain));
                        }
                        RouteResolution::Forward(forward_target) => {
                            let mut chain = entry.chain;
                            chain.target = Some(forward_target);
                            self.forwardings.push(ForwardRecord {
                                chain,
                                original: target,
                                hops,
                            });
                        }
                        RouteResolution::Inject(injection) => {
                            self.stats.injection_fallbacks += 1;
                            let mut chain = entry.chain;
                            chain.target = Some(injection);
                            self.forwardings.push(ForwardRecord {
                                chain,
                                original: target,
                                hops,
                            });
                        }
                    }
                }
            }
        }
        Ok(actions)
    }

    /// 进入 maintenance：只在 idle 相位允许，且目标模式必须与当前不同。
    pub(crate) fn begin_maintenance(
        &mut self,
        target: RouteMode,
        epoch: u64,
    ) -> Result<(), RawInvariant> {
        if self.maintenance != MaintenancePhase::Idle {
            return Err(RawInvariant::new("routing maintenance 已在进行中"));
        }
        if target == self.mode {
            return Err(RawInvariant::new("routing 模式切换的目标与当前模式相同"));
        }
        self.maintenance = match (self.mode, target) {
            (RouteMode::Direct, RouteMode::Radix) => MaintenancePhase::FreezeTargetCache,
            (RouteMode::Radix, RouteMode::Direct) => MaintenancePhase::DrainBuckets,
            _ => return Err(RawInvariant::new("routing 模式切换方向未登记")),
        };
        self.maintenance_epoch = epoch;
        Ok(())
    }

    /// 推进 maintenance 相位；每一步的前置条件由相位自身定义。
    pub(crate) fn advance_maintenance(&mut self) -> Result<MaintenancePhase, RawInvariant> {
        let next = match (self.mode, self.maintenance) {
            (RouteMode::Direct, MaintenancePhase::FreezeTargetCache) => {
                MaintenancePhase::FlushStaging
            }
            (RouteMode::Direct, MaintenancePhase::FlushStaging) => {
                // publish-mode：翻转模式并分配 bucket 表；切换计数只在这里递增。
                self.mode = RouteMode::Radix;
                self.buckets =
                    vec![Vec::new(); self.level_count as usize * self.bucket_count as usize];
                self.stats.maintenance_switches += 1;
                self.maintenance_epoch += 1;
                self.maintenance = MaintenancePhase::Idle;
                return Ok(MaintenancePhase::Idle);
            }
            (RouteMode::Radix, MaintenancePhase::DrainBuckets) => {
                if self.pending_batches() > self.forwardings.len() as u64 {
                    return Err(RawInvariant::new("drain-buckets 相位要求 bucket 已排空"));
                }
                MaintenancePhase::DrainForwarding
            }
            (RouteMode::Radix, MaintenancePhase::DrainForwarding) => {
                if !self.forwardings.is_empty() {
                    return Err(RawInvariant::new("drain-forwarding 相位要求转发记录已排空"));
                }
                MaintenancePhase::PublishMode
            }
            (RouteMode::Radix, MaintenancePhase::PublishMode) => {
                // publish-mode：翻转模式并释放 bucket 表；target cache 由 world 恢复。
                self.mode = RouteMode::Direct;
                self.buckets = Vec::new();
                self.stats.maintenance_switches += 1;
                self.maintenance_epoch += 1;
                MaintenancePhase::RestoreTargetCache
            }
            (RouteMode::Direct, MaintenancePhase::RestoreTargetCache) => MaintenancePhase::Idle,
            (mode, phase) => {
                return Err(RawInvariant::new(format!(
                    "routing 相位 {phase:?} 与模式 {mode:?} 的组合没有登记的下一步"
                )));
            }
        };
        self.maintenance = next;
        Ok(next)
    }
}
