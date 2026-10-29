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
	// notify takes a notification. Notifications and replies are delivered in the order they came, one at a time.
	notify func(method string, params json.RawMessage)

	// The messages read and not yet delivered. The reader only queues them, so it reads on to the end of the stream
	// while a callback runs, and the process's exit is seen even when that callback is waiting on it.
	queued  []func()
	waking  chan struct{}
	drained bool
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
	c := &conn{output: output, pending: map[int64]chan reply{}, nextID: 1, waking: make(chan struct{}, 1)}
	go c.deliver()
	return c
}

// queue hands delivery to the goroutine that delivers, in the order of the calls.
func (c *conn) queue(delivery func()) {
	c.mu.Lock()
	c.queued = append(c.queued, delivery)
	c.mu.Unlock()
	select {
	case c.waking <- struct{}{}:
	default:
	}
}

// deliver runs what is queued, until the connection has ended and everything before the end is delivered.
func (c *conn) deliver() {
	for range c.waking {
		for {
			c.mu.Lock()
			if len(c.queued) == 0 {
				drained := c.drained
				c.mu.Unlock()
				if drained {
					return
				}
				break
			}
			delivery := c.queued[0]
			c.queued = c.queued[1:]
			c.mu.Unlock()
			delivery()
		}
	}
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
		method := *m.Method
		if len(m.ID) == 0 || string(m.ID) == "null" {
			c.queue(func() { c.notify(method, m.Params) })
		} else {
			c.queue(func() { go c.answer(m.ID, method, m.Params) })
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
	answered := reply{result: m.Result}
	if m.Error != nil {
		answered = reply{err: &RPCError{Code: m.Error.Code, Message: m.Error.Message}}
	}
	// After the notifications before it, so a run's events have all been delivered when its reply is.
	c.queue(func() { waiting <- answered })
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

// end says the other side is gone. Every request still waiting gets reason once what came before is delivered, and
// any made later gets it at once.
func (c *conn) end(reason error) {
	c.mu.Lock()
	if c.ended != nil {
		c.mu.Unlock()
		return
	}
	c.ended = reason
	waiting := c.pending
	c.pending = map[int64]chan reply{}
	c.mu.Unlock()
	c.queue(func() {
		for _, each := range waiting {
			each <- reply{err: reason}
		}
		c.mu.Lock()
		c.drained = true
		c.mu.Unlock()
	})
}
