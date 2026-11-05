package dev.agentlabs.fastbrowse;

import static org.junit.jupiter.api.Assertions.assertArrayEquals;
import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertInstanceOf;
import static org.junit.jupiter.api.Assertions.assertNotNull;
import static org.junit.jupiter.api.Assertions.assertNull;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;
import static org.junit.jupiter.api.Assumptions.assumeTrue;

import tools.jackson.databind.JsonNode;
import tools.jackson.databind.ObjectMapper;
import tools.jackson.databind.node.ObjectNode;
import dev.agentlabs.fastbrowse.protocol.CancellationReceipt;
import dev.agentlabs.fastbrowse.protocol.CostBasis;
import dev.agentlabs.fastbrowse.protocol.CostLine;
import dev.agentlabs.fastbrowse.protocol.Limits;
import dev.agentlabs.fastbrowse.protocol.Protocol;
import dev.agentlabs.fastbrowse.protocol.RunResult;
import dev.agentlabs.fastbrowse.protocol.SecretRef;
import dev.agentlabs.fastbrowse.protocol.Status;
import java.io.ByteArrayOutputStream;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.time.Duration;
import java.util.ArrayList;
import java.util.List;
import java.util.Map;
import java.util.Optional;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CompletionException;
import java.util.concurrent.CopyOnWriteArrayList;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicReference;
import org.junit.jupiter.api.AfterEach;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.Timeout;
import org.junit.jupiter.api.io.TempDir;

/**
 * The client against the real server, with one of the stand-in runs it carries behind FASTBROWSE_TEST_SERVER in place
 * of the agent, so a run needs no browser and no model.
 */
@Timeout(60)
class FastbrowseTest {
    private static final ObjectMapper JSON = new ObjectMapper();
    private final List<Fastbrowse> started = new ArrayList<>();

    /** FASTBROWSE_TEST_BINARY, or the workspace's debug build. */
    private static Path serverBinary() {
        String named = System.getenv("FASTBROWSE_TEST_BINARY");
        if (named != null && !named.isEmpty()) {
            return Path.of(named);
        }
        Path built = Path.of("..", "target", "debug", "fastbrowse").toAbsolutePath();
        assumeTrue(Files.exists(built), "no fastbrowse binary: run `cargo build -p fastbrowse-cli --features scripted`, or set FASTBROWSE_TEST_BINARY");
        return built;
    }

    private Fastbrowse scripted(String run, StartOptions.Builder options) {
        Fastbrowse fastbrowse = Fastbrowse.start(options
                .binaryPath(serverBinary())
                .env(Map.of(
                        "FASTBROWSE_TEST_SERVER", "scripted_run",
                        "FASTBROWSE_TEST_RUN", run,
                        // The server refuses a run it has no model keys for, whatever would have run it.
                        "OPENROUTER_API_KEY", "unused",
                        "AI_GATEWAY_API_KEY", "unused",
                        "BROWSER_USE_API_KEY", "unused"))
                .build());
        started.add(fastbrowse);
        return fastbrowse;
    }

    private Fastbrowse scripted(String run) {
        return scripted(run, StartOptions.builder());
    }

    @AfterEach
    void closeAll() {
        started.forEach(Fastbrowse::close);
    }

    @Test
    void startReportsTheBinarysVersion() {
        assertFalse(scripted("echo").version().isEmpty());
    }

    @Test
    void aRunSendsItsOptionsUnderTheirWireNames() throws Exception {
        Fastbrowse fastbrowse = scripted("echo", StartOptions.builder()
                .browser(BrowserOptions.builder().local(true).foreground(true).build()));
        RunResult result = fastbrowse.run("find the price", RunOptions.builder()
                .start("https://shop.example/")
                .inputs(Map.of("size", "M"))
                .attachments(List.of(new Attachment("a.txt", "text/plain", "hello".getBytes(StandardCharsets.UTF_8))))
                .limits(Limits.builder().maxSteps(5L).build())
                .outputSchema(JSON.readTree("{\"type\": \"object\", \"properties\": {\"price\": {\"type\": \"number\"}}}"))
                .until(url -> true)
                .build()).join();
        assertEquals(Status.COMPLETE, result.status());
        assertEquals("find the price", result.answer());
        JsonNode params = result.data().path("params");
        assertEquals("https://shop.example/", params.path("start").asString());
        assertTrue(params.path("local").asBoolean());
        assertTrue(params.path("foreground").asBoolean());
        assertTrue(params.path("until").asBoolean());
        assertEquals(5, params.path("limits").path("max_steps").asInt());
        // What was not asked for is not sent, so the server applies its own defaults.
        for (String key : List.of("frames", "context", "cdp_port", "secrets")) {
            assertFalse(params.has(key), key + " was sent");
        }
        // The stand-in echoes the bytes the server decoded, as a list of numbers.
        StringBuilder decoded = new StringBuilder();
        result.data().path("attachments").get(0).forEach(each -> decoded.append((char) each.asInt()));
        assertEquals("hello", decoded.toString());
    }

