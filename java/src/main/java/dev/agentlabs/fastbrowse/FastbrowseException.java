package dev.agentlabs.fastbrowse;

/** Every error this library throws, so one catch tells its failures from the caller's own. */
public class FastbrowseException extends RuntimeException {
    private static final long serialVersionUID = 1L;

    public FastbrowseException(String message) {
        super(message);
    }

    public FastbrowseException(String message, Throwable cause) {
        super(message, cause);
    }
}
