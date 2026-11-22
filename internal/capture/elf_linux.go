//go:build linux

package capture

import (
	"bytes"
	"debug/elf"
	"fmt"
	"os"
)

// The account key material passes through the cipher configuration helper,
// which references a fixed configuration string. The hook is the function
// reached from the anchor's referencing function through a relocated global
// data pointer. Instruction decoding validates every candidate; the ELF is
// only read, never executed.

const hookFingerprintLen = 16

var (
	x8664LeaRSI = [3]byte{0x48, 0x8d, 0x35}
	x8664LeaRDI = [3]byte{0x48, 0x8d, 0x3d}
)

type captureArch int

const (
	archX8664 captureArch = iota
	archAarch64
)

type hookLocation struct {
	architecture    captureArch
	fileOffset      uint64
	virtualAddress  uint64
	codeFingerprint [hookFingerprintLen]byte
}

type sectionData struct {
	address    uint64
	fileOffset uint64
	data       []byte
}

// findHook parses the desktop client binary, locates the account-material
// hook from the cipher configuration anchor, and returns its file offset,
// virtual address, and a short code fingerprint used to re-verify the process
// at attach time.
func findHook(binaryPath string) (hookLocation, error) {
	image, err := os.ReadFile(binaryPath)
	if err != nil {
		return hookLocation{}, fmt.Errorf("Cannot read desktop client binary: %w", err)
	}
	f, err := elf.NewFile(bytes.NewReader(image))
	if err != nil {
		return hookLocation{}, fmt.Errorf("Cannot parse desktop client ELF: %w", err)
	}
	defer f.Close()

	var architecture captureArch
	switch f.Machine {
	case elf.EM_X86_64:
		architecture = archX8664
	case elf.EM_AARCH64:
		architecture = archAarch64
	default:
		return hookLocation{}, fmt.Errorf("Unsupported Linux desktop client architecture: %s", f.Machine)
	}

	text, err := sectionBytes(f, ".text")
	if err != nil {
		return hookLocation{}, err
	}
	rodata, err := sectionBytes(f, ".rodata")
	if err != nil {
		return hookLocation{}, err
	}
	ehFrame, err := sectionBytes(f, ".eh_frame")
	if err != nil {
		return hookLocation{}, err
	}
	ehFrameHdr, err := sectionBytes(f, ".eh_frame_hdr")
	if err != nil {
		return hookLocation{}, err
	}
	unwind := &unwindSections{
		ehFrameAddr:    ehFrame.address,
		ehFrame:        ehFrame.data,
		ehFrameHdrAddr: ehFrameHdr.address,
		ehFrameHdr:     ehFrameHdr.data,
		order:          f.ByteOrder,
	}
	relocations, err := relocationOffsets(f)
	if err != nil {
		return hookLocation{}, err
	}

	var virtualAddress uint64
	switch architecture {
	case archX8664:
		virtualAddress, err = findX8664Hook(text, rodata, relocations, unwind)
	case archAarch64:
		virtualAddress, err = findAarch64Hook(text, rodata, relocations, unwind)
	}
	if err != nil {
		return hookLocation{}, err
	}

	relative := virtualAddress - text.address
	if virtualAddress < text.address || relative >= uint64(len(text.data)) {
		return hookLocation{}, fmt.Errorf("Located hook falls outside the ELF text section")
	}
	if relative+hookFingerprintLen > uint64(len(text.data)) {
		return hookLocation{}, fmt.Errorf("Located hook has no complete code fingerprint")
	}
	var fingerprint [hookFingerprintLen]byte
	copy(fingerprint[:], text.data[relative:relative+hookFingerprintLen])
	return hookLocation{
		architecture:    architecture,
		fileOffset:      text.fileOffset + relative,
		virtualAddress:  virtualAddress,
		codeFingerprint: fingerprint,
	}, nil
}

// fileOffsetForVA converts a hook's virtual address to the file offset a
// uprobe registers against, using the ELF loadable segments. The kernel
// matches a uprobe by file offset within the mapped inode, so the conversion
// goes through the program headers (PT_LOAD) rather than section headers.
func fileOffsetForVA(binaryPath string, va uint64) (uint64, error) {
	image, err := os.ReadFile(binaryPath)
	if err != nil {
		return 0, fmt.Errorf("Cannot read desktop client binary: %w", err)
	}
	f, err := elf.NewFile(bytes.NewReader(image))
	if err != nil {
		return 0, fmt.Errorf("Cannot parse desktop client ELF: %w", err)
	}
	defer f.Close()
	var segments []loadSegment
	for _, p := range f.Progs {
		if p.Type != elf.PT_LOAD {
			continue
		}
		segments = append(segments, loadSegment{
			vaddr:  p.Vaddr,
			offset: p.Off,
			filesz: p.Filesz,
		})
	}
	return fileOffsetFromSegments(segments, va)
}

func sectionBytes(f *elf.File, name string) (*sectionData, error) {
	s := f.Section(name)
	if s == nil {
		return nil, fmt.Errorf("Desktop client ELF is missing %s", name)
	}
	data, err := s.Data()
	if err != nil {
		return nil, fmt.Errorf("Cannot read ELF section %s: %w", name, err)
	}
	return &sectionData{address: s.Addr, fileOffset: s.Offset, data: data}, nil
}

// relocationOffsets collects the r_offset of every RELA entry (.rela.dyn and
// .rela.plt), i.e. every address the dynamic linker writes. A confirmed hook
// reaches its target through one of these relocated global data slots.
func relocationOffsets(f *elf.File) (map[uint64]bool, error) {
	out := map[uint64]bool{}
	found := false
	for _, s := range f.Sections {
		if s.Type != elf.SHT_RELA {
			continue
		}
		found = true
		data, err := s.Data()
		if err != nil {
			return nil, fmt.Errorf("Cannot read ELF relocation section %s: %w", s.Name, err)
		}
		// Elf64_Rela is r_offset(8) r_info(8) r_addend(8).
		for off := 0; off+24 <= len(data); off += 24 {
			out[f.ByteOrder.Uint64(data[off:off+8])] = true
		}
	}
	if !found {
		return nil, fmt.Errorf("Desktop client ELF has no dynamic relocation table")
	}
	return out, nil
}
