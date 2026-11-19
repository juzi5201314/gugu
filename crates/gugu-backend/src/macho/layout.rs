//! 段、节与 `__LINKEDIT` 的地址分配。段在虚拟地址与文件偏移上都首尾相接，并按架构页
//! 对齐；`__TEXT` 从文件偏移 0 开始并包含 Mach header 与全部 load command。

use super::{Arch, IMAGE_BASE, Location, MachORequest, SectionContent, Segment};
use super::{commands, fixups, sign};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlacedSection {
    pub addr: u64,
    /// 零初始化节没有文件内容，为 0。
    pub fileoff: u64,
    pub size: u64,
    /// load command 中从 1 开始的节序号（`n_sect`）。
    pub ordinal: u8,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlacedSegment {
    pub segment: Segment,
    pub vmaddr: u64,
    pub vmsize: u64,
    pub fileoff: u64,
    pub filesize: u64,
    /// 本段节在 [`MachORequest::sections`] 中的下标，按请求顺序。
    pub sections: Vec<usize>,
}

/// `__LINKEDIT` 内各块的文件偏移与长度；代码签名在最后。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Linkedit {
    pub vmaddr: u64,
    pub vmsize: u64,
    pub fileoff: u64,
    pub fixups_off: u64,
    pub fixups_size: u64,
    pub exports_off: u64,
    pub exports_size: u64,
    pub symoff: u64,
    pub stroff: u64,
    pub strsize: u64,
    pub signature_off: u64,
    pub signature_size: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Layout {
    pub arch: Arch,
    pub ncmds: u32,
    pub sizeofcmds: u32,
    pub sections: Vec<PlacedSection>,
    /// 有节的段（`__TEXT` 恒在），不含 `__PAGEZERO` 与 `__LINKEDIT`。
    pub segments: Vec<PlacedSegment>,
    pub linkedit: Linkedit,
    pub file_size: u64,
}

pub(super) const HEADER_SIZE: u64 = 32;
pub(super) const EXPORTS_TRIE: [u8; 2] = [0, 0];

impl Layout {
    pub(super) fn compute(request: &MachORequest) -> Result<Layout, String> {
        let arch = request.arch;
        let groups: Vec<(Segment, Vec<usize>)> = Segment::ALL
            .iter()
            .map(|&segment| {
                let members = (0..request.sections.len())
                    .filter(|&i| request.sections[i].segment == segment)
                    .collect::<Vec<_>>();
                (segment, members)
            })
            .filter(|(segment, members)| *segment == Segment::Text || !members.is_empty())
            .collect();
        let (ncmds, sizeofcmds) = commands::size_of_commands(request, &groups);
        let mut sections = vec![
            PlacedSection {
                addr: 0,
                fileoff: 0,
                size: 0,
                ordinal: 0,
            };
            request.sections.len()
        ];
        let mut cursor = Cursor {
            vm: IMAGE_BASE,
            file: 0,
            ordinal: 0,
        };
        let mut segments = Vec::new();
        for (segment, members) in groups {
            let start = if segment == Segment::Text {
                HEADER_SIZE + u64::from(sizeofcmds)
            } else {
                0
            };
            segments.push(place_segment(
                request,
                segment,
                members,
                start,
                &mut cursor,
                &mut sections,
            )?);
        }
        let mut layout = Layout {
            arch,
            ncmds,
            sizeofcmds,
            sections,
            segments,
            linkedit: Linkedit {
                vmaddr: cursor.vm,
                vmsize: 0,
                fileoff: cursor.file,
                fixups_off: 0,
                fixups_size: 0,
                exports_off: 0,
                exports_size: 0,
                symoff: 0,
                stroff: 0,
                strsize: 0,
                signature_off: 0,
                signature_size: 0,
            },
            file_size: 0,
        };
        layout.place_linkedit(request)?;
        Ok(layout)
    }

    fn place_linkedit(&mut self, request: &MachORequest) -> Result<(), String> {
        let page = self.arch.page_size();
        let link = &mut self.linkedit;
        link.fixups_off = link.fileoff;
        link.fixups_size = fixups::blob_size(request, &self.segments, self.arch);
        link.exports_off = align(link.fixups_off + link.fixups_size, 8);
        link.exports_size = EXPORTS_TRIE.len() as u64;
        link.symoff = align(link.exports_off + link.exports_size, 8);
        link.stroff = link.symoff + 16 * request.symbols.len() as u64;
        link.strsize = align(commands::string_table_size(request), 8);
        link.signature_off = align(link.stroff + link.strsize, 16);
        link.signature_size = sign::signature_size(request, link.signature_off);
        let end = link.signature_off + link.signature_size;
        link.vmsize = align(end - link.fileoff, page);
        self.file_size = end;
        if self.file_size > u64::from(u32::MAX) {
            return Err("Mach-O image exceeds 4 GiB".to_owned());
        }
        Ok(())
    }

    /// 位置的链接期虚拟地址（基址 [`IMAGE_BASE`]）。
    pub fn address(&self, at: Location) -> Result<u64, String> {
        let section = self
            .sections
            .get(at.section)
            .ok_or_else(|| format!("Mach-O location names missing section {}", at.section))?;
        if at.offset > section.size {
            return Err(format!(
                "Mach-O location {:#x} is outside its section",
                at.offset
            ));
        }
        Ok(section.addr + at.offset)
    }

    pub(super) fn text(&self) -> &PlacedSegment {
        &self.segments[0]
    }

    /// load command 中的段序号（0 为 `__PAGEZERO`）。
    pub(super) fn segment_index(&self, segment: Segment) -> Option<usize> {
        self.segments
            .iter()
            .position(|placed| placed.segment == segment)
            .map(|index| index + 1)
    }
}

struct Cursor {
    vm: u64,
    file: u64,
    ordinal: u8,
}

fn place_segment(
    request: &MachORequest,
    segment: Segment,
    members: Vec<usize>,
    start: u64,
    cursor: &mut Cursor,
    sections: &mut [PlacedSection],
) -> Result<PlacedSegment, String> {
    let page = request.arch.page_size();
    let (vmaddr, fileoff) = (cursor.vm, cursor.file);
    let mut offset = start;
    let mut file_end = start;
    for &index in &members {
        let section = &request.sections[index];
        offset = align(offset, section.align);
        cursor.ordinal = cursor
            .ordinal
            .checked_add(1)
            .ok_or("Mach-O image has more than 255 sections")?;
        let zerofill = matches!(section.content, SectionContent::ZeroFill(_));
        sections[index] = PlacedSection {
            addr: vmaddr + offset,
            fileoff: if zerofill { 0 } else { fileoff + offset },
            size: section.content.size(),
            ordinal: cursor.ordinal,
        };
        offset = offset
            .checked_add(section.content.size())
            .ok_or("Mach-O section size overflows")?;
        if !zerofill {
            file_end = offset;
        }
    }
    let placed = PlacedSegment {
        segment,
        vmaddr,
        vmsize: align(offset.max(1), page),
        fileoff,
        filesize: align(file_end, page),
        sections: members,
    };
    cursor.vm += placed.vmsize;
    cursor.file += placed.filesize;
    Ok(placed)
}

pub(super) fn align(value: u64, align: u64) -> u64 {
    value.div_ceil(align) * align
}
