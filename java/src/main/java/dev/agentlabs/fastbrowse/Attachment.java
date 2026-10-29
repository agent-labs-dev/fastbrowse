package dev.agentlabs.fastbrowse;

/** A file a run may upload. */
public record Attachment(String name, String mimeType, byte[] content) {}
