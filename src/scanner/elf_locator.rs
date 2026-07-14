use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use gimli::{BaseAddresses, EhFrame, EhFrameHdr, LittleEndian, UnwindSection};
use object::{Architecture, Object, ObjectSection};
use yaxpeax_arch::{Arch, Decoder, U8Reader};
use yaxpeax_arm::armv8::a64::{ARMv8, Instruction, Opcode, Operand, SizeCode};

use crate::dictionary;

use super::linux_process::ProcessIdentity;

const X86_64_LEA_RSI: &[u8; 3] = b"\x48\x8d\x35";
const X86_64_LEA_RDI: &[u8; 3] = b"\x48\x8d\x3d";

// ARM DDI 0487 defines ADRP by this fixed instruction-class mask. The semantic
// decoder still validates every candidate; the mask only avoids decoding the
// entire 70+ MiB text section multiple times.
const AARCH64_ADRP_CLASS_MASK: u32 = 0x9f00_0000;
const AARCH64_ADRP_CLASS: u32 = 0x9000_0000;
const HOOK_FINGERPRINT_LEN: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CaptureArchitecture {
    X86_64,
    Aarch64,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct HookLocation {
    pub(super) architecture: CaptureArchitecture,
    pub(super) file_offset: u64,
    pub(super) virtual_address: u64,
    pub(super) code_fingerprint: [u8; HOOK_FINGERPRINT_LEN],
}

struct SectionData<'a> {
    address: u64,
    file_offset: u64,
    data: &'a [u8],
}

struct UnwindSections<'a> {
    eh_frame_address: u64,
    eh_frame: &'a [u8],
    eh_frame_hdr_address: u64,
    eh_frame_hdr: &'a [u8],
    text_address: u64,
}

trait FunctionRanges {
    fn function_range(&self, address: u64) -> Result<std::ops::Range<u64>, String>;
}

impl FunctionRanges for UnwindSections<'_> {
    fn function_range(&self, address: u64) -> Result<std::ops::Range<u64>, String> {
        let bases = BaseAddresses::default()
            .set_eh_frame(self.eh_frame_address)
            .set_eh_frame_hdr(self.eh_frame_hdr_address)
            .set_text(self.text_address);
        let eh_frame = EhFrame::new(self.eh_frame, LittleEndian);
        let eh_frame_hdr = EhFrameHdr::new(self.eh_frame_hdr, LittleEndian);
        let parsed_header = eh_frame_hdr
            .parse(&bases, 8)
            .map_err(|error| format!("Cannot parse ELF unwind header: {error}"))?;
        let table = parsed_header
            .table()
            .ok_or_else(|| "ELF unwind header has no FDE table".to_string())?;
        let fde = table
            .fde_for_address(&eh_frame, &bases, address, EhFrame::cie_from_offset)
            .map_err(|error| {
                format!("Cannot resolve function boundary from ELF unwind data: {error}")
            })?;
        let start = fde.initial_address();
        let end = start
            .checked_add(fde.len())
            .ok_or_else(|| "ELF unwind function range overflow".to_string())?;
        Ok(start..end)
    }
}

