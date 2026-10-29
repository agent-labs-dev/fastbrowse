package fastbrowse

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"testing"
	"time"
)

// serverBinary is the fastbrowse binary the tests start: FASTBROWSE_TEST_BINARY, or the workspace's debug build.
func serverBinary(t *testing.T) string {
	t.Helper()
	if named := os.Getenv("FASTBROWSE_TEST_BINARY"); named != "" {
		return named
	}
	built, _ := filepath.Abs(filepath.Join("..", "target", "debug", executable()))
	if _, err := os.Stat(built); err != nil {
		t.Skip("no fastbrowse binary: run `cargo build -p fastbrowse-cli`, or set FASTBROWSE_TEST_BINARY")
	}
	return built
}

// scripted starts the real server with one of the stand-in runs it carries behind FASTBROWSE_TEST_SERVER in place
// of the agent, so a run needs no browser and no model.
func scripted(t *testing.T, run string, options Options) *Client {
	t.Helper()
	options.BinaryPath = serverBinary(t)
	options.Env = map[string]string{
		"FASTBROWSE_TEST_SERVER": "scripted_run",
		"FASTBROWSE_TEST_RUN":    run,
		// The server refuses a run it has no model keys for, whatever would have run it.
		"OPENROUTER_API_KEY": "unused", "AI_GATEWAY_API_KEY": "unused", "BROWSER_USE_API_KEY": "unused",
	}
	client, err := Start(context.Background(), options)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = client.Close(context.Background()) })
	return client
}

func run(t *testing.T, client *Client, task string, options RunOptions) *RunResult {
	t.Helper()
	result, err := client.Run(context.Background(), task, options)
	if err != nil {
		t.Fatal(err)
	}
	return result
}

func TestStartReportsTheBinarysVersion(t *testing.T) {
	client := scripted(t, "echo", Options{})
	if client.Version() == "" {
		t.Fatal("no version")
	}
}

func TestARunSendsItsOptionsUnderTheirWireNames(t *testing.T) {
	client := scripted(t, "echo", Options{Browser: BrowserOptions{Local: true, Foreground: true}})
	result := run(t, client, "find the price", RunOptions{
		Start:       "https://shop.example/",
		Inputs:      map[string]string{"size": "M"},
		Attachments: []Attachment{{Name: "a.txt", MimeType: "text/plain", Content: []byte("hello")}},
		Limits:      &Limits{MaxSteps: ptr[int64](5)},
		OutputSchema: map[string]any{
			"type": "object", "properties": map[string]any{"price": map[string]any{"type": "number"}},
		},
		Until: func(context.Context, string) (bool, error) { return true, nil },
	})
	if result.Status != StatusComplete || *result.Answer != "find the price" {
		t.Fatalf("got %s %v", result.Status, result.Answer)
	}
	var echoed struct {
		Params      map[string]any `json:"params"`
		Attachments [][]byte       `json:"attachments"`
	}
	if err := result.DecodeData(&echoed); err != nil {
		t.Fatal(err)
	}
	params := echoed.Params
	for key, want := range map[string]any{
		"task": "find the price", "start": "https://shop.example/", "local": true, "foreground": true, "until": true,
	} {
		if params[key] != want {
			t.Errorf("%s: got %v, want %v", key, params[key], want)
		}
	}
	if limits := params["limits"].(map[string]any); limits["max_steps"] != float64(5) {
		t.Errorf("limits: %v", limits)
	}
	// What was not asked for is not sent, so the server applies its own defaults.
	for _, key := range []string{"frames", "context", "cdp_port", "secrets"} {
		if _, sent := params[key]; sent {
			t.Errorf("%s was sent: %v", key, params[key])
		}
	}
	if string(echoed.Attachments[0]) != "hello" {
		t.Errorf("attachment: %q", echoed.Attachments[0])
	}
}

func TestARunsOwnBrowserReplacesTheClients(t *testing.T) {
	client := scripted(t, "echo", Options{Browser: BrowserOptions{CDPPort: 9222}})
	result := run(t, client, "t", RunOptions{Browser: &BrowserOptions{Local: true}})
	var echoed struct {
		Params map[string]any `json:"params"`
	}
	_ = result.DecodeData(&echoed)
	if _, sent := echoed.Params["cdp_port"]; sent || echoed.Params["local"] != true {
		t.Fatalf("params: %v", echoed.Params)
	}
}

