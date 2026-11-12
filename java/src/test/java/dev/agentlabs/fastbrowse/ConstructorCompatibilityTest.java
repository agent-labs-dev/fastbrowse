package dev.agentlabs.fastbrowse;

import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertNull;

import dev.agentlabs.fastbrowse.protocol.RunParams;
import org.junit.jupiter.api.Test;

class ConstructorCompatibilityTest {
    @Test
    void publishedRunConstructorsKeepLaunchUnspecified() {
        assertNull(new RunParams(null, null, null, null, null, null, null, null, null, null, null, null, null, null, null, null, null, null, null, null, null, null, null, null, null, null, null, null, null, null, null, null).launch());
        assertNull(new RunParams(null, null, null, null, null, null, null, null, null, null, null, null, null, null, null, null, null, null, null, null, null, null, null, null, null, null, null, null, null, null, null, null, null, null).launch());
        assertNull(new RunParams(null, null, null, null, null, null, null, null, null, null, null, null, null, null, null, null, null, null, null, null, null, null, null, null, null, null, null, null, null, null, null, null, null, null, null).launch());
    }

    @Test
    void publishedBrowserConstructorDoesNotRequestLaunch() {
        BrowserOptions browser = new BrowserOptions(false, null, null, null, null, false, null, false,
                null, null, null, null, null, false, null);
        assertFalse(browser.launch());
    }
}