pub(super) fn find_hook(binary_path: &Path) -> Result<HookLocation, String> {
    let image = std::fs::read(binary_path)
        .map_err(|error| format!("Cannot read desktop client binary: {error}"))?;
    let object = object::File::parse(image.as_slice())
        .map_err(|error| format!("Cannot parse desktop client ELF: {error}"))?;
    let architecture = match object.architecture() {
        Architecture::X86_64 => CaptureArchitecture::X86_64,
        Architecture::Aarch64 => CaptureArchitecture::Aarch64,
        architecture => {
            return Err(format!(
                "Unsupported Linux desktop client architecture: {architecture:?}"
            ))
        }
    };
    let text = section_data(&object, ".text")?;
    let rodata = section_data(&object, ".rodata")?;
    let eh_frame = section_data(&object, ".eh_frame")?;
    let eh_frame_hdr = section_data(&object, ".eh_frame_hdr")?;
    let unwind = UnwindSections {
        eh_frame_address: eh_frame.address,
        eh_frame: eh_frame.data,
        eh_frame_hdr_address: eh_frame_hdr.address,
        eh_frame_hdr: eh_frame_hdr.data,
        text_address: text.address,
    };
    let relocation_addresses = object
        .dynamic_relocations()
        .ok_or_else(|| "Desktop client ELF has no dynamic relocation table".to_string())?
        .map(|(address, _)| address)
        .collect::<BTreeSet<_>>();

    let virtual_address = match architecture {
        CaptureArchitecture::X86_64 => {
            find_x86_64_hook(&text, &rodata, &relocation_addresses, &unwind)?
        }
        CaptureArchitecture::Aarch64 => {
            find_aarch64_hook(&text, &rodata, &relocation_addresses, &unwind)?
        }
    };
    let relative = virtual_address
        .checked_sub(text.address)
        .filter(|relative| *relative < text.data.len() as u64)
        .ok_or_else(|| "Located hook falls outside the ELF text section".to_string())?;
    let relative_usize =
        usize::try_from(relative).map_err(|_| "Located hook offset is too large".to_string())?;
    let code_fingerprint: [u8; HOOK_FINGERPRINT_LEN] = text
        .data
        .get(relative_usize..relative_usize + HOOK_FINGERPRINT_LEN)
        .ok_or_else(|| "Located hook has no complete code fingerprint".to_string())?
        .try_into()
        .map_err(|_| "Invalid hook code fingerprint".to_string())?;
    Ok(HookLocation {
        architecture,
        file_offset: text.file_offset + relative,
        virtual_address,
        code_fingerprint,
    })
}

pub(super) fn runtime_address(
    identity: &ProcessIdentity,
    hook_file_offset: u64,
) -> Result<u64, String> {
    let maps_path = PathBuf::from(format!("/proc/{}/maps", identity.pid));
    let maps = std::fs::read_to_string(&maps_path)
        .map_err(|error| format!("Cannot read {}: {error}", maps_path.display()))?;
    let expected_inode = identity.executable_inode;
    let expected_path = identity.executable_path.to_string_lossy();

    for line in maps.lines() {
        let fields = line.split_whitespace().collect::<Vec<_>>();
        if fields.len() < 6 || !fields[1].contains('x') {
            continue;
        }
        let Some((start_text, end_text)) = fields[0].split_once('-') else {
            continue;
        };
        let (Ok(start), Ok(end), Ok(mapping_offset), Ok(inode)) = (
            u64::from_str_radix(start_text, 16),
            u64::from_str_radix(end_text, 16),
            u64::from_str_radix(fields[2], 16),
            fields[4].parse::<u64>(),
        ) else {
            continue;
        };
        if inode != expected_inode || fields[5] != expected_path {
            continue;
        }
        let mapping_size = end.saturating_sub(start);
        if hook_file_offset < mapping_offset
            || hook_file_offset >= mapping_offset.saturating_add(mapping_size)
        {
            continue;
        }
        return start
            .checked_add(hook_file_offset - mapping_offset)
            .ok_or_else(|| "Runtime hook address overflow".to_string());
    }

    Err(format!(
        "Cannot map capture hook into desktop client process {}",
        identity.pid
    ))
}

fn section_data<'data>(
    object: &object::File<'data>,
    name: &str,
) -> Result<SectionData<'data>, String> {
    let section = object
        .section_by_name(name)
        .ok_or_else(|| format!("Desktop client ELF is missing {name}"))?;
    let data = section
        .data()
        .map_err(|error| format!("Cannot read ELF section {name}: {error}"))?;
    let (file_offset, _) = section
        .file_range()
        .ok_or_else(|| format!("ELF section {name} has no file range"))?;
    Ok(SectionData {
        address: section.address(),
        file_offset,
        data,
    })
}

