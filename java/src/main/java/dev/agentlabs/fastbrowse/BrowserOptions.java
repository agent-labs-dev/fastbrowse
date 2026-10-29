package dev.agentlabs.fastbrowse;

import dev.agentlabs.fastbrowse.protocol.LocalChrome;

/**
 * Which browser a run drives: a Browser Use Cloud browser by default, local Chrome with {@code local}, or a browser
 * that is already running through {@code cdpUrl} or {@code cdpPort}.
 *
 * @param local local Chrome instead of a Browser Use Cloud browser
 * @param chrome which local Chrome, and how; a headed window or a kept profile implies {@code local}
 * @param cloudProfile the Browser Use Cloud profile a cloud browser starts with
 * @param cdpUrl the DevTools WebSocket URL of a running browser; the run neither starts nor stops it
 * @param cdpPort the DevTools port of a browser running on this machine
 * @param attach drive a window already open in that browser, and open no tab
 * @param targetMatch part of the URL or title of the window to attach to
 * @param foreground bring the run's tab and its window to the front, to watch it
 * @param proxyCountry the two-letter country a cloud browser browses from
 * @param viewportWidth the page's width in CSS pixels, with {@code viewportHeight}
 * @param viewportHeight the page's height in CSS pixels, with {@code viewportWidth}
 * @param cloudAllowResizing let a cloud browser's window be resized
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
        boolean cloudAllowResizing) {

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
        private boolean cloudAllowResizing;

        private Builder() {}

        public Builder local(boolean local) {
            this.local = local;
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

        public Builder cloudAllowResizing(boolean cloudAllowResizing) {
            this.cloudAllowResizing = cloudAllowResizing;
            return this;
        }

        public BrowserOptions build() {
            return new BrowserOptions(local, chrome, cloudProfile, cdpUrl, cdpPort, attach, targetMatch, foreground,
                    proxyCountry, viewportWidth, viewportHeight, cloudAllowResizing);
        }
    }
}