    @Test
    void aRunsOwnBrowserReplacesTheClients() {
        Fastbrowse fastbrowse = scripted("echo", StartOptions.builder()
                .browser(BrowserOptions.builder().cdpPort(9222).build()));
        RunResult result = fastbrowse.run("t", RunOptions.builder()
                .browser(BrowserOptions.builder().local(true).build()).build()).join();
        JsonNode params = result.data().path("params");
        assertFalse(params.has("cdp_port"));
        assertTrue(params.path("local").asBoolean());
    }

    @Test
    void cloudBrowserAndRoutingOptionsReachTheServerUnderTheirWireNames() throws Exception {
        String extension = "0f4c7a52-3b1e-4d8a-9c6f-2e5b8d1a7c30";
        Fastbrowse fastbrowse = scripted("echo", StartOptions.builder()
                .browser(BrowserOptions.builder().cloudExtensions(List.of(extension)).cloudTimeoutMinutes(90).build()));
        JsonNode params = fastbrowse.run("t", RunOptions.builder()
                .browserFirst(true)
                .allowedOrigins(List.of("https://shop.example"))
                .build()).join().data().path("params");
        assertEquals(JSON.readTree("""
                {"task": "t", "cloud_extensions": ["%s"], "cloud_timeout_minutes": 90,
                 "allowed_origins": ["https://shop.example"], "browser": true}
                """.formatted(extension)), withoutRunId(params));

        // A run's own browser replaces the client's, cloud options with it, which is what lets this run fetch.
        JsonNode fetched = fastbrowse.run("t", RunOptions.builder()
                .browser(BrowserOptions.CLOUD)
                .fetch(true)
                .build()).join().data().path("params");
        assertEquals(JSON.readTree("{\"task\": \"t\", \"fetch\": true}"), withoutRunId(fetched));
    }

    private static JsonNode withoutRunId(JsonNode params) {
        ObjectNode copy = (ObjectNode) params.deepCopy();
        copy.remove("run_id");
        return copy;
    }

    @Test
    void fetchAndBrowserFirstTogetherAreRefusedByTheServer() {
        CompletionException refused = assertThrows(CompletionException.class, () -> scripted("echo")
                .run("t", RunOptions.builder().fetch(true).browserFirst(true).build()).join());
        RpcException error = assertInstanceOf(RpcException.class, refused.getCause());
        assertEquals(Protocol.ERROR_INVALID_PARAMS, error.code());
        assertTrue(error.getMessage().contains("fetch never starts a browser and browser always does"), error.getMessage());
    }

    @Test
    void anEmptyAllowedOriginsReachesTheServerAndIsRefusedRatherThanUnscoped() {
        CompletionException refused = assertThrows(CompletionException.class, () -> scripted("echo")
                .run("t", RunOptions.builder().allowedOrigins(List.of()).build()).join());
        RpcException error = assertInstanceOf(RpcException.class, refused.getCause());
        assertEquals(Protocol.ERROR_INVALID_PARAMS, error.code());
        assertTrue(error.getMessage().contains("allowed_origins is empty"), error.getMessage());
    }

    @Test
    void eventsArriveInTheOrderTheRunProducedThem() {
        List<Event> events = new CopyOnWriteArrayList<>();
        scripted("three_events").run("t", RunOptions.builder().onEvent(events::add).build()).join();
        assertEquals(3, events.size());
        assertEquals("https://live.example/1", events.get(0).browser().liveUrl());
        assertEquals(0, events.get(1).step().step().index());
        assertEquals(1, events.get(2).step().step().index());
    }

    @Test
    void anEventCallbackThatThrowsEndsTheRunWithStatusError() {
        AtomicInteger calls = new AtomicInteger();
        RunResult result = scripted("three_events").run("t", RunOptions.builder().onEvent(event -> {
            calls.incrementAndGet();
            throw new IllegalStateException("full disk");
        }).build()).join();
        assertEquals(Status.ERROR, result.status());
        assertEquals("run/event: full disk", result.error());
        assertEquals(1, calls.get());
    }

    @Test
    void aStatusIsAResultNotAnException() {
        assertEquals(Status.NEEDS_LOGIN, scripted("needs_login").run("t", RunOptions.defaults()).join().status());
    }

