# Benchmark: /Users/jnsdls/fastbrowse/.claude/worktrees/agent-a2d82d94bb841378f/.venv/bin/fastbrowse serve --stdio

## replay, latency zero (3 runs)

Passed 105/108 task runs. Stability: every run identical to the first, 0 replay misses. Timing-sensitive: mock-support-portal, the page shows its modal 1.2 s after load, and the step it lands on depends on agent speed.

| | median | total | max |
|---|---:|---:|---:|
| wall s | 1.85 | 111.81 | 14.47 |
| agent CPU s | 0.38 | 14.27 | 0.58 |
| startup s | 0.204 | 7.37 | 0.222 |
| peak RSS MiB | 74.7 | - | 76.4 |

| task | pass | wall s | CPU s | startup s | RSS MiB | steps | misses |
|---|---:|---:|---:|---:|---:|---:|---:|
| search-price | 3/3 | 2.61 | 0.36 | 0.199 | 74.2 | 5 | 0 |
| contact-form | 3/3 | 2.32 | 0.37 | 0.209 | 74.3 | 5 | 0 |
| contact-needs-confirmation | 3/3 | 2.00 | 0.36 | 0.202 | 74.1 | 5 | 0 |
| table-extract | 3/3 | 0.61 | 0.32 | 0.202 | 74.4 | 1 | 0 |
| login-wall | 3/3 | 0.59 | 0.30 | 0.205 | 73.8 | 0 | 0 |
| confirm-dialog | 3/3 | 0.89 | 0.33 | 0.203 | 73.8 | 2 | 0 |
| mock-cold-sign-in | 3/3 | 14.23 | 0.40 | 0.204 | 74.9 | 5 | 0 |
| mock-supplied-sign-in-route | 3/3 | 14.47 | 0.40 | 0.205 | 74.9 | 6 | 0 |
| mock-sign-in | 3/3 | 2.04 | 0.40 | 0.210 | 74.8 | 5 | 0 |
| mock-sign-in-two-step | 3/3 | 4.80 | 0.53 | 0.202 | 75.5 | 17 | 0 |
| mock-sign-in-code-given | 3/3 | 2.75 | 0.42 | 0.204 | 74.8 | 7 | 0 |
| mock-sign-out | 3/3 | 2.02 | 0.38 | 0.199 | 74.5 | 5 | 0 |
| mock-basket | 3/3 | 3.58 | 0.46 | 0.202 | 74.7 | 11 | 0 |
| mock-order-validation | 3/3 | 4.91 | 0.54 | 0.200 | 75.2 | 19 | 0 |
| mock-order-pause | 3/3 | 3.70 | 0.46 | 0.205 | 74.4 | 10 | 0 |
| mock-password-change | 3/3 | 3.16 | 0.45 | 0.210 | 74.5 | 9 | 0 |
| mock-password-pause | 3/3 | 2.82 | 0.40 | 0.199 | 74.5 | 8 | 0 |
| mock-cheapest-product | 3/3 | 1.20 | 0.38 | 0.204 | 75.4 | 5 | 0 |
| mock-priciest-product | 3/3 | 1.44 | 0.41 | 0.205 | 75.7 | 8 | 0 |
| mock-stock-count | 3/3 | 0.60 | 0.31 | 0.203 | 74.0 | 1 | 0 |
| mock-sign-in-per-digit | 3/3 | 6.59 | 0.58 | 0.203 | 75.8 | 22 | 0 |
| mock-iframe-note | 3/3 | 1.71 | 0.36 | 0.205 | 74.1 | 2 | 0 |
| mock-shadow-dom | 3/3 | 1.53 | 0.35 | 0.203 | 74.2 | 2 | 0 |
| mock-infinite-scroll | 3/3 | 1.12 | 0.40 | 0.206 | 76.4 | 6 | 0 |
| mock-support-portal | 0/3 | 2.77 | 0.49 | 0.205 | 75.1 | 21 | 0 |
| mock-compare-prices | 3/3 | 1.47 | 0.39 | 0.203 | 74.7 | 5 | 0 |
| mock-pagination-exhaustion | 3/3 | 1.20 | 0.39 | 0.209 | 75.2 | 5 | 0 |
| mock-report-total | 3/3 | 0.90 | 0.34 | 0.204 | 74.6 | 2 | 0 |
| mock-drag-card | 3/3 | 1.12 | 0.36 | 0.208 | 74.8 | 2 | 0 |
| mock-new-tab-story | 3/3 | 1.02 | 0.35 | 0.204 | 74.4 | 2 | 0 |
| mock-book-table | 3/3 | 1.34 | 0.37 | 0.205 | 74.9 | 3 | 0 |
| mock-newsletter-gate | 3/3 | 1.33 | 0.36 | 0.208 | 74.4 | 3 | 0 |
| mock-invoice-export | 3/3 | 3.31 | 0.48 | 0.208 | 75.1 | 12 | 0 |
| mock-upload-document | 3/3 | 1.22 | 0.36 | 0.202 | 74.8 | 4 | 0 |
| mock-false-success | 3/3 | 13.79 | 0.37 | 0.202 | 74.4 | 6 | 0 |
| mock-hidden-instruction | 3/3 | 0.66 | 0.34 | 0.222 | 74.6 | 1 | 0 |

