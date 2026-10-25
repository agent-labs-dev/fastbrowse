# Benchmark: /Users/jnsdls/fastbrowse/.claude/worktrees/agent-af7576a2383dac2f6/.venv/bin/fastbrowse serve --stdio

## replay, latency zero (3 runs)

Passed 105/108 task runs. Stability: every run identical to the first, 0 replay misses. Timing-sensitive: mock-support-portal, the page shows its modal 1.2 s after load, and the step it lands on depends on agent speed.

| | median | total | max |
|---|---:|---:|---:|
| wall s | 1.81 | 107.80 | 14.44 |
| agent CPU s | 0.38 | 13.87 | 0.52 |
| startup s | 0.207 | 7.45 | 0.216 |
| peak RSS MiB | 74.9 | - | 76.6 |

| task | pass | wall s | CPU s | startup s | RSS MiB | steps | misses |
|---|---:|---:|---:|---:|---:|---:|---:|
| search-price | 3/3 | 2.58 | 0.35 | 0.213 | 74.6 | 5 | 0 |
| contact-form | 3/3 | 2.28 | 0.36 | 0.207 | 74.4 | 5 | 0 |
| contact-needs-confirmation | 3/3 | 1.94 | 0.33 | 0.206 | 74.5 | 5 | 0 |
| table-extract | 3/3 | 0.61 | 0.32 | 0.207 | 74.7 | 1 | 0 |
| login-wall | 3/3 | 0.59 | 0.32 | 0.212 | 74.2 | 0 | 0 |
| confirm-dialog | 3/3 | 0.88 | 0.31 | 0.207 | 74.0 | 2 | 0 |
| mock-cold-sign-in | 3/3 | 14.21 | 0.40 | 0.215 | 75.4 | 5 | 0 |
| mock-supplied-sign-in-route | 3/3 | 14.44 | 0.40 | 0.207 | 75.1 | 6 | 0 |
| mock-sign-in | 3/3 | 2.03 | 0.40 | 0.207 | 75.1 | 5 | 0 |
| mock-sign-in-two-step | 3/3 | 3.25 | 0.45 | 0.203 | 75.2 | 10 | 0 |
| mock-sign-in-code-given | 3/3 | 2.72 | 0.42 | 0.206 | 75.1 | 7 | 0 |
| mock-sign-out | 3/3 | 2.03 | 0.38 | 0.207 | 74.8 | 5 | 0 |
| mock-basket | 3/3 | 3.55 | 0.46 | 0.206 | 75.1 | 11 | 0 |
| mock-order-validation | 3/3 | 4.75 | 0.52 | 0.208 | 75.4 | 19 | 0 |
| mock-order-pause | 3/3 | 3.65 | 0.43 | 0.207 | 74.9 | 10 | 0 |
| mock-password-change | 3/3 | 3.09 | 0.42 | 0.203 | 75.0 | 9 | 0 |
| mock-password-pause | 3/3 | 2.80 | 0.39 | 0.206 | 74.7 | 8 | 0 |
| mock-cheapest-product | 3/3 | 1.19 | 0.38 | 0.208 | 75.6 | 5 | 0 |
| mock-priciest-product | 3/3 | 1.41 | 0.39 | 0.205 | 75.7 | 8 | 0 |
| mock-stock-count | 3/3 | 0.61 | 0.32 | 0.210 | 74.5 | 1 | 0 |
| mock-sign-in-per-digit | 3/3 | 4.93 | 0.47 | 0.204 | 75.6 | 15 | 0 |
| mock-iframe-note | 3/3 | 1.68 | 0.35 | 0.211 | 74.4 | 2 | 0 |
| mock-shadow-dom | 3/3 | 1.52 | 0.35 | 0.207 | 74.6 | 2 | 0 |
| mock-infinite-scroll | 3/3 | 1.11 | 0.39 | 0.202 | 76.6 | 6 | 0 |
| mock-support-portal | 0/3 | 2.75 | 0.48 | 0.206 | 75.8 | 21 | 0 |
| mock-compare-prices | 3/3 | 1.46 | 0.38 | 0.203 | 75.1 | 5 | 0 |
| mock-pagination-exhaustion | 3/3 | 1.19 | 0.39 | 0.216 | 75.5 | 5 | 0 |
| mock-report-total | 3/3 | 0.91 | 0.35 | 0.211 | 74.8 | 2 | 0 |
| mock-drag-card | 3/3 | 1.12 | 0.36 | 0.207 | 74.9 | 2 | 0 |
| mock-new-tab-story | 3/3 | 0.98 | 0.34 | 0.204 | 74.6 | 2 | 0 |
| mock-book-table | 3/3 | 1.36 | 0.37 | 0.208 | 74.9 | 3 | 0 |
| mock-newsletter-gate | 3/3 | 1.32 | 0.35 | 0.204 | 74.6 | 3 | 0 |
| mock-invoice-export | 3/3 | 3.30 | 0.48 | 0.204 | 75.3 | 12 | 0 |
| mock-upload-document | 3/3 | 1.22 | 0.36 | 0.203 | 75.0 | 4 | 0 |
| mock-false-success | 3/3 | 13.77 | 0.36 | 0.205 | 74.8 | 6 | 0 |
| mock-hidden-instruction | 3/3 | 0.61 | 0.32 | 0.203 | 74.8 | 1 | 0 |

