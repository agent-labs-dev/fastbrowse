package dev.agentlabs.fastbrowse;

import dev.agentlabs.fastbrowse.protocol.Protocol;

/**
 * The server's error reply to a request. {@link #code()} is one of the {@code Protocol.ERROR_*} codes:
 * {@link Protocol#ERROR_BUSY} while another run on the same client is active, {@link Protocol#ERROR_CONFIGURATION}
 * for a missing key, {@link Protocol#ERROR_INVALID_PARAMS} for an option the server refuses.
 */
public class RpcException extends FastbrowseException {
    private static final long serialVersionUID = 1L;
    private final int code;

    public RpcException(int code, String message) {
        super(message);
        this.code = code;
    }

    public int code() {
        return code;
    }
}
