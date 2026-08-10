package action

import (
	"bufio"
	"context"
	"fmt"
	"os"
	"runtime"
	"strings"
	"syscall"
	"time"

	"github.com/puppetterm/agent/internal/protocol"
)

// SystemSnapshot is the structured payload returned by the snapshot action.
type SystemSnapshot struct {
	Hostname      string      `json:"hostname"`
	Timestamp     string      `json:"timestamp"`
	UptimeSeconds int64       `json:"uptime_seconds"`
	Load          [3]float64  `json:"load"` // 1, 5, 15 minute load averages
	Mem           MemInfo     `json:"mem"`
	CPU           CPUInfo     `json:"cpu"`
	Disk          []DiskUsage `json:"disk"`
}

// MemInfo reports memory figures in kibibytes.
type MemInfo struct {
	TotalKB     uint64  `json:"total_kb"`
	AvailableKB uint64  `json:"available_kb"`
	UsedKB      uint64  `json:"used_kb"`
	PercentUsed float64 `json:"percent_used"`
}

// CPUInfo describes the CPU(s).
type CPUInfo struct {
	Model string `json:"model"`
	Cores int    `json:"cores"`
}

// DiskUsage reports usage for one filesystem.
type DiskUsage struct {
	Mount       string  `json:"mount"`
	TotalBytes  uint64  `json:"total_bytes"`
	FreeBytes   uint64  `json:"free_bytes"`
	UsedBytes   uint64  `json:"used_bytes"`
	PercentUsed float64 `json:"percent_used"`
}

// Snapshot collects system state and returns it as a structured result.
func Snapshot(ctx context.Context, req protocol.Request, out *protocol.Encoder) int {
	_ = out.Result(0, collectSnapshot(), req.RequestID)
	return 0
}

func collectSnapshot() SystemSnapshot {
	hostname, _ := os.Hostname()
	return SystemSnapshot{
		Hostname:      hostname,
		Timestamp:     time.Now().UTC().Format(time.RFC3339),
		UptimeSeconds: readUptime(),
		Load:          readLoad(),
		Mem:           readMem(),
		CPU:           CPUInfo{Model: readCPUModel(), Cores: runtime.NumCPU()},
		Disk:          readDisk(),
	}
}

func readUptime() int64 {
	data, err := os.ReadFile("/proc/uptime")
	if err != nil {
		return 0
	}
	var up float64
	if _, err := fmt.Sscanf(string(data), "%f", &up); err != nil {
		return 0
	}
	return int64(up)
}

func readLoad() [3]float64 {
	var l [3]float64
	data, err := os.ReadFile("/proc/loadavg")
	if err != nil {
		return l
	}
	fmt.Sscanf(string(data), "%f %f %f", &l[0], &l[1], &l[2])
	return l
}

func readMem() MemInfo {
	f, err := os.Open("/proc/meminfo")
	if err != nil {
		return MemInfo{}
	}
	defer f.Close()

	vals := map[string]uint64{}
	sc := bufio.NewScanner(f)
	for sc.Scan() {
		fields := strings.Fields(sc.Text())
		if len(fields) < 2 {
			continue
		}
		key := strings.TrimSuffix(fields[0], ":")
		var v uint64
		if _, err := fmt.Sscanf(fields[1], "%d", &v); err != nil {
			continue
		}
		vals[key] = v
	}

	total := vals["MemTotal"]
	available := vals["MemAvailable"]
	used := uint64(0)
	if total > available {
		used = total - available
	}
	percent := 0.0
	if total > 0 {
		percent = float64(used) / float64(total) * 100
	}
	return MemInfo{TotalKB: total, AvailableKB: available, UsedKB: used, PercentUsed: percent}
}

func readCPUModel() string {
	f, err := os.Open("/proc/cpuinfo")
	if err != nil {
		return ""
	}
	defer f.Close()

	sc := bufio.NewScanner(f)
	for sc.Scan() {
		line := sc.Text()
		if strings.HasPrefix(line, "model name") {
			if i := strings.Index(line, ":"); i >= 0 {
				return strings.TrimSpace(line[i+1:])
			}
		}
	}
	return ""
}

func readDisk() []DiskUsage {
	seen := map[string]string{} // device -> first mount point
	if f, err := os.Open("/proc/mounts"); err == nil {
		sc := bufio.NewScanner(f)
		for sc.Scan() {
			fields := strings.Fields(sc.Text())
			if len(fields) < 3 {
				continue
			}
			dev, mount, fstype := fields[0], fields[1], fields[2]
			// Keep real block devices only; skip pseudo/snap/squashfs mounts.
			if !strings.HasPrefix(dev, "/dev/") || strings.HasPrefix(mount, "/proc") ||
				strings.HasPrefix(mount, "/sys") || strings.HasPrefix(mount, "/dev") ||
				strings.HasPrefix(mount, "/snap") ||
				fstype == "tmpfs" || fstype == "overlay" || fstype == "squashfs" {
				continue
			}
			if _, ok := seen[dev]; !ok {
				seen[dev] = mount
			}
		}
		f.Close()
	}
	if len(seen) == 0 {
		seen["/dev/root"] = "/"
	}

	var out []DiskUsage
	for _, mount := range seen {
		var st syscall.Statfs_t
		if err := syscall.Statfs(mount, &st); err != nil {
			continue
		}
		bsize := uint64(st.Bsize)
		total := st.Blocks * bsize
		used := (st.Blocks - st.Bfree) * bsize
		free := st.Bavail * bsize
		percent := 0.0
		if total > 0 {
			percent = float64(used) / float64(total) * 100
		}
		out = append(out, DiskUsage{
			Mount:       mount,
			TotalBytes:  total,
			FreeBytes:   free,
			UsedBytes:   used,
			PercentUsed: percent,
		})
	}
	return out
}
