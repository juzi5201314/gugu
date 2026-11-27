use std::collections::BTreeSet;

use super::*;

pub(super) fn source_section(
    prepared: &[Prepared<'_>],
    stack_index: &[Option<u32>],
    sources: &SourceMap,
) -> Result<Vec<u8>, Vec<Diagnostic>> {
    let mut records = Vec::new();
    for item in prepared {
        let Some(function) = stack_index[item.unwind_index as usize] else {
            continue;
        };
        for site in &item.fragment.sites {
            if let Some(record) = source_record(item.body, site, function, sources) {
                records.push(record);
            }
        }
    }
    records.sort_by(|left, right| {
        (
            left.function,
            left.pc_start,
            left.pc_end,
            left.path.as_str(),
        )
            .cmp(&(
                right.function,
                right.pc_start,
                right.pc_end,
                right.path.as_str(),
            ))
    });
    encode_sources(&records)
}

struct SourceRecord {
    function: u32,
    pc_start: u32,
    pc_end: u32,
    path: String,
    line: u32,
    column: u32,
    flags: u32,
}

fn source_record(
    body: &Body,
    site: &SitePayload,
    function: u32,
    sources: &SourceMap,
) -> Option<SourceRecord> {
    if site.bytes.1 <= site.bytes.0 {
        return None;
    }
    let info = source_info(body, site)?;
    let location = &info.location;
    let snapshot = sources.snapshot(SourceFileId::new(location.source));
    let path = snapshot
        .map(|snapshot| snapshot.logical_path().to_owned())
        .unwrap_or_else(|| "<synthetic>".to_owned());
    let (line, column) = snapshot
        .and_then(|snapshot| {
            snapshot
                .line_column(location.start as usize)
                .ok()
                .map(|position| (position.line, position.column))
        })
        .unwrap_or((1, 1));
    let mut flags = 0_u32;
    if site.op == "Trap"
        || site.op == "ResumePanic"
        || site
            .cold_edges
            .iter()
            .any(|edge| matches!(edge.kind, super::super::inst::ColdEdgeKind::Trap))
    {
        flags |= 1;
    }
    if location.expansion != 0 || snapshot.is_none() {
        flags |= 1 << 1;
    }
    Some(SourceRecord {
        function,
        pc_start: site.bytes.0,
        pc_end: site.bytes.1,
        path,
        line,
        column,
        flags,
    })
}

fn source_info<'a>(body: &'a Body, site: &SitePayload) -> Option<&'a crate::gir::body::SourceInfo> {
    let block = body.blocks.get(site.block as usize)?;
    if site.instruction == block.instructions.end {
        return Some(&block.source);
    }
    Some(&body.instructions.get(site.instruction as usize)?.source)
}

fn encode_sources(records: &[SourceRecord]) -> Result<Vec<u8>, Vec<Diagnostic>> {
    let mut paths = BTreeSet::new();
    for record in records {
        paths.insert(record.path.clone());
    }
    let mut pool = Vec::new();
    let mut offsets = std::collections::BTreeMap::new();
    for path in &paths {
        let offset = u32::try_from(pool.len()).map_err(|_| vec![invalid("源码路径池溢出")])?;
        offsets.insert(path.clone(), offset);
        pool.extend_from_slice(path.as_bytes());
    }
    let mut output = vec![0_u8; 24];
    output[..8].copy_from_slice(b"GUGUSRC1");
    output[8..10].copy_from_slice(&1_u16.to_le_bytes());
    output[10] = 8;
    output[11] = 1;
    output[12..16].copy_from_slice(&(records.len() as u32).to_le_bytes());
    output[16..20].copy_from_slice(&(pool.len() as u32).to_le_bytes());
    for record in records {
        let offset = offsets[&record.path];
        let path_len =
            u32::try_from(record.path.len()).map_err(|_| vec![invalid("源码路径过长")])?;
        output.extend_from_slice(&record.function.to_le_bytes());
        output.extend_from_slice(&record.pc_start.to_le_bytes());
        output.extend_from_slice(&record.pc_end.to_le_bytes());
        output.extend_from_slice(&offset.to_le_bytes());
        output.extend_from_slice(&path_len.to_le_bytes());
        output.extend_from_slice(&record.line.to_le_bytes());
        output.extend_from_slice(&record.column.to_le_bytes());
        output.extend_from_slice(&record.flags.to_le_bytes());
    }
    output.extend_from_slice(&pool);
    Ok(output)
}

pub(super) fn source_count(bytes: &[u8]) -> Result<u32, Vec<Diagnostic>> {
    if bytes.len() < 24 || &bytes[..8] != b"GUGUSRC1" {
        return Err(vec![invalid("源码记录 section 魔数不匹配")]);
    }
    Ok(u32::from_le_bytes(
        bytes[12..16].try_into().expect("计数字段"),
    ))
}
