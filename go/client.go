package fastbrowse

import (
	"context"
	"crypto/rand"
	"encoding/base64"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"os/exec"
	"sync"
	"time"
)

// Three times the 10 s the server gives Chrome to exit before it kills it: a server killed first orphans that Chrome.
const defaultGracePeriod = 30 * time.Second

// Client drives one fastbrowse process, which runs one task at a time. Start several for runs in parallel.
type Client struct {
	version string
	browser BrowserOptions
	conn    *conn
	process *exec.Cmd
	// exited is closed once the process is gone and its output read to the end.
	exited chan struct{}

	mu   sync.Mutex
	runs map[string]*served

	closing sync.Once
	closed  error
}

// served is a run waiting on its reply, and the callbacks the server's calls about it go to.
type served struct {
	ctx     context.Context
	options RunOptions
	cancel  func()
	// failure is what ended the run from this side: the first OnEvent or OnFrame that returned an error.
	failure string
}

// Start starts a fastbrowse process and checks that it speaks this client's protocol version. Cancelling ctx stops
// a start that is still under way, the download of the binary included; it has no effect once Start has returned.
func Start(ctx context.Context, options Options) (*Client, error) {
	binary, err := resolveBinary(ctx, options.BinaryPath)
	if err != nil {
		return nil, err
	}
	process := exec.Command(binary, "serve", "--stdio")
	process.Env = os.Environ()
	for name, value := range options.Env {
		process.Env = append(process.Env, name+"="+value)
	}
	process.Stderr = options.Stderr
	if process.Stderr == nil {
		process.Stderr = os.Stderr
	}
	stdin, err := process.StdinPipe()
	if err != nil {
		return nil, err
	}
	stdout, err := process.StdoutPipe()
	if err != nil {
		return nil, err
	}
	if err := process.Start(); err != nil {
		return nil, fmt.Errorf("fastbrowse: could not start %s: %w", binary, err)
	}
	c := &Client{browser: options.Browser, conn: newConn(stdin), process: process, exited: make(chan struct{}), runs: map[string]*served{}}
	c.conn.serve = c.serve
	c.conn.notify = c.notify
	go func() {
		c.conn.read(stdout)
		_ = process.Wait()
		c.conn.end(&ProcessExitedError{State: process.ProcessState.String()})
		close(c.exited)
	}()

	result, err := c.request(ctx, MethodInitialize, InitializeParams{ProtocolVersion: ProtocolVersion})
	if err == nil {
		var hello InitializeResult
		if err = json.Unmarshal(result, &hello); err == nil && hello.ProtocolVersion != ProtocolVersion {
			err = fmt.Errorf("fastbrowse: this client speaks protocol version %d, and fastbrowse %s at %s speaks "+
				"protocol version %d: use matching versions of the two", ProtocolVersion, hello.FastbrowseVersion,
				binary, hello.ProtocolVersion)
		}
		c.version = hello.FastbrowseVersion
	}
	if err != nil {
		_ = process.Process.Kill()
		<-c.exited
		return nil, err
	}
	return c, nil
}

// Version is the version of the fastbrowse binary the client drives.
func (c *Client) Version() string { return c.version }

// request calls a method and waits for its reply, or for ctx.
func (c *Client) request(ctx context.Context, method string, params any) (json.RawMessage, error) {
	waiting, err := c.conn.call(method, params)
	if err != nil {
		return nil, err
	}
	select {
	case r := <-waiting:
		return r.result, r.err
	case <-ctx.Done():
		return nil, ctx.Err()
	}
}

// Run runs a task and returns its result, whatever status it ended in: a run that needs a login or got stuck is
// read from Status, and is not an error.
//
// The error is an *RPCError when the server refuses the request, which it does before a browser opens: for a bad
// option, for a missing key, and with ErrorCodeBusy while another run on this Client is active. A run that fails
// after that returns StatusError. It is a *ProcessExitedError when the fastbrowse process is gone.
//
// Cancelling ctx stops the run. Run then returns once the run's browser has closed, with an error that wraps
// ctx's cause. A ctx that is already done starts no run.
func (c *Client) Run(ctx context.Context, task string, options RunOptions) (*RunResult, error) {
	if err := context.Cause(ctx); err != nil {
		return nil, fmt.Errorf("fastbrowse: the run was not started: %w", err)
	}
	runID := newRunID()
	params, err := runParams(runID, task, c.browser, options)
	if err != nil {
		return nil, fmt.Errorf("fastbrowse: the output schema could not be written: %w", err)
	}
	var cancelling sync.Once
	cancel := func() {
		cancelling.Do(func() {
			// The reply says only that the request was read, and a process that has gone answers the run itself.
			_, _ = c.conn.call(MethodRunCancel, RunCancelParams{RunID: runID})
		})
	}
	run := &served{ctx: ctx, options: options, cancel: cancel}
	// The server names the run in every message it sends, which is how a run refused as busy hears nothing of the
	// one that is active.
	c.mu.Lock()
	c.runs[runID] = run
	c.mu.Unlock()
	defer func() {
		c.mu.Lock()
		delete(c.runs, runID)
		c.mu.Unlock()
	}()

	waiting, err := c.conn.call(MethodRun, params)
	if err != nil {
		return nil, err
	}
	var r reply
	select {
	case r = <-waiting:
	case <-ctx.Done():
		cancel()
		r = <-waiting
	}
	c.mu.Lock()
	failure := run.failure
	c.mu.Unlock()
	var refused *RPCError
	cancelled := errors.As(r.err, &refused) && refused.Code == ErrorCodeCancelled
	switch {
	case failure != "" && (r.err == nil || cancelled):
		// A run can finish before the cancellation reaches it, and its callback failed all the same.
		return endedBy(failure), nil
	case cancelled && ctx.Err() != nil:
		return nil, fmt.Errorf("fastbrowse: the run was stopped: %w", context.Cause(ctx))
	case r.err != nil:
		return nil, r.err
	}
	var result RunResult
	if err := json.Unmarshal(r.result, &result); err != nil {
		return nil, fmt.Errorf("fastbrowse: the result could not be read: %w", err)
	}
	return &result, nil
}

