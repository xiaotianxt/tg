// Package voice reads cached voice messages from the media database, sniffs
// their payload, and turns native voice payloads into WAV through the
// external native voice decoder.
package voice

import (
	"bytes"
	"errors"
	"fmt"
	"io"
	"os"
	"os/exec"
	"strconv"
	"strings"

	"github.com/xiaotianxt/tg/internal/dict"
	"github.com/xiaotianxt/tg/internal/env"
	"github.com/xiaotianxt/tg/internal/sqlcipher"
)

// Format is what a voice payload's leading bytes say it is.
type Format int

const (
	// Raw matches no known header.
	Raw Format = iota
	// NativeEncoded is a native voice stream the decoder turns into WAV.
	NativeEncoded
	// Amr is an AMR file.
	Amr
)

// Ext is the file extension a payload of this format is saved with.
func (f Format) Ext() string {
	switch f {
	case NativeEncoded:
		return "voice"
	case Amr:
		return "amr"
	default:
		return "aud"
	}
}

// Payload is the voice bytes to save, with any leading pad byte removed.
type Payload struct {
	Bytes  []byte
	Format Format
}

var amrHeader = []byte("#!AMR\n")

// Sniff classifies a VoiceInfo.voice_data blob. Native voice payloads may
// carry one leading pad byte, which is dropped. It reports false for an
// empty blob.
func Sniff(data []byte) (Payload, bool) {
	switch {
	case len(data) == 0:
		return Payload{}, false
	case bytes.HasPrefix(data, dict.NativeVoiceHeader):
		return Payload{data, NativeEncoded}, true
	case len(data) > 1 && bytes.HasPrefix(data[1:], dict.NativeVoiceHeader):
		return Payload{data[1:], NativeEncoded}, true
	case bytes.HasPrefix(data, amrHeader):
		return Payload{data, Amr}, true
	default:
		return Payload{data, Raw}, true
	}
}

// Row is one cached voice message.
type Row struct {
	LocalID int64
	Data    []byte
}

// LoadRows reads the voice rows of username from an open media_0.db
// connection (Name2Id.rowid → VoiceInfo.chat_name_id), oldest first. A chat
// absent from Name2Id has no rows.
func LoadRows(conn *sqlcipher.Conn, username string) ([]Row, error) {
	var chatID int64
	found, err := conn.QueryRow("SELECT rowid FROM Name2Id WHERE user_name = ?1", []any{username}, func(st *sqlcipher.Stmt) error {
		chatID = st.ColumnInt64(0)
		return nil
	})
	if err != nil {
		return nil, fmt.Errorf("resolve voice chat id: %w", err)
	}
	if !found {
		return nil, nil
	}
	var rows []Row
	err = conn.Query(`SELECT local_id, voice_data FROM VoiceInfo
		WHERE chat_name_id = ?1 AND create_time > 0
		ORDER BY create_time ASC, local_id ASC, svr_id ASC`, []any{chatID}, func(st *sqlcipher.Stmt) error {
		rows = append(rows, Row{LocalID: st.ColumnInt64(0), Data: st.ColumnBlob(1)})
		return nil
	})
	if err != nil {
		return nil, fmt.Errorf("read voice rows: %w", err)
	}
	return rows, nil
}

// SampleRate is the rate WAV output is decoded at.
const SampleRate = 24000

// Decoder runs the external native voice decoder.
type Decoder struct {
	// Path is the decoder executable.
	Path string
	// TempDir holds the short-lived native input file.
	TempDir string
}

// ResolveDecoder finds the decoder: $TG_VOICE_DECODER, else the Homebrew
// decoder command on PATH or a Homebrew prefix.
func ResolveDecoder(e env.Env) (Decoder, error) {
	p := e.Tool("TG_VOICE_DECODER", dict.NativeVoiceDecoderCommand)
	if p == "" {
		return Decoder{}, errors.New("no native voice decoder found. Install the Homebrew decoder or set TG_VOICE_DECODER")
	}
	return Decoder{Path: p, TempDir: e.TempDir}, nil
}

// wavHeaderLen is the canonical RIFF/WAVE header size the decoder writes.
const wavHeaderLen = 44