## replay, latency recorded (1 runs)

Passed 36/36 task runs. Stability: every run identical to the first, 0 replay misses. Timing-sensitive: mock-support-portal, the page shows its modal 1.2 s after load, and the step it lands on depends on agent speed.

| | median | total | max |
|---|---:|---:|---:|
| wall s | 20.33 | 826.74 | 50.72 |
| agent CPU s | 0.41 | 15.19 | 0.63 |
| startup s | 0.209 | 7.63 | 0.230 |
| peak RSS MiB | 75.0 | - | 76.8 |

| task | pass | wall s | CPU s | startup s | RSS MiB | steps | misses |
|---|---:|---:|---:|---:|---:|---:|---:|
| search-price | 1/1 | 8.48 | 0.36 | 0.201 | 74.6 | 5 | 0 |
| contact-form | 1/1 | 7.39 | 0.36 | 0.209 | 74.2 | 5 | 0 |
| contact-needs-confirmation | 1/1 | 30.82 | 0.35 | 0.220 | 74.4 | 5 | 0 |
| table-extract | 1/1 | 7.68 | 0.36 | 0.227 | 75.0 | 1 | 0 |
| login-wall | 1/1 | 2.67 | 0.34 | 0.228 | 74.1 | 0 | 0 |
| confirm-dialog | 1/1 | 3.03 | 0.35 | 0.230 | 74.5 | 2 | 0 |
| mock-cold-sign-in | 1/1 | 22.10 | 0.51 | 0.221 | 75.2 | 5 | 0 |
| mock-supplied-sign-in-route | 1/1 | 44.97 | 0.45 | 0.229 | 75.3 | 6 | 0 |
| mock-sign-in | 1/1 | 13.88 | 0.39 | 0.207 | 74.7 | 5 | 0 |
| mock-sign-in-two-step | 1/1 | 35.02 | 0.47 | 0.204 | 75.4 | 10 | 0 |
| mock-sign-in-code-given | 1/1 | 29.21 | 0.46 | 0.204 | 75.0 | 7 | 0 |
| mock-sign-out | 1/1 | 14.67 | 0.40 | 0.208 | 74.9 | 5 | 0 |
| mock-basket | 1/1 | 36.44 | 0.46 | 0.209 | 75.0 | 11 | 0 |
| mock-order-validation | 1/1 | 46.25 | 0.51 | 0.210 | 75.6 | 19 | 0 |
| mock-order-pause | 1/1 | 22.22 | 0.40 | 0.219 | 74.9 | 10 | 0 |
| mock-password-change | 1/1 | 16.20 | 0.42 | 0.221 | 75.2 | 9 | 0 |
| mock-password-pause | 1/1 | 20.36 | 0.39 | 0.220 | 74.4 | 8 | 0 |
| mock-cheapest-product | 1/1 | 40.00 | 0.49 | 0.222 | 76.3 | 5 | 0 |
| mock-priciest-product | 1/1 | 25.27 | 0.45 | 0.220 | 76.0 | 8 | 0 |
| mock-stock-count | 1/1 | 20.30 | 0.38 | 0.211 | 74.7 | 1 | 0 |
| mock-sign-in-per-digit | 1/1 | 50.72 | 0.57 | 0.205 | 75.2 | 15 | 0 |
| mock-iframe-note | 1/1 | 5.05 | 0.36 | 0.209 | 74.8 | 2 | 0 |
| mock-shadow-dom | 1/1 | 5.25 | 0.33 | 0.207 | 74.5 | 2 | 0 |
| mock-infinite-scroll | 1/1 | 29.55 | 0.44 | 0.211 | 76.8 | 6 | 0 |
| mock-support-portal | 1/1 | 13.82 | 0.39 | 0.223 | 74.7 | 6 | 0 |
| mock-compare-prices | 1/1 | 24.33 | 0.46 | 0.206 | 75.0 | 5 | 0 |
| mock-pagination-exhaustion | 1/1 | 50.40 | 0.43 | 0.210 | 75.7 | 5 | 0 |
| mock-report-total | 1/1 | 17.03 | 0.38 | 0.207 | 74.6 | 2 | 0 |
| mock-drag-card | 1/1 | 14.24 | 0.44 | 0.211 | 75.0 | 2 | 0 |
| mock-new-tab-story | 1/1 | 8.54 | 0.36 | 0.208 | 74.6 | 2 | 0 |
| mock-book-table | 1/1 | 16.21 | 0.46 | 0.212 | 75.2 | 3 | 0 |
| mock-newsletter-gate | 1/1 | 41.68 | 0.41 | 0.202 | 74.2 | 3 | 0 |
| mock-invoice-export | 1/1 | 40.23 | 0.63 | 0.202 | 75.7 | 12 | 0 |
| mock-upload-document | 1/1 | 14.74 | 0.40 | 0.201 | 75.0 | 5 | 0 |
| mock-false-success | 1/1 | 39.29 | 0.42 | 0.203 | 75.1 | 6 | 0 |
| mock-hidden-instruction | 1/1 | 8.72 | 0.39 | 0.197 | 75.2 | 1 | 0 |

## Footprint

| what | MiB |
|---|---:|
| site-packages, fastbrowse and runtime deps | 13.0 |
| npm package, darwin-arm64, unpacked | 50.4 |