fn find_x86_64_hook(
    text: &SectionData<'_>,
    rodata: &SectionData<'_>,
    relocation_addresses: &BTreeSet<u64>,
    unwind: &impl FunctionRanges,
) -> Result<u64, String> {
    let mut candidates = BTreeSet::new();
    for anchor_offset in find_bytes(rodata.data, dictionary::cipher_config_anchor()) {
        let anchor_address = rodata.address + anchor_offset as u64;
        for first_ref in find_x86_rip_refs(text, X86_64_LEA_RSI, anchor_address) {
            if first_ref < 7 || text.data.get(first_ref - 7..first_ref - 4) != Some(X86_64_LEA_RDI)
            {
                continue;
            }
            let Some(displacement) = read_i32(text.data, first_ref - 4) else {
                continue;
            };
            let global_address = add_signed(text.address + first_ref as u64, displacement as i64)?;
            if !relocation_addresses.contains(&global_address) {
                continue;
            }
            let anchor_function = unwind.function_range(text.address + first_ref as u64)?;
            for second_ref in find_x86_rip_refs(text, X86_64_LEA_RSI, global_address) {
                let address = text.address + second_ref as u64;
                let function = unwind.function_range(address)?;
                if function.start != anchor_function.start {
                    candidates.insert(function.start);
                }
            }
        }
    }
    single_hook(candidates, "x86_64")
}

fn find_x86_rip_refs(text: &SectionData<'_>, opcode: &[u8; 3], target: u64) -> Vec<usize> {
    if text.data.len() < 7 {
        return Vec::new();
    }
    (0..=text.data.len() - 7)
        .filter(|offset| text.data.get(*offset..*offset + 3) == Some(opcode))
        .filter(|offset| {
            let Some(displacement) = read_i32(text.data, *offset + 3) else {
                return false;
            };
            add_signed(text.address + *offset as u64 + 7, displacement as i64).ok() == Some(target)
        })
        .collect()
}

fn find_aarch64_hook(
    text: &SectionData<'_>,
    rodata: &SectionData<'_>,
    relocation_addresses: &BTreeSet<u64>,
    unwind: &impl FunctionRanges,
) -> Result<u64, String> {
    let adrp_indices = aarch64_adrp_indices(text.data);
    let mut candidates = BTreeSet::new();
    for anchor_offset in find_bytes(rodata.data, dictionary::cipher_config_anchor()) {
        let anchor_address = rodata.address + anchor_offset as u64;
        for anchor_ref in aarch64_adrp_add_refs(text, &adrp_indices, anchor_address) {
            let anchor_function = unwind.function_range(instruction_address(text, anchor_ref))?;
            let anchor_block = aarch64_basic_block(text, anchor_function.clone(), anchor_ref)?;
            let slots = aarch64_load_targets_in_block(text, &adrp_indices, anchor_block);
            for slot in slots {
                if !relocation_addresses.contains(&slot) {
                    continue;
                }
                for slot_ref in aarch64_adrp_ldr_refs(text, &adrp_indices, slot) {
                    let function = unwind.function_range(instruction_address(text, slot_ref))?;
                    if function.start != anchor_function.start {
                        candidates.insert(function.start);
                    }
                }
            }
        }
    }
    single_hook(candidates, "AArch64")
}

fn aarch64_adrp_indices(text: &[u8]) -> Vec<usize> {
    (0..text.len() / 4)
        .filter(|index| {
            raw_instruction(text, *index)
                .is_some_and(|word| word & AARCH64_ADRP_CLASS_MASK == AARCH64_ADRP_CLASS)
        })
        .collect()
}