// DecodeWAV decodes a native voice payload into a WAV file at dest,
// replacing any earlier file. The protocol is
// `<decoder> decode -i <in>.voice -o <dest> --sample-rate 24000 --tolerant skip --wav`,
// and dest must then start with a complete RIFF/WAVE header. On error dest
// may hold partial output; the caller removes it.
func (d Decoder) DecodeWAV(native []byte, dest string) error {
	if err := os.Remove(dest); err != nil && !errors.Is(err, os.ErrNotExist) {
		return fmt.Errorf("remove previous WAV: %w", err)
	}
	in, err := os.CreateTemp(d.TempDir, "tg-voice-*.voice")
	if err != nil {
		return fmt.Errorf("create temporary native voice file: %w", err)
	}
	defer os.Remove(in.Name())
	_, err = in.Write(native)
	if cerr := in.Close(); err == nil {
		err = cerr
	}
	if err != nil {
		return fmt.Errorf("write temporary native voice file: %w", err)
	}

	cmd := exec.Command(d.Path, "decode", "-i", in.Name(), "-o", dest,
		"--sample-rate", strconv.Itoa(SampleRate), "--tolerant", "skip", "--wav")
	var stderr bytes.Buffer
	cmd.Stderr = &stderr
	if err := cmd.Run(); err != nil {
		var exit *exec.ExitError
		if !errors.As(err, &exit) {
			return fmt.Errorf("run voice decoder %s: %w", d.Path, err)
		}
		msg := fmt.Sprintf("voice decoder %s failed with %s", d.Path, exit.ProcessState)
		if s := strings.TrimSpace(stderr.String()); s != "" {
			msg += ": " + truncate(s, 500)
		}
		return errors.New(msg)
	}
	return checkWAVHeader(dest)
}

func checkWAVHeader(path string) error {
	f, err := os.Open(path)
	if err != nil {
		return fmt.Errorf("voice decoder did not produce a complete WAV header: %w", err)
	}
	defer f.Close()
	var h [wavHeaderLen]byte
	if _, err := io.ReadFull(f, h[:]); err != nil {
		return fmt.Errorf("voice decoder did not produce a complete WAV header: %w", err)
	}
	if string(h[:4]) != "RIFF" || string(h[8:12]) != "WAVE" {
		return errors.New("voice decoder produced an invalid WAV header")
	}
	return nil
}

func truncate(s string, maxRunes int) string {
	r := []rune(s)
	if len(r) <= maxRunes {
		return s
	}
	return string(r[:maxRunes]) + "..."
}

// Outcome is how one voice message was saved.
type Outcome int

const (
	// WAV: a native payload decoded to WAV.
	WAV Outcome = iota
	// Native: a native payload saved as is because no decoder was available
	// or decoding failed; Result.Reason says why.
	Native
	// Passthrough: a non-native payload (AMR or raw) saved as is.
	Passthrough
)

// Result is one exported voice message.
type Result struct {
	Outcome Outcome
	// Path is the written file.
	Path string
	// Format is the payload format.
	Format Format
	// Reason is why a Native outcome did not produce WAV: ErrNoDecoder or
	// the decode error (joined with any failure to remove partial WAV).
	Reason error
}

// ErrNoDecoder is Result.Reason when Export ran without a decoder.
var ErrNoDecoder = errors.New("no native voice decoder")

// Export saves one payload. pathFor maps an extension ("wav" or
// Format.Ext()) to the destination path; the caller owns file naming and
// the directory, which must exist. Native payloads are decoded with d (nil
// for none); a failed decode removes the partial WAV and falls back to
// writing the native file. The error covers only failures to write the
// fallback or passthrough file.
func Export(p Payload, d *Decoder, pathFor func(ext string) string) (Result, error) {
	if p.Format != NativeEncoded {
		path := pathFor(p.Format.Ext())
		return Result{Outcome: Passthrough, Path: path, Format: p.Format}, writePayload(path, p.Bytes)
	}
	reason := ErrNoDecoder
	if d != nil {
		wav := pathFor("wav")
		err := d.DecodeWAV(p.Bytes, wav)
		if err == nil {
			return Result{Outcome: WAV, Path: wav, Format: p.Format}, nil
		}
		if rmErr := os.Remove(wav); rmErr != nil && !errors.Is(rmErr, os.ErrNotExist) {
			err = errors.Join(err, fmt.Errorf("cannot remove failed voice WAV %s: %w", wav, rmErr))
		}
		reason = err
	}
	path := pathFor(p.Format.Ext())
	return Result{Outcome: Native, Path: path, Format: p.Format, Reason: reason}, writePayload(path, p.Bytes)
}

func writePayload(path string, data []byte) error {
	if err := os.WriteFile(path, data, 0o644); err != nil {
		return fmt.Errorf("cannot write voice file: %w", err)
	}
	return nil
}
