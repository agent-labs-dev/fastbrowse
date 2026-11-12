package dev.agentlabs.fastbrowse;

import tools.jackson.core.JacksonException;
import tools.jackson.databind.DeserializationFeature;
import tools.jackson.databind.JsonNode;
import tools.jackson.databind.ObjectMapper;
import tools.jackson.databind.json.JsonMapper;
import com.fasterxml.jackson.annotation.JsonInclude;
import dev.agentlabs.fastbrowse.protocol.BrowserEvent;
import dev.agentlabs.fastbrowse.protocol.CancellationReceipt;
import dev.agentlabs.fastbrowse.protocol.CostBreakdown;
import dev.agentlabs.fastbrowse.protocol.InitializeParams;
import dev.agentlabs.fastbrowse.protocol.InitializeResult;
import dev.agentlabs.fastbrowse.protocol.Protocol;
import dev.agentlabs.fastbrowse.protocol.RunAttachment;
import dev.agentlabs.fastbrowse.protocol.RunCancelParams;
import dev.agentlabs.fastbrowse.protocol.RunParams;
import dev.agentlabs.fastbrowse.protocol.RunResult;
import dev.agentlabs.fastbrowse.protocol.SecretsResolveParams;
import dev.agentlabs.fastbrowse.protocol.Status;
import dev.agentlabs.fastbrowse.protocol.StepEvent;
import dev.agentlabs.fastbrowse.protocol.RunUntilParams;
import java.io.IOException;
import java.nio.file.Path;
import java.time.Duration;
import java.util.Base64;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.Optional;
import java.util.UUID;
import java.util.concurrent.CancellationException;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CompletionException;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.ExecutionException;
import java.util.concurrent.Future;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.TimeoutException;

/**
 * Runs browser tasks with fastbrowse. A client starts the fastbrowse binary as {@code fastbrowse serve --stdio} and
 * speaks its JSON-RPC protocol; it runs one task at a time, so start one per run that goes on in parallel.
 *
 * <pre>{@code
 * try (Fastbrowse fastbrowse = Fastbrowse.start()) {
 *     RunResult result = fastbrowse.run("What is the top story's title?",
 *             RunOptions.builder().start("https://news.ycombinator.com/").build()).join();
 *     System.out.println(result.status() + " " + result.answer());
 * }
 * }</pre>
 */
public final class Fastbrowse implements AutoCloseable {
    private static final ObjectMapper JSON = JsonMapper.builder()
            .disable(DeserializationFeature.FAIL_ON_UNKNOWN_PROPERTIES)
            .changeDefaultPropertyInclusion(inclusion -> JsonInclude.Value.construct(JsonInclude.Include.NON_NULL, JsonInclude.Include.NON_NULL))
            .build();

    private final Process process;
    private final Connection connection;
    private final BrowserOptions browser;
    private final Duration gracePeriod;
    private final CompletableFuture<Void> exited = new CompletableFuture<>();
    private final Map<String, Served> runs = new ConcurrentHashMap<>();
    private volatile String version;
    /** The reply of the last run cancelled from here, which the next run waits for: its browser is still closing. */
    private volatile CompletableFuture<?> settling = CompletableFuture.completedFuture(null);
    private CompletableFuture<Void> closed;

    /** A run waiting on its reply, and where the server's calls about it go. */
    private final class Served {
        final String runId;
        final RunOptions options;
        /** What ended the run from this side: the first onEvent or onFrame that threw. */
        volatile String failure;
        // Guarded by this: whether the run request went out, and whether the run was cancelled before or after.
        private boolean sent;
        private boolean cancelled;

        Served(String runId, RunOptions options) {
            this.runId = runId;
            this.options = options;
        }

        /**
         * Sends the run request, unless the run was cancelled while it waited for an earlier run's browser. Under the
         * same lock as {@link #cancel()}, so a cancellation is written after the request it stops.
         */
        synchronized CompletableFuture<JsonNode> send(RunParams params) {
            if (cancelled) {
                return CompletableFuture.failedFuture(new CancellationException("the run was cancelled before it started"));
            }
            sent = true;
            return connection.call(Protocol.METHOD_RUN, params);
        }

        /** Stops the run: on the server once it has been sent, and before it is sent by not sending it. */
        synchronized void cancel() {
            cancelled = true;
            if (sent) {
                // The reply says only that the request was read, and a process that has gone answers the run itself.
                connection.call(Protocol.METHOD_RUN_CANCEL, new RunCancelParams(runId));
            }
        }
    }