fn aarch64_adrp_add_refs(
    text: &SectionData<'_>,
    adrp_indices: &[usize],
    target: u64,
) -> Vec<usize> {
    adrp_indices
        .iter()
        .copied()
        .filter(|index| {
            let Some((register, page)) = decode_adrp(text, *index) else {
                return false;
            };
            for add_index in following_basic_block_indices(text.data, *index) {
                let Some(instruction) = decode_aarch64(text.data, add_index) else {
                    return false;
                };
                if decode_add_immediate_instruction(instruction).is_some_and(
                    |(destination, base, immediate)| {
                        destination == register
                            && base == register
                            && add_signed(page, immediate).ok() == Some(target)
                    },
                ) {
                    return true;
                }
                if instruction_writes_register(instruction, register) {
                    return false;
                }
            }
            false
        })
        .collect()
}

fn aarch64_load_targets_in_block(
    text: &SectionData<'_>,
    adrp_indices: &[usize],
    block: std::ops::Range<usize>,
) -> BTreeSet<u64> {
    adrp_indices
        .iter()
        .copied()
        .filter(|index| block.contains(index))
        .flat_map(|index| aarch64_load_targets_from(text, index))
        .collect()
}

fn aarch64_adrp_ldr_refs(
    text: &SectionData<'_>,
    adrp_indices: &[usize],
    target: u64,
) -> Vec<usize> {
    adrp_indices
        .iter()
        .copied()
        .filter(|index| aarch64_load_targets_from(text, *index).contains(&target))
        .collect()
}

fn aarch64_load_targets_from(text: &SectionData<'_>, adrp_index: usize) -> Vec<u64> {
    let Some((register, page)) = decode_adrp(text, adrp_index) else {
        return Vec::new();
    };
    for index in following_basic_block_indices(text.data, adrp_index) {
        let Some(instruction) = decode_aarch64(text.data, index) else {
            return Vec::new();
        };
        if let Some((_, base, displacement)) = decode_ldr_u64_instruction(instruction) {
            if base == register {
                return add_signed(page, displacement).into_iter().collect();
            }
        }
        if instruction_writes_register(instruction, register) {
            return Vec::new();
        }
    }
    Vec::new()
}

fn decode_adrp(text: &SectionData<'_>, index: usize) -> Option<(u16, u64)> {
    let instruction = decode_aarch64(text.data, index)?;
    let [Operand::Register(SizeCode::X, register), Operand::PCOffset(displacement), ..] =
        instruction.operands
    else {
        return None;
    };
    if instruction.opcode != Opcode::ADRP {
        return None;
    }
    let pc_page = instruction_address(text, index) & !0xfff;
    Some((register, add_signed(pc_page, displacement).ok()?))
}

#[cfg(test)]
fn decode_add_immediate(text: &[u8], index: usize) -> Option<(u16, u16, i64)> {
    let instruction = decode_aarch64(text, index)?;
    decode_add_immediate_instruction(instruction)
}

fn decode_add_immediate_instruction(instruction: Instruction) -> Option<(u16, u16, i64)> {
    if instruction.opcode != Opcode::ADD {
        return None;
    }
    let destination = register_or_sp(instruction.operands[0])?;
    let base = register_or_sp(instruction.operands[1])?;
    let immediate = match instruction.operands[2] {
        Operand::Immediate(value) => i64::from(value),
        Operand::ImmShift(value, shift) => i64::from(value) << shift,
        _ => return None,
    };
    Some((destination, base, immediate))
}

#[cfg(test)]
fn decode_ldr_u64(text: &[u8], index: usize) -> Option<(u16, u16, i64)> {
    let instruction = decode_aarch64(text, index)?;
    decode_ldr_u64_instruction(instruction)
}

fn decode_ldr_u64_instruction(instruction: Instruction) -> Option<(u16, u16, i64)> {
    if instruction.opcode != Opcode::LDR {
        return None;
    }
    let Operand::Register(SizeCode::X, destination) = instruction.operands[0] else {
        return None;
    };
    let Operand::RegPreIndex(base, displacement, false) = instruction.operands[1] else {
        return None;
    };
    Some((destination, base, i64::from(displacement)))
}