    @Test
    void untilDecidesWhetherTheRunIsComplete() {
        AtomicReference<String> asked = new AtomicReference<>();
        RunResult result = scripted("ends_on_an_order").run("t", RunOptions.builder().until(url -> {
            asked.set(url);
            return false;
        }).build()).join();
        assertEquals(Status.UNVERIFIED, result.status());
        assertEquals("https://shop.example/orders/17", asked.get());
    }

    @Test
    void aSecretIsResolvedEachTimeItIsTyped() {
        AtomicInteger calls = new AtomicInteger();
        RunResult result = scripted("types_secrets").run("t", RunOptions.builder()
                .inputs(Map.of("name", "password", "origin", "https://shop.example"))
                .secrets(List.of(new SecretRef("password", List.of("https://shop.example"))), (name, origin) -> {
                    calls.incrementAndGet();
                    return Optional.of(name + "@" + origin);
                })
                .build()).join();
        assertEquals(2, calls.get());
        assertEquals("password@https://shop.example", result.data().get(0).asString());
    }

    @Test
    void aResolverThatThrowsEndsTheRunWithoutWhatItSaid() {
        RunResult result = scripted("types_secrets").run("t", RunOptions.builder()
                .inputs(Map.of("name", "password", "origin", "https://shop.example"))
                .secrets(List.of(new SecretRef("password", List.of("https://shop.example"))), (name, origin) -> {
                    throw new IllegalStateException("vault said hunter2");
                })
                .build()).join();
        assertEquals(Status.ERROR, result.status());
        assertFalse(result.error().contains("hunter2"));
    }

    @Test
    void framesArriveAsTheirBytes() {
        List<byte[]> frames = new CopyOnWriteArrayList<>();
        scripted("two_frames").run("t", RunOptions.builder().onFrame(frames::add).build()).join();
        assertEquals(2, frames.size());
        assertEquals(256, frames.get(0).length);
        assertArrayEquals(new byte[] {(byte) 0xff, (byte) 0xd8, ' ', 's', 'e', 'c', 'o', 'n', 'd'}, frames.get(1));
    }

    @Test
    void cancellingTheFutureStopsTheRunAndTheNextRunWaitsForItsBrowser(@TempDir Path directory) throws Exception {
        Fastbrowse fastbrowse = scripted("until_cancelled");
        Path closed = directory.resolve("closed");
        CountDownLatch started = new CountDownLatch(1);
        CompletableFuture<RunResult> run = fastbrowse.run("t", RunOptions.builder()
                .inputs(Map.of("closed", closed.toString()))
                .onEvent(event -> started.countDown())
                .build());
        assertTrue(started.await(10, TimeUnit.SECONDS));
        assertTrue(run.cancel(true));
        // Not refused as busy: it starts once the cancelled run's browser has closed.
        RunResult next = fastbrowse.run("t", RunOptions.defaults()).join();
        assertEquals(Status.COMPLETE, next.status());
        assertTrue(Files.exists(closed), "the next run started before the browser closed");
    }

    @Test
    void aRunCancelledWhileItWaitsForTheBrowserBeforeItIsNeverSent(@TempDir Path directory) throws Exception {
        Fastbrowse fastbrowse = scripted("until_cancelled");
        CountDownLatch started = new CountDownLatch(1);
        CompletableFuture<RunResult> first = fastbrowse.run("t", RunOptions.builder()
                .inputs(Map.of("closed", directory.resolve("closed").toString()))
                .onEvent(event -> started.countDown())
                .build());
        assertTrue(started.await(10, TimeUnit.SECONDS));
        first.cancel(true);
        // Queued behind the first run's closing browser, and cancelled there.
        CompletableFuture<RunResult> second = fastbrowse.run("t", RunOptions.builder()
                .inputs(Map.of("closed", directory.resolve("second").toString()))
                .build());
        second.cancel(true);
        // The stand-in answers with how many runs the server was sent: the first and this one.
        RunResult third = fastbrowse.run("t", RunOptions.defaults()).get(15, TimeUnit.SECONDS);
        assertEquals(2, third.data().asInt());
        // Never sent, so it did and spent nothing.
        assertEquals(Optional.empty(), Fastbrowse.cancellation(second).get(15, TimeUnit.SECONDS));
    }

