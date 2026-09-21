//! 固定宽度的 x86_64 字节助手。只服务 rt0 与 runtime 入口，不替代指令表编码器。

pub(crate) struct Asm {
    bytes: Vec<u8>,
    labels: Vec<Option<usize>>,
    patches: Vec<Patch>,
}

struct Patch {
    at: usize,
    label: u32,
    next: usize,
}

impl Asm {
    pub(crate) fn new() -> Self {
        Self {
            bytes: Vec::new(),
            labels: Vec::new(),
            patches: Vec::new(),
        }
    }

    pub(crate) fn offset(&self) -> usize {
        self.bytes.len()
    }

    pub(crate) fn label(&mut self) -> u32 {
        let id = u32::try_from(self.labels.len()).expect("标签数量");
        self.labels.push(None);
        id
    }

    pub(crate) fn bind(&mut self, label: u32) {
        self.labels[label as usize] = Some(self.bytes.len());
    }

    pub(crate) fn finish(mut self) -> Vec<u8> {
        for patch in &self.patches {
            let target = self.labels[patch.label as usize].expect("标签已绑定");
            let disp = i32::try_from(target as i64 - patch.next as i64).expect("短距离跳转");
            self.bytes[patch.at..patch.at + 4].copy_from_slice(&disp.to_le_bytes());
        }
        self.bytes
    }

    pub(crate) fn emit(&mut self, bytes: &[u8]) {
        self.bytes.extend_from_slice(bytes);
    }

    pub(crate) fn ret(&mut self) {
        self.emit(&[0xc3]);
    }

    pub(crate) fn syscall(&mut self) {
        self.emit(&[0x0f, 0x05]);
    }

    pub(crate) fn ud2(&mut self) {
        self.emit(&[0x0f, 0x0b]);
    }

    pub(crate) fn mfence(&mut self) {
        self.emit(&[0x0f, 0xae, 0xf0]);
    }

    pub(crate) fn push(&mut self, reg: u8) {
        self.rex_b(reg, false);
        self.emit(&[0x50 + low(reg)]);
    }

    pub(crate) fn pop(&mut self, reg: u8) {
        self.rex_b(reg, false);
        self.emit(&[0x58 + low(reg)]);
    }

    pub(crate) fn xor_self32(&mut self, reg: u8) {
        self.rex_rb(reg, reg, false);
        self.emit(&[0x31, modrm(3, reg, reg)]);
    }

    pub(crate) fn mov_imm32(&mut self, reg: u8, value: u32) {
        self.rex_b(reg, false);
        self.emit(&[0xb8 + low(reg)]);
        self.emit(&value.to_le_bytes());
    }

    pub(crate) fn mov_imm64(&mut self, reg: u8, value: u64) {
        self.rex_b(reg, true);
        self.emit(&[0xb8 + low(reg)]);
        self.emit(&value.to_le_bytes());
    }

    pub(crate) fn mov_load(&mut self, dest: u8, base: u8, disp: i32) {
        self.mem(0x8b, dest, base, disp);
    }

    pub(crate) fn mov_store(&mut self, base: u8, disp: i32, src: u8) {
        self.mem(0x89, src, base, disp);
    }

    pub(crate) fn mov_store32(&mut self, base: u8, disp: i32, src: u8) {
        self.rex_rb(src, base, false);
        self.emit(&[0x89]);
        self.mem_tail(src, base, disp);
    }

    pub(crate) fn mov_reg(&mut self, dest: u8, src: u8) {
        self.rex_rb(src, dest, true);
        self.emit(&[0x89, modrm(3, src, dest)]);
    }

    pub(crate) fn div_r64(&mut self, reg: u8) {
        self.rex_b(reg, true);
        self.emit(&[0xf7, modrm(3, 6, reg)]);
    }

    pub(crate) fn movzx8(&mut self, dest: u8, base: u8, disp: i32) {
        self.rex_rb(dest, base, false);
        self.emit(&[0x0f, 0xb6]);
        self.mem_tail(dest, base, disp);
    }

