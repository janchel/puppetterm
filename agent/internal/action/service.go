package action

import (
	"context"
	"encoding/json"
	"os/exec"
	"regexp"
	"strings"

	"github.com/puppetterm/agent/internal/protocol"
)

// unitPattern restricts unit names to systemd's typical safe charset.
var unitPattern = regexp.MustCompile(`^[A-Za-z0-9_.@:-]+$`)

// Read-only ops run as the SSH user; state-changing ops go through scoped
// `sudo -n` (granted by the installer's sudoers file).
var serviceReadOps = map[string]bool{
	"status": true, "is-active": true, "is-enabled": true,
}

var serviceOps = map[string]bool{
	"status": true, "is-active": true, "is-enabled": true,
	"start": true, "stop": true, "restart": true, "enable": true, "disable": true,
}

type serviceParams struct {
	Unit string `json:"unit"`
	Op   string `json:"op"`
}

type serviceResult struct {
	Unit    string `json:"unit"`
	Op      string `json:"op"`
	Active  string `json:"active,omitempty"`
	Enabled string `json:"enabled,omitempty"`
	Exit    int    `json:"exit"`
}

// Service controls systemd units via systemctl.
func Service(ctx context.Context, req protocol.Request, out *protocol.Encoder) int {
	var p serviceParams
	if err := json.Unmarshal(req.Params, &p); err != nil {
		_ = out.Errorf(req.RequestID, "invalid service params: %v", err)
		return 1
	}
	if p.Unit == "" || !unitPattern.MatchString(p.Unit) {
		_ = out.Errorf(req.RequestID, "invalid unit name %q", p.Unit)
		return 1
	}
	if !serviceOps[p.Op] {
		_ = out.Errorf(req.RequestID, "unsupported op %q", p.Op)
		return 1
	}

	cmdline := []string{"systemctl", p.Op, p.Unit}
	if !serviceReadOps[p.Op] {
		cmdline = append([]string{"sudo", "-n"}, cmdline...)
	}
	cmd := exec.CommandContext(ctx, cmdline[0], cmdline[1:]...)
	code := execStream(ctx, req.RequestID, out, cmd, nil)

	active := systemctlValue(ctx, "is-active", p.Unit)
	enabled := systemctlValue(ctx, "is-enabled", p.Unit)

	_ = out.Result(0, serviceResult{
		Unit: p.Unit, Op: p.Op, Active: active, Enabled: enabled, Exit: code,
	}, req.RequestID)
	return 0
}

func systemctlValue(ctx context.Context, op, unit string) string {
	cmd := exec.CommandContext(ctx, "systemctl", op, unit)
	out, err := cmd.Output()
	if err != nil {
		return ""
	}
	return strings.TrimSpace(string(out))
}
