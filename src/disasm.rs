//! Disassembly length decoder: x86-64, table-driven.
//!
//! The contract is **length accuracy**: given bytes and an
//! offset, [`decode`] returns the instruction's exact length
//! and a best-effort text rendering. The text is a mnemonic plus
//! simple operand detail for the common forms (push/pop of a
//! register, `mov reg, imm`, relative branches with their
//! displacement, the one-byte staples); instruction forms the
//! renderer does not know get an honest generic text
//! (`op 0x..` / `vex 0x..`) — never an invented operand list.
//! Correct instruction boundaries over cleverness, per the
//! roadmap's scoping.
//!
//! Coverage: legacy prefixes, REX, the one-byte map, the 0F /
//! 0F38 / 0F3A maps (including 3DNow's suffix byte), VEX (2- and
//! 3-byte), EVEX, and AMD XOP, with ModRM / SIB / displacement /
//! immediate sizing per the Intel length rules in both 64-bit
//! and 67h-prefixed 32-bit addressing. In 64-bit mode 0xC4,
//! 0xC5 and 0x62 are unconditionally VEX / VEX / EVEX (LES, LDS
//! and BOUND do not exist in 64-bit mode); after a REX prefix
//! they are invalid, as hardware has it.
//!
//! Invalid opcodes (the ones 64-bit mode dropped: PUSH ES,
//! DAA, PUSHA, AAM, ...) and input that ends mid-instruction
//! return a typed [`DisasmError`] — never a panic. A caller
//! walking a stream advances by the returned length on success
//! and by one byte on error, so a walk always makes progress.

use std::fmt;

/// One decoded instruction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Instruction {
    /// Offset the instruction was decoded at.
    pub offset: usize,
    /// Its length in bytes (always at least 1).
    pub length: usize,
    /// Best-effort text (see the module docs).
    pub text: String,
}

/// Why a decode failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisasmError {
    /// The slice ends before the instruction does (or the offset
    /// is at/past the end).
    Truncated,
    /// The opcode is not a valid 64-bit instruction.
    Invalid,
}

impl fmt::Display for DisasmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DisasmError::Truncated => write!(f, "the input ends mid-instruction"),
            DisasmError::Invalid => write!(f, "not a valid 64-bit opcode"),
        }
    }
}

impl std::error::Error for DisasmError {}

/// Byte cursor over the input: every read is bounds-checked and
/// reports [`DisasmError::Truncated`] instead of panicking.
struct Dec<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Dec<'a> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn byte(&mut self) -> Result<u8, DisasmError> {
        let b = self.peek().ok_or(DisasmError::Truncated)?;
        self.pos += 1;
        Ok(b)
    }

    fn skip(&mut self, n: usize) -> Result<(), DisasmError> {
        if self.bytes.len() - self.pos < n {
            return Err(DisasmError::Truncated);
        }
        self.pos += n;
        Ok(())
    }

    /// Read `n` (at most 8) little-endian bytes as an unsigned
    /// value.
    fn uint(&mut self, n: usize) -> Result<u64, DisasmError> {
        let mut value: u64 = 0;
        for i in 0..n {
            value |= (self.byte()? as u64) << (8 * i);
        }
        Ok(value)
    }

    /// Read `n` bytes as a sign-extended displacement.
    fn disp(&mut self, n: usize) -> Result<i64, DisasmError> {
        let raw = self.uint(n)?;
        let shift = 64 - 8 * n;
        Ok(((raw << shift) as i64) >> shift)
    }
}

/// Prefix state that affects sizing.
#[derive(Clone, Copy, Default)]
struct Ctx {
    op16: bool,
    addr32: bool,
    rep: u8,
    rex: u8,
}

impl Ctx {
    /// Operand-size immediate length (imm16/32 forms).
    fn imm_z(&self) -> usize {
        if self.op16 {
            2
        } else {
            4
        }
    }
    /// Relative-branch displacement length (rel16/32 forms).
    fn rel_z(&self) -> usize {
        self.imm_z()
    }
}

