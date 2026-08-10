// Package audit writes an append-only, per-invocation audit log on the host.
//
// Every action the agent executes is recorded with a timestamp, action,
// request id, exit code, and truncated params. The log is opened in append
// mode and never modified, so even a compromised client cannot rewrite it.
package audit

import (
	"fmt"
	"os"
	"path/filepath"
	"time"
)

// logPath resolves the audit log location.
//
//	PUPPETTERM_AUDIT_LOG  explicit path (tests / custom setups)
//	/var/log/puppetterm/  created + owned by the SSH user during install
//	~/.puppetterm/        fallback if /var/log is not writable
func logPath() string {
	if p := os.Getenv("PUPPETTERM_AUDIT_LOG"); p != "" {
		return p
	}
	preferred := "/var/log/puppetterm/audit.log"
	if f, err := os.OpenFile(preferred, os.O_APPEND|os.O_CREATE|os.O_WRONLY, 0o640); err == nil {
		f.Close()
		return preferred
	}
	if home, err := os.UserHomeDir(); err == nil && home != "" {
		dir := filepath.Join(home, ".puppetterm")
		_ = os.MkdirAll(dir, 0o700)
		return filepath.Join(dir, "audit.log")
	}
	return "puppetterm-audit.log"
}

// Record appends one audit line. Best-effort — failures are silently dropped
// so a logging problem can never break an action.
func Record(action, requestID, params string, exit int) {
	if len(params) > 500 {
		params = params[:500] + "..."
	}
	f, err := os.OpenFile(logPath(), os.O_APPEND|os.O_CREATE|os.O_WRONLY, 0o640)
	if err != nil {
		return
	}
	defer f.Close()
	ts := time.Now().UTC().Format(time.RFC3339)
	line := fmt.Sprintf("%s action=%s request_id=%s exit=%d params=%s\n",
		ts, action, requestID, exit, params)
	_, _ = f.WriteString(line)
}
