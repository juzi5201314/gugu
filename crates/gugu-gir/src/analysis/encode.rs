//! 分析三层 query 的二进制规范编码。
//!
//! 小端、长度前缀、枚举判别字节；同一输入编码出的字节恒等，因此可直接作为
//! query payload 进入结果指纹。布局以可切片为第一目标：
//!
//! - SCC payload 把每条实例记录写成连续区间，并记录 mono_key 与函数摘要各自
//!   的字节区间。函数层 query 的 payload 直接取摘要区间（`Bytes::slice`
//!   零拷贝），不再重编码。
//! - world payload 按全局 mono_key 序复制各 SCC 的记录字节，只重新编码 proof
//!   表（proof 的排序与冲突降级是全局语义，无法按 SCC 复用）。
//!
//! 字节布局随 schema 常量递增而变更；解码只做结构校验，身份校验
//! （schema、input_fingerprint）留在 `query.rs` 的调用侧。

use std::collections::HashMap;
use std::ops::Range;

use bytes::Bytes;

use super::types::{
    AnalysisWorldV1, FunctionSummary, InstanceSummaryRecord, ProofFact, ProofStatus,
    ReturnRelation, RuntimeCheckKey, SccSummaryV1,
};
use crate::hir::{CheckKind, ExprId, TypeId};

/// 解码失败：payload 与当前 schema 的布局不符。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DecodeError;

/// SCC 的可切片编码结果：payload 加上每条实例记录的字节定位。
pub struct EncodedScc {
    pub payload: Bytes,
    /// 与实例顺序平行的 `(整条记录区间, 函数摘要区间)`；记录区间以 mono_key 的
    /// 4 字节长度前缀开头，两区间均相对 payload。
    pub records: Vec<(Range<u32>, Range<u32>)>,
}

impl EncodedScc {
    /// 第 `record` 条实例的 mono_key 字节（剥离长度前缀）。
    pub fn key_bytes(&self, record: usize) -> &[u8] {
        let (record_range, _) = &self.records[record];
        let start = record_range.start as usize;
        let len = u32::from_le_bytes(self.payload[start..start + 4].try_into().expect("u32 长度"))
            as usize;
        &self.payload[start + 4..start + 4 + len]
    }
}

/// 编码整个 SCC 摘要并记录实例区间。
pub fn encode_scc(summary: &SccSummaryV1) -> EncodedScc {
    let mut encoder = Encoder::new();
    encoder.bool(summary.budget_exhausted);
    encoder.len_of(summary.instances.len());
    let mut records = Vec::with_capacity(summary.instances.len());
    for record in &summary.instances {
        let key_start = encoder.len();
        encoder.bytes(&record.mono_key);
        let summary_start = encoder.len();
        encode_function_summary(&record.summary, &mut encoder);
        records.push((key_start..summary_start, summary_start..encoder.len()));
    }
    encoder.len_of(summary.proofs.len());
    for fact in &summary.proofs {
        encode_proof_fact(fact, &mut encoder);
    }
    EncodedScc {
        payload: Bytes::from(encoder.finish()),
        records,
    }
}

/// 解码 SCC payload，重建类型化摘要与实例区间。
pub fn decode_scc(payload: &Bytes) -> Result<(SccSummaryV1, EncodedScc), DecodeError> {
    let mut decoder = Decoder::new(payload);
    let budget_exhausted = decoder.bool()?;
    let instance_count = decoder.len_of()?;
    let mut instances = Vec::with_capacity(instance_count);
    let mut records = Vec::with_capacity(instance_count);
    for _ in 0..instance_count {
        let key_start = decoder.len();
        let mono_key = decoder.bytes()?.to_vec();
        let summary_start = decoder.len();
        let summary = decode_function_summary(&mut decoder)?;
        instances.push(InstanceSummaryRecord { mono_key, summary });
        records.push((key_start..summary_start, summary_start..decoder.len()));
    }
    let proof_count = decoder.len_of()?;
    let mut proofs = Vec::with_capacity(proof_count);
    for _ in 0..proof_count {
        proofs.push(decode_proof_fact(&mut decoder)?);
    }
    decoder.expect_end()?;
    Ok((
        SccSummaryV1 {
            instances,
            proofs,
            budget_exhausted,
        },
        EncodedScc {
            payload: payload.clone(),
            records,
        },
    ))
}

