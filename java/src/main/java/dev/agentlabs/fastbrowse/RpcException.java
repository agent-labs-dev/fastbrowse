package dev.agentlabs.fastbrowse;

import dev.agentlabs.fastbrowse.protocol.CancellationReceipt;
import dev.agentlabs.fastbrowse.protocol.Protocol;
import java.util.Optional;

/**
 * The server's error reply to a request. {@link #code()} is one of the {@code Protocol.ERROR_*} codes:
 * {@link Protocol#ERROR_BUSY} while another run on the same client is active, {@link Protocol#ERROR_CONFIGURATION}
 * for a missing key, {@link Protocol#ERROR_INVALID_PARAMS} for an option the server refuses.
 */
public class RpcException extends FastbrowseException {
    private static final long serialVersionUID = 1L;
    private final int code;
    private final transient CancellationReceipt receipt;

    public RpcException(int code, String message) {
        this(code, message, null);
    }

    public RpcException(int code, String message, CancellationReceipt receipt) {
        super(message);
        this.code = code;
        this.receipt = receipt;
    }

    public int code() {
        return code;
    }

    /**
     * What the run did and spent before it stopped, on {@link Protocol#ERROR_CANCELLED} only: the steps it reported,
     * every known charge, an unpriced one as an {@code unknown} line, and its finished videos. Empty on every other
     * code.
     */
    public Optional<CancellationReceipt> receipt() {
        return Optional.ofNullable(receipt);
    }
}
