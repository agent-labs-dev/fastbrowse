package fastbrowse

import "fmt"

// RPCError is the server's error reply to a request. Code is one of the protocol's error codes: ErrorCodeBusy
// while another run on the same Client is active, ErrorCodeConfiguration for a missing key, ErrorCodeInvalidParams
// for an option the server refuses.
type RPCError struct {
	Code    ErrorCode
	Message string
	// What the run did and spent before it stopped, on ErrorCodeCancelled only; nil on every other code.
	Receipt *CancellationReceipt
}

func (e *RPCError) Error() string { return fmt.Sprintf("fastbrowse: %s (code %d)", e.Message, e.Code) }

// CancelledError is what Run returns when its ctx stopped the run. It wraps ctx's cause, so
// errors.Is(err, context.Canceled) and errors.Is(err, context.DeadlineExceeded) hold as they would for ctx.Err().
type CancelledError struct {
	// What the run did and spent before it stopped: the steps it reported, every known charge, an unpriced one as
	// an unknown line, and its finished videos. A cancelled run proved nothing, so it has no status and no answer.
	Receipt CancellationReceipt
	cause   error
}

func (e *CancelledError) Error() string { return "fastbrowse: the run was stopped: " + e.cause.Error() }

func (e *CancelledError) Unwrap() error { return e.cause }

// ProcessExitedError says the fastbrowse process is gone, so what was waiting on it, or is asked of it later, has
// no answer.
type ProcessExitedError struct {
	// How the process ended, as os.ProcessState writes it: "exit status 7", "signal: killed".
	State string
}

func (e *ProcessExitedError) Error() string {
	return "fastbrowse: the fastbrowse process ended: " + e.State
}
