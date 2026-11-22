// Package par runs independent per-database work on a bounded number of
// goroutines and returns results in input order.
package par

import (
	"runtime"
	"sync"
)

// Auto caps for Jobs when the user passes --jobs 0. Source database reads
// are I/O and decrypt bound; more readers than this only contend.
const (
	SourceDBAutoCap  = 2
	QueryScanAutoCap = 4
)

// Jobs is requested when positive, else the CPU count capped at autoCap.
func Jobs(requested, autoCap int) int {
	if requested > 0 {
		return requested
	}
	return max(1, min(runtime.NumCPU(), max(autoCap, 1)))
}

// Map applies f to every item on up to jobs goroutines and returns results
// in input order.
func Map[T, R any](items []T, jobs int, f func(T) R) []R {
	out := make([]R, len(items))
	jobs = max(1, min(jobs, len(items)))
	if jobs == 1 {
		for i, it := range items {
			out[i] = f(it)
		}
		return out
	}
	var wg sync.WaitGroup
	next := make(chan int)
	for range jobs {
		wg.Go(func() {
			for i := range next {
				out[i] = f(items[i])
			}
		})
	}
	for i := range items {
		next <- i
	}
	close(next)
	wg.Wait()
	return out
}
