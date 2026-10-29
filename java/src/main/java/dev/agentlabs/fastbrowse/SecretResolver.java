package dev.agentlabs.fastbrowse;

import java.util.Optional;

/**
 * Gives the value of a secret as a run is about to type it. Called each time a value is typed and never for an origin
 * the secret's ref does not cover, so a one-time code is computed when it is needed. It runs on a thread of its own
 * and may block. An exception ends the run with status {@code error}, and what it says is kept out of the result.
 */
@FunctionalInterface
public interface SecretResolver {
    Optional<String> resolve(String name, String origin) throws Exception;
}
