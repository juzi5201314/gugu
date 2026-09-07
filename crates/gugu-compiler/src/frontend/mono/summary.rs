//! `PublicFunctionSummary`（query 28）：按 `MonoKey` 投影跨 package 公共摘要。
//!
//! payload 只含可消费语义内容：接口 place 限定参数序号/返回值/公开 static 稳定键，
//! 私有状态折叠为 hidden-state 标志；对象 key 由内容摘要产生，body 变化但摘要
//! 不变时保持同一对象 key（red/green）。

use super::keys::{MonoKind, hash_domain};
use crate::frontend::analysis::{
    ANALYSIS_SEMANTICS_REVISION, AnalysisWorldV1, FunctionSummary, ReturnRelation,
};
use crate::query::{DependencyFingerprint, QueryEngine, QueryKey, QueryKind};
use crate::target::TargetName;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// `PublicFunctionSummaryV1` schema。
pub(crate) const PUBLIC_SCHEMA: u32 = 2;
/// 公共摘要策略 revision（`PublicSummaryPolicyV1`）。
pub(crate) const PUBLIC_POLICY_REVISION: u32 = 2;

/// 无条件效果位集合；未知 bit 必须被 verifier 拒绝。
pub(crate) const EFFECT_MAY_ALLOCATE: u16 = 1;
pub(crate) const EFFECT_MAY_PANIC: u16 = 2;
pub(crate) const EFFECT_MAY_SUSPEND: u16 = 4;
pub(crate) const EFFECT_MAY_CALL_UNKNOWN: u16 = 8;
pub(crate) const EFFECT_MAY_MUTATE_LEN: u16 = 16;
pub(crate) const EFFECT_ALIAS_HEAP: u16 = 32;
pub(crate) const EFFECT_ALIAS_FOREIGN: u16 = 64;
pub(crate) const EFFECT_READS_HIDDEN: u16 = 128;
pub(crate) const EFFECT_WRITES_HIDDEN: u16 = 256;
pub(crate) const KNOWN_EFFECTS: u16 = 511;

/// 条件事实：返回值长度等于参数长度。
pub(crate) const FACT_RETURN_LEN_EQ_PARAM: u16 = 1;

/// 跨 package 公共函数摘要 payload。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct PublicFunctionSummaryV1 {
    pub schema_revision: u32,
    pub analysis_semantics_revision: u32,
    pub public_policy_revision: u32,
    pub target_semantics: String,
    /// 实例 `MonoKey` 规范字节。
    pub mono_key: Vec<u8>,
    pub signature_and_abi_fingerprint: [u8; 32],
    pub parameter_count: u32,
    /// 接口 place：读取的参数序号，严格递增。
    pub read_params: Vec<u32>,
    /// 接口 place：写入的参数序号，严格递增。
    pub write_params: Vec<u32>,
    /// 无条件效果位集合（`KNOWN_EFFECTS` 内）。
    pub effects: u16,
    /// 条件事实（按 (kind, parameter) 排序）。
    pub conditional_facts: Vec<PublicConditionalFactV1>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub(crate) struct PublicConditionalFactV1 {
    pub kind: u16,
    pub parameter: u32,
}

