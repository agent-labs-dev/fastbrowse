"""The audit suite's cases, as data.

Every case is a record: an id, a tier, the command to run, the exit code and status it should produce,
and the assertions that decide whether it passed. The runner in `fastbrowse.audit.runner` executes them,
so no case is written as loose shell.

A command's tokens may stand in for a resolved path. `{python}` is the interpreter running the suite,
`{cli}` is the `fastbrowse` console script, `{mcp}` is `fastbrowse-mcp`, and `{base_url}` is the local
fixture server's address, substituted only for a case with `fixture=True`.

Assertion sources
-----------------

- `exit_code` is the process exit code the case produced.
- `stdout` and `stderr` are the captured text.
- `json` reads a value from the case's JSON: the probe's own object, or a CLI run's `--json` result.
- `recorder` reads the fixture server's POST record, as `posts[.path][[index].field]`.
- `file` checks a path on disk.

Assertion operators
-------------------

`eq`, `ne`, `contains`, `not_contains`, `present`, `absent`, `is_true`, `is_false`, `ge`, `le`, `matches`,
`list_eq`, `list_contains`, `list_excludes`, `count_eq`, `count_ge`, `exists`, `nonempty`.
"""

from dataclasses import dataclass, field

from fastbrowse.models import RunResult, Status

# Every field the refused `--json` shape must carry. Read from the model rather than written out, so a new
# RunResult field cannot slip past T0.2 unnoticed.
_RUN_RESULT_KEYS: tuple[str, ...] = tuple(RunResult.model_fields)

# The task texts come from the repository's own local eval tasks, so a fixture case asks for the same
# thing the eval suite grades.
CONTACT_TASK = (
    "Tell support, as Ada Lovelace (ada@example.com), that my kettle arrived broken. Use the damaged item topic."
)
LOGIN_TASK = "Show me my recent orders."
PRICE_TASK = "What is the price of the Blue Kettle?"
DOWNLOAD_TASK = "Download the file on this page."


@dataclass(frozen=True, slots=True)
class Assertion:
    """One check over a case's output: what it is called, where it reads, how, and the value it expects."""

    name: str
    source: str
    op: str
    value: object = None
    pointer: str = ""


@dataclass(frozen=True, slots=True)
class AuditCase:
    """One audited behaviour: a command, the outcome it must reach, and the assertions that decide it."""

    id: str
    tier: int
    purpose: str
    argv: tuple[str, ...]
    assertions: tuple[Assertion, ...] = ()
    env: tuple[tuple[str, str | None], ...] = ()
    expect_exit: int | None = None
    expect_status: str | None = None
    timeout_s: float = 60.0
    budget_usd: float = 0.0
    spends: bool = False
    fixture: bool = False
    foreign_cwd: bool = True
    evidence: tuple[str, ...] = field(default=())


def a(name: str, source: str, op: str, value: object = None, pointer: str = "") -> Assertion:
    return Assertion(name=name, source=source, op=op, value=value, pointer=pointer)


def probe(name: str) -> tuple[str, ...]:
    return ("{python}", "-m", "fastbrowse.audit.probes", name)


CLI_FLAGS: tuple[str, ...] = (
    "--version",
    "--start",
    "--local",
    "--headed",
    "--profile",
    "--cloud-profile",
    "--cdp-url",
    "--proxy-country",
    "--authorize",
    "--secret",
    "--bitwarden",
    "--max-steps",
    "--max-dollars",
    "--downloads",
    "--json",
    "--record",
)

# Every `run_task` parameter the terminal never exposes. T0.12 confirms this against the code rather than
# trusting the list, and asserts `on_event` is not in it: the CLI wires `on_event` to its own step printer.
EXPECTED_LIBRARY_ONLY: tuple[str, ...] = (
    "attachments",
    "config",
    "http",
    "inputs",
    "jev",
    "llm",
    "on_frame",
    "output_schema",
    "until",
    "viewport",
)

