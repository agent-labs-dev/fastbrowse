# Benchmark: /Users/jnsdls/fastbrowse/.claude/worktrees/agent-af7576a2383dac2f6/.venv/bin/fastbrowse serve --stdio

## replay, latency zero (3 runs)

Passed 105/108 task runs. Stability: every run identical to the first, 0 replay misses. Timing-sensitive: mock-support-portal, the page shows its modal 1.2 s after load, and the step it lands on depends on agent speed.

| | median | total | max |
|---|---:|---:|---:|
| wall s | 1.82 | 110.67 | 14.42 |
| agent CPU s | 0.37 | 13.80 | 0.55 |
| startup s | 0.198 | 7.14 | 0.201 |
| peak RSS MiB | 74.9 | - | 76.6 |

| task | pass | wall s | CPU s | startup s | RSS MiB | steps | misses |
|---|---:|---:|---:|---:|---:|---:|---:|
| search-price | 3/3 | 2.59 | 0.35 | 0.198 | 74.6 | 5 | 0 |
| contact-form | 3/3 | 2.29 | 0.35 | 0.201 | 74.5 | 5 | 0 |
| contact-needs-confirmation | 3/3 | 1.97 | 0.35 | 0.198 | 74.4 | 5 | 0 |
| table-extract | 3/3 | 0.59 | 0.30 | 0.198 | 74.5 | 1 | 0 |
| login-wall | 3/3 | 0.58 | 0.30 | 0.199 | 74.0 | 0 | 0 |
| confirm-dialog | 3/3 | 0.87 | 0.32 | 0.199 | 74.2 | 2 | 0 |
| mock-cold-sign-in | 3/3 | 14.21 | 0.38 | 0.198 | 75.2 | 5 | 0 |
| mock-supplied-sign-in-route | 3/3 | 14.42 | 0.38 | 0.197 | 75.1 | 6 | 0 |
| mock-sign-in | 3/3 | 2.03 | 0.39 | 0.198 | 75.0 | 5 | 0 |
| mock-sign-in-two-step | 3/3 | 4.74 | 0.53 | 0.196 | 76.0 | 17 | 0 |
| mock-sign-in-code-given | 3/3 | 2.69 | 0.41 | 0.198 | 75.1 | 7 | 0 |
| mock-sign-out | 3/3 | 2.03 | 0.38 | 0.197 | 74.6 | 5 | 0 |
| mock-basket | 3/3 | 3.55 | 0.46 | 0.199 | 75.0 | 11 | 0 |
| mock-order-validation | 3/3 | 4.82 | 0.53 | 0.197 | 75.5 | 19 | 0 |
| mock-order-pause | 3/3 | 3.66 | 0.43 | 0.197 | 74.7 | 10 | 0 |
| mock-password-change | 3/3 | 3.09 | 0.42 | 0.198 | 74.7 | 9 | 0 |
| mock-password-pause | 3/3 | 2.81 | 0.39 | 0.197 | 74.8 | 8 | 0 |
| mock-cheapest-product | 3/3 | 1.18 | 0.37 | 0.198 | 75.6 | 5 | 0 |
| mock-priciest-product | 3/3 | 1.42 | 0.40 | 0.198 | 75.7 | 8 | 0 |
| mock-stock-count | 3/3 | 0.59 | 0.30 | 0.197 | 74.4 | 1 | 0 |
| mock-sign-in-per-digit | 3/3 | 6.50 | 0.55 | 0.197 | 76.0 | 22 | 0 |
| mock-iframe-note | 3/3 | 1.68 | 0.34 | 0.200 | 74.5 | 2 | 0 |
| mock-shadow-dom | 3/3 | 1.50 | 0.34 | 0.199 | 74.6 | 2 | 0 |
| mock-infinite-scroll | 3/3 | 1.11 | 0.39 | 0.198 | 76.6 | 6 | 0 |
| mock-support-portal | 0/3 | 2.75 | 0.46 | 0.197 | 75.5 | 21 | 0 |
| mock-compare-prices | 3/3 | 1.46 | 0.38 | 0.200 | 75.0 | 5 | 0 |
| mock-pagination-exhaustion | 3/3 | 1.18 | 0.37 | 0.197 | 75.4 | 5 | 0 |
| mock-report-total | 3/3 | 0.88 | 0.33 | 0.199 | 74.9 | 2 | 0 |
| mock-drag-card | 3/3 | 1.10 | 0.35 | 0.199 | 74.9 | 2 | 0 |
| mock-new-tab-story | 3/3 | 0.94 | 0.34 | 0.200 | 74.9 | 2 | 0 |
| mock-book-table | 3/3 | 1.33 | 0.36 | 0.199 | 74.9 | 3 | 0 |
| mock-newsletter-gate | 3/3 | 1.30 | 0.35 | 0.200 | 74.5 | 3 | 0 |
| mock-invoice-export | 3/3 | 3.28 | 0.47 | 0.200 | 75.4 | 12 | 0 |
| mock-upload-document | 3/3 | 1.20 | 0.35 | 0.197 | 75.0 | 4 | 0 |
| mock-false-success | 3/3 | 13.75 | 0.36 | 0.199 | 74.9 | 6 | 0 |
| mock-hidden-instruction | 3/3 | 0.61 | 0.32 | 0.200 | 75.0 | 1 | 0 |