    @Test
    void aCancelledRunsReceiptArrivesOnceItsBrowserHasClosed(@TempDir Path directory) throws Exception {
        Fastbrowse fastbrowse = scripted("until_cancelled");
        Path closed = directory.resolve("closed");
        CountDownLatch started = new CountDownLatch(1);
        CompletableFuture<RunResult> run = fastbrowse.run("t", RunOptions.builder()
                .inputs(Map.of("closed", closed.toString()))
                .onEvent(event -> started.countDown())
                .build());
        assertTrue(started.await(10, TimeUnit.SECONDS));
        assertTrue(run.cancel(true));
        CancellationReceipt receipt = Fastbrowse.cancellation(run).get(15, TimeUnit.SECONDS).orElseThrow();
        assertTrue(Files.exists(closed), "the receipt arrived before the browser closed");
        // What the run did and spent before it stopped, with the unpriced browser left unpriced.
        assertEquals(1, receipt.steps().size());
        assertEquals(List.of(CostBasis.METERED, CostBasis.UNKNOWN),
                receipt.cost().lines().stream().map(CostLine::basis).toList());
        assertEquals(Double.valueOf(0.004), receipt.cost().lines().get(0).dollars());
        assertNull(receipt.cost().lines().get(1).dollars());
        assertEquals(List.of(), receipt.recordings());
    }

    @Test
    void aRunAThrowingCallbackStoppedKeepsWhatItDidAndSpent(@TempDir Path directory) {
        RunResult result = scripted("until_cancelled").run("t", RunOptions.builder()
                .inputs(Map.of("closed", directory.resolve("closed").toString()))
                .onEvent(event -> {
                    throw new IllegalStateException("full disk");
                })
                .build()).join();
        assertEquals(Status.ERROR, result.status());
        assertEquals("run/event: full disk", result.error());
        assertEquals(1, result.steps().size());
        assertEquals(2, result.cost().lines().size());
    }

    @Test
    void aFutureNotFromRunHasNoReceipt() {
        assertThrows(IllegalArgumentException.class, () -> Fastbrowse.cancellation(new CompletableFuture<>()));
    }

    @Test
    void aSecondRunWhileOneIsActiveIsRefusedAsBusy() throws Exception {
        Fastbrowse fastbrowse = scripted("never_ends");
        CountDownLatch started = new CountDownLatch(1);
        CompletableFuture<RunResult> first = fastbrowse.run("t", RunOptions.builder().onEvent(event -> started.countDown()).build());
        assertTrue(started.await(10, TimeUnit.SECONDS));
        CompletionException refused = assertThrows(CompletionException.class, () -> fastbrowse.run("t", RunOptions.defaults()).join());
        assertEquals(Protocol.ERROR_BUSY, assertInstanceOf(RpcException.class, refused.getCause()).code());
        first.cancel(true);
    }

    @Test
    void aRunOnAProcessThatExitsFailsWithProcessExited() {
        Fastbrowse fastbrowse = scripted("exits", StartOptions.builder().stderr(new ByteArrayOutputStream()));
        CompletionException failed = assertThrows(CompletionException.class, () -> fastbrowse.run("t", RunOptions.defaults()).join());
        assertEquals(7, assertInstanceOf(ProcessExitedException.class, failed.getCause()).exitCode());
        CompletionException later = assertThrows(CompletionException.class, () -> fastbrowse.run("t", RunOptions.defaults()).join());
        assertInstanceOf(ProcessExitedException.class, later.getCause());
    }

    @Test
    void closeWaitsForTheProcessAndCanBeCalledAgain() {
        Fastbrowse fastbrowse = scripted("echo");
        fastbrowse.close();
        fastbrowse.close();
        CompletionException failed = assertThrows(CompletionException.class, () -> fastbrowse.run("t", RunOptions.defaults()).join());
        assertInstanceOf(ProcessExitedException.class, failed.getCause());
    }

    @Test
    void closeFromACallbackReturns() throws Exception {
        Fastbrowse fastbrowse = scripted("never_ends", StartOptions.builder().gracePeriod(Duration.ofSeconds(5)));
        CompletableFuture<RunResult> run = fastbrowse.run("t", RunOptions.builder().onEvent(event -> fastbrowse.close()).build());
        assertThrows(Exception.class, () -> run.get(15, TimeUnit.SECONDS));
        assertTrue(run.isDone());
    }

    @Test
    void structuredDataIsReadIntoTheCallersType() throws Exception {
        RunResult result = scripted("found").run("t", RunOptions.builder()
                .inputs(Map.of("found", "{\"price\": 12.5}"))
                .outputSchema(JSON.readTree("{\"type\": \"object\", \"properties\": {\"price\": {\"type\": \"number\"}}, \"required\": [\"price\"]}"))
                .build()).join();
        record Found(double price) {}
        assertEquals(12.5, JSON.treeToValue(result.data(), Found.class).price());
        assertNotNull(result.cost());
    }

    @Test
    void legacyRunOptionsConstructorStillBuilds() {
        RunOptions options = new RunOptions(null, null, null, null, null, null, null, null, null, null, null, null,
                null, null, null, null);
        assertNull(options.recipe());
        assertNull(options.saveRecipe());
    }
}