fn instruction_writes_register(instruction: Instruction, register: u16) -> bool {
    if is_store_opcode(instruction.opcode) || is_control_flow_opcode(instruction.opcode) {
        return false;
    }
    matches!(
        instruction.operands[0],
        Operand::Register(_, destination)
            | Operand::RegisterOrSP(_, destination)
            if destination == register
    )
}

fn is_store_opcode(opcode: Opcode) -> bool {
    matches!(
        opcode,
        Opcode::STLR
            | Opcode::STLLR
            | Opcode::STLRB
            | Opcode::STLLRB
            | Opcode::STLRH
            | Opcode::STLLRH
            | Opcode::STLXP
            | Opcode::STLXR
            | Opcode::STLXRB
            | Opcode::STLXRH
            | Opcode::STP
            | Opcode::STR
            | Opcode::STTR
            | Opcode::STTRB
            | Opcode::STTRH
            | Opcode::STRB
            | Opcode::STRH
            | Opcode::STRW
            | Opcode::STUR
            | Opcode::STURB
            | Opcode::STURH
            | Opcode::STXP
            | Opcode::STXR
            | Opcode::STXRB
            | Opcode::STXRH
    )
}

fn is_control_flow_opcode(opcode: Opcode) -> bool {
    matches!(
        opcode,
        Opcode::TBZ
            | Opcode::TBNZ
            | Opcode::CBZ
            | Opcode::CBNZ
            | Opcode::B
            | Opcode::BR
            | Opcode::Bcc(_)
            | Opcode::BL
            | Opcode::BLR
            | Opcode::RET
            | Opcode::ERET
            | Opcode::DRPS
            | Opcode::BLRAA
            | Opcode::BLRAAZ
            | Opcode::BLRAB
            | Opcode::BLRABZ
            | Opcode::BRAA
            | Opcode::BRAAZ
            | Opcode::BRAB
            | Opcode::BRABZ
            | Opcode::RETAA
            | Opcode::RETAB
            | Opcode::ERETAA
            | Opcode::ERETAB
            | Opcode::BCcc(_)
    )
}

fn register_or_sp(operand: Operand) -> Option<u16> {
    match operand {
        Operand::Register(SizeCode::X, register) | Operand::RegisterOrSP(SizeCode::X, register) => {
            Some(register)
        }
        _ => None,
    }
}

fn decode_aarch64(text: &[u8], index: usize) -> Option<Instruction> {
    let offset = index.checked_mul(4)?;
    let bytes = text.get(offset..offset + 4)?;
    let mut reader = U8Reader::new(bytes);
    <ARMv8 as Arch>::Decoder::default().decode(&mut reader).ok()
}

fn following_basic_block_indices(text: &[u8], index: usize) -> impl Iterator<Item = usize> + '_ {
    (index.saturating_add(1)..text.len() / 4)
        .take_while(|candidate| !is_control_flow(text, *candidate))
}

fn aarch64_basic_block(
    text: &SectionData<'_>,
    function: std::ops::Range<u64>,
    containing_index: usize,
) -> Result<std::ops::Range<usize>, String> {
    let function_start = address_to_instruction_index(text, function.start)?;
    let function_end = address_to_instruction_index(text, function.end)?;
    if containing_index < function_start || containing_index >= function_end {
        return Err("Anchor reference falls outside its unwind function".to_string());
    }

    let start = (function_start..containing_index)
        .rev()
        .find(|index| is_control_flow(text.data, *index))
        .map(|index| index + 1)
        .unwrap_or(function_start);
    let end = (containing_index..function_end)
        .find(|index| is_control_flow(text.data, *index))
        .unwrap_or(function_end);
    Ok(start..end)
}