// Close asks the fastbrowse process to shut down, which stops a run under way, and waits until it has exited. A
// process still running when ctx is done, or after thirty seconds when ctx has no deadline, is killed. Every call
// after the first returns what the first did.
func (c *Client) Close(ctx context.Context) error {
	c.closing.Do(func() {
		// The reply says nothing the exit does not, and a process that died earlier answers with an error.
		_, _ = c.conn.call(MethodShutdown, nil)
		grace := time.NewTimer(defaultGracePeriod)
		defer grace.Stop()
		if _, ok := ctx.Deadline(); ok {
			grace.Stop()
		}
		select {
		case <-c.exited:
		case <-ctx.Done():
			_ = c.process.Process.Kill()
			<-c.exited
			c.closed = fmt.Errorf("fastbrowse: the process was killed: %w", context.Cause(ctx))
		case <-grace.C:
			_ = c.process.Process.Kill()
			<-c.exited
			c.closed = fmt.Errorf("fastbrowse: the process was killed after %s", defaultGracePeriod)
		}
	})
	return c.closed
}

func (c *Client) run(runID string) *served {
	c.mu.Lock()
	defer c.mu.Unlock()
	return c.runs[runID]
}

// fail ends a run from this side, for a callback that returned an error. Only the first failure counts.
func (c *Client) fail(run *served, method ServerMethod, err error) {
	c.mu.Lock()
	first := run.failure == ""
	if first {
		run.failure = fmt.Sprintf("%s: %s", method, err)
	}
	c.mu.Unlock()
	if first {
		run.cancel()
	}
}

// notify takes the server's notifications: what a run did, and frames of its page.
func (c *Client) notify(method string, params json.RawMessage) {
	switch ServerMethod(method) {
	case ServerMethodRunEvent:
		var message struct {
			RunID string `json:"run_id"`
			Event Event  `json:"event"`
		}
		if json.Unmarshal(params, &message) != nil {
			return
		}
		if run := c.run(message.RunID); run != nil && run.options.OnEvent != nil && !c.failed(run) {
			if err := run.options.OnEvent(message.Event); err != nil {
				c.fail(run, ServerMethodRunEvent, err)
			}
		}
	case ServerMethodRunFrame:
		var message RunFrame
		if json.Unmarshal(params, &message) != nil {
			return
		}
		jpeg, err := base64.StdEncoding.DecodeString(message.Frame)
		if err != nil {
			return
		}
		if run := c.run(message.RunID); run != nil && run.options.OnFrame != nil && !c.failed(run) {
			if err := run.options.OnFrame(jpeg); err != nil {
				c.fail(run, ServerMethodRunFrame, err)
			}
		}
	}
}

func (c *Client) failed(run *served) bool {
	c.mu.Lock()
	defer c.mu.Unlock()
	return run.failure != ""
}

// serve answers the server's requests: whether the run is done, and the value of a secret.
func (c *Client) serve(method string, params json.RawMessage) (any, error) {
	switch ServerMethod(method) {
	case ServerMethodRunUntil:
		var asked RunUntilParams
		if err := json.Unmarshal(params, &asked); err != nil {
			return nil, err
		}
		run := c.run(asked.RunID)
		if run == nil || run.options.Until == nil {
			return nil, fmt.Errorf("run %q has no until callback waiting", asked.RunID)
		}
		return run.options.Until(run.ctx, asked.URL)
	case ServerMethodSecretsResolve:
		var asked SecretsResolveParams
		if err := json.Unmarshal(params, &asked); err != nil {
			return nil, err
		}
		run := c.run(asked.RunID)
		if run == nil || run.options.Secrets == nil || run.options.Secrets.Resolve == nil {
			return nil, fmt.Errorf("run %q has no secrets resolver waiting", asked.RunID)
		}
		value, ok, err := run.options.Secrets.Resolve(run.ctx, asked.Name, asked.Origin)
		if err != nil {
			// What a resolver returns may quote the value, so it stays in this process.
			return nil, fmt.Errorf("the resolver failed for the secret %q", asked.Name)
		}
		if !ok {
			return nil, nil
		}
		return value, nil
	}
	return nil, &RPCError{Code: ErrorCodeMethodNotFound, Message: fmt.Sprintf("unknown method %q", method)}
}

func newRunID() string {
	id := make([]byte, 16)
	_, _ = rand.Read(id)
	return hex.EncodeToString(id)
}

// endedBy is the result of a run that a failing callback ended, in the shape the server gives one.
func endedBy(failure string) *RunResult {
	return &RunResult{
		Status:     StatusError,
		Data:       json.RawMessage("null"),
		Evidence:   []Evidence{},
		Steps:      []StepResult{},
		Cost:       CostBreakdown{Lines: []CostLine{}},
		Artifacts:  []Artifact{},
		Citations:  []Citation{},
		Error:      &failure,
		WouldFire:  []Tripwire{},
		Recordings: []string{},
	}
}