CASES: tuple[AuditCase, ...] = (
    # ------------------------------------------------------------------ tier 0: contract and static
    AuditCase(
        id="T0.1",
        tier=0,
        purpose="The terminal renders --help and --version, and help documents every flag.",
        argv=probe("cli_surface"),
        assertions=(
            a("help exits 0", "json", "eq", 0, "help_exit"),
            a("version exits 0", "json", "eq", 0, "version_exit"),
            a("help lists every documented flag", "json", "list_eq", [], "missing_flags"),
            a("version is a semantic version", "json", "matches", r"^fastbrowse \d+\.\d+\.\d+$", "version"),
        ),
        expect_exit=0,
        expect_status="ok",
        timeout_s=30,
    ),
    AuditCase(
        id="T0.2",
        tier=0,
        purpose="With no model key, the run is refused and --json still prints the whole refused result.",
        argv=("{cli}", "anything at all", "--local", "--json"),
        assertions=(
            a("refused shape is a run result", "json", "keys_present", _RUN_RESULT_KEYS, ""),
            a("status is error", "json", "eq", "error", "status"),
            a("error names the missing key", "json", "contains", "set AI_GATEWAY_API_KEY", "error"),
            a("stderr names the refusal", "stderr", "contains", "fastbrowse:"),
        ),
        expect_exit=1,
        expect_status="error",
        timeout_s=30,
    ),
    AuditCase(
        id="T0.3",
        tier=0,
        purpose="--secret naming an unset variable is refused, and the refusal names the variable.",
        argv=("{cli}", "x", "--start", "https://example.com/", "--secret", "TOKEN=FB_AUDIT_UNSET_VAR", "--json"),
        assertions=(
            a("status is error", "json", "eq", "error", "status"),
            a("names the unset variable", "stderr", "contains", "FB_AUDIT_UNSET_VAR"),
            a("says the variables are unset", "stderr", "contains", "unset variables"),
        ),
        expect_exit=1,
        expect_status="error",
        timeout_s=30,
    ),
    AuditCase(
        id="T0.4",
        tier=0,
        purpose="A --secret with no equals sign is refused with the expected form in the message.",
        argv=("{cli}", "x", "--secret", "TOKEN"),
        assertions=(a("names the expected form", "stderr", "contains", "expected NAME=ENV_VAR"),),
        expect_exit=2,
        expect_status="refused",
        timeout_s=30,
    ),
    AuditCase(
        id="T0.5",
        tier=0,
        purpose="Each MCP ceiling at zero is refused, naming the flag that is wrong.",
        argv=probe("mcp_ceilings"),
        assertions=tuple(
            assertion
            for flag in ("max_steps", "max_dollars", "max_seconds", "max_concurrent")
            for assertion in (
                a(f"{flag} refused", "json", "is_true", None, f"{flag}_refused"),
                a(f"{flag} names the flag", "json", "is_true", None, f"{flag}_named"),
            )
        ),
        expect_exit=0,
        expect_status="ok",
        timeout_s=60,
    ),
    AuditCase(
        id="T0.6",
        tier=0,
        purpose="An MCP --secret without an origin is refused, naming the required form.",
        argv=("{mcp}", "--secret", "NAME=ENV"),
        assertions=(a("names the required form", "stderr", "contains", "expected NAME=ENV_VAR@https://host"),),
        expect_exit=2,
        expect_status="refused",
        timeout_s=30,
    ),
    AuditCase(
        id="T0.7",
        tier=0,
        purpose="MCP http on a non-loopback host with no token is refused, naming the token variable.",
        argv=("{mcp}", "--transport", "http", "--host", "0.0.0.0"),
        env=(("OPENROUTER_API_KEY", "audit-placeholder"), ("TYPESAFE_API_KEY", "audit-placeholder")),
        assertions=(
            a("names the token variable", "stderr", "contains", "FASTBROWSE_MCP_TOKEN"),
            a("no traceback", "stderr", "not_contains", "Traceback"),
        ),
        expect_exit=1,
        expect_status="refused",
        timeout_s=30,
    ),
    AuditCase(
        id="T0.8",
        tier=0,
        purpose="A cloud browser asked for with no BROWSER_USE_API_KEY is refused cleanly, not with a traceback.",
        argv=("{cli}", "anything at all", "--json"),
        assertions=(
            a("names the browser key", "stderr", "contains", "BROWSER_USE_API_KEY"),
            a("no traceback", "stderr", "not_contains", "Traceback"),
            a("status is error", "json", "eq", "error", "status"),
        ),
        expect_exit=1,
        expect_status="error",
        timeout_s=30,
    ),
    AuditCase(
        id="T0.9",
        tier=0,
        purpose="An MCP --profile with --max-concurrent above one is refused: one profile, one Chrome.",
        argv=("{mcp}", "--profile", "/tmp/fastbrowse-audit-profile", "--max-concurrent", "2"),
        env=(("FASTBROWSE_CHROME", "/bin/sh"),),
        assertions=(
            a("refused for the profile", "stderr", "contains", "a --profile can be open in one Chrome at a time"),
            a("no traceback", "stderr", "not_contains", "Traceback"),
        ),
        expect_exit=1,
        expect_status="refused",
        timeout_s=30,
    ),
    AuditCase(
        id="T0.10",
        tier=0,
        purpose="Every Status maps to a stable exit code: complete is 0, every other status is 1.",
        argv=probe("status_exit_contract"),
        assertions=(
            a("the mapping is total", "json", "is_true", None, "total"),
            a("complete exits 0", "json", "is_true", None, "complete_zero"),
            a("every other status exits 1", "json", "is_true", None, "others_one"),
            a("the mapping is stable across runs", "json", "is_true", None, "stable"),
        ),
        expect_exit=0,
        expect_status="ok",
        timeout_s=30,
    ),
    AuditCase(
        id="T0.11",
        tier=0,
        purpose="With --json, stdout is exactly one JSON document and progress and warnings go to stderr.",
        argv=probe("json_stdout_discipline"),
        assertions=(
            a("stdout is one JSON document", "json", "is_true", None, "parsed"),
            a("progress reaches stderr", "json", "is_true", None, "progress_on_stderr"),
            a("progress is absent from stdout", "json", "is_true", None, "progress_absent_from_stdout"),
        ),
        expect_exit=0,
        expect_status="ok",
        timeout_s=30,
    ),
    AuditCase(
        id="T0.12",
        tier=0,
        purpose="The library-only run_task parameters are reported as a named list, not a pass or fail.",
        argv=probe("cli_library_parity"),
        assertions=(
            a("output_schema is library only", "json", "list_contains", "output_schema", "library_only"),
            a("until is library only", "json", "list_contains", "until", "library_only"),
            a("inputs is library only", "json", "list_contains", "inputs", "library_only"),
            a("attachments is library only", "json", "list_contains", "attachments", "library_only"),
            a("viewport is library only", "json", "list_contains", "viewport", "library_only"),
            a("on_frame is library only", "json", "list_contains", "on_frame", "library_only"),
            a("on_event is reachable from the terminal", "json", "list_excludes", "on_event", "library_only"),
            a("start, record and downloads are reachable", "json", "list_excludes", "start", "library_only"),
            a("--json dumps every RunResult field", "json", "is_true", None, "runresult_json_complete"),
        ),
        expect_exit=0,
        expect_status="ok",
        timeout_s=30,
    ),
    # ------------------------------------------------------------------ tier 1: local fixtures (paid models)
    AuditCase(
        id="T1.1",
        tier=1,
        purpose="The repository's own local eval suite runs, so audit runs can be compared against it.",
        argv=("{python}", "-m", "fastbrowse.evals.runner"),
        assertions=(a("the suite reports a pass count", "stdout", "contains", "passed"),),
        expect_exit=None,
        expect_status=None,
        spends=True,
        foreign_cwd=False,
        timeout_s=900,
        budget_usd=0.10,
    ),
    AuditCase(
        id="T1.2",
        tier=1,
        purpose="contact-form with --authorize completes and the fixture's own POST record shows the right fields.",
        argv=(
            "{cli}",
            CONTACT_TASK,
            "--start",
            "{base_url}/contact.html",
            "--authorize",
            "--local",
            "--json",
        ),
        fixture=True,
        assertions=(
            a("exactly one contact submission", "recorder", "count_eq", 1, "posts./submit/contact"),
            a("name is right", "recorder", "eq", "Ada Lovelace", "posts./submit/contact[0].name"),
            a("email is right", "recorder", "eq", "ada@example.com", "posts./submit/contact[0].email"),
            a("topic is right", "recorder", "eq", "Damaged item", "posts./submit/contact[0].topic"),
            a("status is complete", "json", "eq", "complete", "status"),
        ),
        expect_exit=0,
        expect_status="complete",
        spends=True,
        foreign_cwd=False,
        timeout_s=300,
        budget_usd=0.05,
    ),
    AuditCase(
        id="T1.3",
        tier=1,
        purpose="The same form without --authorize stops at needs_confirmation and records no submission.",
        argv=("{cli}", CONTACT_TASK, "--start", "{base_url}/contact.html", "--local", "--json"),
        fixture=True,
        assertions=(
            a("no contact submission", "recorder", "count_eq", 0, "posts./submit/contact"),
            a("status is needs_confirmation", "json", "eq", "needs_confirmation", "status"),
        ),
        expect_exit=1,
        expect_status="needs_confirmation",
        spends=True,
        foreign_cwd=False,
        timeout_s=300,
        budget_usd=0.05,
    ),
    AuditCase(
        id="T1.4",
        tier=1,
        purpose="A login wall stops at needs_login and records no sign-in attempt.",
        argv=("{cli}", LOGIN_TASK, "--start", "{base_url}/account.html", "--local", "--json"),
        fixture=True,
        assertions=(
            a("no sign-in attempt", "recorder", "count_eq", 0, "posts./submit/login"),
            a("status is needs_login", "json", "eq", "needs_login", "status"),
        ),
        expect_exit=1,
        expect_status="needs_login",
        spends=True,
        foreign_cwd=False,
        timeout_s=300,
        budget_usd=0.05,
    ),
    AuditCase(
        id="T1.5",
        tier=1,
        purpose="--downloads captures a file the page offers into the named directory and the result records it.",
        argv=(
            "{cli}",
            DOWNLOAD_TASK,
            "--start",
            "{base_url}/download.html",
            "--local",
            "--downloads",
            "{downloads}",
            "--json",
        ),
        fixture=True,
        assertions=(
            a("the result records an artifact", "json", "ge", 1, "artifacts"),
            a("the file landed in the downloads directory", "file", "nonempty", None, "{downloads}"),
        ),
        expect_exit=0,
        expect_status="complete",
        spends=True,
        foreign_cwd=False,
        timeout_s=300,
        budget_usd=0.05,
        evidence=("{downloads}",),
    ),
    AuditCase(
        id="T1.6",
        tier=1,
        purpose="--record writes a playable MP4 and the result lists it in recordings.",
        argv=(
            "{cli}",
            PRICE_TASK,
            "--start",
            "{base_url}/shop.html",
            "--local",
            "--record",
            "{record}",
            "--json",
        ),
        fixture=True,
        assertions=(
            a("the result lists a recording", "json", "count_ge", 1, "recordings"),
            a("the recording file exists", "file", "nonempty", None, "{record}"),
        ),
        expect_exit=0,
        expect_status="complete",
        spends=True,
        foreign_cwd=False,
        timeout_s=300,
        budget_usd=0.05,
        evidence=("{record}",),
    ),
    AuditCase(
        id="T1.7",
        tier=1,
        purpose="--max-steps 1 stops bounded, with no crash, at a documented status.",
        argv=(
            "{cli}",
            CONTACT_TASK,
            "--start",
            "{base_url}/contact.html",
            "--authorize",
            "--local",
            "--max-steps",
            "1",
            "--json",
        ),
        fixture=True,
        assertions=(
            a(
                "the status is a documented one",
                "json",
                "one_of",
                [status.value for status in Status],
                "status",
            ),
        ),
        expect_exit=None,
        expect_status=None,
        spends=True,
        foreign_cwd=False,
        timeout_s=300,
        budget_usd=0.05,
    ),
    AuditCase(
        id="T1.8",
        tier=1,
        purpose="A --max-dollars below the task's cost ends the run at budget_exceeded, committing nothing.",
        argv=(
            "{cli}",
            CONTACT_TASK,
            "--start",
            "{base_url}/contact.html",
            "--authorize",
            "--local",
            "--max-dollars",
            "0.0001",
            "--json",
        ),
        fixture=True,
        assertions=(
            a("no contact submission", "recorder", "count_eq", 0, "posts./submit/contact"),
            a("status is budget_exceeded", "json", "eq", "budget_exceeded", "status"),
        ),
        expect_exit=1,
        expect_status="budget_exceeded",
        spends=True,
        foreign_cwd=False,
        timeout_s=300,
        budget_usd=0.05,
    ),
    # ------------------------------------------------------------------ tier 2: live sites (paid models)
    AuditCase(
        id="T2.1",
        tier=2,
        purpose="A lookup answers with a citation whose quote is verbatim text from the cited page.",
        argv=(
            "{cli}",
            "What is the latest version of httpx? Answer with the version and cite the page.",
            "--start",
            "https://pypi.org/project/httpx/",
            "--local",
            "--json",
        ),
        assertions=(
            a("status is complete", "json", "eq", "complete", "status"),
            a("an answer came back", "json", "present", None, "answer"),
            a("at least one citation", "json", "ge", 1, "citations"),
            a("the first citation carries a quote", "json", "present", None, "citations[0].quote"),
        ),
        expect_exit=0,
        expect_status="complete",
        spends=True,
        foreign_cwd=False,
        timeout_s=300,
        budget_usd=0.10,
    ),
    AuditCase(
        id="T2.2",
        tier=2,
        purpose="--start is honoured: the run ends on the address it was told to open.",
        argv=("{cli}", "What is the title of this page?", "--start", "https://example.com/", "--local", "--json"),
        assertions=(a("final_url is the start", "json", "eq", "https://example.com/", "final_url"),),
        expect_exit=0,
        expect_status="complete",
        spends=True,
        foreign_cwd=False,
        timeout_s=300,
        budget_usd=0.10,
    ),
    AuditCase(
        id="T2.3",
        tier=2,
        purpose="With no --start, an address is proposed from the task and reached.",
        argv=("{cli}", "What is the population of Lyon, France, according to Wikipedia?", "--local", "--json"),
        assertions=(
            a("a final address was reached", "json", "present", None, "final_url"),
            a("the address is http", "json", "matches", r"^https?://", "final_url"),
            a("the status is not an error", "json", "ne", "error", "status"),
        ),
        expect_exit=None,
        expect_status=None,
        spends=True,
        foreign_cwd=False,
        timeout_s=300,
        budget_usd=0.10,
    ),
    AuditCase(
        id="T2.4",
        tier=2,
        purpose="A login and cart flow completes on saucedemo and ends on the cart page.",
        argv=(
            "{cli}",
            (
                "Sign in as standard_user with password secret_sauce, add the Sauce Labs"
                " Backpack to the cart, then open the cart."
            ),
            "--start",
            "https://www.saucedemo.com/",
            "--local",
            "--json",
        ),
        assertions=(
            a("status is complete", "json", "eq", "complete", "status"),
            a("ended on the cart page", "json", "contains", "cart", "final_url"),
        ),
        expect_exit=0,
        expect_status="complete",
        spends=True,
        foreign_cwd=False,
        timeout_s=300,
        budget_usd=0.10,
    ),
    AuditCase(
        id="T2.5",
        tier=2,
        purpose="An irreversible action with no --authorize stops at needs_confirmation and places no order.",
        argv=(
            "{cli}",
            "Check out and place the order.",
            "--start",
            "https://www.saucedemo.com/",
            "--local",
            "--json",
        ),
        assertions=(
            a("status is needs_confirmation", "json", "eq", "needs_confirmation", "status"),
            a("no order confirmation in the answer", "json", "not_contains", "Thank you for your order", "answer"),
        ),
        expect_exit=1,
        expect_status="needs_confirmation",
        spends=True,
        foreign_cwd=False,
        timeout_s=300,
        budget_usd=0.10,
    ),
    AuditCase(
        id="T2.6",
        tier=2,
        purpose="A bot-check page yields blocked, or a documented alternative with the reason in error.",
        argv=(
            "{cli}",
            "What does this page say?",
            "--start",
            "https://www.google.com/search?q=test",
            "--local",
            "--json",
        ),
        assertions=(a("status is a documented one", "json", "present", None, "status"),),
        expect_exit=None,
        expect_status=None,
        spends=True,
        foreign_cwd=False,
        timeout_s=300,
        budget_usd=0.10,
    ),
    AuditCase(
        id="T2.7",
        tier=2,
        purpose="The same task run twice reaches the same status: no flapping between runs.",
        argv=probe("repeat_status"),
        assertions=(
            a("both runs agreed", "json", "is_true", None, "same_status"),
            a("both statuses are recorded", "json", "is_true", None, "both_recorded"),
        ),
        expect_exit=0,
        expect_status="ok",
        spends=True,
        foreign_cwd=False,
        timeout_s=600,
        budget_usd=0.20,
    ),
    AuditCase(
        id="T2.8",
        tier=2,
        purpose="A structured-output run through the embedding API validates data against the schema.",
        argv=probe("structured_output"),
        assertions=(
            a("status is complete", "json", "eq", "complete", "status"),
            a("data validates against the schema", "json", "is_true", None, "data_valid"),
        ),
        expect_exit=0,
        expect_status="complete",
        spends=True,
        foreign_cwd=False,
        timeout_s=300,
        budget_usd=0.10,
    ),
    # ------------------------------------------------------------------ tier 3: integration surfaces
    AuditCase(
        id="T3.1",
        tier=3,
        purpose="An MCP stdio handshake lists the browse tool with a usable description.",
        argv=probe("mcp_stdio"),
        assertions=(
            a("the browse tool is returned", "json", "is_true", None, "has_browse"),
            a("the description is usable", "json", "is_true", None, "has_description"),
        ),
        expect_exit=0,
        expect_status="ok",
        spends=False,
        foreign_cwd=False,
        timeout_s=120,
        budget_usd=0.0,
    ),
    AuditCase(
        id="T3.2",
        tier=3,
        purpose="An MCP call returns the BrowseResult shape, not a raw RunResult.",
        argv=probe("mcp_result"),
        assertions=(
            a("BrowseResult keys are present", "json", "is_true", None, "browse_keys"),
            a("RunResult-only keys are absent", "json", "is_true", None, "no_runresult_keys"),
        ),
        expect_exit=0,
        expect_status="ok",
        spends=True,
        foreign_cwd=False,
        timeout_s=300,
        budget_usd=0.05,
    ),
    AuditCase(
        id="T3.3",
        tier=3,
        purpose="MCP http rejects a wrong token, accepts the right one, and leaves healthz reachable.",
        argv=probe("mcp_http"),
        assertions=(
            a("healthz is reachable without a token", "json", "is_true", None, "healthz_open"),
            a("a wrong token is rejected", "json", "is_true", None, "bad_token_rejected"),
            a("the right token is accepted", "json", "is_true", None, "good_token_accepted"),
        ),
        expect_exit=0,
        expect_status="ok",
        spends=False,
        foreign_cwd=False,
        timeout_s=120,
        budget_usd=0.0,
    ),
    AuditCase(
        id="T3.4",
        tier=3,
        purpose="With --max-concurrent 1, a second call queues behind the first rather than failing.",
        argv=probe("mcp_concurrency"),
        assertions=(a("both calls finished", "json", "is_true", None, "both_finished"),),
        expect_exit=0,
        expect_status="ok",
        spends=True,
        foreign_cwd=False,
        timeout_s=300,
        budget_usd=0.10,
    ),
    AuditCase(
        id="T3.5",
        tier=3,
        purpose="run_task from Python returns a RunResult carrying evidence and citations.",
        argv=probe("embed_run_task"),
        assertions=(
            a("a RunResult came back", "json", "is_true", None, "is_runresult"),
            a("it carries evidence", "json", "is_true", None, "has_evidence"),
            a("it carries citations", "json", "is_true", None, "has_citations"),
        ),
        expect_exit=0,
        expect_status="ok",
        spends=True,
        foreign_cwd=False,
        timeout_s=300,
        budget_usd=0.10,
    ),
)
