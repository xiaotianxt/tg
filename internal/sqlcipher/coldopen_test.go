package sqlcipher

import (
	"os"
	"os/exec"
	"path/filepath"
	"sync"
	"testing"
)

// coldOpenEnv names the database a child process opens concurrently as its
// first SQLite work. Opening in a fresh process is the point: SQLCipher's
// setup runs during the first sqlite3_initialize, so only a cold process can
// race it.
const coldOpenEnv = "TG_SQLCIPHER_COLD_OPEN"

func TestMain(m *testing.M) {
	if path := os.Getenv(coldOpenEnv); path != "" {
		os.Exit(coldOpen(path))
	}
	os.Exit(m.Run())
}

func coldOpen(path string) int {
	var wg sync.WaitGroup
	errs := make(chan error, 16)
	for range 16 {
		wg.Add(1)
		go func() {
			defer wg.Done()
			c, err := Open(path, OpenOptions{Key: []byte(keySpec), ReadOnly: true})
			if err == nil {
				err = c.Exec("SELECT count(*) FROM t")
				c.Close()
			}
			if err != nil {
				errs <- err
			}
		}()
	}
	wg.Wait()
	close(errs)
	if err, ok := <-errs; ok {
		os.Stderr.WriteString(err.Error() + "\n")
		return 1
	}
	return 0
}

// Concurrent first opens in a fresh process must all key successfully.
func TestConcurrentOpensInColdProcess(t *testing.T) {
	path := filepath.Join(t.TempDir(), "x.db")
	w, err := Open(path, OpenOptions{Key: []byte(keySpec)})
	if err != nil {
		t.Fatal(err)
	}
	if err := w.Exec("CREATE TABLE t(x); INSERT INTO t VALUES (1)"); err != nil {
		t.Fatal(err)
	}
	w.Close()
	// Children run side by side: the race window is short and only shows
	// when the processes contend for CPU, as e2e's parallel tg runs do.
	var wg sync.WaitGroup
	for i := range 200 {
		wg.Add(1)
		go func() {
			defer wg.Done()
			cmd := exec.Command(os.Args[0], "-test.run=^$")
			cmd.Env = append(os.Environ(), coldOpenEnv+"="+path)
			if out, err := cmd.CombinedOutput(); err != nil {
				t.Errorf("cold process %d: %v\n%s", i, err, out)
			}
		}()
	}
	wg.Wait()
}
