package fastbrowse

import (
	"encoding/json"
	"fmt"
)

// Event is one thing a run did: the browser opening, then each step. Exactly one of Browser and Step is set.
type Event struct {
	Browser *BrowserEvent
	Step    *StepEvent
}

func (e *Event) UnmarshalJSON(data []byte) error {
	var tagged struct {
		Type string `json:"type"`
	}
	if err := json.Unmarshal(data, &tagged); err != nil {
		return err
	}
	switch tagged.Type {
	case "browser":
		e.Browser = new(BrowserEvent)
		return json.Unmarshal(data, e.Browser)
	case "step":
		e.Step = new(StepEvent)
		return json.Unmarshal(data, e.Step)
	}
	return fmt.Errorf("fastbrowse: an event of unknown type %q", tagged.Type)
}

// DecodeData reads the run's structured data, which RunOptions.OutputSchema shaped, into v.
func (r *RunResult) DecodeData(v any) error {
	return json.Unmarshal(r.Data, v)
}
