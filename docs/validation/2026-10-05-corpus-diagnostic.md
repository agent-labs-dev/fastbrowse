# External corpus execution diagnostics

This is a local integration check, not a benchmark claim. It uses the pinned WindTunnel EasyAppointments
capsule and tasks at revision `5ca8644e23826ebb30108e7bad240b61043bfe67`, with seed 1180 and one task from
each of four strata. Preflight reached all four starts. The [attempt records](2026-10-05-corpus-diagnostic.jsonl)
retain every physical attempt across these checks. The task and corpus digests, provider route, build,
completion and independent grade are separate fields.

| Check | Attempts | Completed | Independently graded | Passed | Ungraded |
|---|---|---|---|---|---|
| Without authorization, trusted state grader | 4 | 2 | 3 | 1 | 1 |
| With authorization, trusted state grader | 4 | 2 | 4 | 2 | 0 |
| Without a state grader | 4 | 2 | 3 | 1 | 1 |

A native answer predicate checks read tasks. The trusted state grader invokes the pinned capsule's
observer and tests its appointment state. The authorized run passed the booking's state predicate and
the provider-name answer task. The admin task lacked a password, and the task requesting the single
word MISSING did not pass its answer check. The unauthorized booking stopped before confirmation.
Without a state grader its pass state remains null. These checks have different authorization and grader
settings, so they are not pooled into one score.

The first physical attempt exposed a module-entrypoint bug: the independent grader imported one Grade
model while `python -m` created another. Pydantic rejected the grade before the attempt row was written.
The raw result survived. The first record reconstructs that interruption from its retained task and
result artifacts; it is explicitly ungraded and contributes no score. The CLI now delegates to its
canonical module, and a subprocess regression checks that a native grade reaches the durable ledger.

These are integration diagnostics from working builds, including dirty trees while the CLI fix was
under test. They are not release results. Admin credentials and an explicit answer-format dev evaluation
are prerequisites for a broader corpus claim. Online-Mind2Web remains unavailable without authorized
gated dataset access and a verified source digest. Neither missing access nor ungraded attempts is a
benchmark pass.
