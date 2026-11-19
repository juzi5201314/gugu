use super::{ElfRequest, Location, PHNUM, SEGMENT_ALIGN, SectionContent, Segment};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlacedSection {
    pub addr: u64,
    pub size: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Layout {
    pub sections: Vec<PlacedSection>,
    pub file_size: u64,
    pub rela: u64,
    pub shstrtab: u64,
    pub shoff: u64,
    pub(crate) phnum: u16,
    pub(crate) rela_count: u64,
    pub(crate) shnum: u16,
    pub(crate) segments: [(u64, u64, u64); 3],
}

impl Layout {
    pub fn address(&self, at: Location) -> Result<u64, String> {
        self.sections
            .get(at.section)
            .filter(|s| at.offset <= s.size)
            .and_then(|s| s.addr.checked_add(at.offset))
            .ok_or_else(|| format!("invalid ELF location {at:?}"))
    }

    pub(super) fn compute(
        request: &ElfRequest,
        imports: usize,
        dynamic: bool,
    ) -> Result<Self, String> {
        let mut result = Self {
            sections: vec![PlacedSection { addr: 0, size: 0 }; request.sections.len()],
            file_size: 0,
            rela: 0,
            shstrtab: 0,
            shoff: 0,
            phnum: PHNUM + if dynamic { 2 } else { 0 },
            rela_count: (request.relatives.len() + imports) as u64,
            shnum: if dynamic { 7 } else { 3 },
            segments: [(0, 0, 0); 3],
        };
        let mut cursor = 64 + result.phnum as u64 * 56;
        for (index, segment) in Segment::ALL.into_iter().enumerate() {
            let start = if index == 0 {
                0
            } else {
                align(cursor, SEGMENT_ALIGN)?
            };
            cursor = cursor.max(start);
            let (next, mut file_end) = result.place_sections(request, segment, cursor)?;
            cursor = next;
            if segment == Segment::DataConst {
                cursor = result.metadata(cursor, result.rela_count)?;
                file_end = cursor;
            }
            if cursor == start {
                cursor = add(cursor, 1)?;
                file_end = cursor;
            }
            result.segments[index] = (start, file_end - start, cursor - start);
            result.file_size = file_end;
        }
        Ok(result)
    }

    fn place_sections(
        &mut self,
        request: &ElfRequest,
        segment: Segment,
        mut cursor: u64,
    ) -> Result<(u64, u64), String> {
        let mut file_end = cursor;
        let mut zero = false;
        for (i, section) in request
            .sections
            .iter()
            .enumerate()
            .filter(|(_, s)| s.segment == segment)
        {
            let is_zero = matches!(section.content, SectionContent::ZeroFill(_));
            if zero && !is_zero {
                return Err("ELF file-backed section follows zero-fill".into());
            }
            cursor = align(cursor, section.align)?;
            self.sections[i] = PlacedSection {
                addr: cursor,
                size: section.content.size(),
            };
            cursor = add(cursor, section.content.size())?;
            if !is_zero {
                file_end = cursor;
            }
            zero |= is_zero;
        }
        Ok((cursor, file_end))
    }

    fn metadata(&mut self, cursor: u64, relatives: u64) -> Result<u64, String> {
        self.rela = align(cursor, 8)?;
        self.shstrtab = add(
            self.rela,
            relatives.checked_mul(24).ok_or("ELF RELA size overflow")?,
        )?;
        self.shoff = align(
            add(self.shstrtab, super::headers::SHSTRTAB.len() as u64)?,
            8,
        )?;
        add(self.shoff, u64::from(self.shnum) * 64)
    }
}

fn add(value: u64, size: u64) -> Result<u64, String> {
    value
        .checked_add(size)
        .ok_or_else(|| "ELF layout overflow".into())
}

fn align(value: u64, alignment: u64) -> Result<u64, String> {
    add(value, alignment - 1).map(|v| v & !(alignment - 1))
}