/// 编码全程序 world。
///
/// 实例记录从 `parts` 的 SCC payload 复制：先按 mono_key 字节建首见映射（与
/// `world_from_sccs` 的稳定排序去重同源——首见即胜者），再按已排序去重的
/// `world.instances` 逐条定位复制，保证 payload 与类型化 world 同源。
pub fn encode_world(world: &AnalysisWorldV1, parts: &[EncodedScc]) -> Bytes {
    let mut sources = HashMap::with_capacity(world.instances.len());
    for (part_index, part) in parts.iter().enumerate() {
        for record_index in 0..part.records.len() {
            sources
                .entry(part.key_bytes(record_index))
                .or_insert((part_index, record_index));
        }
    }
    let mut encoder = Encoder::new();
    encoder.u32(world.schema);
    encoder.bytes(&world.input_fingerprint);
    encoder.len_of(world.exhausted_owners.len());
    for &owner in &world.exhausted_owners {
        encoder.u32(owner);
    }
    encoder.u32(world.runtime_checks_elided_count);
    encoder.len_of(world.instances.len());
    for record in &world.instances {
        let &(part_index, record_index) = sources
            .get(record.mono_key.as_slice())
            .expect("world 实例必有来源 SCC 记录");
        let part = &parts[part_index];
        let (key_range, summary_range) = &part.records[record_index];
        // mono_key 与摘要紧邻编码，整条记录从 key 区间起点连续到摘要区间终点。
        encoder.raw(&part.payload[key_range.start as usize..summary_range.end as usize]);
    }
    encoder.len_of(world.proofs.len());
    for fact in &world.proofs {
        encode_proof_fact(fact, &mut encoder);
    }
    Bytes::from(encoder.finish())
}

/// 解码全程序 world。
pub fn decode_world(payload: &Bytes) -> Result<AnalysisWorldV1, DecodeError> {
    let mut decoder = Decoder::new(payload);
    let schema = decoder.u32()?;
    let input_fingerprint: [u8; 32] = decoder.bytes()?.try_into().map_err(|_| DecodeError)?;
    let exhausted_count = decoder.len_of()?;
    let mut exhausted_owners = Vec::with_capacity(exhausted_count);
    for _ in 0..exhausted_count {
        exhausted_owners.push(decoder.u32()?);
    }
    let runtime_checks_elided_count = decoder.u32()?;
    let instance_count = decoder.len_of()?;
    let mut instances = Vec::with_capacity(instance_count);
    for _ in 0..instance_count {
        let mono_key = decoder.bytes()?.to_vec();
        let summary = decode_function_summary(&mut decoder)?;
        instances.push(InstanceSummaryRecord { mono_key, summary });
    }
    let proof_count = decoder.len_of()?;
    let mut proofs = Vec::with_capacity(proof_count);
    for _ in 0..proof_count {
        proofs.push(decode_proof_fact(&mut decoder)?);
    }
    decoder.expect_end()?;
    Ok(AnalysisWorldV1 {
        schema,
        input_fingerprint,
        instances,
        proofs,
        exhausted_owners,
        runtime_checks_elided_count,
    })
}

