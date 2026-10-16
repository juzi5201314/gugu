//! 内存账本的分类与分区契约。
//!
//! `RuntimeStats` 里的字节口径必须互不重复：物理口径按 `runtime_committed_bytes` 分区，
//! 虚拟口径按 `range_reserved_bytes` 分区，两个口径本身互斥——一个 range 要么已提交物理页，
//! 要么还停在预留/已 decommit 的虚拟状态，不可能同时计入两边。
//!
//! 每个分区里恰好有一个兜底成员（`residual`），其余成员由独立的 owner 计数器累加；兜底成员
//! 只能由「总量减去其余成员」得到。该结构使重复计数在契约校验阶段就被拒绝，而不是等运行期
//! 统计对不上才发现。

use serde::{Deserialize, Serialize};

use super::model::RawModelError;

/// 账本契约段的 schema 版本。
pub(crate) const LEDGER_SCHEMA: u32 = 1;

/// 物理分区：`runtime_committed_bytes` 的组成。
pub(crate) const LEDGER_PARTITION_COMMITTED: &str = "runtime-committed-bytes";
/// 虚拟分区：`range_reserved_bytes` 的组成，与物理分区互斥。
pub(crate) const LEDGER_PARTITION_RESERVED: &str = "address-space-reserved-bytes";

/// 一个账本分区的口径。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum LedgerPlane {
    /// 已提交的物理页。
    Physical,
    /// 已预留但未提交的虚拟地址空间。
    Virtual,
}

impl LedgerPlane {
    /// 返回口径名。
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Physical => "physical",
            Self::Virtual => "virtual",
        }
    }
}

/// 一个账本分类。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct LedgerCategoryV1 {
    /// 分类名；`dump` 与追踪事件使用它。
    pub(crate) name: String,
    /// 所属分区。
    pub(crate) partition: String,
    /// 对应的 `RuntimeStats` 字段名。
    pub(crate) counter: String,
    /// 是否为该分区的兜底成员；每个分区恰好一个。
    pub(crate) residual: bool,
}

/// 一个账本分区。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct LedgerPartitionV1 {
    pub(crate) name: String,
    pub(crate) plane: LedgerPlane,
    /// 分区总量的 `RuntimeStats` 字段名。
    pub(crate) total: String,
    /// 按固定顺序排列的成员分类名。
    pub(crate) categories: Vec<String>,
}

/// 内存账本契约段。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct LedgerSchemaV1 {
    pub(crate) schema: u32,
    pub(crate) partitions: Vec<LedgerPartitionV1>,
    pub(crate) categories: Vec<LedgerCategoryV1>,
}

impl LedgerSchemaV1 {
    /// 返回固定的账本分类结构。
    ///
    /// 分类名按字典序排列，便于稳定编码与点查；分区成员表另按「独立计数器在前、兜底成员在
    /// 末位」排列，与 `docs/src/spec/runtime.md` 的 `RuntimeStats` 说明一致。
    pub(crate) fn fixed() -> Self {
        let committed = [
            ("pending-return-bytes", "pending_return_bytes"),
            ("reclaimable-bytes", "reclaimable_bytes"),
            ("owner-cache-bytes", "owner_cache_bytes"),
        ];
        let mut categories: Vec<LedgerCategoryV1> = committed
            .into_iter()
            .map(|(name, counter)| LedgerCategoryV1 {
                name: name.to_owned(),
                partition: LEDGER_PARTITION_COMMITTED.to_owned(),
                counter: counter.to_owned(),
                residual: false,
            })
            .collect();
        categories.push(LedgerCategoryV1 {
            name: "live-bytes".to_owned(),
            partition: LEDGER_PARTITION_COMMITTED.to_owned(),
            counter: "runtime_committed_bytes - pending_return_bytes - reclaimable_bytes - owner_cache_bytes"
                .to_owned(),
            residual: true,
        });
        categories.push(LedgerCategoryV1 {
            name: "reserved-bytes".to_owned(),
            partition: LEDGER_PARTITION_RESERVED.to_owned(),
            counter: "range_reserved_bytes".to_owned(),
            residual: true,
        });
        categories.sort_by(|left, right| left.name.cmp(&right.name));
        Self {
            schema: LEDGER_SCHEMA,
            partitions: vec![
                LedgerPartitionV1 {
                    name: LEDGER_PARTITION_RESERVED.to_owned(),
                    plane: LedgerPlane::Virtual,
                    total: "range_reserved_bytes".to_owned(),
                    categories: vec!["reserved-bytes".to_owned()],
                },
                LedgerPartitionV1 {
                    name: LEDGER_PARTITION_COMMITTED.to_owned(),
                    plane: LedgerPlane::Physical,
                    total: "runtime_committed_bytes".to_owned(),
                    categories: vec![
                        "pending-return-bytes".to_owned(),
                        "reclaimable-bytes".to_owned(),
                        "owner-cache-bytes".to_owned(),
                        "live-bytes".to_owned(),
                    ],
                },
            ],
            categories,
        }
    }

