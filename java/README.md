# fastbrowse for Java and Kotlin

A browser agent that picks instead of generating. Every page is indexed into the controls it actually has, and a
choice model picks one of them. An LLM plans, reads and writes prose. Code owns authorization gates, secret
resolution and cost limits, and reports an answer it cannot verify as unverified.

Add the client and the binary for each platform you run on (`natives-darwin-arm64`, `natives-darwin-x64`,
`natives-linux-arm64`, `natives-linux-x64`, `natives-win32-x64`). Java 17 or newer.

```xml
<dependency>
  <groupId>dev.agentlabs</groupId>
  <artifactId>fastbrowse</artifactId>
  <version>0.9.0</version>
</dependency>
<dependency>
  <groupId>dev.agentlabs</groupId>
  <artifactId>fastbrowse</artifactId>
  <version>0.9.0</version>
  <classifier>natives-linux-x64</classifier>
  <scope>runtime</scope>
</dependency>
```

```kotlin
// Gradle
implementation("dev.agentlabs:fastbrowse:0.9.0")
runtimeOnly("dev.agentlabs:fastbrowse:0.9.0:natives-linux-x64")
```

```java
try (Fastbrowse fastbrowse = Fastbrowse.start()) {
    RunResult result = fastbrowse.run("What is the top story's title?", RunOptions.builder()
            .start("https://news.ycombinator.com/")
            .limits(Limits.builder().maxDollars(0.25).build())
            .build()).join();
    System.out.println(result.status() + " " + result.answer());
}
```

```kotlin
Fastbrowse.start().use { fastbrowse ->
    val result = fastbrowse.run("What is the top story's title?", RunOptions.builder()
        .start("https://news.ycombinator.com/")
        .build()).await() // kotlinx-coroutines-jdk8; cancelling the coroutine stops the run
    println("${result.status()} ${result.answer()}")
}
```

`Fastbrowse.start()` runs the fastbrowse binary as `fastbrowse serve --stdio`. The binary comes from the natives jar
for the platform, unpacked once into the user cache directory; `StartOptions.binaryPath` or `FASTBROWSE_BINARY` runs
a binary of your own instead. Model keys come from the environment, as the fastbrowse command reads them:
`TYPESAFE_API_KEY` for Jev and `OPENROUTER_API_KEY` for the LLM. A run uses a Browser Use Cloud browser with
`BROWSER_USE_API_KEY`, or local Chrome with `BrowserOptions.builder().local(true)`.

`run` returns a `CompletableFuture<RunResult>`. A run ends in a `Status`; a run that needs a login or got stuck is
read from the result, not thrown. Cancelling the future stops the run, and the next run on the client waits until its
browser has closed. One client runs one task at a time: a second `run` while one is active fails with an
`RpcException` whose code is `Protocol.ERROR_BUSY`. Start one client per concurrent run.

`RunOptions` carries the rest: `outputSchema` for structured data, read back from `result.data()`; `secrets`, whose
values stay in your process and are asked for as they are typed; `until`, a check on the address a run ends on; and
`onEvent` and `onFrame` to follow a run as it goes. `outputSchema` and `result.data()` are Jackson 3 `JsonNode`s,
from `tools.jackson.databind`. The
[project README](https://github.com/agent-labs-dev/fastbrowse) covers the statuses, citations and costs.
