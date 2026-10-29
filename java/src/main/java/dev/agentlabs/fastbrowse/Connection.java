package dev.agentlabs.fastbrowse;

import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.ObjectMapper;
import com.fasterxml.jackson.databind.node.ObjectNode;
import dev.agentlabs.fastbrowse.protocol.Protocol;
import java.io.BufferedReader;
import java.io.IOException;
import java.io.InputStream;
import java.io.InputStreamReader;
import java.io.OutputStream;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.List;
import java.util.Map;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.atomic.AtomicLong;

/**
 * JSON-RPC 2.0 over the fastbrowse process's stdin and stdout, one message per line. Both sides may call the other.
 *
 * <p>The reader only queues what it reads, and one thread delivers it in order: notifications, replies, and the end
 * of the connection. A callback that runs long, or waits on {@link Fastbrowse#close()}, never keeps the reader from
 * seeing the process exit. Requests the server makes are answered on threads of their own.
 */
final class Connection {
    /** Takes a notification, on the delivery thread. */
    interface Notifications {
        void notify(String method, JsonNode params);
    }

    /** Answers a request the server makes. What it returns is the result; what it throws is the error reply. */
    interface Requests {
        Object serve(String method, JsonNode params) throws Exception;
    }

    private final ObjectMapper json;
    private final OutputStream output;
    private final Map<Long, CompletableFuture<JsonNode>> pending = new ConcurrentHashMap<>();
    private final AtomicLong nextId = new AtomicLong(1);
    private final ExecutorService delivery = Executors.newSingleThreadExecutor(daemon("fastbrowse-delivery"));
    private final ExecutorService answering = Executors.newCachedThreadPool(daemon("fastbrowse-callback"));
    private final Notifications notifications;
    private final Requests requests;
    private volatile FastbrowseException ended;

    Connection(ObjectMapper json, OutputStream output, Notifications notifications, Requests requests) {
        this.json = json;
        this.output = output;
        this.notifications = notifications;
        this.requests = requests;
    }

    static java.util.concurrent.ThreadFactory daemon(String name) {
        return runnable -> {
            Thread thread = new Thread(runnable, name);
            thread.setDaemon(true);
            return thread;
        };
    }

    /** Reads messages from input until it ends. */
    void read(InputStream input) {
        try (BufferedReader lines = new BufferedReader(new InputStreamReader(input, StandardCharsets.UTF_8))) {
            String line;
            while ((line = lines.readLine()) != null) {
                if (!line.isEmpty() && !receive(line)) {
                    return;
                }
            }
        } catch (IOException e) {
            // The stream closes with the process, which ends the connection with the exit's own reason.
        }
    }

    /** Handles one line, and says whether to read on. */
    private boolean receive(String line) {
        JsonNode message;
        try {
            message = json.readTree(line);
        } catch (IOException e) {
            // The server keeps everything but messages off this stream, so a line that is not one means the binary is
            // not a fastbrowse server at all. Waiting on it would hang every request.
            String start = line.length() > 200 ? line.substring(0, 200) : line;
            end(new FastbrowseException("fastbrowse sent a line that is not JSON: " + start));
            return false;
        }
        JsonNode method = message.get("method");
        JsonNode id = message.get("id");
        JsonNode params = message.path("params");
        if (method != null && method.isTextual()) {
            if (id == null || id.isNull()) {
                delivery.execute(() -> notifications.notify(method.asText(), params));
            } else {
                delivery.execute(() -> answering.execute(() -> answer(id, method.asText(), params)));
            }
            return true;
        }
        if (id == null || !id.canConvertToLong()) {
            return true;
        }
        CompletableFuture<JsonNode> waiting = pending.remove(id.asLong());
        if (waiting == null) {
            return true;
        }
        JsonNode error = message.get("error");
        // After the notifications before it, so a run's events have all been delivered when its reply is.
        if (error != null && !error.isNull()) {
            RpcException refused = new RpcException(error.path("code").asInt(), error.path("message").asText());
            delivery.execute(() -> waiting.completeExceptionally(refused));
        } else {
            JsonNode result = message.path("result");
            delivery.execute(() -> waiting.complete(result));
        }
        return true;
    }

    private void answer(JsonNode id, String method, JsonNode params) {
        ObjectNode reply = json.createObjectNode().put("jsonrpc", "2.0").set("id", id);
        try {
            reply.set("result", json.valueToTree(requests.serve(method, params)));
        } catch (RpcException e) {
            reply.putObject("error").put("code", e.code()).put("message", e.getMessage());
        } catch (Exception e) {
            reply.putObject("error").put("code", Protocol.ERROR_INTERNAL_ERROR).put("message", String.valueOf(e.getMessage()));
        }
        send(reply);
    }

    /** Calls a method. The future completes with the result, or with an {@link RpcException} for an error reply. */
    CompletableFuture<JsonNode> call(String method, Object params) {
        CompletableFuture<JsonNode> waiting = new CompletableFuture<>();
        FastbrowseException reason = ended;
        if (reason != null) {
            waiting.completeExceptionally(reason);
            return waiting;
        }
        long id = nextId.getAndIncrement();
        pending.put(id, waiting);
        ObjectNode request = json.createObjectNode().put("jsonrpc", "2.0").put("id", id).put("method", method);
        request.set("params", params == null ? json.createObjectNode() : json.valueToTree(params));
        send(request);
        // A process that ended between the check above and the put has no one left to answer.
        if (ended != null && pending.remove(id) != null) {
            waiting.completeExceptionally(ended);
        }
        return waiting;
    }

    private void send(JsonNode message) {
        byte[] line = (message.toString() + "\n").getBytes(StandardCharsets.UTF_8);
        synchronized (output) {
            try {
                output.write(line);
                output.flush();
            } catch (IOException e) {
                // A write to a server that has gone fails here. The request it carried is answered by end, which runs
                // when the process exits, so the error has nothing more to say.
            }
        }
    }

    /**
     * The other side is gone. Every request still waiting gets {@code reason} once what came before is delivered, and
     * any made later gets it at once.
     */
    void end(FastbrowseException reason) {
        synchronized (this) {
            if (ended != null) {
                return;
            }
            ended = reason;
        }
        List<CompletableFuture<JsonNode>> waiting = new ArrayList<>(pending.values());
        pending.clear();
        delivery.execute(() -> waiting.forEach(each -> each.completeExceptionally(reason)));
        delivery.shutdown();
        answering.shutdown();
    }
}
