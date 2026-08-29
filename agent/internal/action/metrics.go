package action

import (
	"bufio"
	"context"
	"os"
	"strconv"
	"strings"
	"time"

	"github.com/puppetterm/agent/internal/protocol"
)

// Metrics returns a compact live resource snapshot (CPU%, MEM%, load1) sampled
// on the host. CPU% is derived from two /proc/stat reads over a short interval
// so it reflects real usage rather than load average. Kept minimal (no disk/
// hostname/model payload like snapshot) so the client can poll it cheaply.
func Metrics(ctx context.Context, req protocol.Request, out *protocol.Encoder) int {
	cpuPct, err := sampleCPU(250 * time.Millisecond)
	if err != nil {
		_ = out.Errorf(req.RequestID, "cpu: %v", err)
		return 1
	}
	memPct, err := memPercent()
	if err != nil {
		_ = out.Errorf(req.RequestID, "mem: %v", err)
		return 1
	}
	load1 := loadOne()

	_ = out.Result(0, map[string]any{
		"cpu_percent": cpuPct,
		"mem_percent": memPct,
		"load1":       load1,
		"ts":          time.Now().UTC().Format(time.RFC3339),
	}, req.RequestID)
	return 0
}

// readProcStat returns the idle and total jiffy counts from the aggregate
// "cpu " line of /proc/stat.
func readProcStat() (idle, total uint64, err error) {
	f, e := os.Open("/proc/stat")
	if e != nil {
		return 0, 0, e
	}
	defer f.Close()
	sc := bufio.NewScanner(f)
	for sc.Scan() {
		line := sc.Text()
		if !strings.HasPrefix(line, "cpu ") {
			continue
		}
		fields := strings.Fields(line)[1:] // drop the "cpu" label
		var sum uint64
		for i, v := range fields {
			n, e2 := strconv.ParseUint(v, 10, 64)
			if e2 != nil {
				continue
			}
			sum += n
			if i == 3 { // idle is the 4th field (index 3)
				idle += n
			}
		}
		total = sum
		return idle, total, nil
	}
	return 0, 0, os.ErrNotExist
}

// sampleCPU reads /proc/stat twice over d and returns busy percentage.
func sampleCPU(d time.Duration) (float64, error) {
	i1, t1, e := readProcStat()
	if e != nil {
		return 0, e
	}
	time.Sleep(d)
	i2, t2, e := readProcStat()
	if e != nil {
		return 0, e
	}
	if t2 <= t1 {
		return 0, nil
	}
	totalDelta := t2 - t1
	idleDelta := i2 - i1
	busy := totalDelta - idleDelta
	if busy > totalDelta {
		busy = totalDelta
	}
	return float64(busy) / float64(totalDelta) * 100.0, nil
}

// memPercent returns (MemTotal - MemAvailable) / MemTotal * 100. MemAvailable
// already accounts for reclaimable caches, matching the snapshot's used% idea.
func memPercent() (float64, error) {
	f, e := os.Open("/proc/meminfo")
	if e != nil {
		return 0, e
	}
	defer f.Close()
	var total, avail uint64
	sc := bufio.NewScanner(f)
	for sc.Scan() {
		line := sc.Text()
		switch {
		case strings.HasPrefix(line, "MemTotal:"):
			total = parseKb(line)
		case strings.HasPrefix(line, "MemAvailable:"):
			avail = parseKb(line)
		}
	}
	if total == 0 {
		return 0, os.ErrNotExist
	}
	used := total - avail
	return float64(used) / float64(total) * 100.0, nil
}

func parseKb(line string) uint64 {
	fields := strings.Fields(line)
	if len(fields) < 2 {
		return 0
	}
	n, _ := strconv.ParseUint(fields[1], 10, 64)
	return n
}

// loadOne returns the 1-minute load average from /proc/loadavg.
func loadOne() float64 {
	data, e := os.ReadFile("/proc/loadavg")
	if e != nil {
		return 0
	}
	fields := strings.Fields(string(data))
	if len(fields) == 0 {
		return 0
	}
	v, _ := strconv.ParseFloat(fields[0], 64)
	return v
}
