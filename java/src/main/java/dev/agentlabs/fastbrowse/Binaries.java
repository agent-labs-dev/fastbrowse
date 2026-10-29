package dev.agentlabs.fastbrowse;

import java.io.IOException;
import java.io.InputStream;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.StandardCopyOption;
import java.util.Locale;
import java.util.Map;
import java.util.Properties;

/** Where the fastbrowse executable comes from. */
final class Binaries {
    private static final String OWN_BINARY =
            "set StartOptions.binaryPath or FASTBROWSE_BINARY to run a fastbrowse binary of your own";

    private Binaries() {}

    /**
     * The executable to start: the caller's own path first, then the one the environment names, then the binary in the
     * {@code natives-<platform>} jar on the classpath, unpacked once into the user cache directory.
     */
    static Path resolve(Path path, Map<String, String> environment) {
        if (path != null) {
            return path;
        }
        String named = environment.get("FASTBROWSE_BINARY");
        if (named != null && !named.isEmpty()) {
            return Path.of(named);
        }
        String platform = platform(System.getProperty("os.name"), System.getProperty("os.arch"));
        String executable = platform.startsWith("win32-") ? "fastbrowse.exe" : "fastbrowse";
        String resource = "/dev/agentlabs/fastbrowse/natives/" + platform + "/" + executable;
        try (InputStream bundled = Binaries.class.getResourceAsStream(resource)) {
            if (bundled == null) {
                throw new FastbrowseException("no fastbrowse binary for " + platform + " is on the classpath: add "
                        + "dev.agentlabs:fastbrowse:" + version() + ":natives-" + platform + ", or " + OWN_BINARY);
            }
            return unpack(bundled, cache().resolve(version()).resolve(platform), executable);
        } catch (IOException e) {
            throw new FastbrowseException("could not unpack the fastbrowse binary for " + platform, e);
        }
    }

    /** The release's name for a platform, as its natives jars and the npm packages are named. */
    static String platform(String os, String arch) {
        String system = os.toLowerCase(Locale.ROOT);
        String cpu = arch.toLowerCase(Locale.ROOT);
        String name = (system.startsWith("mac") ? "darwin" : system.startsWith("windows") ? "win32" : system.startsWith("linux") ? "linux" : system)
                + "-" + (cpu.equals("amd64") || cpu.equals("x86_64") ? "x64" : cpu.equals("aarch64") ? "arm64" : cpu);
        if (!java.util.Set.of("darwin-arm64", "darwin-x64", "linux-arm64", "linux-x64", "win32-x64").contains(name)) {
            throw new FastbrowseException("there is no fastbrowse binary for " + os + " " + arch + "; " + OWN_BINARY);
        }
        return name;
    }

    /**
     * Writes the binary under {@code directory} unless it is there already. It is written beside its place and moved
     * into it, so a start that stops partway leaves nothing a later start would run.
     */
    static Path unpack(InputStream bundled, Path directory, String executable) throws IOException {
        Path binary = directory.resolve(executable);
        if (Files.isExecutable(binary)) {
            return binary;
        }
        Files.createDirectories(directory);
        Path written = Files.createTempFile(directory, "binary-", ".tmp");
        try {
            Files.copy(bundled, written, StandardCopyOption.REPLACE_EXISTING);
            if (!written.toFile().setExecutable(true)) {
                throw new IOException("could not make " + written + " executable");
            }
            try {
                Files.move(written, binary, StandardCopyOption.ATOMIC_MOVE);
            } catch (IOException e) {
                // Another start unpacked it first, and Windows will not move over a file.
                if (!Files.isExecutable(binary)) {
                    throw e;
                }
            }
            return binary;
        } finally {
            Files.deleteIfExists(written);
        }
    }

    /** The version this library was built as, which the natives jar beside it carries the binary of. */
    static String version() {
        Properties properties = new Properties();
        try (InputStream in = Binaries.class.getResourceAsStream("/dev/agentlabs/fastbrowse/version.properties")) {
            if (in != null) {
                properties.load(in);
            }
        } catch (IOException e) {
            // A library without the file names no version; the cache directory says so.
        }
        return properties.getProperty("version", "unknown");
    }

    private static Path cache() {
        String os = System.getProperty("os.name").toLowerCase(Locale.ROOT);
        String home = System.getProperty("user.home");
        if (os.startsWith("windows")) {
            String local = System.getenv("LOCALAPPDATA");
            return Path.of(local != null ? local : home, "fastbrowse");
        }
        if (os.startsWith("mac")) {
            return Path.of(home, "Library", "Caches", "fastbrowse");
        }
        String xdg = System.getenv("XDG_CACHE_HOME");
        return Path.of(xdg != null && !xdg.isEmpty() ? xdg : Path.of(home, ".cache").toString(), "fastbrowse");
    }
}