fn encode_function_summary(summary: &FunctionSummary, out: &mut Encoder) {
    out.len_of(summary.preconditions.len());
    out.option_i64(summary.return_lo);
    out.option_i64(summary.return_hi);
    out.len_of(summary.return_relations.len());
    for relation in &summary.return_relations {
        match *relation {
            ReturnRelation::EqLen { parameter } => {
                out.u8(0);
                out.u32(parameter);
            }
        }
    }
    out.len_of(summary.read_params.len());
    for parameter in &summary.read_params {
        out.u32(*parameter);
    }
    out.len_of(summary.write_params.len());
    for parameter in &summary.write_params {
        out.u32(*parameter);
    }
    // 十个效果 bool 打包进 u16，按字段声明序从低位起。
    let flags = u16::from(summary.unknown_param_access)
        | u16::from(summary.alias_heap) << 1
        | u16::from(summary.alias_foreign) << 2
        | u16::from(summary.reads_hidden_state) << 3
        | u16::from(summary.writes_hidden_state) << 4
        | u16::from(summary.may_allocate) << 5
        | u16::from(summary.may_panic) << 6
        | u16::from(summary.may_suspend) << 7
        | u16::from(summary.may_call_unknown) << 8
        | u16::from(summary.may_mutate_len) << 9;
    out.u16(flags);
}

pub fn decode_function_summary(decoder: &mut Decoder<'_>) -> Result<FunctionSummary, DecodeError> {
    let preconditions = vec![(); decoder.len_of()?];
    let return_lo = decoder.option_i64()?;
    let return_hi = decoder.option_i64()?;
    let mut return_relations = Vec::with_capacity(decoder.len_of()?);
    for _ in 0..return_relations.capacity() {
        let discriminant = decoder.u8()?;
        return_relations.push(match discriminant {
            0 => ReturnRelation::EqLen {
                parameter: decoder.u32()?,
            },
            _ => return Err(DecodeError),
        });
    }
    let mut read_params = Vec::with_capacity(decoder.len_of()?);
    for _ in 0..read_params.capacity() {
        read_params.push(decoder.u32()?);
    }
    let mut write_params = Vec::with_capacity(decoder.len_of()?);
    for _ in 0..write_params.capacity() {
        write_params.push(decoder.u32()?);
    }
    let flags = decoder.u16()?;
    let flag = |bit: u16| flags & (1 << bit) != 0;
    Ok(FunctionSummary {
        preconditions,
        return_lo,
        return_hi,
        return_relations,
        read_params,
        write_params,
        unknown_param_access: flag(0),
        alias_heap: flag(1),
        alias_foreign: flag(2),
        reads_hidden_state: flag(3),
        writes_hidden_state: flag(4),
        may_allocate: flag(5),
        may_panic: flag(6),
        may_suspend: flag(7),
        may_call_unknown: flag(8),
        may_mutate_len: flag(9),
    })
}

fn encode_proof_fact(fact: &ProofFact, out: &mut Encoder) {
    out.u32(fact.key.owner_index);
    out.u32(fact.key.expression.0);
    match &fact.key.kind {
        CheckKind::Division { ty, divisor } => {
            out.u8(0);
            out.u32(ty.0);
            out.u32(divisor.0);
        }
        CheckKind::Shift { ty, amount } => {
            out.u8(1);
            out.u32(ty.0);
            out.u32(amount.0);
        }
        CheckKind::Bounds { slice } => {
            out.u8(2);
            out.bool(*slice);
        }
        CheckKind::Utf8Boundary => out.u8(3),
        CheckKind::FloatToInt {
            signed,
            bits,
            value,
        } => {
            out.u8(4);
            out.bool(*signed);
            out.u16(*bits);
            out.u32(value.0);
        }
        CheckKind::UnicodeScalar { value } => {
            out.u8(5);
            out.u32(value.0);
        }
    }
    out.u8(match fact.status {
        ProofStatus::Proved => 0,
        ProofStatus::Disproved => 1,
        ProofStatus::Unknown => 2,
    });
}

