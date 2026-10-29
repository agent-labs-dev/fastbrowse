package dev.agentlabs.fastbrowse;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.io.ByteArrayInputStream;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.Map;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.io.TempDir;

class BinariesTest {
    @Test
    void theCallersPathComesFirstAndThenTheEnvironments() {
        assertEquals(Path.of("/mine"), Binaries.resolve(Path.of("/mine"), Map.of("FASTBROWSE_BINARY", "/env")));
        assertEquals(Path.of("/env"), Binaries.resolve(null, Map.of("FASTBROWSE_BINARY", "/env")));
    }

    @Test
    void withoutANativesJarTheErrorNamesTheOneToAdd() {
        // The test classpath carries no natives jar, so the bundled binary is not there.
        FastbrowseException missing = assertThrows(FastbrowseException.class, () -> Binaries.resolve(null, Map.of()));
        assertTrue(missing.getMessage().contains("dev.agentlabs:fastbrowse:"), missing.getMessage());
        assertTrue(missing.getMessage().contains(":natives-"), missing.getMessage());
    }

    @Test
    void platformsAreNamedAsTheReleaseNamesThem() {
        assertEquals("darwin-arm64", Binaries.platform("Mac OS X", "aarch64"));
        assertEquals("linux-x64", Binaries.platform("Linux", "amd64"));
        assertEquals("win32-x64", Binaries.platform("Windows 11", "amd64"));
        assertThrows(FastbrowseException.class, () -> Binaries.platform("FreeBSD", "amd64"));
    }

    @Test
    void theBinaryIsUnpackedOnceAndRunnable(@TempDir Path cache) throws Exception {
        Path binary = Binaries.unpack(new ByteArrayInputStream("#!/bin/sh\n".getBytes(StandardCharsets.UTF_8)), cache, "fastbrowse");
        assertTrue(Files.isExecutable(binary));
        Path again = Binaries.unpack(new ByteArrayInputStream("other".getBytes(StandardCharsets.UTF_8)), cache, "fastbrowse");
        assertEquals(binary, again);
        assertEquals("#!/bin/sh\n", Files.readString(binary));
    }
}
