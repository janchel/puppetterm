package action

import (
	"context"
	"io"
	"os/exec"
	"sync"
	"syscall"

	"github.com/puppetterm/agent/internal/protocol"
)

// execStream runs cmd, streaming its stdout/stderr as output events, and
// returns the process exit code. If stdin is non-nil it is fed to the command.
//
// The command runs in its own process group so a context cancel/timeout can
// kill the whole tree — otherwise grandchildren (e.g. `tail -f` children)
// would hold the output pipes open.
func execStream(
	ctx context.Context,
	reqID string,
	out *protocol.Encoder,
	cmd *exec.Cmd,
	stdin io.Reader,
) int {
	cmd.Stdin = stdin
	if cmd.SysProcAttr == nil {
		cmd.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
	}

	var stdout, stderr io.Reader
	if cmd.Stdout == nil {
		if p, err := cmd.StdoutPipe(); err == nil {
			stdout = p
		}
	}
	if cmd.Stderr == nil {
		if p, err := cmd.StderrPipe(); err == nil {
			stderr = p
		}
	}

	if err := cmd.Start(); err != nil {
		_ = out.Errorf(reqID, "start: %v", err)
		return 1
	}

	stop := context.AfterFunc(ctx, func() {
		if cmd.Process != nil {
			_ = syscall.Kill(-cmd.Process.Pid, syscall.SIGKILL)
		}
	})
	defer stop()

	var wg sync.WaitGroup
	if stdout != nil {
		wg.Add(1)
		go pumpStream(stdout, protocol.StreamStdout, reqID, out, &wg)
	}
	if stderr != nil {
		wg.Add(1)
		go pumpStream(stderr, protocol.StreamStderr, reqID, out, &wg)
	}
	wg.Wait()

	if err := cmd.Wait(); err != nil {
		if ctx.Err() != nil {
			_ = out.Errorf(reqID, "command stopped: %v", ctx.Err())
			return exitTimeout
		}
		if ee, ok := err.(*exec.ExitError); ok {
			return ee.ExitCode()
		}
		_ = out.Errorf(reqID, "command failed: %v", err)
		return 1
	}
	return 0
}