fn decode_proof_fact(decoder: &mut Decoder<'_>) -> Result<ProofFact, DecodeError> {
    let owner_index = decoder.u32()?;
    let expression = ExprId(decoder.u32()?);
    let kind = match decoder.u8()? {
        0 => CheckKind::Division {
            ty: TypeId(decoder.u32()?),
            divisor: ExprId(decoder.u32()?),
        },
        1 => CheckKind::Shift {
            ty: TypeId(decoder.u32()?),
            amount: ExprId(decoder.u32()?),
        },
        2 => CheckKind::Bounds {
            slice: decoder.bool()?,
        },
        3 => CheckKind::Utf8Boundary,
        4 => CheckKind::FloatToInt {
            signed: decoder.bool()?,
            bits: decoder.u16()?,
            value: ExprId(decoder.u32()?),
        },
        5 => CheckKind::UnicodeScalar {
            value: ExprId(decoder.u32()?),
        },
        _ => return Err(DecodeError),
    };
    let status = match decoder.u8()? {
        0 => ProofStatus::Proved,
        1 => ProofStatus::Disproved,
        2 => ProofStatus::Unknown,
        _ => return Err(DecodeError),
    };
    Ok(ProofFact {
        key: RuntimeCheckKey {
            owner_index,
            expression,
            kind,
        },
        status,
    })
}

/// 规范字节编码器：小端、长度前缀，输出确定性字节序列。
pub struct Encoder {
    bytes: Vec<u8>,
}

impl Encoder {
    pub fn new() -> Self {
        Self { bytes: Vec::new() }
    }

    /// 已编码字节数；调用方以此记录区间起点与终点。
    pub fn len(&self) -> u32 {
        u32::try_from(self.bytes.len()).expect("payload 长度适配 u32")
    }

    pub fn finish(self) -> Vec<u8> {
        self.bytes
    }

    pub fn u8(&mut self, value: u8) {
        self.bytes.push(value);
    }

    pub fn u16(&mut self, value: u16) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    pub fn u32(&mut self, value: u32) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    pub fn bool(&mut self, value: bool) {
        self.bytes.push(u8::from(value));
    }

    /// 元素个数前缀。
    pub fn len_of(&mut self, count: usize) {
        self.u32(u32::try_from(count).expect("元素个数适配 u32"));
    }

    /// 长度前缀字节串。
    pub fn bytes(&mut self, value: &[u8]) {
        self.len_of(value.len());
        self.bytes.extend_from_slice(value);
    }

    /// 无长度前缀的原始字节（复制已编码的记录区间）。
    pub fn raw(&mut self, value: &[u8]) {
        self.bytes.extend_from_slice(value);
    }

    pub fn option_i64(&mut self, value: Option<i64>) {
        match value {
            Some(value) => {
                self.u8(1);
                self.bytes.extend_from_slice(&value.to_le_bytes());
            }
            None => self.u8(0),
        }
    }
}

/// 规范字节解码器；越界、残留或非法判别字节都是 `DecodeError`。
pub struct Decoder<'a> {
    bytes: &'a [u8],
    cursor: usize,
}

