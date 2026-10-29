package dev.agentlabs.fastbrowse;

import java.io.OutputStream;
import java.nio.file.Path;
import java.time.Duration;
import java.util.Map;

/**
 * How to start the fastbrowse process, and the browser every run uses unless the run names another.
 *
 * @param binaryPath the fastbrowse executable to run; without it, {@code FASTBROWSE_BINARY} names one, and without
 *     that it is the binary in the {@code natives-<platform>} jar on the classpath
 * @param env environment variables for the fastbrowse process on top of this one's; model and browser keys are read
 *     from that environment
 * @param stderr where the process's log goes; null is this process's stderr
 * @param browser the browser every run drives unless {@link RunOptions#browser()} names another
 * @param gracePeriod how long {@link Fastbrowse#close()} gives the process to exit before it is killed
 */
public record StartOptions(
        Path binaryPath, Map<String, String> env, OutputStream stderr, BrowserOptions browser, Duration gracePeriod) {

    public static StartOptions defaults() {
        return builder().build();
    }

    public static Builder builder() {
        return new Builder();
    }

    public static final class Builder {
        private Path binaryPath;
        private Map<String, String> env = Map.of();
        private OutputStream stderr;
        private BrowserOptions browser = BrowserOptions.CLOUD;
        // Three times the 10 s the server gives Chrome to exit before it kills it: a server killed first orphans that
        // Chrome.
        private Duration gracePeriod = Duration.ofSeconds(30);

        private Builder() {}

        public Builder binaryPath(Path binaryPath) {
            this.binaryPath = binaryPath;
            return this;
        }

        public Builder env(Map<String, String> env) {
            this.env = Map.copyOf(env);
            return this;
        }

        public Builder stderr(OutputStream stderr) {
            this.stderr = stderr;
            return this;
        }

        public Builder browser(BrowserOptions browser) {
            this.browser = browser;
            return this;
        }

        public Builder gracePeriod(Duration gracePeriod) {
            this.gracePeriod = gracePeriod;
            return this;
        }

        public StartOptions build() {
            return new StartOptions(binaryPath, env, stderr, browser, gracePeriod);
        }
    }
}