/// Consume a ModRM byte plus any SIB and displacement it
/// implies; returns the ModRM `reg` field (groups need it).
fn sizing_modrm(d: &mut Dec, ctx: &Ctx) -> Result<u8, DisasmError> {
    let m = d.byte()?;
    let mode = m >> 6;
    let reg = (m >> 3) & 0x07;
    let rm = m & 0x07;
    if mode == 3 {
        return Ok(reg);
    }
    if ctx.addr32 {
        // 32-bit addressing: rm 4 = SIB, rm 5 with mod 0 = disp32.
        if rm == 4 {
            let sib = d.byte()?;
            if mode == 0 && (sib & 0x07) == 5 {
                d.skip(4)?;
            }
        } else if mode == 0 && rm == 5 {
            d.skip(4)?;
        }
    } else {
        // 64-bit addressing: rm 4 = SIB (whose base 5 with mod 0
        // means disp32 and no base), rm 5 with mod 0 = RIP + disp32.
        if rm == 4 {
            let sib = d.byte()?;
            if mode == 0 && (sib & 0x07) == 5 {
                d.skip(4)?;
            }
        } else if mode == 0 && rm == 5 {
            d.skip(4)?;
        }
    }
    match mode {
        1 => d.skip(1)?,
        2 => d.skip(4)?,
        _ => {}
    }
    Ok(reg)
}

/// A signed displacement the way listings write it: `+0x10`,
/// `-0x5` (sign and magnitude — the `{:+#x}` format would print
/// two's complement for negatives).
fn fmt_rel(v: i64) -> String {
    if v < 0 {
        format!("-0x{:x}", v.unsigned_abs())
    } else {
        format!("+0x{v:x}")
    }
}

const REG64: [&str; 8] = ["rax", "rcx", "rdx", "rbx", "rsp", "rbp", "rsi", "rdi"];

fn reg64(index: u8, rex_b: bool) -> String {
    let i = (index & 0x07) as usize;
    if rex_b {
        format!("r{}", i + 8)
    } else {
        REG64[i].to_string()
    }
}

/// Decode the instruction at `offset` in `bytes`.
pub fn decode(bytes: &[u8], offset: usize) -> Result<Instruction, DisasmError> {
    if offset >= bytes.len() {
        return Err(DisasmError::Truncated);
    }
    let mut d = Dec { bytes, pos: offset };
    let mut ctx = Ctx::default();
    // Legacy prefixes (any number, any order).
    while let Some(b) = d.peek() {
        match b {
            0x66 => ctx.op16 = true,
            0x67 => ctx.addr32 = true,
            0xF2 | 0xF3 => ctx.rep = b,
            0xF0 | 0x2E | 0x36 | 0x3E | 0x26 | 0x64 | 0x65 => {}
            _ => break,
        }
        d.byte()?;
    }
    // VEX / EVEX sit where the opcode would start (after legacy
    // prefixes, before any REX — a REX before them makes them
    // invalid, handled by the one-byte map never seeing them:
    // the REX loop below consumes the REX and the C4/C5/62 that
    // follows lands in the map's invalid set).
    let text = match d.peek() {
        Some(0xC4) | Some(0xC5) | Some(0x62) => decode_vex(&mut d, &ctx)?,
        _ => {
            // REX prefixes: only the last counts, all count for
            // length.
            while let Some(b) = d.peek() {
                if (0x40..=0x4F).contains(&b) {
                    ctx.rex = b;
                    d.byte()?;
                } else {
                    break;
                }
            }
            let op = d.byte()?;
            decode_opcode(&mut d, op, &ctx)?
        }
    };
    Ok(Instruction {
        offset,
        length: d.pos - offset,
        text,
    })
}

/// Decode starting at offset 0.
pub fn decode_one(bytes: &[u8]) -> Result<Instruction, DisasmError> {
    decode(bytes, 0)
}

/// The shared tail of VEX / EVEX / XOP decoding: ModRM sizing
/// and the map-dependent immediate, from the opcode byte on.
/// `map` is 1 (0F), 2 (0F38) or 3 (0F3A). `is_vex` gates the
/// 0F-map 0x77 special case (VZEROALL / VZEROUPPER exist only
/// in VEX encoding, not EVEX or XOP).
fn vex_tail(
    d: &mut Dec,
    ctx: &Ctx,
    map: u8,
    l_bit: u8,
    is_vex: bool,
) -> Result<String, DisasmError> {
    let op = d.byte()?;
    if is_vex && map == 1 && op == 0x77 {
        // VZEROUPPER (L=0) / VZEROALL (L=1): no ModRM.
        return Ok(if l_bit == 1 {
            "vzeroall".to_string()
        } else {
            "vzeroupper".to_string()
        });
    }
    sizing_modrm(d, ctx)?;
    let imm = match map {
        3 => 1, // 0F3A always carries an imm8
        1 if matches!(op, 0x70..=0x73 | 0xC2 | 0xC6) => 1,
        _ => 0,
    };
    d.skip(imm)?;
    Ok(format!("vex 0x{op:02x}"))
}