fn address_to_instruction_index(text: &SectionData<'_>, address: u64) -> Result<usize, String> {
    let relative = address
        .checked_sub(text.address)
        .ok_or_else(|| "Unwind function precedes the text section".to_string())?;
    if relative % 4 != 0 || relative > text.data.len() as u64 {
        return Err("Unwind function boundary is not an AArch64 text address".to_string());
    }
    usize::try_from(relative / 4).map_err(|_| "AArch64 text offset is too large".to_string())
}

fn is_control_flow(text: &[u8], index: usize) -> bool {
    let Some(instruction) = decode_aarch64(text, index) else {
        return true;
    };
    is_control_flow_opcode(instruction.opcode)
}

fn instruction_address(text: &SectionData<'_>, index: usize) -> u64 {
    text.address + (index * 4) as u64
}

fn raw_instruction(text: &[u8], index: usize) -> Option<u32> {
    let offset = index.checked_mul(4)?;
    let bytes: [u8; 4] = text.get(offset..offset + 4)?.try_into().ok()?;
    Some(u32::from_le_bytes(bytes))
}

fn single_hook(candidates: BTreeSet<u64>, architecture: &str) -> Result<u64, String> {
    match candidates.len() {
        1 => Ok(*candidates.first().expect("length checked")),
        0 => Err(format!(
            "Cannot locate the {architecture} account-material hook from the cipher configuration anchor"
        )),
        count => Err(format!(
            "Found {count} possible {architecture} account-material hooks; refusing an ambiguous capture"
        )),
    }
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Vec<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return Vec::new();
    }
    haystack
        .windows(needle.len())
        .enumerate()
        .filter_map(|(offset, bytes)| (bytes == needle).then_some(offset))
        .collect()
}

fn add_signed(base: u64, displacement: i64) -> Result<u64, String> {
    if displacement >= 0 {
        base.checked_add(displacement as u64)
    } else {
        base.checked_sub(displacement.unsigned_abs())
    }
    .ok_or_else(|| "ELF relative address overflow".to_string())
}

fn read_i32(data: &[u8], offset: usize) -> Option<i32> {
    let bytes: [u8; 4] = data.get(offset..offset + 4)?.try_into().ok()?;
    Some(i32::from_le_bytes(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decoder_resolves_real_aarch64_address_materialization_instructions() {
        let text = SectionData {
            address: 0x064b_0a4c,
            file_offset: 0,
            data: &[
                0xc8, 0xe2, 0x00, 0x90, // adrp x8, slot page
                0x40, 0x50, 0xfd, 0xd0, // adrp x0, anchor page
                0x00, 0xf4, 0x12, 0x91, // add x0, x0, #0x4bd
                0x08, 0xf5, 0x47, 0xf9, // ldr x8, [x8, #0xfe8]
            ],
        };

        let (anchor_register, _) = decode_adrp(&text, 1).unwrap();
        let (destination, base, immediate) = decode_add_immediate(text.data, 2).unwrap();
        assert_eq!((anchor_register, destination, base), (0, 0, 0));
        assert_eq!(immediate, 0x4bd);
        assert!(decode_ldr_u64(text.data, 3).is_some());
    }

    #[test]
    fn hook_selection_rejects_ambiguity() {
        let error = single_hook(BTreeSet::from([0x1000, 0x2000]), "AArch64").unwrap_err();
        assert!(error.contains("ambiguous"));
    }

    #[test]
    fn resolves_external_client_elf_when_fixture_is_requested() {
        let Some(path) = std::env::var_os("TG_TEST_CLIENT_ELF") else {
            return;
        };
        let hook = find_hook(Path::new(&path)).unwrap();
        if let Some(expected) = std::env::var_os("TG_TEST_EXPECTED_HOOK") {
            let expected = expected.to_string_lossy();
            let expected = expected.strip_prefix("0x").unwrap_or(&expected);
            let expected = u64::from_str_radix(expected, 16).unwrap();
            assert_eq!(hook.virtual_address, expected);
        }
    }
}
