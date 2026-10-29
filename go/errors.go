package fastbrowse

import "fmt"

// RPCError is the server's error reply to a request. Code is one of the protocol's error codes: ErrorCodeBusy
// while another run on the same Client is active, ErrorCodeConfiguration for a missing key, ErrorCodeInvalidParams
// for an option the server refuses.
type RPCError struct {
	Code    ErrorCode
	Message string
}

func (e *RPCError) Error() string { return fmt.Sprintf("fastbrowse: %s (code %d)", e.Message, e.Code) }

// ProcessExitedError says the fastbrowse process is gone, so what was waiting on it, or is asked of it later, has
// no answer.
type ProcessExitedError struct {
	// How the process ended, as os.ProcessState writes it: "exit status 7", "signal: killed".
	State string
}

func (e *ProcessExitedError) Error() string {
	return "fastbrowse: the fastbrowse process ended: " + e.State
}
