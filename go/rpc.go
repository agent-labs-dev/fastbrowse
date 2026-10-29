package fastbrowse

import (
	"bufio"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"sync"
)

// conn is JSON-RPC 2.0 over a pair of byte streams, one message per line. Both sides may call the other.
type conn struct {
	writing sync.Mutex
	output  io.Writer

	mu      sync.Mutex
	pending map[int64]chan reply
	nextID  int64
	ended   error

	// serve answers a request the server makes; what it returns is the result, and an error is the error reply.
	// It runs on a goroutine of its own, so a slow callback does not hold up the replies behind it.
	serve func(method string, params json.RawMessage) (any, error)
	// notify takes a notification, in the order they came, on the goroutine that reads.
	notify func(method string, params json.RawMessage)
}

type reply struct {
	result json.RawMessage
	err    error
}

type message struct {
	JSONRPC string          `json:"jsonrpc"`
	ID      json.RawMessage `json:"id,omitempty"`
	Method  string          `json:"method,omitempty"`
	Params  any             `json:"params,omitempty"`
	Result  any             `json:"result,omitempty"`
	Error   *Error          `json:"error,omitempty"`
}

type incoming struct {
	ID     json.RawMessage `json:"id"`
	Method *string         `json:"method"`
	Params json.RawMessage `json:"params"`
	Result json.RawMessage `json:"result"`
	Error  *Error          `json:"error"`
}

func newConn(output io.Writer) *conn {
	return &conn{output: output, pending: map[int64]chan reply{}, nextID: 1}
}

// read takes messages from input until it ends, then returns.
func (c *conn) read(input io.Reader) {
	lines := bufio.NewReader(input)
	for {
		line, err := lines.ReadBytes('\n')
		if len(line) > 0 && !c.receive(line) {
			return
		}
		if err != nil {
			return
		}
	}
}

// receive handles one line, and says whether to read on.
func (c *conn) receive(line []byte) bool {
	var m incoming
	if err := json.Unmarshal(line, &m); err != nil {
		// The server keeps everything but messages off this stream, so a line that is not one means the binary is
		// not a fastbrowse server at all. Waiting on it would hang every request.
		text := string(line)
		if len(text) > 200 {
			text = text[:200]
		}
		c.end(fmt.Errorf("fastbrowse: the server sent a line that is not JSON: %s", text))
		return false
	}
	if m.Method != nil {
		if len(m.ID) == 0 || string(m.ID) == "null" {
			c.notify(*m.Method, m.Params)
		} else {
			go c.answer(m.ID, *m.Method, m.Params)
		}
		return true
	}
	var id int64
	if json.Unmarshal(m.ID, &id) != nil {
		return true
	}
	c.mu.Lock()
	waiting, ok := c.pending[id]
	delete(c.pending, id)
	c.mu.Unlock()
	if !ok {
		return true
	}
	if m.Error != nil {
		waiting <- reply{err: &RPCError{Code: m.Error.Code, Message: m.Error.Message}}
	} else {
		waiting <- reply{result: m.Result}
	}
	return true
}

func (c *conn) answer(id json.RawMessage, method string, params json.RawMessage) {
	result, err := c.serve(method, params)
	if err != nil {
		code := ErrorCodeInternalError
		var refused *RPCError
		if errors.As(err, &refused) {
			code = refused.Code
		}
		c.send(message{ID: id, Error: &Error{Code: code, Message: err.Error()}})
		return
	}
	if result == nil {
		result = json.RawMessage("null")
	}
	c.send(message{ID: id, Result: result})
}

// call sends a request and returns the channel its reply arrives on.
func (c *conn) call(method string, params any) (<-chan reply, error) {
	c.mu.Lock()
	if c.ended != nil {
		err := c.ended
		c.mu.Unlock()
		return nil, err
	}
	id := c.nextID
	c.nextID++
	waiting := make(chan reply, 1)
	c.pending[id] = waiting
	c.mu.Unlock()
	if params == nil {
		params = struct{}{}
	}
	encoded, _ := json.Marshal(id)
	c.send(message{ID: encoded, Method: method, Params: params})
	return waiting, nil
}

func (c *conn) send(m message) {
	m.JSONRPC = "2.0"
	line, err := json.Marshal(m)
	if err != nil {
		c.end(fmt.Errorf("fastbrowse: a message could not be written: %w", err))
		return
	}
	c.writing.Lock()
	defer c.writing.Unlock()
	// A write to a server that has gone fails here. The request it carried is answered by end, which runs when
	// the process exits, so the error has nothing more to say.
	_, _ = c.output.Write(append(line, '\n'))
}

// end says the other side is gone. Every request still waiting gets reason, and so does any made later.
func (c *conn) end(reason error) {
	c.mu.Lock()
	defer c.mu.Unlock()
	if c.ended != nil {
		return
	}
	c.ended = reason
	for id, waiting := range c.pending {
		waiting <- reply{err: reason}
		delete(c.pending, id)
	}
}