impl PublicFunctionSummaryV1 {
    /// 内容寻址对象 key：`gugu-analysis-summary-v1` 域摘要。
    pub(crate) fn object_key(&self) -> [u8; 32] {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&self.schema_revision.to_le_bytes());
        bytes.extend_from_slice(&self.analysis_semantics_revision.to_le_bytes());
        bytes.extend_from_slice(&self.public_policy_revision.to_le_bytes());
        encode_bytes(&mut bytes, self.target_semantics.as_bytes());
        encode_bytes(&mut bytes, &self.mono_key);
        bytes.extend_from_slice(&self.signature_and_abi_fingerprint);
        bytes.extend_from_slice(&self.parameter_count.to_le_bytes());
        for parameters in [&self.read_params, &self.write_params] {
            bytes.extend_from_slice(
                &u64::try_from(parameters.len())
                    .expect("参数数量适配 GBC1")
                    .to_le_bytes(),
            );
            for parameter in parameters {
                bytes.extend_from_slice(&parameter.to_le_bytes());
            }
        }
        bytes.extend_from_slice(&self.effects.to_le_bytes());
        bytes.extend_from_slice(
            &u64::try_from(self.conditional_facts.len())
                .expect("条件事实数量适配 GBC1")
                .to_le_bytes(),
        );
        for fact in &self.conditional_facts {
            bytes.extend_from_slice(&fact.kind.to_le_bytes());
            bytes.extend_from_slice(&fact.parameter.to_le_bytes());
        }
        hash_domain("gugu-analysis-summary-v1", &bytes)
    }

    /// verifier：未知效果 bit、乱序条件事实与空实例键都拒绝。
    pub(crate) fn verify(&self) -> Result<(), crate::Diagnostic> {
        if self.schema_revision != PUBLIC_SCHEMA
            || self.analysis_semantics_revision != ANALYSIS_SEMANTICS_REVISION
            || self.public_policy_revision != PUBLIC_POLICY_REVISION
        {
            return Err(invalid("公共摘要 schema 或分析策略版本不受支持"));
        }
        for parameters in [&self.read_params, &self.write_params] {
            if parameters.windows(2).any(|pair| pair[0] >= pair[1])
                || parameters
                    .iter()
                    .any(|parameter| *parameter >= self.parameter_count)
            {
                return Err(invalid("公共摘要参数 place 未规范排序或超出签名边界"));
            }
        }
        if self.effects & !KNOWN_EFFECTS != 0 {
            return Err(invalid("公共摘要含未知效果位"));
        }
        if self.mono_key.len() < 32 {
            return Err(invalid("公共摘要缺少实例键"));
        }
        if self
            .conditional_facts
            .windows(2)
            .any(|pair| pair[0] >= pair[1])
        {
            return Err(invalid("公共摘要条件事实未按规范排序"));
        }
        if self.conditional_facts.iter().any(|fact| {
            fact.kind != FACT_RETURN_LEN_EQ_PARAM || fact.parameter >= self.parameter_count
        }) {
            return Err(invalid("公共摘要含未登记的条件事实"));
        }
        Ok(())
    }
}

/// 为闭世界全部公共函数实例投影公共摘要；返回 action key 用的映射。
///
/// 只有已完成实例 SCC 的摘要可被投影；生产者输入参与 query 身份，内容独立寻址。
pub(crate) fn project(
    target: TargetName,
    mono: &super::collect::MonoWorldV1,
    world: &AnalysisWorldV1,
    analysis_dependency: &DependencyFingerprint,
    queries: &QueryEngine,
) -> Result<BTreeMap<String, [u8; 32]>, Vec<crate::Diagnostic>> {
    let mut summaries = BTreeMap::new();
    for instance in &mono.instances {
        if instance.kind != MonoKind::Function || !instance.public {
            continue;
        }
        let index = world
            .instances
            .binary_search_by(|record| record.mono_key.cmp(&instance.mono_key))
            .map_err(|_| vec![invalid("公共函数缺少已完成的实例摘要")])?;
        let summary = &world.instances[index].summary;
        let payload = build_payload(
            target,
            &instance.mono_key,
            instance.signature_and_abi_fingerprint,
            instance.parameter_count,
            summary,
        );
        let key = QueryKey::new(
            QueryKind::PublicFunctionSummary,
            PUBLIC_SCHEMA,
            public_key_bytes(&instance.mono_key, analysis_dependency.fingerprint()),
        );
        let mut fresh = None;
        let result = queries
            .compute(key, |context| {
                context.record_dependency(
                    analysis_dependency.key().clone(),
                    analysis_dependency.fingerprint(),
                );
                let bytes = serde_json::to_vec(&payload).expect("公共摘要序列化");
                fresh = Some(payload);
                Ok((bytes, Vec::new()))
            })
            .map_err(|error| vec![invalid(error.to_string())])?;
        let payload: PublicFunctionSummaryV1 = fresh
            .or_else(|| serde_json::from_slice(result.payload()).ok())
            .ok_or_else(|| vec![invalid("PublicFunctionSummary 缓存不合法")])?;
        payload.verify().map_err(|error| vec![error])?;
        if payload.mono_key != instance.mono_key
            || payload.target_semantics != target.to_string()
            || payload.signature_and_abi_fingerprint != instance.signature_and_abi_fingerprint
            || payload.parameter_count != instance.parameter_count
        {
            return Err(vec![invalid(
                "公共摘要与生产者签名、target 或实例身份不一致",
            )]);
        }
        let digest = payload.object_key();
        summaries.insert(hex(&digest_of(&instance.mono_key)), digest);
    }
    Ok(summaries)
}

