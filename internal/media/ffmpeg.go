package media

import (
	"bytes"
	"errors"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"

	"github.com/xiaotianxt/tg/internal/env"
	"github.com/xiaotianxt/tg/internal/par"
)

// FFmpegPath is the ffmpeg binary HEVC conversion runs: $TG_FFMPEG, else
// ffmpeg found on PATH or a Homebrew prefix. `tg doctor` reports the same
// one.
func FFmpegPath(e env.Env) (string, error) {
	if p := e.Tool("TG_FFMPEG", "ffmpeg"); p != "" {
		return p, nil
	}
	return "", errors.New("ffmpeg not found; converting tggf images needs it. Install it with `brew install ffmpeg` or set TG_FFMPEG to its path")
}

// jpegOutputArgs are shared by single and batched conversions; both must
// stay identical so a batch yields the same bytes as one process per image.
var jpegOutputArgs = []string{"-frames:v", "1", "-f", "image2pipe", "-vcodec", "mjpeg"}

// HEVCToJPEG converts one HEVC picture to JPEG with one ffmpeg process.
func HEVCToJPEG(ffmpeg string, hevc []byte) ([]byte, error) {
	args := []string{"-hide_banner", "-loglevel", "error", "-f", "hevc", "-i", "pipe:0"}
	args = append(append(args, jpegOutputArgs...), "pipe:1")
	cmd := exec.Command(ffmpeg, args...)
	cmd.Stdin = bytes.NewReader(hevc)
	var stdout, stderr bytes.Buffer
	cmd.Stdout, cmd.Stderr = &stdout, &stderr
	if err := cmd.Run(); err != nil {
		var exit *exec.ExitError
		if !errors.As(err, &exit) {
			return nil, fmt.Errorf("start ffmpeg (%s) for tggf: %w", ffmpeg, err)
		}
		return nil, fmt.Errorf("ffmpeg tggf decode failed: %s", strings.TrimSpace(stderr.String()))
	}
	if DetectExt(stdout.Bytes()) != "jpg" {
		return nil, errors.New("ffmpeg tggf output is not JPEG")
	}
	return stdout.Bytes(), nil
}

// HEVCBatchToJPEG converts several HEVC pictures with one ffmpeg process.
//
// Process start-up costs about as much CPU as decoding one image. Each input
// gets its own decoder and encoder, which yields the same bytes as
// HEVCToJPEG. Inputs skip stream probing (about a third of the CPU), so each
// output maps its input explicitly; an ffmpeg that cannot run unprobed inputs
// fails the batch. Any failure fails the whole batch.
func HEVCBatchToJPEG(ffmpeg string, hevcs [][]byte) ([][]byte, error) {
	// The 0700 directory holds plaintext only until it is removed.
	dir, err := os.MkdirTemp("", "tg-hevc-")
	if err != nil {
		return nil, fmt.Errorf("create temporary HEVC dir: %w", err)
	}
	defer os.RemoveAll(dir)

	args := []string{"-hide_banner", "-loglevel", "error", "-nostdin"}
	for i, hevc := range hevcs {
		input := filepath.Join(dir, strconv.Itoa(i)+".hevc")
		if err := os.WriteFile(input, hevc, 0o600); err != nil {
			return nil, fmt.Errorf("write temporary HEVC: %w", err)
		}
		args = append(args, "-nofind_stream_info", "-f", "hevc", "-i", input)
	}
	outputs := make([]string, len(hevcs))
	for i := range hevcs {
		outputs[i] = filepath.Join(dir, strconv.Itoa(i)+".jpg")
		args = append(args, "-map", strconv.Itoa(i)+":v")
		args = append(append(args, jpegOutputArgs...), outputs[i])
	}
	cmd := exec.Command(ffmpeg, args...)
	var stderr bytes.Buffer
	cmd.Stderr = &stderr
	if err := cmd.Run(); err != nil {
		var exit *exec.ExitError
		if !errors.As(err, &exit) {
			return nil, fmt.Errorf("start ffmpeg (%s) for tggf: %w", ffmpeg, err)
		}
		return nil, fmt.Errorf("ffmpeg tggf batch failed: %s", strings.TrimSpace(stderr.String()))
	}
	jpgs := make([][]byte, len(outputs))
	for i, p := range outputs {
		jpg, err := os.ReadFile(p)
		if err != nil {
			return nil, fmt.Errorf("read ffmpeg batch output: %w", err)
		}
		if DetectExt(jpg) != "jpg" {
			return nil, errors.New("ffmpeg tggf output is not JPEG")
		}
		jpgs[i] = jpg
	}
	return jpgs, nil
}

// JPEGBatchSize is images per ffmpeg process. Each keeps a decoder and an
// encoder alive, so larger batches trade memory for start-up cost; 64 was
// slower than 16.
const JPEGBatchSize = 16

// JPEGResult is one converted picture or why it failed.
type JPEGResult struct {
	JPEG []byte
	Err  error
}

// HEVCsToJPEG converts many pictures in batches of JPEGBatchSize on up to
// jobs processes. A failed batch is retried one picture at a time so each
// reports its own error. Results are in input order.
func HEVCsToJPEG(ffmpeg string, hevcs [][]byte, jobs int) []JPEGResult {
	var batches [][][]byte
	for start := 0; start < len(hevcs); start += JPEGBatchSize {
		batches = append(batches, hevcs[start:min(start+JPEGBatchSize, len(hevcs))])
	}
	converted := par.Map(batches, jobs, func(batch [][]byte) []JPEGResult {
		out := make([]JPEGResult, len(batch))
		if jpgs, err := HEVCBatchToJPEG(ffmpeg, batch); err == nil {
			for i, jpg := range jpgs {
				out[i].JPEG = jpg
			}
			return out
		}
		for i, hevc := range batch {
			out[i].JPEG, out[i].Err = HEVCToJPEG(ffmpeg, hevc)
		}
		return out
	})
	results := make([]JPEGResult, 0, len(hevcs))
	for _, batch := range converted {
		results = append(results, batch...)
	}
	return results
}

// TGGFToJPEG converts a TGGF container to JPEG.
func TGGFToJPEG(ffmpeg string, data []byte) ([]byte, error) {
	hevc, err := TGGFPartition(data)
	if err != nil {
		return nil, err
	}
	return HEVCToJPEG(ffmpeg, hevc)
}