impl<'a> Decoder<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, cursor: 0 }
    }

    /// 已消费字节数；调用方以此重建记录区间。
    pub fn len(&self) -> u32 {
        u32::try_from(self.cursor).expect("已消费长度适配 u32")
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8], DecodeError> {
        let end = self
            .cursor
            .checked_add(count)
            .filter(|&end| end <= self.bytes.len())
            .ok_or(DecodeError)?;
        let slice = &self.bytes[self.cursor..end];
        self.cursor = end;
        Ok(slice)
    }

    pub fn expect_end(&self) -> Result<(), DecodeError> {
        if self.cursor == self.bytes.len() {
            Ok(())
        } else {
            Err(DecodeError)
        }
    }

    pub fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.take(1)?[0])
    }

    pub fn u16(&mut self) -> Result<u16, DecodeError> {
        Ok(u16::from_le_bytes(
            self.take(2)?.try_into().expect("u16 长度"),
        ))
    }

    pub fn u32(&mut self) -> Result<u32, DecodeError> {
        Ok(u32::from_le_bytes(
            self.take(4)?.try_into().expect("u32 长度"),
        ))
    }

    pub fn bool(&mut self) -> Result<bool, DecodeError> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(DecodeError),
        }
    }

    pub fn bytes(&mut self) -> Result<&'a [u8], DecodeError> {
        let count = self.len_of()?;
        self.take(count)
    }

    pub fn len_of(&mut self) -> Result<usize, DecodeError> {
        usize::try_from(self.u32()?).map_err(|_| DecodeError)
    }

    pub fn option_i64(&mut self) -> Result<Option<i64>, DecodeError> {
        match self.u8()? {
            0 => Ok(None),
            1 => Ok(Some(i64::from_le_bytes(
                self.take(8)?.try_into().expect("i64 长度"),
            ))),
            _ => Err(DecodeError),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_summary() -> FunctionSummary {
        FunctionSummary {
            preconditions: Vec::new(),
            return_lo: Some(-3),
            return_hi: None,
            return_relations: vec![ReturnRelation::EqLen { parameter: 2 }],
            read_params: vec![0, 1],
            write_params: Vec::new(),
            unknown_param_access: false,
            alias_heap: true,
            alias_foreign: false,
            reads_hidden_state: true,
            writes_hidden_state: false,
            may_allocate: true,
            may_panic: false,
            may_suspend: true,
            may_call_unknown: false,
            may_mutate_len: true,
        }
    }

    fn sample_scc() -> SccSummaryV1 {
        SccSummaryV1 {
            instances: vec![
                InstanceSummaryRecord {
                    mono_key: vec![1, 2, 3],
                    summary: sample_summary(),
                },
                InstanceSummaryRecord {
                    mono_key: vec![4],
                    summary: FunctionSummary::conservative(),
                },
            ],
            proofs: vec![ProofFact {
                key: RuntimeCheckKey {
                    owner_index: 7,
                    expression: ExprId(9),
                    kind: CheckKind::FloatToInt {
                        signed: true,
                        bits: 64,
                        value: ExprId(11),
                    },
                },
                status: ProofStatus::Unknown,
            }],
            budget_exhausted: true,
        }
    }

    fn sample_world(summary: &SccSummaryV1) -> AnalysisWorldV1 {
        AnalysisWorldV1 {
            schema: 1,
            input_fingerprint: [9; 32],
            instances: summary.instances.clone(),
            proofs: summary.proofs.clone(),
            exhausted_owners: vec![3, 7],
            runtime_checks_elided_count: 0,
        }
    }

    #[test]
    fn scc_roundtrip_preserves_summary_and_slices() {
        let summary = sample_scc();
        let encoded = encode_scc(&summary);
        let (decoded, encoded_view) = decode_scc(&encoded.payload).expect("SCC 解码");
        assert_eq!(decoded, summary);
        assert_eq!(encoded_view.records, encoded.records);
        // 函数层 payload 是记录内的摘要区间，解码即原摘要。
        for (record, (_, summary_range)) in summary.instances.iter().zip(&encoded.records) {
            let slice = encoded
                .payload
                .slice(summary_range.start as usize..summary_range.end as usize);
            let mut decoder = Decoder::new(&slice);
            let decoded_summary = decode_function_summary(&mut decoder).expect("摘要解码");
            decoder.expect_end().expect("摘要区间恰好一个摘要");
            assert_eq!(decoded_summary, record.summary);
        }
    }

    #[test]
    fn world_roundtrip_matches_typed_world() {
        let summary = sample_scc();
        let encoded = encode_scc(&summary);
        let world = sample_world(&summary);
        let payload = encode_world(&world, std::slice::from_ref(&encoded));
        let decoded = decode_world(&payload).expect("world 解码");
        assert_eq!(decoded, world);
    }

    #[test]
    fn encoding_is_deterministic() {
        let first = encode_scc(&sample_scc());
        let second = encode_scc(&sample_scc());
        assert_eq!(first.payload, second.payload);
        assert_eq!(first.records, second.records);
        let world = sample_world(&sample_scc());
        assert_eq!(
            encode_world(&world, std::slice::from_ref(&first)),
            encode_world(&world, std::slice::from_ref(&second))
        );
    }

    #[test]
    fn decode_rejects_truncated_payload() {
        let summary = sample_scc();
        let encoded = encode_scc(&summary);
        let end = encoded.payload.len();
        assert!(decode_scc(&encoded.payload.slice(..end - 1)).is_err());
        assert_eq!(decode_world(&encoded.payload), Err(DecodeError));
    }
}