    private Fastbrowse(Process process, BrowserOptions browser, Duration gracePeriod) {
        this.process = process;
        this.browser = browser;
        this.gracePeriod = gracePeriod;
        this.connection = new Connection(JSON, process.getOutputStream(), this::notify, this::serve);
    }

    /** Starts a fastbrowse process with the default options. */
    public static Fastbrowse start() {
        return start(StartOptions.defaults());
    }

    /** Starts a fastbrowse process and checks that it speaks this client's protocol version. */
    public static Fastbrowse start(StartOptions options) {
        Map<String, String> environment = new HashMap<>(System.getenv());
        environment.putAll(options.env());
        Path binary = Binaries.resolve(options.binaryPath(), environment);
        ProcessBuilder builder = new ProcessBuilder(binary.toString(), "serve", "--stdio");
        builder.environment().putAll(options.env());
        builder.redirectError(options.stderr() == null ? ProcessBuilder.Redirect.INHERIT : ProcessBuilder.Redirect.PIPE);
        Process process;
        try {
            process = builder.start();
        } catch (IOException e) {
            throw new FastbrowseException("could not start " + binary + ": " + e.getMessage(), e);
        }
        Fastbrowse fastbrowse = new Fastbrowse(process, options.browser(), options.gracePeriod());
        if (options.stderr() != null) {
            Thread copy = Connection.daemon("fastbrowse-stderr").newThread(() -> {
                try {
                    process.getErrorStream().transferTo(options.stderr());
                } catch (IOException e) {
                    // The caller's stream closed; the process goes on without it.
                }
            });
            copy.start();
        }
        Connection.daemon("fastbrowse-reader").newThread(() -> {
            fastbrowse.connection.read(process.getInputStream());
            int code;
            try {
                code = process.waitFor();
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
                code = -1;
            }
            fastbrowse.connection.end(new ProcessExitedException(code));
            fastbrowse.exited.complete(null);
        }).start();
        try {
            JsonNode reply = fastbrowse.connection.call(Protocol.METHOD_INITIALIZE, new InitializeParams(Protocol.VERSION))
                    .get(fastbrowse.gracePeriod.toMillis(), TimeUnit.MILLISECONDS);
            InitializeResult hello = JSON.treeToValue(reply, InitializeResult.class);
            if (hello.protocolVersion() != Protocol.VERSION) {
                throw new FastbrowseException("this client speaks protocol version " + Protocol.VERSION + ", and "
                        + "fastbrowse " + hello.fastbrowseVersion() + " at " + binary + " speaks protocol version "
                        + hello.protocolVersion() + ": use matching versions of the two");
            }
            fastbrowse.version = hello.fastbrowseVersion();
            return fastbrowse;
        } catch (Exception e) {
            process.destroyForcibly();
            fastbrowse.exited.join();
            throw unwrapped(e);
        }
    }

    /** The version of the fastbrowse binary this client drives. */
    public String version() {
        return version;
    }

    /**
     * Runs a task. The future completes with its result whatever status it ended in: a run that needs a login or got
     * stuck is read from {@code status()}, and is not an exception.
     *
     * <p>It completes exceptionally with an {@link RpcException} when the server refuses the request, which it does
     * before a browser opens: for a bad option, for a missing key, and with {@link Protocol#ERROR_BUSY} while another
     * run on this client is active; and with a {@link ProcessExitedException} when the process is gone.
     *
     * <p>Cancelling the future stops the run. The future is cancelled at once, and the run's browser goes on closing:
     * the next run on this client starts once it has. {@link #cancellation(Future)} has what the run did and spent
     * by then.
     */
    public CompletableFuture<RunResult> run(String task, RunOptions options) {
        String runId = UUID.randomUUID().toString();
        RunParams params = params(runId, task, options.browser() != null ? options.browser() : browser, options);
        CompletableFuture<JsonNode> reply = new CompletableFuture<>();
        Served served = new Served(runId, options);
        RunFuture result = new RunFuture(reply, served);
        // The server names the run in every message it sends, which is how a run refused as busy hears nothing of the
        // one that is active.
        runs.put(runId, served);
        settling.whenComplete((ignored, error) -> served.send(params).whenComplete((value, failure) -> {
            if (failure != null) {
                reply.completeExceptionally(failure);
            } else {
                reply.complete(value);
            }
        }));
        reply.whenComplete((value, failure) -> {
            runs.remove(runId);
            Optional<CancellationReceipt> spent = Optional.empty();
            RunResult finished = null;
            boolean cancelled = failure instanceof RpcException refused && refused.code() == Protocol.ERROR_CANCELLED;
            if (cancelled) {
                spent = ((RpcException) failure).receipt();
            } else if (failure == null) {
                try {
                    finished = JSON.treeToValue(value, RunResult.class);
                    // A run can finish before the cancellation reaches it, and what it spent is owed all the same.
                    spent = Optional.of(new CancellationReceipt(finished.steps(), finished.cost(), finished.recordings(), finished.browser()));
                } catch (JacksonException e) {
                    failure = new FastbrowseException("the result could not be read", e);
                }
            }
            if (served.failure != null && (finished != null || cancelled)) {
                // A run can finish before the cancellation reaches it, and its callback failed all the same.
                result.complete(endedBy(served.failure, spent.orElse(null)));
            } else if (failure != null) {
                result.completeExceptionally(failure);
            } else {
                result.complete(finished);
            }
            result.receipt.complete(result.isCancelled() ? spent : Optional.empty());
        });
        return result;
    }