    pub(crate) fn lea_disp(&mut self, dest: u8, base: u8, index: u8, scale: u8, disp: i32) {
        self.rex_index(dest, base, index, true);
        self.emit(&[0x8d]);
        self.emit(&[modrm_mod(2, dest, 4)]);
        self.emit(&[sib(scale, index, base)]);
        self.emit(&disp.to_le_bytes());
    }

    pub(crate) fn lea_rip(&mut self, dest: u8, origin: u64, target: u64) {
        let next = origin + self.bytes.len() as u64 + 7;
        let disp = i32::try_from(target as i64 - next as i64).expect("RIP 相对距离");
        self.rex_b(dest, true);
        self.emit(&[0x8d, modrm_mod(0, dest, 5)]);
        self.emit(&disp.to_le_bytes());
    }

    pub(crate) fn call_vaddr(&mut self, origin: u64, target: u64) {
        let next = origin + self.bytes.len() as u64 + 5;
        let disp = i32::try_from(target as i64 - next as i64).expect("调用距离");
        self.emit(&[0xe8]);
        self.emit(&disp.to_le_bytes());
    }

    pub(crate) fn add_imm32(&mut self, reg: u8, value: i32) {
        self.alu_imm(0, reg, value);
    }

    pub(crate) fn sub_imm32(&mut self, reg: u8, value: i32) {
        self.alu_imm(5, reg, value);
    }

    pub(crate) fn cmp_imm32(&mut self, reg: u8, value: i32) {
        self.alu_imm(7, reg, value);
    }

    pub(crate) fn cmp_mem(&mut self, reg: u8, base: u8, disp: i32) {
        self.mem(0x3b, reg, base, disp);
    }

    pub(crate) fn test_self(&mut self, reg: u8) {
        self.rex_rb(reg, reg, true);
        self.emit(&[0x85, modrm(3, reg, reg)]);
    }

    pub(crate) fn inc_mem64(&mut self, base: u8, disp: i32) {
        self.mem(0xff, 0, base, disp);
    }

    pub(crate) fn dec_mem64(&mut self, base: u8, disp: i32) {
        self.mem(0xff, 1, base, disp);
    }

    pub(crate) fn dec_reg(&mut self, reg: u8) {
        self.rex_b(reg, true);
        self.emit(&[0xff, modrm(3, 1, reg)]);
    }

    pub(crate) fn jmp(&mut self, label: u32) {
        self.jump(0xe9, None, label);
    }

    pub(crate) fn je(&mut self, label: u32) {
        self.jump(0x0f, Some(0x84), label);
    }

    pub(crate) fn jne(&mut self, label: u32) {
        self.jump(0x0f, Some(0x85), label);
    }

    pub(crate) fn jae(&mut self, label: u32) {
        self.jump(0x0f, Some(0x83), label);
    }

    pub(crate) fn jb(&mut self, label: u32) {
        self.jump(0x0f, Some(0x82), label);
    }

    pub(crate) fn ja(&mut self, label: u32) {
        self.jump(0x0f, Some(0x87), label);
    }

    pub(crate) fn jle(&mut self, label: u32) {
        self.jump(0x0f, Some(0x8e), label);
    }

    pub(crate) fn add_reg(&mut self, dest: u8, src: u8) {
        self.rex_rb(src, dest, true);
        self.emit(&[0x01, modrm(3, src, dest)]);
    }

    pub(crate) fn cmp_reg(&mut self, left: u8, right: u8) {
        self.rex_rb(right, left, true);
        self.emit(&[0x39, modrm(3, right, left)]);
    }

    pub(crate) fn store_bl(&mut self) {
        self.emit(&[0x88, 0x18]);
    }

    pub(crate) fn copy_byte(&mut self) {
        self.emit(&[0x8a, 0x13, 0x88, 0x10]);
    }

    pub(crate) fn shl1(&mut self, reg: u8) {
        self.rex_b(reg, true);
        self.emit(&[0xd1, modrm(3, 4, reg)]);
    }

    pub(crate) fn rcl1(&mut self, reg: u8) {
        self.rex_b(reg, true);
        self.emit(&[0xd1, modrm(3, 2, reg)]);
    }