fn build_payload(
    target: TargetName,
    mono_key: &[u8],
    signature_fingerprint: [u8; 32],
    parameter_count: u32,
    summary: &FunctionSummary,
) -> PublicFunctionSummaryV1 {
    let mut effects = 0u16;
    effects |= u16::from(summary.may_allocate) * EFFECT_MAY_ALLOCATE;
    effects |= u16::from(summary.may_panic) * EFFECT_MAY_PANIC;
    effects |= u16::from(summary.may_suspend) * EFFECT_MAY_SUSPEND;
    effects |= u16::from(summary.may_call_unknown) * EFFECT_MAY_CALL_UNKNOWN;
    effects |= u16::from(summary.may_mutate_len) * EFFECT_MAY_MUTATE_LEN;
    effects |= u16::from(summary.alias_heap) * EFFECT_ALIAS_HEAP;
    effects |= u16::from(summary.alias_foreign) * EFFECT_ALIAS_FOREIGN;
    effects |= u16::from(summary.reads_hidden_state) * EFFECT_READS_HIDDEN;
    effects |= u16::from(summary.writes_hidden_state) * EFFECT_WRITES_HIDDEN;
    let mut conditional_facts: Vec<_> = summary
        .return_relations
        .iter()
        .map(|relation| PublicConditionalFactV1 {
            kind: FACT_RETURN_LEN_EQ_PARAM,
            parameter: match relation {
                ReturnRelation::EqLen { parameter } => *parameter,
            },
        })
        .collect();
    conditional_facts.sort();
    conditional_facts.dedup();
    PublicFunctionSummaryV1 {
        schema_revision: PUBLIC_SCHEMA,
        analysis_semantics_revision: ANALYSIS_SEMANTICS_REVISION,
        public_policy_revision: PUBLIC_POLICY_REVISION,
        target_semantics: target.to_string(),
        mono_key: mono_key.to_vec(),
        signature_and_abi_fingerprint: signature_fingerprint,
        parameter_count,
        read_params: if summary.unknown_param_access {
            (0..parameter_count).collect()
        } else {
            summary.read_params.clone()
        },
        write_params: if summary.unknown_param_access {
            (0..parameter_count).collect()
        } else {
            summary.write_params.clone()
        },
        effects,
        conditional_facts,
    }
}

fn public_key_bytes(mono_key: &[u8], producer: [u8; 32]) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&ANALYSIS_SEMANTICS_REVISION.to_le_bytes());
    bytes.extend_from_slice(&PUBLIC_POLICY_REVISION.to_le_bytes());
    bytes.extend_from_slice(&producer);
    encode_bytes(&mut bytes, mono_key);
    bytes
}

fn encode_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(
        &u64::try_from(bytes.len())
            .expect("对象字段长度适配 GBC1")
            .to_le_bytes(),
    );
    out.extend_from_slice(bytes);
}

fn digest_of(mono_key: &[u8]) -> [u8; 32] {
    hash_domain("gugu-mono-v1", mono_key)
}

fn hex(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn invalid(message: impl Into<String>) -> crate::Diagnostic {
    crate::Diagnostic::error(crate::DiagnosticCode::InvalidType, message.into(), None)
}