    /**
     * What a run this client started did and spent before it was cancelled: the steps it reported, every known
     * charge, an unpriced one as an {@code unknown} line, and its finished videos. Cancelling the future returns before the run's
     * browser has closed, so this completes later, once the server's reply arrives. It is empty for a run that was not
     * cancelled through its future, or that was cancelled before it was sent. A cancelled run proved nothing, so the
     * receipt has no status and no answer.
     *
     * @throws IllegalArgumentException for a future that {@link #run(String, RunOptions)} did not return
     */
    public static CompletableFuture<Optional<CancellationReceipt>> cancellation(Future<RunResult> run) {
        if (run instanceof RunFuture started) {
            return started.receipt.copy();
        }
        throw new IllegalArgumentException("not a future that Fastbrowse.run returned");
    }

    /** The future {@link #run(String, RunOptions)} returns, which stops the run when it is cancelled. */
    private final class RunFuture extends CompletableFuture<RunResult> {
        private final CompletableFuture<JsonNode> reply;
        private final Served served;
        final CompletableFuture<Optional<CancellationReceipt>> receipt = new CompletableFuture<>();

        RunFuture(CompletableFuture<JsonNode> reply, Served served) {
            this.reply = reply;
            this.served = served;
        }

        @Override
        public boolean cancel(boolean mayInterruptIfRunning) {
            settling = reply.handle((value, error) -> null);
            served.cancel();
            return super.cancel(mayInterruptIfRunning);
        }
    }

    /**
     * Asks the process to shut down, which stops a run under way, and waits until it has exited. A process still
     * running after the grace period is killed. Every call after the first waits on the first.
     */
    @Override
    public void close() {
        try {
            closeAsync().join();
        } catch (CompletionException e) {
            throw unwrapped(e);
        }
    }

    /** {@link #close()} without waiting: the future completes once the process has exited. */
    public synchronized CompletableFuture<Void> closeAsync() {
        if (closed == null) {
            // The reply says nothing the exit does not, and a process that died earlier answers with an error.
            connection.call(Protocol.METHOD_SHUTDOWN, null);
            closed = exited.copy()
                    .orTimeout(gracePeriod.toMillis(), TimeUnit.MILLISECONDS)
                    .exceptionallyCompose(timedOut -> {
                        process.destroyForcibly();
                        return exited;
                    });
        }
        return closed;
    }

    private void notify(String method, JsonNode params) {
        Served run = runs.get(params.path("run_id").asString());
        if (run == null || run.failure != null) {
            return;
        }
        try {
            if (method.equals(Protocol.SERVER_METHOD_RUN_EVENT) && run.options.onEvent() != null) {
                run.options.onEvent().accept(event(params.path("event")));
            } else if (method.equals(Protocol.SERVER_METHOD_RUN_FRAME) && run.options.onFrame() != null) {
                run.options.onFrame().accept(Base64.getDecoder().decode(params.path("frame").asString()));
            }
        } catch (Exception e) {
            run.failure = method + ": " + e.getMessage();
            run.cancel();
        }
    }