## replay, latency recorded (1 runs)

Passed 36/36 task runs. Stability: every run identical to the first, 0 replay misses. Timing-sensitive: mock-support-portal, the page shows its modal 1.2 s after load, and the step it lands on depends on agent speed.

| | median | total | max |
|---|---:|---:|---:|
| wall s | 19.92 | 916.11 | 123.25 |
| agent CPU s | 0.43 | 16.69 | 0.83 |
| startup s | 0.191 | 6.90 | 0.198 |
| peak RSS MiB | 75.1 | - | 76.5 |

| task | pass | wall s | CPU s | startup s | RSS MiB | steps | misses |
|---|---:|---:|---:|---:|---:|---:|---:|
| search-price | 1/1 | 8.52 | 0.39 | 0.194 | 74.5 | 5 | 0 |
| contact-form | 1/1 | 7.45 | 0.39 | 0.194 | 74.4 | 5 | 0 |
| contact-needs-confirmation | 1/1 | 30.86 | 0.38 | 0.192 | 74.3 | 5 | 0 |
| table-extract | 1/1 | 7.68 | 0.35 | 0.191 | 74.9 | 1 | 0 |
| login-wall | 1/1 | 2.64 | 0.32 | 0.196 | 74.8 | 0 | 0 |
| confirm-dialog | 1/1 | 3.00 | 0.34 | 0.192 | 74.6 | 2 | 0 |
| mock-cold-sign-in | 1/1 | 22.05 | 0.49 | 0.194 | 75.3 | 5 | 0 |
| mock-supplied-sign-in-route | 1/1 | 45.01 | 0.53 | 0.191 | 75.5 | 6 | 0 |
| mock-sign-in | 1/1 | 13.92 | 0.44 | 0.190 | 75.0 | 5 | 0 |
| mock-sign-in-two-step | 1/1 | 53.66 | 0.80 | 0.193 | 76.0 | 17 | 0 |
| mock-sign-in-code-given | 1/1 | 29.35 | 0.51 | 0.194 | 75.5 | 7 | 0 |
| mock-sign-out | 1/1 | 14.66 | 0.42 | 0.195 | 74.7 | 5 | 0 |
| mock-basket | 1/1 | 36.48 | 0.58 | 0.192 | 75.2 | 11 | 0 |
| mock-order-validation | 1/1 | 46.60 | 0.71 | 0.191 | 75.4 | 19 | 0 |
| mock-order-pause | 1/1 | 22.40 | 0.52 | 0.188 | 74.8 | 10 | 0 |
| mock-password-change | 1/1 | 14.19 | 0.46 | 0.193 | 75.3 | 9 | 0 |
| mock-password-pause | 1/1 | 19.59 | 0.42 | 0.190 | 74.9 | 8 | 0 |
| mock-cheapest-product | 1/1 | 39.97 | 0.47 | 0.192 | 76.0 | 5 | 0 |
| mock-priciest-product | 1/1 | 25.30 | 0.50 | 0.195 | 76.2 | 8 | 0 |
| mock-stock-count | 1/1 | 20.26 | 0.36 | 0.194 | 74.4 | 1 | 0 |
| mock-sign-in-per-digit | 1/1 | 123.25 | 0.83 | 0.195 | 76.1 | 22 | 0 |
| mock-iframe-note | 1/1 | 5.03 | 0.35 | 0.189 | 74.3 | 2 | 0 |
| mock-shadow-dom | 1/1 | 5.27 | 0.35 | 0.186 | 74.5 | 2 | 0 |
| mock-infinite-scroll | 1/1 | 29.66 | 0.56 | 0.198 | 76.5 | 6 | 0 |
| mock-support-portal | 1/1 | 13.85 | 0.43 | 0.188 | 74.7 | 6 | 0 |
| mock-compare-prices | 1/1 | 24.35 | 0.45 | 0.191 | 75.2 | 5 | 0 |
| mock-pagination-exhaustion | 1/1 | 50.42 | 0.45 | 0.189 | 75.3 | 5 | 0 |
| mock-report-total | 1/1 | 17.03 | 0.41 | 0.186 | 75.2 | 2 | 0 |
| mock-drag-card | 1/1 | 14.24 | 0.43 | 0.191 | 75.2 | 2 | 0 |
| mock-new-tab-story | 1/1 | 8.55 | 0.37 | 0.191 | 74.8 | 2 | 0 |
| mock-book-table | 1/1 | 16.16 | 0.40 | 0.191 | 75.4 | 3 | 0 |
| mock-newsletter-gate | 1/1 | 41.70 | 0.41 | 0.190 | 74.4 | 3 | 0 |
| mock-invoice-export | 1/1 | 40.26 | 0.65 | 0.190 | 75.5 | 12 | 0 |
| mock-upload-document | 1/1 | 14.76 | 0.44 | 0.190 | 75.3 | 5 | 0 |
| mock-false-success | 1/1 | 39.27 | 0.40 | 0.190 | 74.6 | 6 | 0 |
| mock-hidden-instruction | 1/1 | 8.70 | 0.37 | 0.194 | 75.0 | 1 | 0 |

## Footprint

| what | MiB |
|---|---:|
| site-packages, fastbrowse and runtime deps | 13.0 |
| npm package, darwin-arm64, unpacked | 50.4 |
