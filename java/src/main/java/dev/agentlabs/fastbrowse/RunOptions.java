package dev.agentlabs.fastbrowse;

import com.fasterxml.jackson.databind.JsonNode;
import dev.agentlabs.fastbrowse.protocol.Authorization;
import dev.agentlabs.fastbrowse.protocol.Limits;
import dev.agentlabs.fastbrowse.protocol.SecretRef;
import java.util.List;
import java.util.Map;
import java.util.function.Consumer;
import java.util.function.Predicate;

/**
 * One run's options. {@code RunOptions.builder().build()} runs the task with the client's browser and no limits.
 *
 * @param browser replaces the client's browser for this run
 * @param start the address to start on; without one, the first address is proposed from the task
 * @param context background the run may use but never treats as the task, such as where the request came from
 * @param inputs named values the task may use
 * @param attachments files the run may upload
 * @param limits caps on steps, calls, dollars and seconds
 * @param authorization what the run may do that it otherwise stops before
 * @param outputSchema the shape of the data to read, as JSON Schema (draft 2020-12) with an object at its root; the
 *     data is {@code RunResult.data()}
 * @param downloads a directory that keeps the files the run downloads
 * @param record where to write a video of the run; the server needs ffmpeg for it
 * @param cursor draw visual cursor feedback for visible local Chrome on Linux X11 with the Cua Driver
 * @param secrets the secrets the run may type, by name and origin; the values stay with {@code resolver}
 * @param resolver gives a secret's value when the run is about to type it
 * @param until called with the address the run ended on; false keeps it from {@code complete}
 * @param onEvent called with the browser event and then each step event, in order, on one thread; a slow one holds
 *     up the run, and one that throws ends it with status {@code error}
 * @param onFrame called with JPEG frames of the active tab while the run goes on; without it the page is never
 *     filmed, and one that throws ends the run with status {@code error}
 */
public record RunOptions(
        BrowserOptions browser,
        String start,
        String context,
        Map<String, String> inputs,
        List<Attachment> attachments,
        Limits limits,
        Authorization authorization,
        JsonNode outputSchema,
        String downloads,
        String record,
        boolean cursor,
        List<SecretRef> secrets,
        SecretResolver resolver,
        Predicate<String> until,
        Consumer<Event> onEvent,
        Consumer<byte[]> onFrame) {

    public static RunOptions defaults() {
        return builder().build();
    }

    public static Builder builder() {
        return new Builder();
    }

    public static final class Builder {
        private BrowserOptions browser;
        private String start;
        private String context;
        private Map<String, String> inputs;
        private List<Attachment> attachments;
        private Limits limits;
        private Authorization authorization;
        private JsonNode outputSchema;
        private String downloads;
        private String record;
        private boolean cursor;
        private List<SecretRef> secrets;
        private SecretResolver resolver;
        private Predicate<String> until;
        private Consumer<Event> onEvent;
        private Consumer<byte[]> onFrame;

        private Builder() {}

        public Builder browser(BrowserOptions browser) {
            this.browser = browser;
            return this;
        }

        public Builder start(String start) {
            this.start = start;
            return this;
        }

        public Builder context(String context) {
            this.context = context;
            return this;
        }

        public Builder inputs(Map<String, String> inputs) {
            this.inputs = Map.copyOf(inputs);
            return this;
        }

        public Builder attachments(List<Attachment> attachments) {
            this.attachments = List.copyOf(attachments);
            return this;
        }

        public Builder limits(Limits limits) {
            this.limits = limits;
            return this;
        }

        public Builder authorization(Authorization authorization) {
            this.authorization = authorization;
            return this;
        }

        public Builder outputSchema(JsonNode outputSchema) {
            this.outputSchema = outputSchema;
            return this;
        }

        public Builder downloads(String downloads) {
            this.downloads = downloads;
            return this;
        }

        public Builder record(String record) {
            this.record = record;
            return this;
        }

        public Builder cursor(boolean cursor) {
            this.cursor = cursor;
            return this;
        }

        public Builder secrets(List<SecretRef> secrets, SecretResolver resolver) {
            this.secrets = List.copyOf(secrets);
            this.resolver = resolver;
            return this;
        }

        public Builder until(Predicate<String> until) {
            this.until = until;
            return this;
        }

        public Builder onEvent(Consumer<Event> onEvent) {
            this.onEvent = onEvent;
            return this;
        }

        public Builder onFrame(Consumer<byte[]> onFrame) {
            this.onFrame = onFrame;
            return this;
        }

        public RunOptions build() {
            return new RunOptions(browser, start, context, inputs, attachments, limits, authorization, outputSchema,
                    downloads, record, cursor, secrets, resolver, until, onEvent, onFrame);
        }
    }
}
