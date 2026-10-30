# Credits

The people outside the team whose reports, ideas and code shaped fastbrowse. Each one is also thanked in the
changelog entry for the change they led to. Adapted third-party code is credited in [NOTICE](NOTICE).

| Who | What | Notes |
|---|---|---|
| [@Dhaiwat10](https://github.com/Dhaiwat10) | Found that page captures read hidden table rows and rows in hidden table sections, and fixed it ([#126](https://github.com/agent-labs-dev/fastbrowse/issues/126), [#127](https://github.com/agent-labs-dev/fastbrowse/pull/127)) | Code contributor |
| [@jmclark-lab](https://github.com/jmclark-lab) | Let the verifier count the run's own step history as evidence for an action requirement, which stopped a re-click loop ([#105](https://github.com/agent-labs-dev/fastbrowse/pull/105)) | Code contributor |
| [@imranye](https://github.com/imranye) | Pointing the LLM at a local OpenAI-compatible endpoint such as vLLM ([#354](https://github.com/agent-labs-dev/fastbrowse/pull/354)) | Code contributor; landed in [#359](https://github.com/agent-labs-dev/fastbrowse/pull/359) with the commit kept as authored |
| [@pgebheim](https://github.com/pgebheim) | Proposed `--cdp-url`, attaching a run to a browser already running ([#106](https://github.com/agent-labs-dev/fastbrowse/pull/106)) | The flag landed in a later change |
| [@Adam13y](https://github.com/Adam13y) | Known-answer safety scenarios graded on what the page recorded: a form that looks submitted but is not, and a hidden instruction on a page ([#214](https://github.com/agent-labs-dev/fastbrowse/issues/214)). The first caught a run reporting `complete` after the server failed, fixed in [#240](https://github.com/agent-labs-dev/fastbrowse/pull/240) | |
| [@roshanshah11](https://github.com/roshanshah11) | Found that a correct quote was dropped when the model normalized its punctuation ([#121](https://github.com/agent-labs-dev/fastbrowse/issues/121)) | |
| [@MrJev](https://github.com/MrJev) | Found that the README's dev setup left `pytest` failing at collection ([#82](https://github.com/agent-labs-dev/fastbrowse/issues/82)) | |
| Bruno Bueno ([@bbueno](https://www.producthunt.com/@bbueno)), on Product Hunt | Asked whether a `--secret` value is masked in the run replay, which led to redacting every step and trace event on its way out ([#587](https://github.com/agent-labs-dev/fastbrowse/issues/587)) | |
| Oleksii Sekundant ([@oleksii_sekundant](https://www.producthunt.com/@oleksii_sekundant)) and JC ([@jcbuilds](https://www.producthunt.com/@jcbuilds)), on Product Hunt | Asked to see the element choices beside a fallback, which became `Config(step_choices=True)` ([#588](https://github.com/agent-labs-dev/fastbrowse/issues/588)) | |

Thanks also to everyone else who reported a bug, asked a question or suggested a feature, here or on Product Hunt.
