package dev.agentlabs.fastbrowse;

import dev.agentlabs.fastbrowse.protocol.BrowserEvent;
import dev.agentlabs.fastbrowse.protocol.StepEvent;

/** One thing a run did: the browser opening, then each step. Exactly one of the two is set. */
public record Event(BrowserEvent browser, StepEvent step) {}