func TestEventsArriveInTheOrderTheRunProducedThem(t *testing.T) {
	client := scripted(t, "three_events", Options{})
	var events []Event
	run(t, client, "t", RunOptions{OnEvent: func(event Event) error { events = append(events, event); return nil }})
	if len(events) != 3 || events[0].Browser == nil || *events[0].Browser.LiveURL != "https://live.example/1" ||
		events[1].Step.Step.Index != 0 || events[2].Step.Step.Index != 1 {
		t.Fatalf("events: %+v", events)
	}
}

func TestAnEventCallbackThatFailsEndsTheRunWithStatusError(t *testing.T) {
	client := scripted(t, "three_events", Options{})
	calls := 0
	result := run(t, client, "t", RunOptions{OnEvent: func(Event) error { calls++; return errors.New("full disk") }})
	if result.Status != StatusError || *result.Error != "run/event: full disk" || calls != 1 {
		t.Fatalf("got %s %v after %d calls", result.Status, result.Error, calls)
	}
}

func TestAStatusIsAResultNotAnError(t *testing.T) {
	client := scripted(t, "needs_login", Options{})
	if result := run(t, client, "t", RunOptions{}); result.Status != StatusNeedsLogin {
		t.Fatalf("status %s", result.Status)
	}
}

func TestUntilDecidesWhetherTheRunIsComplete(t *testing.T) {
	client := scripted(t, "ends_on_an_order", Options{})
	var asked string
	result := run(t, client, "t", RunOptions{Until: func(_ context.Context, url string) (bool, error) {
		asked = url
		return false, nil
	}})
	if result.Status != StatusUnverified || asked != "https://shop.example/orders/17" {
		t.Fatalf("got %s after %q", result.Status, asked)
	}
}

func TestASecretIsResolvedEachTimeItIsTyped(t *testing.T) {
	client := scripted(t, "types_secrets", Options{})
	calls := 0
	result := run(t, client, "t", RunOptions{
		Inputs: map[string]string{"name": "password", "origin": "https://shop.example"},
		Secrets: &Secrets{
			Refs: []SecretRef{{Name: "password", Origins: []string{"https://shop.example"}}},
			Resolve: func(_ context.Context, name, origin string) (string, bool, error) {
				calls++
				return name + "@" + origin, true, nil
			},
		},
	})
	var typed []string
	_ = result.DecodeData(&typed)
	if calls != 2 || len(typed) != 2 || typed[0] != "password@https://shop.example" {
		t.Fatalf("got %v after %d calls", typed, calls)
	}
}

func TestAResolverErrorEndsTheRunWithoutWhatItSaid(t *testing.T) {
	client := scripted(t, "types_secrets", Options{})
	result := run(t, client, "t", RunOptions{
		Inputs: map[string]string{"name": "password", "origin": "https://shop.example"},
		Secrets: &Secrets{
			Refs: []SecretRef{{Name: "password", Origins: []string{"https://shop.example"}}},
			Resolve: func(context.Context, string, string) (string, bool, error) {
				return "", false, errors.New("vault said hunter2")
			},
		},
	})
	if result.Status != StatusError || strings.Contains(*result.Error, "hunter2") {
		t.Fatalf("got %s %v", result.Status, result.Error)
	}
}

func TestFramesArriveAsTheirBytes(t *testing.T) {
	client := scripted(t, "two_frames", Options{})
	var frames [][]byte
	run(t, client, "t", RunOptions{OnFrame: func(jpeg []byte) error { frames = append(frames, jpeg); return nil }})
	if len(frames) != 2 || len(frames[0]) != 256 || !bytes.Equal(frames[1], []byte("\xff\xd8 second")) {
		t.Fatalf("frames: %q", frames)
	}
}

