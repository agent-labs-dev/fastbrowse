# Head-to-head: fastbrowse against Browser Use

Runs fastbrowse and the open-source [Browser Use](https://github.com/browser-use/browser-use) agent on the same six
public tasks, on one machine, one run at a time, and records every run. This is a small rerunnable comparison: it is
separate from the campaigns in [docs/benchmarks.md](../../docs/benchmarks.md), and it publishes nothing to the
benchmark page.

## Rerun it

Needs `uv`, a Rust toolchain, Chromium or Chrome, and `OPENROUTER_API_KEY`. The harness itself is standard library only.

```sh
python3 bench/head_to_head/run.py prepare                        # release build of this checkout; pinned Browser Use
python3 bench/head_to_head/run.py run --campaign "$(date +%F)"   # 6 tasks x 2 arms x 3 repeats
python3 bench/head_to_head/report.py "$(date +%F)"               # results.md and summary.json
python3 bench/head_to_head/infographic.py "$(date +%F)"          # infographic.html, one self-contained file
```

Set `HEAD_TO_HEAD_MACHINE` to name the machine in the record. Rerunning a campaign skips the runs it already has, so
a stopped campaign carries on. `--only TASK ...`, `--repeats N` and `--arms ARM ...` narrow it;
`--arms browser-use-default-effort` adds the sensitivity arm below. Bump `BROWSER_USE_PIN` in `run.py` to measure a
newer Browser Use, and check out another commit and rerun `prepare` to measure another fastbrowse.

## What is held the same

| | Both arms |
|:--|:--|
| Machine and network | The same machine, one run at a time, with the order swapped from task to task and repeat to repeat |
| Browser | A fresh headless Chrome per run, same binary and flags, started by the harness before the clock starts and handed over by CDP URL |
| Text model | `google/gemini-3.8-flash` at `low` reasoning effort, through OpenRouter on one key |
| Prompt | The same words: `Start at <url>. <task>` |
| Credentials | The same two passed as secrets on the checkout task; fastbrowse also gets `--authorize`, which that task needs |
| State | A new working directory and `HOME` per run, and an environment holding only `PATH`, `LANG` and the key, plus each arm's settings |
| Limits | No step or dollar cap. A run still going after 900 s is killed and counted as a timeout |
| Interpreter | Python 3.13, each arm in its own virtual environment; fastbrowse is a release wheel, not the debug build `uv sync` makes |

## What is deliberately different

Each agent otherwise runs as it ships:

- fastbrowse also calls Jev, its choice model, for its decisions. That is the product. Its calls and cost are counted.
- fastbrowse's default text model is not Gemini. It is pinned here because Browser Use needs a model with vision and
  structured output, and one model on both sides removes model choice from the comparison. These numbers do not
  describe fastbrowse's default model.
- Browser Use is at temperature 0, as the Browser Use OSS arm of the private eval runner sets it. fastbrowse sets
  its own.
- Browser Use sets its viewport to 1920x1080 itself. fastbrowse keeps the window it is handed, 1280x900.

`browser-use-default-effort` is Browser Use without the reasoning field, so at the provider's default effort. It shows
whether pinning `low` helped or hurt that arm, and it is reported apart from the head-to-head.

## The tasks

All six come from the public catalog, `src/fastbrowse/evals/public-tasks.json`, with prompts copied verbatim. They were
chosen before any comparison run, by rule: expected to complete on both arms, on a public practice site that does not
wall off headless Chrome, with an answer that does not change from run to run, and a mix of reading and interaction.

| Task | Length | Why it is that length | Passes when |
|:--|:--|:--|:--|
| `books-travel-priciest` | short | One category page | Names the book and 56.88 |
| `quotes-search` | short | One search form | Quotes the Einstein line |
| `dynamic-loading` | short | One button and a wait | Reports the text that loads |
| `stretch-books-nonfiction-five-star` | long | Six listing pages to aggregate | Three titles, each with its price |
| `stretch-quotes-top-authors` | long | Ten listing pages to aggregate | Three authors, each with their count |
| `saucedemo-checkout` | long | Sign-in, two items, cart, a form, three checkout pages | 43.18, and ended on the order-complete page |

Length was assigned from the work a task needs, before any were timed.

## What is measured

- **Passed:** the run ended normally (`complete` for fastbrowse, `done` and reporting success for Browser Use) and its
  answer passes `grade` in `tasks.py`. Citations are stripped first, so a citation number or a URL cannot pass as a
  value. `verify_truth` re-derives what it can from the sites with plain HTTP at the start of a campaign and raises
  when an expected answer has moved; `environment.json` records what it checked and what it could not.
- **Wall clock:** process spawn to process exit, Python start-up included. Chrome's start is outside it.
- **Steps and tool calls:** as each agent counts them, which is **not the same unit**. A Browser Use step is one model
  turn and can hold several actions, including its opening `navigate` and its closing `done`. A fastbrowse step is
  one browser action; its reads and checks are model calls, not steps. Compare model calls for work done.
- **Model calls, tokens and cost:** from the provider's own `usage` on every response. Browser Use's are metered by
  an HTTP hook in `browser_use_arm.py`; fastbrowse's come from its result, which its trace can be reconciled against.
  Token totals are not comparable either: Jev's input is large and billed at a far lower rate. Compare `llm_*` tokens
  for the shared model, and dollars for the whole.
- **Agent CPU and peak memory:** the agent process alone, from `wait4`. Chrome is not included.

## The hosted arms

`hosted.py` adds Browser Use's cloud agent to a campaign, with the same tasks, repeats and graders, in the same
`runs.jsonl`. It needs `BROWSER_USE_API_KEY`:

```sh
python3 bench/head_to_head/hosted.py run --campaign NAME      # browser-use-cloud, bu-ultrafast, bu-fast
python3 bench/head_to_head/hosted.py settle --campaign NAME   # at least 90 s later: each session's settled bill
```

| Arm | `model` sent | What Browser Use says it is |
|:--|:--|:--|
| `browser-use-cloud` | left out | The API's default: `gpt-6-luna` at high reasoning |
| `bu-ultrafast` | `bu-ultrafast` | GPT-6 Luna at low reasoning on OpenAI's priority tier |
| `bu-fast` | `bu-fast` | GPT-6 Luna at medium reasoning on the standard tier |

Most of what "What is held the same" holds does not hold here, so read these arms with that in mind:

- **Machine, network and browser are Browser Use's.** The presets are not in the open-source package. Only the clock
  starts here: create request to the first poll that sees the run finished, at 0.25 s a poll. `server_seconds`, from
  the run's own events, is beside it and excludes this machine's round trip.
- **The model differs.** These arms run GPT-6 Luna and the local arms Gemini 3.8 Flash, so a gap can be the model.
- **Proxy.** `proxyCountryCode` is sent as null on every hosted run, because the local arms have none and the docs and
  the schema disagree on the default.
- **Prompt.** The shared words, and on the checkout the two public credentials too: secret bindings need a vault.
- **Steps** are the `step_start` events in the run's stream, one per model turn. **Tool calls** are its tool events.
  These event types are not documented, so a newer API could change them.
- **Final URL** is read from the browser over CDP before the harness stops it. **`dollars`** is the session's model and
  search charges. The browser and proxy bill is apart, in `browser_dollars` and `proxy_dollars`.
- **`model_seconds`** adds up each model request to its response, from the same stream.

## What a campaign leaves

`results/<campaign>/`: `environment.json` (machine, Chrome, both versions, fastbrowse's commit), `runs.jsonl` (one line
per run), `results.md`, `summary.json`, `infographic.html`, and `raw/<run>/` with each arm's output, gzipped. fastbrowse's
traces also land in `raw/`, and `.gitignore` keeps them out of the repo, since they run to tens of MB a campaign.

## Limits of a run

- Three repeats per task is enough to show large differences and not small ones.
- The machine is shared, and other work may run during a campaign; `load_1m_before` and `load_1m_after` on each run
  show how busy it was. The network, the sites and OpenRouter are live and vary.
- Six tasks on practice sites predict little about hard or bot-walled sites, where headless Chrome is refused
  (`browser_refusal` in `src/fastbrowse/evals/live_tasks.py`).
- Browser cost is not in any local arm's figure, since Chrome runs locally.