## replay, latency recorded (1 runs)

Passed 36/36 task runs. Stability: every run identical to the first, 0 replay misses. Timing-sensitive: mock-support-portal, the page shows its modal 1.2 s after load, and the step it lands on depends on agent speed.

| | median | total | max |
|---|---:|---:|---:|
| wall s | 21.15 | 938.52 | 123.33 |
| agent CPU s | 0.45 | 17.56 | 0.83 |
| startup s | 0.204 | 8.39 | 0.296 |
| peak RSS MiB | 74.7 | - | 76.4 |

| task | pass | wall s | CPU s | startup s | RSS MiB | steps | misses |
|---|---:|---:|---:|---:|---:|---:|---:|
| search-price | 1/1 | 8.58 | 0.43 | 0.199 | 74.3 | 5 | 0 |
| contact-form | 1/1 | 7.50 | 0.41 | 0.198 | 74.1 | 5 | 0 |
| contact-needs-confirmation | 1/1 | 30.92 | 0.41 | 0.203 | 74.3 | 5 | 0 |
| table-extract | 1/1 | 7.75 | 0.37 | 0.270 | 74.6 | 1 | 0 |
| login-wall | 1/1 | 2.64 | 0.32 | 0.199 | 74.1 | 0 | 0 |
| confirm-dialog | 1/1 | 3.04 | 0.38 | 0.226 | 74.1 | 2 | 0 |
| mock-cold-sign-in | 1/1 | 22.03 | 0.48 | 0.196 | 75.1 | 5 | 0 |
| mock-supplied-sign-in-route | 1/1 | 45.08 | 0.54 | 0.200 | 75.1 | 6 | 0 |
| mock-sign-in | 1/1 | 14.02 | 0.51 | 0.205 | 75.2 | 5 | 0 |
| mock-sign-in-two-step | 1/1 | 53.69 | 0.79 | 0.260 | 75.6 | 17 | 0 |
| mock-sign-in-code-given | 1/1 | 29.47 | 0.57 | 0.289 | 74.5 | 7 | 0 |
| mock-sign-out | 1/1 | 14.79 | 0.42 | 0.284 | 74.3 | 5 | 0 |
| mock-basket | 1/1 | 54.12 | 0.61 | 0.198 | 74.7 | 11 | 0 |
| mock-order-validation | 1/1 | 46.72 | 0.77 | 0.276 | 75.4 | 19 | 0 |
| mock-order-pause | 1/1 | 22.37 | 0.53 | 0.199 | 74.6 | 10 | 0 |
| mock-password-change | 1/1 | 14.28 | 0.49 | 0.278 | 74.8 | 9 | 0 |
| mock-password-pause | 1/1 | 19.64 | 0.44 | 0.198 | 74.3 | 8 | 0 |
| mock-cheapest-product | 1/1 | 40.02 | 0.49 | 0.198 | 75.5 | 5 | 0 |
| mock-priciest-product | 1/1 | 25.45 | 0.55 | 0.288 | 75.6 | 8 | 0 |
| mock-stock-count | 1/1 | 20.27 | 0.36 | 0.199 | 74.3 | 1 | 0 |
| mock-sign-in-per-digit | 1/1 | 123.33 | 0.83 | 0.264 | 75.7 | 22 | 0 |
| mock-iframe-note | 1/1 | 5.15 | 0.40 | 0.279 | 74.1 | 2 | 0 |
| mock-shadow-dom | 1/1 | 5.30 | 0.37 | 0.198 | 74.3 | 2 | 0 |
| mock-infinite-scroll | 1/1 | 29.66 | 0.55 | 0.194 | 76.4 | 6 | 0 |
| mock-support-portal | 1/1 | 13.13 | 0.44 | 0.281 | 74.6 | 6 | 0 |
| mock-compare-prices | 1/1 | 24.39 | 0.49 | 0.193 | 74.7 | 5 | 0 |
| mock-pagination-exhaustion | 1/1 | 31.06 | 0.47 | 0.269 | 75.6 | 5 | 0 |
| mock-report-total | 1/1 | 17.16 | 0.45 | 0.290 | 74.9 | 2 | 0 |
| mock-drag-card | 1/1 | 26.95 | 0.45 | 0.198 | 74.6 | 2 | 0 |
| mock-new-tab-story | 1/1 | 8.65 | 0.41 | 0.296 | 74.7 | 2 | 0 |
| mock-book-table | 1/1 | 16.21 | 0.45 | 0.201 | 74.9 | 3 | 0 |
| mock-newsletter-gate | 1/1 | 41.73 | 0.43 | 0.198 | 74.6 | 3 | 0 |
| mock-invoice-export | 1/1 | 50.48 | 0.68 | 0.213 | 75.7 | 12 | 0 |
| mock-upload-document | 1/1 | 14.88 | 0.48 | 0.287 | 75.0 | 5 | 0 |
| mock-false-success | 1/1 | 39.27 | 0.39 | 0.197 | 74.3 | 6 | 0 |
| mock-hidden-instruction | 1/1 | 8.79 | 0.41 | 0.269 | 74.9 | 1 | 0 |

## Footprint

| what | MiB |
|---|---:|
| site-packages, fastbrowse and runtime deps | 13.0 |
| npm package, darwin-arm64, unpacked | 50.4 |
