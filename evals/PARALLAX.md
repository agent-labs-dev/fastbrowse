# Parallax pin

The harness pins the published `parallax-evals==2.0.0.dev17` release in
`evals/pyproject.toml` and `evals/uv.lock`. The wheel and source archive are
served by PyPI and their downloaded bytes match the lock's SHA-256 hashes:

- Wheel: `1e8d14a3b1bff11e2f46793e0c1d9e1d262e620c03f5170febcfbf1950f224fd`.
- Source archive: `f6e5a81c4b3a75f5b494a5e021df00d1693abb02471d29a53184e5ba93c8c52a`.

The release tag is `v2.0.0.dev17`, on Parallax commit
`a778f9c258819215c63ccc92eda98c7b9f40feb9`. The private repository is not a
consumer dependency source. `uv lock --check` passes with the published lock.

dev17 verifies `deepseek/deepseek-v4.1-flash` as a fallback judge and asks for
each answer's reason before its verdict. That changes the identity of any
fallback policy graded from now on, so `browser-internal-deepseek-v6` and
`browser-official-deepseek-v3` are new policies. Recorded receipts keep the
`policy_sha256` they were graded under: publication compares those recorded
hashes within a group and never recomputes them, so Opus v5 and official v2
ledgers publish unchanged. A v5 campaign resumed under dev17 is refused as two
policies.

## Compatibility

The published `browser-internal-deepseek-v6` policy and `browser-internal-opus-v5`
before it explicitly disable Jev. Their identity retains the historical inactive Jev fields from dev13. This is a
serialization contract for existing receipts, not a description of the current
Jev transport. Jev-enabled identities name the OpenRouter System One transport
and use its current input limit. The explicit Jev policy is now
`browser-internal-jev-v4`; it does not reuse v3's identity. Historical v3 Jev
receipts need dev13 to deserialize their boolean-answer and warnings schema.
Their raw evidence files stay unchanged; they cannot be silently pooled with v4.

An offline replay compared installed Parallax dev13 with dev16 source based on
Parallax commit `4460ebc5007562ae458d14f8b0ab0fd1bb9b6fb9` and this migration.
The same replay against the published dev16 wheel produced the identical
comparison artifact. Its focused compatibility, live, official, tracking and
publication checks passed: 644 tests.
Mock HTTP returned synthetic answers, with no provider calls or new grades.
All 787 saved semantic receipts retained policy hash
`e697ca113f12e9ac2b1dcae724a792721e67b47d1504a46e8990b4f5bfff7ca1`.
The generated request URLs, JSON bodies and raw HTTP body SHA-256 hashes matched, including
part splitting, instructions, schema, questions and image handling. The
canonical JSON comparison artifact has SHA-256
`6665c99e864ac03eb21317f948c4904877e08fe3617fe62bf0a423b7b1b25a6a`.

| Ledger | Semantic receipts | Unchanged source SHA-256 |
| --- | ---: | --- |
| Core baseline | 60 | `efbbc8dffd4d5376be621bf91263b4684bbda9176e0d4a262bc5b51eab957d72` |
| Dev baseline | 66 | `874505a66cc93006e44385b5d305f0b2573b8c7a915b23beca4bed8a3128d7c4` |
| Comparators | 493 | `da582c48724c790d95b96d895f8bf97acc160f1708d7ee0a7c4e28609030475e` |
| Visual replacements | 9 | `310e0ccd4810d3595a7247264be8f7f5ab08a2abe33ffde5c68acac8e5492798` |
| Candidate a19 | 159 | `ecceeb8678fe60d3958a5eb1ddcf1afba8941d6b0663cd192532ef8892f58f8b` |

The comparator ledger also retains its one physical attempt without a semantic
receipt. No receipt, outcome, request identifier or cost ledger was rewritten.
This check establishes request compatibility, not new model-quality evidence.

`tests/test_judge_compatibility.py` pins a text-and-image request constructed
offline with dev13 and its v5 identity. It runs with the removed Jev constant
absent. `tests/test_evidence.py` checks whole-state and divided-state handling.