    pub(crate) fn and_imm32(&mut self, reg: u8, value: i32) {
        self.alu_imm(4, reg, value);
    }

    pub(crate) fn and_reg(&mut self, dest: u8, src: u8) {
        self.rex_rb(src, dest, true);
        self.emit(&[0x21, modrm(3, src, dest)]);
    }

    pub(crate) fn or_imm8(&mut self, reg: u8, value: u8) {
        self.rex_b(reg, true);
        self.emit(&[0x83, modrm(3, 1, reg), value]);
    }

    pub(crate) fn sub_reg(&mut self, dest: u8, src: u8) {
        self.rex_rb(src, dest, true);
        self.emit(&[0x29, modrm(3, src, dest)]);
    }

    pub(crate) fn sbb_reg(&mut self, dest: u8, src: u8) {
        self.rex_rb(src, dest, true);
        self.emit(&[0x19, modrm(3, src, dest)]);
    }

    pub(crate) fn call_reg(&mut self, reg: u8) {
        self.rex_b(reg, false);
        self.emit(&[0xff, modrm(3, 2, reg)]);
    }

    fn jump(&mut self, primary: u8, second: Option<u8>, label: u32) {
        self.emit(&[primary]);
        if let Some(second) = second {
            self.emit(&[second]);
        }
        let at = self.bytes.len();
        self.emit(&[0, 0, 0, 0]);
        self.patches.push(Patch {
            at,
            label,
            next: self.bytes.len(),
        });
    }

    fn alu_imm(&mut self, op: u8, reg: u8, value: i32) {
        self.rex_b(reg, true);
        if let Ok(byte) = i8::try_from(value) {
            self.emit(&[0x83, modrm(3, op, reg), byte as u8]);
        } else {
            self.emit(&[0x81, modrm(3, op, reg)]);
            self.emit(&value.to_le_bytes());
        }
    }

    fn mem(&mut self, opcode: u8, reg: u8, base: u8, disp: i32) {
        self.rex_rb(reg, base, true);
        self.emit(&[opcode]);
        self.mem_tail(reg, base, disp);
    }

    fn mem_tail(&mut self, reg: u8, base: u8, disp: i32) {
        if low(base) == 4 {
            self.emit(&[modrm_mod(2, reg, 4), sib(0, 4, base)]);
        } else {
            self.emit(&[modrm_mod(2, reg, base)]);
        }
        self.emit(&disp.to_le_bytes());
    }

    fn rex_b(&mut self, reg: u8, wide: bool) {
        let mut rex = u8::from(wide) << 3;
        if reg >= 8 {
            rex |= 0x41;
        } else if wide {
            rex |= 0x48;
        }
        if rex != 0 {
            self.emit(&[rex]);
        }
    }

    fn rex_rb(&mut self, reg: u8, base: u8, wide: bool) {
        let mut rex = u8::from(wide) << 3;
        if reg >= 8 {
            rex |= 0x44;
        }
        if base >= 8 {
            rex |= 0x41;
        }
        if wide {
            rex |= 0x48;
        }
        if rex != 0 {
            self.emit(&[rex]);
        }
    }

    fn rex_index(&mut self, dest: u8, base: u8, index: u8, wide: bool) {
        let mut rex = if wide { 0x48 } else { 0 };
        if dest >= 8 {
            rex |= 0x44;
        }
        if index >= 8 {
            rex |= 0x42;
        }
        if base >= 8 {
            rex |= 0x41;
        }
        if rex != 0 {
            self.emit(&[rex]);
        }
    }
}

fn low(reg: u8) -> u8 {
    reg & 7
}

fn modrm(mod_bits: u8, reg: u8, rm: u8) -> u8 {
    (mod_bits << 6) | (low(reg) << 3) | low(rm)
}

fn modrm_mod(mod_bits: u8, reg: u8, rm: u8) -> u8 {
    modrm(mod_bits, reg, rm)
}

fn sib(scale: u8, index: u8, base: u8) -> u8 {
    let scale_bits = match scale {
        0 | 1 => 0,
        2 => 1,
        4 => 2,
        _ => 3,
    };
    (scale_bits << 6) | (low(index) << 3) | low(base)
}
