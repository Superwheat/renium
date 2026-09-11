//! Read-only inspection of Studio's native loading path. Never opens a process.
#[cfg(windows)]
mod windows {
    use anyhow::{Context, Result, bail};
    use iced_x86::{Decoder, DecoderOptions, Mnemonic, OpKind};
    use std::time::Instant;

    struct Section {
        name: String,
        rva: usize,
        raw: usize,
        len: usize,
    }
    struct Image {
        bytes: Vec<u8>,
        base: u64,
        sections: Vec<Section>,
    }
    fn u32_at(bytes: &[u8], at: usize) -> Result<usize> {
        Ok(u32::from_le_bytes(bytes.get(at..at + 4).context("Truncated PE")?.try_into()?) as usize)
    }
    impl Image {
        fn new(bytes: Vec<u8>) -> Result<Self> {
            let pe = u32_at(&bytes, 0x3c)?;
            anyhow::ensure!(bytes.get(pe..pe + 4) == Some(b"PE\0\0"), "Not PE");
            let count = u16::from_le_bytes(bytes[pe + 6..pe + 8].try_into()?) as usize;
            let optional = pe + 24;
            let optional_size = u16::from_le_bytes(bytes[pe + 20..pe + 22].try_into()?) as usize;
            let base = u64::from_le_bytes(bytes[optional + 24..optional + 32].try_into()?);
            let mut sections = Vec::new();
            for i in 0..count {
                let row = optional + optional_size + i * 40;
                let name = bytes.get(row..row + 8).context("Truncated sections")?;
                let name = String::from_utf8_lossy(
                    &name[..name.iter().position(|b| *b == 0).unwrap_or(8)],
                )
                .into_owned();
                let (rva, len, raw) = (
                    u32_at(&bytes, row + 12)?,
                    u32_at(&bytes, row + 16)?,
                    u32_at(&bytes, row + 20)?,
                );
                anyhow::ensure!(bytes.get(raw..raw + len).is_some(), "Section outside image");
                sections.push(Section {
                    name,
                    rva,
                    raw,
                    len,
                });
            }
            Ok(Self {
                bytes,
                base,
                sections,
            })
        }
        fn section(&self, name: &str) -> Result<&Section> {
            self.sections
                .iter()
                .find(|s| s.name == name)
                .context("Missing section")
        }
        fn va(&self, raw: usize) -> Result<u64> {
            let s = self
                .sections
                .iter()
                .find(|s| (s.raw..s.raw + s.len).contains(&raw))
                .context("Unmapped file offset")?;
            Ok(self.base + (s.rva + raw - s.raw) as u64)
        }
        fn raw(&self, va: u64) -> Result<usize> {
            let rva = va.checked_sub(self.base).context("Address below image")? as usize;
            let s = self
                .sections
                .iter()
                .find(|s| (s.rva..s.rva + s.len).contains(&rva))
                .context("Unmapped address")?;
            Ok(s.raw + rva - s.rva)
        }
        fn disassemble(&self, start: usize, len: usize) -> Result<()> {
            let bytes = self
                .bytes
                .get(start..start + len)
                .context("Disassembly outside image")?;
            let mut decoder = Decoder::with_ip(64, bytes, self.va(start)?, DecoderOptions::NONE);
            while decoder.can_decode() {
                let i = decoder.decode();
                print!("{:x}: {:?}", self.raw(i.ip())?, i.mnemonic());
                for op in 0..i.op_count() {
                    print!(
                        " {}",
                        match i.op_kind(op) {
                            OpKind::Register => format!("{:?}", i.op_register(op)),
                            OpKind::NearBranch64 =>
                                format!("file:{:x}", self.raw(i.near_branch_target())?),
                            OpKind::Memory if i.is_ip_rel_memory_operand() => format!(
                                "[file:{:x}]",
                                self.raw(i.ip_rel_memory_address()).unwrap_or(0)
                            ),
                            OpKind::Memory => format!(
                                "[{:?}+{:?}*{}+{:#x}]",
                                i.memory_base(),
                                i.memory_index(),
                                i.memory_index_scale(),
                                i.memory_displacement64()
                            ),
                            _ => format!("{:#x}", i.immediate(op)),
                        }
                    );
                }
                println!();
            }
            Ok(())
        }
        fn find(&self, needle: &str) -> Result<()> {
            let positions =
                memchr::memmem::find_iter(&self.bytes, needle.as_bytes()).collect::<Vec<_>>();
            let targets = positions
                .iter()
                .map(|p| Ok((self.va(*p)?, *p)))
                .collect::<Result<std::collections::HashMap<_, _>>>()?;
            for pos in positions {
                let bytes = &self.bytes[pos..(pos + 180).min(self.bytes.len())];
                let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
                println!("string {pos:x}: {}", String::from_utf8_lossy(&bytes[..end]));
            }
            let text = self.section(".text")?;
            let pdata = self.section(".pdata")?;
            let functions = self.bytes[pdata.raw..pdata.raw + pdata.len]
                .chunks_exact(12)
                .map(|r| (u32_at(r, 0).unwrap(), u32_at(r, 4).unwrap()))
                .collect::<Vec<_>>();
            for index in
                memchr::memchr2_iter(0x48, 0x4c, &self.bytes[text.raw..text.raw + text.len])
            {
                let raw = text.raw + index;
                let Some(bytes) = self.bytes.get(raw..raw + 7) else {
                    continue;
                };
                if bytes[1] != 0x8d || bytes[2] & 0xc7 != 5 {
                    continue;
                }
                let i = Decoder::with_ip(64, bytes, self.va(raw)?, DecoderOptions::NONE).decode();
                if i.mnemonic() != Mnemonic::Lea || !i.is_ip_rel_memory_operand() {
                    continue;
                }
                if let Some(string) = targets.get(&i.ip_rel_memory_address()) {
                    let rva = (i.ip() - self.base) as usize;
                    let function = functions
                        .partition_point(|(begin, _)| *begin <= rva)
                        .checked_sub(1)
                        .and_then(|index| functions.get(index))
                        .filter(|(_, end)| rva < *end);
                    println!(
                        "xref {raw:x} string {string:x} function {:?}",
                        function.map(|(b, e)| (
                            format!("{:x}", self.raw(self.base + *b as u64).unwrap()),
                            format!("{:x}", self.raw(self.base + *e as u64 - 1).unwrap() + 1)
                        ))
                    );
                }
            }
            Ok(())
        }
        fn references(&self, target: usize) -> Result<()> {
            let target_va = self.va(target)?;
            for section in &self.sections {
                for offset in memchr::memmem::find_iter(
                    &self.bytes[section.raw..section.raw + section.len],
                    &target_va.to_le_bytes(),
                ) {
                    println!("pointer {:x}", section.raw + offset);
                }
            }
            let text = self.section(".text")?;
            let pdata = self.section(".pdata")?;
            for row in self.bytes[pdata.raw..pdata.raw + pdata.len].chunks_exact(12) {
                let begin = u32_at(row, 0)?;
                let end = u32_at(row, 4)?;
                if begin < text.rva || end > text.rva + text.len || end <= begin {
                    continue;
                }
                let raw = text.raw + begin - text.rva;
                let mut decoder = Decoder::with_ip(
                    64,
                    &self.bytes[raw..raw + end - begin],
                    self.base + begin as u64,
                    DecoderOptions::NONE,
                );
                while decoder.can_decode() {
                    let i = decoder.decode();
                    if (i.is_ip_rel_memory_operand() && i.ip_rel_memory_address() == target_va)
                        || (i.op_count() > 0
                            && i.op0_kind() == OpKind::NearBranch64
                            && i.near_branch_target() == target_va)
                    {
                        println!(
                            "xref {:x} {:?} function {raw:x} length {}",
                            self.raw(i.ip())?,
                            i.mnemonic(),
                            end - begin
                        );
                    }
                }
            }
            Ok(())
        }
        fn calls(&self, target: usize) -> Result<()> {
            let va = self.va(target)?;
            let pdata = self.section(".pdata")?;
            let row = self.bytes[pdata.raw..pdata.raw + pdata.len]
                .chunks_exact(12)
                .find(|row| self.base + u32_at(row, 0).unwrap() as u64 == va)
                .context("Target is not a function boundary")?;
            let len = u32_at(row, 4)? - u32_at(row, 0)?;
            println!("function {target:x} length {len}");
            for i in Decoder::with_ip(
                64,
                &self.bytes[target..target + len],
                va,
                DecoderOptions::NONE,
            ) {
                if matches!(i.mnemonic(), Mnemonic::Call | Mnemonic::Jmp)
                    && i.op0_kind() == OpKind::NearBranch64
                {
                    println!(
                        "{:x}: {:?} {:x}",
                        self.raw(i.ip())?,
                        i.mnemonic(),
                        self.raw(i.near_branch_target())?
                    );
                } else if i.is_ip_rel_memory_operand()
                    && let Ok(raw) = self.raw(i.ip_rel_memory_address())
                {
                    let bytes = &self.bytes[raw..(raw + 120).min(self.bytes.len())];
                    let len = bytes
                        .iter()
                        .take_while(|b| b.is_ascii_graphic() || **b == b' ')
                        .count();
                    if len >= 8 {
                        println!(
                            "{:x}: string {raw:x} {}",
                            self.raw(i.ip())?,
                            String::from_utf8_lossy(&bytes[..len])
                        );
                    }
                }
            }
            Ok(())
        }
        fn strings(&self, start: usize, len: usize) -> Result<()> {
            let bytes = self
                .bytes
                .get(start..start + len)
                .context("Strings outside image")?;
            let mut at = 0;
            while at < bytes.len() {
                let count = bytes[at..]
                    .iter()
                    .take_while(|b| b.is_ascii_graphic() || **b == b' ')
                    .count();
                if count >= 4 {
                    println!(
                        "{:x}: {}",
                        start + at,
                        String::from_utf8_lossy(&bytes[at..at + count])
                    );
                }
                at += count.max(1);
            }
            Ok(())
        }
    }
    pub fn run() -> Result<()> {
        let started = Instant::now();
        let args = std::env::args().skip(1).collect::<Vec<_>>();
        anyhow::ensure!(
            args.len() >= 3,
            "EXE find STRING | EXE refs HEX_OFFSET | EXE disasm|strings|qwords HEX_OFFSET LENGTH"
        );
        let image = Image::new(std::fs::read(&args[0])?)?;
        match args[1].as_str() {
            "find" => image.find(&args[2])?,
            "refs" => {
                image.references(usize::from_str_radix(args[2].trim_start_matches("0x"), 16)?)?
            }
            "calls" => image.calls(usize::from_str_radix(args[2].trim_start_matches("0x"), 16)?)?,
            "strings" => image.strings(
                usize::from_str_radix(args[2].trim_start_matches("0x"), 16)?,
                args.get(3).context("Missing length")?.parse()?,
            )?,
            "disasm" => image.disassemble(
                usize::from_str_radix(args[2].trim_start_matches("0x"), 16)?,
                args.get(3).context("Missing length")?.parse()?,
            )?,
            "qwords" => {
                let start = usize::from_str_radix(args[2].trim_start_matches("0x"), 16)?;
                let len: usize = args.get(3).context("Missing length")?.parse()?;
                for raw in (start..start + len).step_by(8) {
                    let value = u64::from_le_bytes(
                        image
                            .bytes
                            .get(raw..raw + 8)
                            .context("Truncated qword")?
                            .try_into()?,
                    );
                    println!(
                        "{raw:x}: {value:x} file:{:?}",
                        image.raw(value).ok().map(|v| format!("{v:x}"))
                    );
                }
            }
            _ => bail!("Unknown operation"),
        }
        eprintln!(
            "Read-only inspection: {:.3} ms",
            started.elapsed().as_secs_f64() * 1000.0
        );
        Ok(())
    }
}
#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    windows::run()
}
#[cfg(not(windows))]
fn main() {
    eprintln!("This read-only probe inspects the Windows Studio PE image.");
}