fn decode_vex(d: &mut Dec, ctx: &Ctx) -> Result<String, DisasmError> {
    let first = d.byte()?;
    match first {
        0xC5 => {
            let v = d.byte()?;
            vex_tail(d, ctx, 1, (v >> 2) & 1, true)
        }
        0xC4 => {
            let v1 = d.byte()?;
            let v2 = d.byte()?;
            let map = v1 & 0x1F;
            if !(1..=3).contains(&map) {
                return Err(DisasmError::Invalid);
            }
            vex_tail(d, ctx, map, (v2 >> 2) & 1, true)
        }
        0x62 => {
            // EVEX: three payload bytes; the map is P0's low two
            // bits. Length rules mirror VEX's maps.
            let p0 = d.byte()?;
            d.byte()?;
            d.byte()?;
            let map = p0 & 0x03;
            if map == 0 {
                return Err(DisasmError::Invalid);
            }
            vex_tail(d, ctx, map, 0, false)
        }
        _ => unreachable!("decode_vex called on a non-VEX byte"),
    }
}

/// One-byte-map and 0F-map decoding, from the (first) opcode
/// byte. Consumes everything after it and returns the text.
fn decode_opcode(d: &mut Dec, op: u8, ctx: &Ctx) -> Result<String, DisasmError> {
    // The opcodes 64-bit mode dropped.
    if matches!(
        op,
        0x06 | 0x07
            | 0x0E
            | 0x16
            | 0x17
            | 0x1E
            | 0x27
            | 0x2F
            | 0x37
            | 0x3F
            | 0x60
            | 0x61
            | 0x82
            | 0x9A
            | 0xC4
            | 0xC5
            | 0xCE
            | 0xD4
            | 0xD5
            | 0xD6
            | 0xEA
            | 0x62
    ) {
        return Err(DisasmError::Invalid);
    }
    let text: String = match op {
        0x0F => return decode_0f(d, ctx),
        // The ALU grid 0x00..=0x3F (r = op & 7).
        0x00..=0x3F => {
            let name =
                ["add", "or", "adc", "sbb", "and", "sub", "xor", "cmp"][(op >> 3) as usize & 0x07];
            match op & 0x07 {
                0..=3 => {
                    sizing_modrm(d, ctx)?;
                    name.to_string()
                }
                4 => {
                    d.skip(1)?;
                    name.to_string()
                }
                5 => {
                    d.skip(ctx.imm_z())?;
                    name.to_string()
                }
                // 6/7 were refused above or are segment
                // prefixes, which the prefix loop consumed.
                _ => return Err(DisasmError::Invalid),
            }
        }
        0x50..=0x57 => format!("push {}", reg64(op, ctx.rex & 1 != 0)),
        0x58..=0x5F => format!("pop {}", reg64(op, ctx.rex & 1 != 0)),
        0x63 => {
            sizing_modrm(d, ctx)?;
            "movsxd".to_string()
        }
        0x68 => {
            d.skip(ctx.imm_z())?;
            "push imm".to_string()
        }
        0x69 => {
            sizing_modrm(d, ctx)?;
            d.skip(ctx.imm_z())?;
            "imul".to_string()
        }
        0x6A => {
            d.skip(1)?;
            "push imm8".to_string()
        }
        0x6B => {
            sizing_modrm(d, ctx)?;
            d.skip(1)?;
            "imul".to_string()
        }
        0x70..=0x7F => {
            let name = [
                "jo", "jno", "jb", "jae", "je", "jne", "jbe", "ja", "js", "jns", "jp", "jnp", "jl",
                "jge", "jle", "jg",
            ][(op & 0x0F) as usize];
            let rel = d.disp(1)?;
            format!("{name} rel {}", fmt_rel(rel))
        }
        0x80 | 0x82 => {
            sizing_modrm(d, ctx)?;
            d.skip(1)?;
            "grp1".to_string()
        }
        0x81 => {
            sizing_modrm(d, ctx)?;
            d.skip(ctx.imm_z())?;
            "grp1".to_string()
        }
        0x83 => {
            sizing_modrm(d, ctx)?;
            d.skip(1)?;
            "grp1".to_string()
        }
        0x84..=0x8B => {
            sizing_modrm(d, ctx)?;
            ["test", "test", "xchg", "xchg", "mov", "mov", "mov", "mov"][(op - 0x84) as usize]
                .to_string()
        }
        0x8C | 0x8E => {
            sizing_modrm(d, ctx)?;
            "mov sreg".to_string()
        }
        0x8D => {
            sizing_modrm(d, ctx)?;
            "lea".to_string()
        }
        0x8F => {
            // POP group — unless the next byte opens an XOP
            // prefix (its map field, the low five bits, is >= 8).
            match d.peek() {
                Some(next) if next & 0x1F >= 8 => {
                    let v1 = d.byte()?;
                    d.byte()?; // v2
                    let map = (v1 & 0x1F) - 7; // XOP maps 8/9/10 -> 1/2/3
                    return vex_tail(d, ctx, map, 0, false);
                }
                _ => {
                    sizing_modrm(d, ctx)?;
                    "pop".to_string()
                }
            }
        }
        0x90 => {
            if ctx.rep == 0xF3 {
                "pause".to_string()
            } else {
                "nop".to_string()
            }
        }
        0x91..=0x97 => "xchg".to_string(),
        0x98 => "cwde".to_string(),
        0x99 => "cdq".to_string(),
        0x9B => "wait".to_string(),
        0x9C => "pushf".to_string(),
        0x9D => "popf".to_string(),
        0x9E => "sahf".to_string(),
        0x9F => "lahf".to_string(),
        0xA0..=0xA3 => {
            d.skip(if ctx.addr32 { 4 } else { 8 })?;
            "mov moffs".to_string()
        }
        0xA4..=0xA7 => "movs/cmps".to_string(),
        0xA8 => {
            d.skip(1)?;
            "test".to_string()
        }
        0xA9 => {
            d.skip(ctx.imm_z())?;
            "test".to_string()
        }
        0xAA..=0xAF => "stos/lods".to_string(),
        0xB0..=0xB7 => {
            let value = d.uint(1)?;
            format!("mov {}, 0x{value:x}", reg64(op, ctx.rex & 1 != 0))
        }
        0xB8..=0xBF => {
            let n = if ctx.rex & 0x08 != 0 { 8 } else { ctx.imm_z() };
            let value = d.uint(n)?;
            format!("mov {}, 0x{value:x}", reg64(op, ctx.rex & 1 != 0))
        }
        0xC0 | 0xC1 => {
            sizing_modrm(d, ctx)?;
            d.skip(1)?;
            "shift".to_string()
        }
        0xC2 => {
            let value = d.uint(2)?;
            format!("ret 0x{value:x}")
        }
        0xC3 => "ret".to_string(),
        0xC6 => {
            sizing_modrm(d, ctx)?;
            d.skip(1)?;
            "mov".to_string()
        }
        0xC7 => {
            sizing_modrm(d, ctx)?;
            d.skip(ctx.imm_z())?;
            "mov".to_string()
        }
        0xC8 => {
            d.skip(3)?;
            "enter".to_string()
        }
        0xC9 => "leave".to_string(),
        0xCA => {
            d.skip(2)?;
            "retf".to_string()
        }
        0xCB => "retf".to_string(),
        0xCC => "int3".to_string(),
        0xCD => {
            let value = d.uint(1)?;
            format!("int 0x{value:x}")
        }
        0xCF => "iret".to_string(),
        0xD0..=0xD3 => {
            sizing_modrm(d, ctx)?;
            "shift".to_string()
        }
        0xD7 => "xlat".to_string(),
        0xD8..=0xDF => {
            sizing_modrm(d, ctx)?;
            "x87".to_string()
        }
        0xE0..=0xE3 => {
            let rel = d.disp(1)?;
            format!("loop/jcxz rel {}", fmt_rel(rel))
        }
        0xE4..=0xE7 => {
            d.skip(1)?;
            "in/out".to_string()
        }
        0xE8 => {
            let rel = d.disp(ctx.rel_z())?;
            format!("call rel {}", fmt_rel(rel))
        }
        0xE9 => {
            let rel = d.disp(ctx.rel_z())?;
            format!("jmp rel {}", fmt_rel(rel))
        }
        0xEB => {
            let rel = d.disp(1)?;
            format!("jmp rel {}", fmt_rel(rel))
        }
        0xEC..=0xEF => "in/out".to_string(),
        0xF1 => "int1".to_string(),
        0xF4 => "hlt".to_string(),
        0xF5 => "cmc".to_string(),
        0xF6 => {
            let reg = sizing_modrm(d, ctx)?;
            if reg <= 1 {
                d.skip(1)?;
            }
            "grp3".to_string()
        }
        0xF7 => {
            let reg = sizing_modrm(d, ctx)?;
            if reg <= 1 {
                d.skip(ctx.imm_z())?;
            }
            "grp3".to_string()
        }
        0xF8..=0xFD => ["clc", "stc", "cli", "sti", "cld", "std"][(op - 0xF8) as usize].to_string(),
        0xFE | 0xFF => {
            sizing_modrm(d, ctx)?;
            if op == 0xFE {
                "grp4".to_string()
            } else {
                "grp5".to_string()
            }
        }
        // Remaining single bytes with no ModRM and no immediate:
        // INS/OUTS strings, misc flag and stack ops.
        0x6C..=0x6F => "ins/outs".to_string(),
        _ => format!("op 0x{op:02x}"),
    };
    Ok(text)
}

