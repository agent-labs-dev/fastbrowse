# Eval data sources

External tasks are fetched into the user cache, never committed here. Tests use synthetic tasks authored for
fastbrowse. A digest mismatch stops loading, including when the bad bytes came from the cache.

## online-mind2web

- Name: Online-Mind2Web, OSU NLP Group.
- Upstream: <https://huggingface.co/datasets/osunlp/Online-Mind2Web>.
- Pinned revision: `eacad896a84dc5b65e29b0b06e4699ab0544d701`.
- File: `Online_Mind2Web.json`.
- SHA-256: pending gated access. The loader refuses to download without a verified digest supplied through
  `--sha256`. No digest has been invented or learned from an unverified first download.
- The public upstream tree pins this file's Git blob to `e8a5e5a99f2be9eae14f4e4259bd5af562f80da9`;
  the loader checks that identity as well as the supplied SHA-256.
- Licence: [CC BY 4.0](https://creativecommons.org/licenses/by/4.0/), declared in the
  [dataset card](https://huggingface.co/datasets/osunlp/Online-Mind2Web/blob/eacad896a84dc5b65e29b0b06e4699ab0544d701/README.md).
- Attribution: Xue et al., *An Illusion of Progress? Assessing the Current State of Web Agents* (2025),
  and Deng et al., *Mind2Web: Towards a Generalist Agent for the Web* (2023). The card requests both citations.
- Fetched, not vendored. Hugging Face requires acceptance of the dataset's access conditions and an authorized
  `HF_TOKEN`. The card describes release for research purposes and opposes harmful uses. CC BY 4.0 itself does
  not impose a non-commercial restriction; retain attribution and check the access terms before downloading.

## windtunnel

- Name: WindTunnel, nekuda.
- Upstream: <https://github.com/nekuda-ai/WindTunnel>.
- Pinned revision: `5ca8644e23826ebb30108e7bad240b61043bfe67`.
- Artifact: [repository archive](https://codeload.github.com/nekuda-ai/WindTunnel/tar.gz/5ca8644e23826ebb30108e7bad240b61043bfe67).
- SHA-256: `9254737b8062a4a7140ec2a91f3b111abe648bd6d649b2775d2e68ad1f054d82`. GitHub does not promise
  generated archives stay byte-identical; if the digest stops matching, verify the commit's tree and re-pin
  rather than trusting the new bytes.
- Licence: [Apache-2.0](https://github.com/nekuda-ai/WindTunnel/blob/5ca8644e23826ebb30108e7bad240b61043bfe67/LICENSE)
  for WindTunnel's own task definitions and harness code.
- Attribution: WindTunnel by nekuda; site selection and reference tools by Ilay (nekuda). See the pinned
  [ATTRIBUTION.md](https://github.com/nekuda-ai/WindTunnel/blob/5ca8644e23826ebb30108e7bad240b61043bfe67/ATTRIBUTION.md).
- Fetched, not vendored. The loader reads task YAML from the verified archive without extracting or executing
  upstream code. Calibration tasks are excluded. Running a site or its grader is a separate setup step.
- The sites retain their own MIT, AGPL-3.0 or GPL-3.0 licences. Hi.Events also requires its attribution footer.
  Upstream lines in patches retain their original licences. Fetching the archive does not relicense them;
  preserve the notices and consult upstream attribution before running or redistributing a site.

## Authored fixtures

The static HTML in `src/fastbrowse/evals/fixtures/` and the synthetic test data in `tests/evals/` are authored
for fastbrowse and remain in this repository under its [MIT licence](../LICENSE). They contain no copied
Online-Mind2Web or WindTunnel task rows.