func TestCancellingTheContextStopsTheRunAfterItsBrowserCloses(t *testing.T) {
	client := scripted(t, "until_cancelled", Options{})
	closed := filepath.Join(t.TempDir(), "closed")
	ctx, cancel := context.WithCancel(context.Background())
	_, err := client.Run(ctx, "t", RunOptions{
		Inputs:  map[string]string{"closed": closed},
		OnEvent: func(Event) error { cancel(); return nil },
	})
	if !errors.Is(err, context.Canceled) {
		t.Fatalf("err: %v", err)
	}
	if _, err := os.Stat(closed); err != nil {
		t.Fatal("Run returned before the browser closed")
	}
	// The process is still there for the next run.
	if result := run(t, client, "t", RunOptions{}); result.Status != StatusComplete {
		t.Fatalf("next run: %s", result.Status)
	}
}

func TestADoneContextStartsNoRun(t *testing.T) {
	client := scripted(t, "request_count", Options{})
	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	if _, err := client.Run(ctx, "t", RunOptions{}); !errors.Is(err, context.Canceled) {
		t.Fatalf("err: %v", err)
	}
	var count int
	_ = run(t, client, "t", RunOptions{}).DecodeData(&count)
	if count != 1 {
		t.Fatalf("the server saw %d runs", count)
	}
}

func TestASecondRunWhileOneIsActiveIsRefusedAsBusy(t *testing.T) {
	client := scripted(t, "never_ends", Options{})
	ctx, cancel := context.WithCancel(context.Background())
	started := make(chan struct{})
	var wg sync.WaitGroup
	wg.Add(1)
	go func() {
		defer wg.Done()
		_, _ = client.Run(ctx, "t", RunOptions{OnEvent: func(Event) error { close(started); return nil }})
	}()
	<-started
	_, err := client.Run(context.Background(), "t", RunOptions{})
	cancel()
	wg.Wait()
	var refused *RPCError
	if !errors.As(err, &refused) || refused.Code != ErrorCodeBusy {
		t.Fatalf("err: %v", err)
	}
}

func TestARunOnAProcessThatExitsReturnsProcessExitedError(t *testing.T) {
	client := scripted(t, "exits", Options{Stderr: &bytes.Buffer{}})
	_, err := client.Run(context.Background(), "t", RunOptions{})
	var exited *ProcessExitedError
	if !errors.As(err, &exited) || !strings.Contains(exited.State, "7") {
		t.Fatalf("err: %v", err)
	}
	if _, err := client.Run(context.Background(), "t", RunOptions{}); !errors.As(err, &exited) {
		t.Fatalf("a later run: %v", err)
	}
}

func TestCloseWaitsForTheProcessAndCanBeCalledAgain(t *testing.T) {
	client := scripted(t, "echo", Options{})
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	if err := client.Close(ctx); err != nil {
		t.Fatal(err)
	}
	if client.process.ProcessState == nil {
		t.Fatal("Close returned before the process exited")
	}
	if err := client.Close(ctx); err != nil {
		t.Fatal(err)
	}
	var exited *ProcessExitedError
	if _, err := client.Run(context.Background(), "t", RunOptions{}); !errors.As(err, &exited) {
		t.Fatalf("a run after Close: %v", err)
	}
}

func TestStructuredDataIsReadIntoTheCallersType(t *testing.T) {
	client := scripted(t, "found", Options{})
	result := run(t, client, "t", RunOptions{
		Inputs: map[string]string{"found": `{"price": 12.5}`},
		OutputSchema: json.RawMessage(`{"type": "object", "properties": {"price": {"type": "number"}},
			"required": ["price"]}`),
	})
	var found struct {
		Price float64 `json:"price"`
	}
	if err := result.DecodeData(&found); err != nil || found.Price != 12.5 {
		t.Fatalf("got %+v, %v", found, err)
	}
}

func ptr[T any](value T) *T { return &value }

func TestCloseFromACallbackReturns(t *testing.T) {
	client := scripted(t, "never_ends", Options{})
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	ran := make(chan error, 1)
	go func() {
		_, err := client.Run(context.Background(), "t", RunOptions{OnEvent: func(Event) error {
			return client.Close(ctx)
		}})
		ran <- err
	}()
	select {
	case <-ran:
	case <-time.After(10 * time.Second):
		t.Fatal("Close called from OnEvent never returned")
	}
}
