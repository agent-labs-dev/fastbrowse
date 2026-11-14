# Third-party benchmark data

`fastbrowse_evals` vendors no third-party dataset. The internal live tasks (`live_tasks.py`, `more_tasks.py`) are
authored for fastbrowse and are covered by the repository's [LICENSE](../LICENSE). Parallax dev20 fetches the official
suite data at run time, verifies its pinned digest, and keeps the bytes in its cache. See Parallax's
`docs/official-suites.md` for the source revisions, digests, access requirements, and upstream judge environments.

## Online-Mind2Web

- Name: Online-Mind2Web, OSU NLP Group.
- Upstream dataset: <https://huggingface.co/datasets/osunlp/Online-Mind2Web>.
- Pinned revision: `eacad896a84dc5b65e29b0b06e4699ab0544d701`, file `Online_Mind2Web.json`.
- SHA-256: `7dedf381531d423dc0fc48d21dc1d425655dd75f38b10211a880fa09102f257f` (88,382 bytes, 300 tasks). The file is
  not stored in LFS, so Hugging Face publishes no SHA-256 for it. The digest was taken from bytes that first matched
  the Git blob the public upstream tree lists (`e8a5e5a99f2be9eae14f4e4259bd5af562f80da9`), and the loader checks
  both.
- Licence: [CC BY 4.0](https://creativecommons.org/licenses/by/4.0/), declared in the
  [dataset card](https://huggingface.co/datasets/osunlp/Online-Mind2Web/blob/eacad896a84dc5b65e29b0b06e4699ab0544d701/README.md).
  The WebJudge grader code is from [OSU-NLP-Group/Online-Mind2Web](https://github.com/OSU-NLP-Group/Online-Mind2Web)
  (MIT).
- Attribution: Xue et al., *An Illusion of Progress? Assessing the Current State of Web Agents* (2025), and Deng et
  al., *Mind2Web: Towards a Generalist Agent for the Web* (2023). The card requests both citations.
- Access: Hugging Face requires acceptance of the dataset's access conditions and an authorized `HF_TOKEN`. The card
  describes release for research purposes and opposes harmful uses. Check the access terms before downloading.

## WindTunnel

- Name: WindTunnel, nekuda.
- Upstream: <https://github.com/nekuda-ai/WindTunnel>.
- Pinned revision: `5ca8644e23826ebb30108e7bad240b61043bfe67`, fetched as the
  [repository archive](https://codeload.github.com/nekuda-ai/WindTunnel/tar.gz/5ca8644e23826ebb30108e7bad240b61043bfe67).
- SHA-256: `9254737b8062a4a7140ec2a91f3b111abe648bd6d649b2775d2e68ad1f054d82`. GitHub does not promise that generated
  archives stay byte-identical; if the digest stops matching, verify the commit's tree and re-pin rather than trust
  the new bytes.
- Licence: [Apache-2.0](https://github.com/nekuda-ai/WindTunnel/blob/5ca8644e23826ebb30108e7bad240b61043bfe67/LICENSE)
  for WindTunnel's task definitions and harness code. The sites it runs keep their own MIT, AGPL-3.0 or GPL-3.0
  licences, and Hi.Events also requires its attribution footer. Preserve those notices before running or
  redistributing a site.
- Attribution: WindTunnel by nekuda; see the pinned
  [ATTRIBUTION.md](https://github.com/nekuda-ai/WindTunnel/blob/5ca8644e23826ebb30108e7bad240b61043bfe67/ATTRIBUTION.md).

## BU Bench

- Upstream: <https://github.com/browser-use/benchmark>, whose repository declares no licence.
- The tasks ship encrypted (`BU_Bench_V2.enc`) and are decrypted only inside a run. A run directory holds the
  decrypted goals and upstream records, so it is age-encrypted and never leaves its job in plaintext. Only task ids,
  scores, seconds, dollars and protocol facts are published through the Parallax
  run-directory projection in `fastbrowse_evals.publication`.