    /// 校验分区结构：名字唯一、成员恰好归属一个分区、每个分区恰好一个兜底成员且在末位、
    /// 物理与虚拟口径各自有独立的总量字段。
    pub(crate) fn verify(&self) -> Result<(), RawModelError> {
        if self.schema != LEDGER_SCHEMA {
            return Err(RawModelError::new("账本契约 schema 版本不匹配"));
        }
        if self.partitions.is_empty() || self.categories.is_empty() {
            return Err(RawModelError::new("账本分区与分类都不能为空"));
        }
        for pair in self.partitions.windows(2) {
            if pair[0].name >= pair[1].name {
                return Err(RawModelError::new("账本分区没有按名字稳定排序"));
            }
        }
        for pair in self.categories.windows(2) {
            if pair[0].name >= pair[1].name {
                return Err(RawModelError::new("账本分类没有按名字稳定排序"));
            }
        }
        for category in &self.categories {
            if category.counter.is_empty() {
                return Err(RawModelError::new("账本分类缺少对应的统计字段"));
            }
            let matches = self
                .partitions
                .iter()
                .filter(|partition| partition.name == category.partition)
                .count();
            if matches != 1 {
                return Err(RawModelError::new(format!(
                    "账本分类 `{}` 必须恰好归属一个分区",
                    category.name
                )));
            }
        }
        for partition in &self.partitions {
            if partition.total.is_empty() {
                return Err(RawModelError::new("账本分区缺少总量字段"));
            }
            if partition.categories.is_empty() {
                return Err(RawModelError::new(format!(
                    "账本分区 `{}` 没有任何成员",
                    partition.name
                )));
            }
            let members: Vec<&LedgerCategoryV1> = partition
                .categories
                .iter()
                .map(|name| {
                    self.categories
                        .iter()
                        .find(|category| &category.name == name)
                        .ok_or_else(|| {
                            RawModelError::new(format!(
                                "账本分区 `{}` 引用了未登记的分类 `{name}`",
                                partition.name
                            ))
                        })
                })
                .collect::<Result<_, _>>()?;
            let residuals: Vec<&&LedgerCategoryV1> = members
                .iter()
                .filter(|category| category.residual)
                .collect();
            if residuals.len() != 1 {
                return Err(RawModelError::new(format!(
                    "账本分区 `{}` 必须恰好一个兜底成员",
                    partition.name
                )));
            }
            if !members.last().is_some_and(|category| category.residual) {
                return Err(RawModelError::new(format!(
                    "账本分区 `{}` 的兜底成员必须排在末位",
                    partition.name
                )));
            }
        }
        for category in &self.categories {
            let partition = self
                .partitions
                .iter()
                .find(|partition| partition.name == category.partition)
                .expect("成员归属已校验");
            if !partition.categories.contains(&category.name) {
                return Err(RawModelError::new(format!(
                    "账本分类 `{}` 未登记在所属分区的成员表中",
                    category.name
                )));
            }
        }
        for partition in &self.partitions {
            let claimants = self
                .categories
                .iter()
                .filter(|category| category.partition == partition.name)
                .count();
            if claimants != partition.categories.len() {
                return Err(RawModelError::new(format!(
                    "账本分区 `{}` 的成员表与分类归属不一致",
                    partition.name
                )));
            }
        }
        let physical = self
            .partitions
            .iter()
            .filter(|partition| partition.plane == LedgerPlane::Physical)
            .count();
        let address_space = self
            .partitions
            .iter()
            .filter(|partition| partition.plane == LedgerPlane::Virtual)
            .count();
        if physical != 1 || address_space != 1 {
            return Err(RawModelError::new(
                "账本必须恰好有一个物理分区与一个虚拟分区",
            ));
        }
        if !self.partitions.iter().any(|partition| {
            partition.plane == LedgerPlane::Physical
                && partition.name == LEDGER_PARTITION_COMMITTED
                && partition.total == "runtime_committed_bytes"
        }) {
            return Err(RawModelError::new("物理分区必须是 runtime_committed_bytes"));
        }
        if !self.partitions.iter().any(|partition| {
            partition.plane == LedgerPlane::Virtual
                && partition.name == LEDGER_PARTITION_RESERVED
                && partition.total == "range_reserved_bytes"
        }) {
            return Err(RawModelError::new("虚拟分区必须是 range_reserved_bytes"));
        }
        if self
            .categories
            .iter()
            .filter(|category| category.partition == LEDGER_PARTITION_RESERVED)
            .count()
            != 1
        {
            return Err(RawModelError::new(
                "虚拟分区只登记未提交的预留字节，不能有第二个成员",
            ));
        }
        Ok(())
    }

    /// 返回按固定顺序排列的分类名。
    pub(crate) fn names(&self) -> Vec<String> {
        self.categories
            .iter()
            .map(|category| category.name.clone())
            .collect()
    }

    /// 返回某个分区的成员分类名。
    pub(crate) fn members(&self, partition: &str) -> &[String] {
        self.partitions
            .iter()
            .find(|candidate| candidate.name == partition)
            .map(|candidate| candidate.categories.as_slice())
            .expect("分区名由契约校验保证存在")
    }

    /// 返回规范编码。
    pub(crate) fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&self.schema.to_le_bytes());
        bytes.extend_from_slice(&(self.partitions.len() as u32).to_le_bytes());
        for partition in &self.partitions {
            bytes.extend_from_slice(partition.name.as_bytes());
            bytes.push(0);
            bytes.push(match partition.plane {
                LedgerPlane::Physical => 0,
                LedgerPlane::Virtual => 1,
            });
            bytes.extend_from_slice(partition.total.as_bytes());
            bytes.push(0);
            bytes.extend_from_slice(&(partition.categories.len() as u32).to_le_bytes());
            for name in &partition.categories {
                bytes.extend_from_slice(name.as_bytes());
                bytes.push(0);
            }
        }
        bytes.extend_from_slice(&(self.categories.len() as u32).to_le_bytes());
        for category in &self.categories {
            bytes.extend_from_slice(category.name.as_bytes());
            bytes.push(0);
            bytes.extend_from_slice(category.partition.as_bytes());
            bytes.push(0);
            bytes.extend_from_slice(category.counter.as_bytes());
            bytes.push(0);
            bytes.push(u8::from(category.residual));
        }
        bytes
    }
}