/// The 0F escape map: `d` sits just past the 0F byte.
fn decode_0f(d: &mut Dec, ctx: &Ctx) -> Result<String, DisasmError> {
    let op = d.byte()?;
    let text: String = match op {
        0x38 => {
            let op3 = d.byte()?;
            sizing_modrm(d, ctx)?;
            return Ok(format!("0f38 0x{op3:02x}"));
        }
        0x3A => {
            let op3 = d.byte()?;
            sizing_modrm(d, ctx)?;
            d.skip(1)?;
            return Ok(format!("0f3a 0x{op3:02x}"));
        }
        0x0F => {
            // 3DNow: ModRM addressing, then a suffix opcode byte.
            sizing_modrm(d, ctx)?;
            d.skip(1)?;
            return Ok("3dnow".to_string());
        }
        0x05 => "syscall".to_string(),
        0x0B => "ud2".to_string(),
        0x1F => {
            sizing_modrm(d, ctx)?;
            "nop".to_string()
        }
        0x80..=0x8F => {
            let rel = d.disp(ctx.rel_z())?;
            format!("jcc rel {}", fmt_rel(rel))
        }
        0xA2 => "cpuid".to_string(),
        0x31 => "rdtsc".to_string(),
        0x77 => "emms".to_string(),
        // ModRM, no immediate: the bulk of the map.
        0x00..=0x04
        | 0x0D
        | 0x10..=0x17
        | 0x18..=0x1E
        | 0x20..=0x27
        | 0x28..=0x2F
        | 0x40..=0x6F
        | 0x74..=0x7F
        | 0x90..=0x9F
        | 0xA3
        | 0xA5
        | 0xAB
        | 0xAD
        | 0xAE
        | 0xAF..=0xB9
        | 0xBB..=0xBF
        | 0xC0
        | 0xC1
        | 0xC3
        | 0xC5
        | 0xC7
        | 0xD0..=0xFF => {
            sizing_modrm(d, ctx)?;
            format!("0f 0x{op:02x}")
        }
        // ModRM plus an imm8.
        0x70..=0x73 | 0xA4 | 0xAC | 0xBA | 0xC2 | 0xC4 | 0xC6 => {
            sizing_modrm(d, ctx)?;
            d.skip(1)?;
            format!("0f 0x{op:02x}")
        }
        // No ModRM, no immediate (system and misc).
        0x06
        | 0x07
        | 0x08
        | 0x09
        | 0x30
        | 0x32
        | 0x33
        | 0x34
        | 0x35
        | 0x37
        | 0xA0
        | 0xA1
        | 0xA8
        | 0xA9
        | 0xAA
        | 0xC8..=0xCF => format!("0f 0x{op:02x}"),
        _ => return Err(DisasmError::Invalid),
    };
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// (bytes, expected length, expected text or None to skip the
    /// text check).
    const CORPUS: &[(&[u8], usize, Option<&str>)] = &[
        (&[0x55], 1, Some("push rbp")),
        (&[0x41, 0x54], 2, Some("push r12")),
        (&[0x5D], 1, Some("pop rbp")),
        (&[0x48, 0x89, 0xE5], 3, Some("mov")),
        (&[0x48, 0x83, 0xEC, 0x28], 4, Some("grp1")),
        (
            &[0x48, 0xB8, 0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11],
            10,
            None,
        ),
        (&[0xB8, 0x34, 0x12, 0x00, 0x00], 5, Some("mov rax, 0x1234")),
        (&[0x66, 0xB8, 0x34, 0x12], 4, Some("mov rax, 0x1234")),
        (&[0xB0, 0x7F], 2, Some("mov rax, 0x7f")),
        (&[0xE8, 0x10, 0x00, 0x00, 0x00], 5, Some("call rel +0x10")),
        (&[0xE9, 0xFB, 0xFF, 0xFF, 0xFF], 5, Some("jmp rel -0x5")),
        (&[0xEB, 0xFE], 2, Some("jmp rel -0x2")),
        (&[0x74, 0x05], 2, Some("je rel +0x5")),
        (&[0x0F, 0x84, 0x78, 0x56, 0x34, 0x12], 6, None),
        (&[0xC3], 1, Some("ret")),
        (&[0xC2, 0x08, 0x00], 3, Some("ret 0x8")),
        (&[0xCC], 1, Some("int3")),
        (&[0x90], 1, Some("nop")),
        (&[0xF3, 0x90], 2, Some("pause")),
        (&[0xF3, 0x0F, 0x1E, 0xFA], 4, Some("0f 0x1e")),
        (&[0x0F, 0x05], 2, Some("syscall")),
        (&[0x0F, 0x0B], 2, Some("ud2")),
        (&[0x0F, 0xA2], 2, Some("cpuid")),
        (&[0xFF, 0x15, 0x00, 0x00, 0x00, 0x00], 6, Some("grp5")),
        (&[0x48, 0x8B, 0x05, 0x00, 0x00, 0x00, 0x00], 7, Some("mov")),
        (
            &[0x48, 0x8B, 0x04, 0x25, 0x00, 0x00, 0x00, 0x00],
            8,
            Some("mov"),
        ),
        (&[0x48, 0x8B, 0x45, 0xF8], 4, Some("mov")),
        (&[0x48, 0x8B, 0x85, 0x00, 0x01, 0x00, 0x00], 7, Some("mov")),
        (&[0xF6, 0xC0, 0x01], 3, Some("grp3")), // test al, 1
        (&[0xF6, 0xD8], 2, Some("grp3")),       // neg al
        (&[0xF7, 0xC0, 0x01, 0x00, 0x00, 0x00], 6, Some("grp3")),
        (&[0xC0, 0xE0, 0x02], 3, Some("shift")), // shl al, 2
        (&[0xC8, 0x10, 0x00, 0x02], 4, Some("enter")),
        (
            &[0xA1, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
            9,
            Some("mov moffs"),
        ),
        (&[0x67, 0xA1, 0x00, 0x00, 0x00, 0x00], 6, Some("mov moffs")),
        (&[0x67, 0x48, 0x8B, 0x00], 4, Some("mov")), // 67 + REX + modrm (32-bit addressing)
        (
            &[0x67, 0x8B, 0x04, 0x25, 0x78, 0x56, 0x34, 0x12],
            8,
            Some("mov"),
        ),
        (&[0x0F, 0x38, 0xF1, 0xC0], 4, Some("0f38 0xf1")),
        (&[0x66, 0x0F, 0x3A, 0x0C, 0xC1, 0x08], 6, Some("0f3a 0x0c")),
        (&[0x0F, 0x0F, 0xC0, 0x9E], 4, Some("3dnow")),
        (&[0xC5, 0xF8, 0x77], 3, Some("vzeroupper")),
        (&[0xC5, 0xFC, 0x77], 3, Some("vzeroall")),
        (&[0xC5, 0xF8, 0x28, 0xC1], 4, None), // vmovaps xmm0, xmm1
        (&[0xC4, 0xE2, 0x7D, 0xA8, 0xC2], 5, None), // VEX 0F38: vfmadd-ish
        (&[0xC4, 0xE3, 0x79, 0x08, 0xC1, 0xFF], 6, None), // VEX 0F3A + imm8
        (&[0x62, 0xF1, 0x7C, 0x48, 0x28, 0xC1], 6, None), // EVEX vmovaps zmm
        (&[0x8F, 0x45, 0x00], 3, Some("pop")), // 8F /0, not XOP (map bits < 8)
        (&[0xD8, 0xC0], 2, Some("x87")),
        (&[0x63, 0xC0], 2, Some("movsxd")),
        (&[0x0F, 0x1F, 0x44, 0x00, 0x00], 5, Some("nop")), // multibyte nop
        (&[0x0F, 0xBA, 0xE0, 0x08], 4, Some("0f 0xba")),   // bt group: modrm + imm8
        (
            &[0x48, 0x63, 0x04, 0x25, 0x00, 0x00, 0x00, 0x00],
            8,
            Some("movsxd"),
        ),
    ];

    #[test]
    fn corpus_lengths_and_texts() {
        for (bytes, len, text) in CORPUS {
            let insn =
                decode(bytes, 0).unwrap_or_else(|e| panic!("bytes {bytes:02x?} refused: {e}"));
            assert_eq!(insn.length, *len, "bytes {bytes:02x?}");
            assert_eq!(insn.offset, 0);
            if let Some(want) = text {
                assert_eq!(&insn.text, want, "bytes {bytes:02x?}");
            }
        }
    }

    #[test]
    fn decode_at_an_offset() {
        let bytes = [0x90, 0x90, 0x55, 0xC3];
        let insn = decode(&bytes, 2).unwrap();
        assert_eq!(insn.offset, 2);
        assert_eq!(insn.length, 1);
        assert_eq!(insn.text, "push rbp");
        assert_eq!(decode(&bytes, 4), Err(DisasmError::Truncated));
        assert_eq!(decode(&[], 0), Err(DisasmError::Truncated));
    }

    #[test]
    fn every_proper_prefix_of_a_multibyte_instruction_is_truncated() {
        let samples: &[&[u8]] = &[
            &[0x48, 0xB8, 1, 2, 3, 4, 5, 6, 7, 8],
            &[0x48, 0x8B, 0x04, 0x25, 0, 0, 0, 0],
            &[0xE8, 0, 0, 0, 0],
            &[0xC4, 0xE2, 0x7D, 0xA8, 0xC2],
            &[0x62, 0xF1, 0x7C, 0x48, 0x28, 0xC1],
            &[0xF7, 0xC0, 1, 0, 0, 0],
            &[0x0F, 0x38, 0xF1, 0xC0],
            &[0xC8, 0x10, 0x00, 0x02],
        ];
        for sample in samples {
            assert!(decode(sample, 0).is_ok(), "full sample {sample:02x?}");
            for cut in 1..sample.len() {
                assert_eq!(
                    decode(&sample[..cut], 0),
                    Err(DisasmError::Truncated),
                    "sample {sample:02x?} cut at {cut}"
                );
            }
        }
    }

    #[test]
    fn dropped_64_bit_opcodes_are_invalid() {
        for op in [
            0x06, 0x07, 0x0E, 0x16, 0x27, 0x2F, 0x37, 0x3F, 0x60, 0x61, 0x82, 0xCE, 0xD6, 0xEA,
        ] {
            assert_eq!(
                decode(&[op], 0),
                Err(DisasmError::Invalid),
                "opcode {op:02x}"
            );
        }
        // A REX before VEX is invalid too.
        assert_eq!(
            decode(&[0x48, 0xC5, 0xF8, 0x77], 0),
            Err(DisasmError::Invalid)
        );
    }

    #[test]
    fn a_walk_over_a_mixed_buffer_lands_exactly_on_the_end() {
        let mut buffer: Vec<u8> = Vec::new();
        for (bytes, _, _) in CORPUS {
            buffer.extend_from_slice(bytes);
        }
        let mut pos = 0;
        let mut count = 0;
        while pos < buffer.len() {
            let insn = decode(&buffer, pos).unwrap_or_else(|e| panic!("walk failed at {pos}: {e}"));
            assert!(insn.length >= 1);
            pos += insn.length;
            count += 1;
        }
        assert_eq!(pos, buffer.len());
        assert_eq!(count, CORPUS.len());
    }
}
