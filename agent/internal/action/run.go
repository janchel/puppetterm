package action

import (
	"context"
	"encoding/json"
	"io"
	"os"
	"os/exec"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"syscall"

	"github.com/puppetterm/agent/internal/protocol"
)

// runParams configures a single shell invocation.
type runParams struct {
	Cmd   string   `json:"cmd"`
	Shell string   `json:"shell,omitempty"` // defaults to /bin/sh
	Env   []string `json:"env,omitempty"`   // KEY=VALUE entries appended to the environment
	Dir   string   `json:"dir,omitempty"`   // working directory
}

// exitTimeout is returned when a command is killed by its context (timeout/cancel).
const exitTimeout = 124

// Run executes a shell command, streaming stdout/stderr as output events.
func Run(ctx context.Context, req protocol.Request, out *protocol.Encoder) int {
	var p runParams
	if err := json.Unmarshal(req.Params, &p); err != nil {
		_ = out.Errorf(req.RequestID, "invalid run params: %v", err)
		return 1
	}
	if p.Cmd == "" {
		_ = out.Errorf(req.RequestID, "run action requires a 'cmd' parameter")
		return 1
	}

	shell := p.Shell
	if shell == "" {
		shell = "/bin/sh"
	}
	// Start the command in its own process group so a timeout can kill the
	// whole group. Killing only the direct child (e.g. /bin/sh) would leave
	// grandchildren (e.g. `sh -c "sleep 60"`) holding the output pipes open.
	cmd := exec.Command(shell, "-c", p.Cmd)
	cmd.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
	if len(p.Env) > 0 {
		cmd.Env = append(os.Environ(), p.Env...)
	}
	if p.Dir != "" {
		cmd.Dir = p.Dir
	}

	stdout, err := cmd.StdoutPipe()
	if err != nil {
		_ = out.Errorf(req.RequestID, "stdout pipe: %v", err)
		return 1
	}
	stderr, err := cmd.StderrPipe()
	if err != nil {
		_ = out.Errorf(req.RequestID, "stderr pipe: %v", err)
		return 1
	}

	if err := cmd.Start(); err != nil {
		_ = out.Errorf(req.RequestID, "start: %v", err)
		return 1
	}
	activeCmdPID.Store(int64(cmd.Process.Pid))
	defer activeCmdPID.Store(0)

	// Kill the entire process group when the context (timeout/cancel) fires.
	stop := context.AfterFunc(ctx, func() {
		if cmd.Process != nil {
			_ = syscall.Kill(-cmd.Process.Pid, syscall.SIGKILL)
		}
	})
	defer stop()

	var wg sync.WaitGroup
	wg.Add(2)
	// Bound the output we send back so a single huge command (e.g. `cat` on a
	// large log) can't blow the model's context. PUPPETTERM_AGENT_OUTPUT_WORDS
	// tunes the per-command word budget (0 = unlimited). Beyond the budget we
	// stop streaming and emit a marker telling the model to narrow the command.
	if limit := maxOutputWords(); limit <= 0 {
		go pumpStream(stdout, protocol.StreamStdout, req.RequestID, out, &wg)
		go pumpStream(stderr, protocol.StreamStderr, req.RequestID, out, &wg)
	} else {
		var budget int64 = limit
		var exceeded int32
		go pumpStreamCapped(stdout, protocol.StreamStdout, req.RequestID, out, &wg, &budget, &exceeded)
		go pumpStreamCapped(stderr, protocol.StreamStderr, req.RequestID, out, &wg, &budget, &exceeded)
	}
	wg.Wait()

	if err := cmd.Wait(); err != nil {
		if ctx.Err() != nil {
			_ = out.Errorf(req.RequestID, "command stopped: %v", ctx.Err())
			return exitTimeout
		}
		if ee, ok := err.(*exec.ExitError); ok {
			_ = out.Result(ee.ExitCode(), nil, req.RequestID)
			return 0
		}
		_ = out.Errorf(req.RequestID, "run failed: %v", err)
		return 1
	}

	_ = out.Result(0, nil, req.RequestID)
	return 0
}

// pumpStream copies chunks from r into output events until EOF.
func pumpStream(r io.Reader, stream, requestID string, out *protocol.Encoder, wg *sync.WaitGroup) {
	defer wg.Done()
	buf := make([]byte, 32*1024)
	for {
		n, err := r.Read(buf)
		if n > 0 {
			_ = out.Output(stream, string(buf[:n]), requestID)
		}
		if err != nil {
			return
		}
	}
}

// maxOutputWords returns the per-command word budget (0 = unlimited),
// tunable via PUPPETTERM_AGENT_OUTPUT_WORDS.
func maxOutputWords() int64 {
	if v := os.Getenv("PUPPETTERM_AGENT_OUTPUT_WORDS"); v != "" {
		if n, err := strconv.ParseInt(v, 10, 64); err == nil {
			return n
		}
	}
	return 24000
}

// outputTruncatedMarker tells the model the stream was cut and how to narrow.
func outputTruncatedMarker() string {
	return "\n[OUTPUT TRUNCATED — only the first ~" + strconv.FormatInt(maxOutputWords(), 10) +
		" words of this command's output were returned. To read more, narrow the command: " +
		"use `grep \"pattern\"`, `head -n N`, `tail -n N`, or `sed -n 'START,ENDp' <file>`, " +
		"or use the `read` tool with an `offset`/`limit` to page through the file. " +
		"Prefer grep-first on large logs.]\n"
}

// pumpStreamCapped is like pumpStream but stops emitting once the shared word
// budget is exhausted, then emits a single truncation marker. The budget and
// exceeded flag are shared across the stdout/stderr pumps.
func pumpStreamCapped(r io.Reader, stream, requestID string, out *protocol.Encoder, wg *sync.WaitGroup, budget *int64, exceeded *int32) {
	defer wg.Done()
	buf := make([]byte, 32*1024)
	for {
		n, err := r.Read(buf)
		if n > 0 {
			if atomic.LoadInt32(exceeded) == 1 {
				// already over budget — drop the rest of this stream
			} else {
			chunk := string(buf[:n])
			words := int64(len(strings.Fields(chunk)))
			remaining := atomic.AddInt64(budget, -words)
			_ = out.Output(stream, chunk, requestID)
			if remaining <= 0 && atomic.CompareAndSwapInt32(exceeded, 0, 1) {
				_ = out.Output(stream, outputTruncatedMarker(), requestID)
			}
			}
		}
		if err != nil {
			return
		}
	}
}
