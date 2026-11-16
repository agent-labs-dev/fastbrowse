package dev.agentlabs.fastbrowse;

import dev.agentlabs.fastbrowse.protocol.LocalChrome;
import java.util.List;

/**
 * Which browser a run drives: a Browser Use Cloud browser by default, local Chrome with {@code local}, or a browser
 * that is already running through {@code cdpUrl} or {@code cdpPort}.
 *
 * @param local a browser on this machine instead of a Browser Use Cloud browser: your own Chrome through the fastbrowse
 *     extension, unless {@code launch} asks for a separate one
 * @param chrome which local Chrome, and how; a headed window or a kept profile implies {@code launch}
 * @param cloudProfile the Browser Use Cloud profile a cloud browser starts with
 * @param cdpUrl the DevTools WebSocket URL of a running browser; the run neither starts nor stops it
 * @param cdpPort the DevTools port of a browser running on this machine
 * @param attach drive a window already open in that browser, and open no tab
 * @param targetMatch part of the URL or title of the window to attach to
 * @param foreground bring the run's tab and its window to the front, to watch it
 * @param proxyCountry the two-letter country a cloud browser browses from
 * @param viewportWidth the page's width in CSS pixels, with {@code viewportHeight}
 * @param viewportHeight the page's height in CSS pixels, with {@code viewportWidth}
 * @param cloudExtensions up to three IDs of ready extensions on the Browser Use Cloud account, loaded into a cloud
 *     browser
 * @param cloudTimeoutMinutes how long a cloud browser may live: 15 when null, and the service allows up to 240
 * @param extension drive the user's own Chrome through the fastbrowse extension, in a tab group of the run's own
 * @param extensionBrowser the label of the connected browser to use when more than one has the extension
 * @param launch start a separate local Chrome instead of driving your own through the extension; implies
 *     {@code local}
 */
public record BrowserOptions(
        boolean local,
        LocalChrome chrome,
        String cloudProfile,
        String cdpUrl,
        Integer cdpPort,
        boolean attach,
        String targetMatch,
        boolean foreground,
        String proxyCountry,
        Integer viewportWidth,
        Integer viewportHeight,
        List<String> cloudExtensions,
        Integer cloudTimeoutMinutes,
        boolean extension,
        String extensionBrowser,
        boolean launch) {

    /** The constructor published before separate local launch selection was added. */
    public BrowserOptions(boolean local, LocalChrome chrome, String cloudProfile, String cdpUrl,
            Integer cdpPort, boolean attach, String targetMatch, boolean foreground, String proxyCountry,
            Integer viewportWidth, Integer viewportHeight, List<String> cloudExtensions,
            Integer cloudTimeoutMinutes, boolean extension, String extensionBrowser) {
        this(local, chrome, cloudProfile, cdpUrl, cdpPort, attach, targetMatch, foreground, proxyCountry,
                viewportWidth, viewportHeight, cloudExtensions, cloudTimeoutMinutes, extension, extensionBrowser,
                false);
    }

    /** The default: a Browser Use Cloud browser. */
    public static final BrowserOptions CLOUD = builder().build();

    public static Builder builder() {
        return new Builder();
    }

    public static final class Builder {
        private boolean local;
        private LocalChrome chrome;
        private String cloudProfile;
        private String cdpUrl;
        private Integer cdpPort;
        private boolean attach;
        private String targetMatch;
        private boolean foreground;
        private String proxyCountry;
        private Integer viewportWidth;
        private Integer viewportHeight;
        private List<String> cloudExtensions;
        private Integer cloudTimeoutMinutes;
        private boolean extension;
        private String extensionBrowser;
        private boolean launch;

        private Builder() {}

        public Builder local(boolean local) {
            this.local = local;
            return this;
        }

        public Builder launch(boolean launch) {
            this.launch = launch;
            return this;
        }

        public Builder chrome(LocalChrome chrome) {
            this.chrome = chrome;
            return this;
        }

        public Builder cloudProfile(String cloudProfile) {
            this.cloudProfile = cloudProfile;
            return this;
        }

        public Builder cdpUrl(String cdpUrl) {
            this.cdpUrl = cdpUrl;
            return this;
        }

        public Builder cdpPort(int cdpPort) {
            this.cdpPort = cdpPort;
            return this;
        }

        public Builder attach(boolean attach) {
            this.attach = attach;
            return this;
        }

        public Builder targetMatch(String targetMatch) {
            this.targetMatch = targetMatch;
            return this;
        }

        public Builder foreground(boolean foreground) {
            this.foreground = foreground;
            return this;
        }

        public Builder proxyCountry(String proxyCountry) {
            this.proxyCountry = proxyCountry;
            return this;
        }

        public Builder viewport(int width, int height) {
            this.viewportWidth = width;
            this.viewportHeight = height;
            return this;
        }

        public Builder cloudExtensions(List<String> cloudExtensions) {
            this.cloudExtensions = List.copyOf(cloudExtensions);
            return this;
        }

        public Builder cloudTimeoutMinutes(int cloudTimeoutMinutes) {
            this.cloudTimeoutMinutes = cloudTimeoutMinutes;
            return this;
        }

        public Builder extension(boolean extension) {
            this.extension = extension;
            return this;
        }

        public Builder extensionBrowser(String extensionBrowser) {
            this.extensionBrowser = extensionBrowser;
            return this;
        }

        public BrowserOptions build() {
            return new BrowserOptions(local, chrome, cloudProfile, cdpUrl, cdpPort, attach, targetMatch, foreground,
                    proxyCountry, viewportWidth, viewportHeight, cloudExtensions, cloudTimeoutMinutes, extension,
                    extensionBrowser, launch);
        }
    }
}
