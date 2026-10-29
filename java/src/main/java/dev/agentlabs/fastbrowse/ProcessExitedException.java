package dev.agentlabs.fastbrowse;

/** The fastbrowse process is gone, so what was waiting on it, or is asked of it later, has no answer. */
public class ProcessExitedException extends FastbrowseException {
    private static final long serialVersionUID = 1L;
    private final int exitCode;

    public ProcessExitedException(int exitCode) {
        super("the fastbrowse process exited with code " + exitCode);
        this.exitCode = exitCode;
    }

    /** The process's exit code; on Unix a process killed by a signal reports 128 plus the signal's number. */
    public int exitCode() {
        return exitCode;
    }
}
