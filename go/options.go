package fastbrowse

import (
	"context"
	"encoding/json"
	"io"
)

// Options say how to start the fastbrowse process, and which browser every run uses unless the run says otherwise.
type Options struct {
	// BinaryPath is the fastbrowse executable to run. Without it, the FASTBROWSE_BINARY environment variable names
	// one, and without that it is the release binary this module was published with, downloaded once into the user
	// cache directory and checked against the checksum the module carries.
	BinaryPath string
	// Env adds environment variables for the fastbrowse process to the ones this process has. Model and browser keys
	// are read from that environment.
	Env map[string]string
	// Stderr receives the fastbrowse process's log. Nil is this process's own stderr; io.Discard drops it.
	Stderr io.Writer
	// Browser is the browser every run drives unless RunOptions.Browser names another.
	Browser BrowserOptions
}

// BrowserOptions say which browser a run drives.
type BrowserOptions struct {
	// Local is local Chrome instead of a Browser Use Cloud browser.
	Local bool
	// Chrome says which local Chrome, and how. A headed window or a kept profile implies Local.
	Chrome *LocalChrome
	// CloudProfile is the Browser Use Cloud profile a cloud browser starts with.
	CloudProfile string
	// CDPURL is the DevTools WebSocket URL of a browser that is already running. The run neither starts nor stops it.
	CDPURL string
	// CDPPort is the DevTools port of a browser that is already running on this machine.
	CDPPort int
	// Attach drives a window that is already open in the browser CDPURL or CDPPort names, and opens no tab.
	Attach bool
	// TargetMatch is part of the URL or title of the window to attach to.
	TargetMatch string
	// Foreground brings the run's tab and its browser window to the front, to watch it.
	Foreground bool
	// ProxyCountry is the two-letter country a cloud browser browses from.
	ProxyCountry string
	// Viewport is the width and height of the page, in CSS pixels.
	Viewport *[2]int
	// CloudAllowResizing lets a cloud browser's window be resized.
	CloudAllowResizing bool
}

// Attachment is a file a run may upload.
type Attachment struct {
	Name     string
	MimeType string
	Content  []byte
}

// Secrets are the secrets a run may type. The server and the models see the refs; the values stay with Resolve.
type Secrets struct {
	Refs []SecretRef
	// Resolve returns the value of the secret name, about to be typed on origin, and false when there is none. It
	// is called each time a value is typed and never for an origin the ref does not cover, so a one-time code is
	// computed when it is needed. An error ends the run with status error, and what it says is kept out of the
	// result.
	Resolve func(ctx context.Context, name, origin string) (value string, ok bool, err error)
}

// RunOptions are one run's options. The zero value runs the task with the Client's browser and no limits.
type RunOptions struct {
	// Browser replaces the Client's Options.Browser for this run.
	Browser *BrowserOptions
	// Start is the address to start on. Without one, the first address is proposed from the task.
	Start string
	// Context is background the run may use but never treats as the task, such as where the request came from.
	Context     string
	Inputs      map[string]string
	Attachments []Attachment
	Limits      *Limits
	// Authorization allows what a run otherwise stops before.
	Authorization *Authorization
	// OutputSchema is the shape of the data to read, as JSON Schema (draft 2020-12) with an object at its root:
	// a map, a json.RawMessage, or anything else that marshals to one. The data is RunResult.Data.
	OutputSchema any
	// Downloads is a directory that keeps the files the run downloads.
	Downloads string
	// Record is where to write a video of the run. The server needs ffmpeg for it.
	Record string
	// Cursor draws visual cursor feedback for visible local Chrome on Linux X11 with the Cua Driver.
	Cursor  bool
	Secrets *Secrets
	// Until is called with the address the run ended on. False keeps the run from StatusComplete, and an error
	// ends it with StatusError.
	Until func(ctx context.Context, url string) (bool, error)
	// OnEvent is called with the browser event and then each step event, in the order the run produced them, on
	// the goroutine that reads the server's messages: a slow one holds up the run. An error ends the run with
	// StatusError.
	OnEvent func(Event) error
	// OnFrame is called with JPEG frames of the active tab while the run goes on. Without it the page is never
	// filmed. An error ends the run with StatusError.
	OnFrame func(jpeg []byte) error
}

func optional[T comparable](value T) *T {
	var zero T
	if value == zero {
		return nil
	}
	return &value
}

func runParams(runID, task string, browser BrowserOptions, options RunOptions) (RunParams, error) {
	if options.Browser != nil {
		browser = *options.Browser
	}
	params := RunParams{
		RunID:              runID,
		Task:               task,
		Start:              optional(options.Start),
		Context:            optional(options.Context),
		Inputs:             options.Inputs,
		Limits:             options.Limits,
		Authorization:      options.Authorization,
		Downloads:          optional(options.Downloads),
		Record:             optional(options.Record),
		Cursor:             options.Cursor,
		Local:              browser.Local,
		Chrome:             browser.Chrome,
		CloudProfile:       optional(browser.CloudProfile),
		CDPURL:             optional(browser.CDPURL),
		Attach:             browser.Attach,
		TargetMatch:        optional(browser.TargetMatch),
		Foreground:         browser.Foreground,
		ProxyCountry:       optional(browser.ProxyCountry),
		CloudAllowResizing: browser.CloudAllowResizing,
		// The wire carries whether a callback is held, and the callback stays here.
		Frames: options.OnFrame != nil,
		Until:  options.Until != nil,
	}
	if browser.CDPPort != 0 {
		port := int64(browser.CDPPort)
		params.CDPPort = &port
	}
	if browser.Viewport != nil {
		params.Viewport = &[2]int64{int64(browser.Viewport[0]), int64(browser.Viewport[1])}
	}
	for _, attachment := range options.Attachments {
		params.Attachments = append(params.Attachments, RunAttachment{
			Name: attachment.Name, MimeType: attachment.MimeType, Content: attachment.Content,
		})
	}
	if options.Secrets != nil {
		params.Secrets = options.Secrets.Refs
	}
	if options.OutputSchema != nil {
		schema, err := json.Marshal(options.OutputSchema)
		if err != nil {
			return RunParams{}, err
		}
		params.OutputSchema = schema
	}
	return params, nil
}