    private Object serve(String method, JsonNode params) throws Exception {
        if (method.equals(Protocol.SERVER_METHOD_RUN_UNTIL)) {
            RunUntilParams asked = JSON.treeToValue(params, RunUntilParams.class);
            Served run = runs.get(asked.runId());
            if (run == null || run.options.until() == null) {
                throw new FastbrowseException("run '" + asked.runId() + "' has no until callback waiting");
            }
            return run.options.until().test(asked.url());
        }
        if (method.equals(Protocol.SERVER_METHOD_SECRETS_RESOLVE)) {
            SecretsResolveParams asked = JSON.treeToValue(params, SecretsResolveParams.class);
            Served run = runs.get(asked.runId());
            if (run == null || run.options.resolver() == null) {
                throw new FastbrowseException("run '" + asked.runId() + "' has no secret resolver waiting");
            }
            Optional<String> value;
            try {
                value = run.options.resolver().resolve(asked.name(), asked.origin());
            } catch (Exception e) {
                // What a resolver throws may quote the value, so it stays in this process.
                throw new FastbrowseException("the resolver threw for the secret '" + asked.name() + "'");
            }
            return value.orElse(null);
        }
        throw new RpcException(Protocol.ERROR_METHOD_NOT_FOUND, "unknown method '" + method + "'");
    }

    private static Event event(JsonNode event) {
        return switch (event.path("type").asString()) {
            case "browser" -> new Event(JSON.treeToValue(event, BrowserEvent.class), null);
            case "step" -> new Event(null, JSON.treeToValue(event, StepEvent.class));
            default -> throw new FastbrowseException("an event of unknown type " + event.path("type"));
        };
    }

    private static RunParams params(String runId, String task, BrowserOptions browser, RunOptions options) {
        List<RunAttachment> attachments = options.attachments() == null ? null : options.attachments().stream()
                .map(attachment -> new RunAttachment(attachment.name(), attachment.mimeType(), attachment.content()))
                .toList();
        List<Long> viewport = browser.viewportWidth() == null ? null
                : List.of((long) browser.viewportWidth(), (long) browser.viewportHeight());
        return RunParams.builder()
                .runId(runId)
                .task(task)
                .start(options.start())
                .context(options.context())
                .inputs(options.inputs())
                .attachments(attachments)
                .limits(options.limits())
                .authorization(options.authorization())
                .outputSchema(options.outputSchema())
                .secrets(options.secrets())
                // The wire carries whether a callback is held, and the callback stays here.
                .frames(options.onFrame() != null ? true : null)
                .until(options.until() != null ? true : null)
                .downloads(options.downloads())
                .record(options.record())
                .recipe(options.recipe())
                .saveRecipe(options.saveRecipe())
                .cursor(options.cursor())
                .allowedOrigins(options.allowedOrigins())
                .fetch(options.fetch() ? true : null)
                .browser(options.browserFirst() ? true : null)
                .local(browser.local() ? true : null)
                .launch(browser.launch() ? true : null)
                .chrome(browser.chrome())
                .cloudProfile(browser.cloudProfile())
                .cdpUrl(browser.cdpUrl())
                .cdpPort(browser.cdpPort() == null ? null : (long) browser.cdpPort())
                .attach(browser.attach() ? true : null)
                .targetMatch(browser.targetMatch())
                .foreground(browser.foreground() ? true : null)
                .proxyCountry(browser.proxyCountry())
                .viewport(viewport)
                .cloudExtensions(browser.cloudExtensions())
                .cloudTimeoutMinutes(browser.cloudTimeoutMinutes() == null ? null : (long) browser.cloudTimeoutMinutes())
                .extension(browser.extension() ? true : null)
                .extensionBrowser(browser.extensionBrowser())
                .build();
    }

    /**
     * The result of a run that a throwing callback ended, in the shape the server gives one, with the steps, cost and
     * videos of what it did when the reply carried them.
     */
    private static RunResult endedBy(String failure, CancellationReceipt spent) {
        CancellationReceipt kept = spent != null ? spent : new CancellationReceipt(List.of(), new CostBreakdown(List.of()), List.of(), null);
        return RunResult.builder()
                .status(Status.ERROR)
                .evidence(List.of())
                .steps(kept.steps())
                .cost(kept.cost())
                .artifacts(List.of())
                .citations(List.of())
                .error(failure)
                .recordings(kept.recordings())
                .browser(kept.browser())
                .build();
    }

    private static FastbrowseException unwrapped(Exception e) {
        Throwable cause = e instanceof ExecutionException || e instanceof CompletionException ? e.getCause() : e;
        if (cause instanceof FastbrowseException fastbrowse) {
            return fastbrowse;
        }
        if (cause instanceof TimeoutException) {
            return new FastbrowseException("fastbrowse did not answer the handshake", cause);
        }
        return new FastbrowseException(String.valueOf(cause.getMessage()), cause);
    }
}
