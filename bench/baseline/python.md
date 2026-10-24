# Benchmark: /Users/jnsdls/fastbrowse/.claude/worktrees/agent-a33931a0015dcb7fa/.venv/bin/fastbrowse serve --stdio

## replay, latency zero (3 runs)

Passed 106/108 task runs. Stability: differing tasks run 2 against run 1: none; run 3 against run 1: mock-support-portal, 2 replay misses. Timing-sensitive: mock-support-portal, the page shows its modal 1.2 s after load, and the step it lands on depends on agent speed.

| | median | total | max |
|---|---:|---:|---:|
| wall s | 1.88 | 104.07 | 14.76 |
| agent CPU s | 0.50 | 18.40 | 0.79 |
| startup s | 0.260 | 9.37 | 0.336 |
| peak RSS MiB | 75.4 | - | 76.5 |

| task | pass | wall s | CPU s | startup s | RSS MiB | steps | misses |
|---|---:|---:|---:|---:|---:|---:|---:|
| search-price | 3/3 | 2.65 | 0.39 | 0.221 | 74.9 | 5 | 0 |
| contact-form | 3/3 | 2.48 | 0.44 | 0.232 | 74.8 | 6 | 0 |
| contact-needs-confirmation | 3/3 | 2.10 | 0.43 | 0.238 | 74.8 | 5 | 0 |
| table-extract | 3/3 | 0.67 | 0.36 | 0.229 | 74.5 | 1 | 0 |
| login-wall | 3/3 | 0.67 | 0.34 | 0.220 | 74.3 | 0 | 0 |
| confirm-dialog | 3/3 | 1.00 | 0.38 | 0.236 | 74.5 | 4 | 0 |
| mock-cold-sign-in | 3/3 | 14.76 | 0.61 | 0.228 | 75.9 | 5 | 0 |
| mock-supplied-sign-in-route | 3/3 | 2.23 | 0.50 | 0.336 | 75.5 | 5 | 0 |
| mock-sign-in | 3/3 | 2.12 | 0.45 | 0.226 | 75.6 | 5 | 0 |
| mock-sign-in-two-step | 3/3 | 5.04 | 0.67 | 0.222 | 76.4 | 17 | 0 |
| mock-sign-in-code-given | 3/3 | 2.84 | 0.49 | 0.222 | 75.8 | 7 | 0 |
| mock-sign-out | 3/3 | 2.11 | 0.44 | 0.218 | 75.0 | 5 | 0 |
| mock-basket | 3/3 | 3.57 | 0.53 | 0.216 | 75.9 | 11 | 0 |
| mock-order-validation | 3/3 | 4.99 | 0.62 | 0.211 | 76.2 | 19 | 0 |
| mock-order-pause | 3/3 | 3.75 | 0.51 | 0.216 | 75.6 | 10 | 0 |
| mock-password-change | 3/3 | 3.21 | 0.47 | 0.227 | 75.0 | 9 | 0 |
| mock-password-pause | 3/3 | 2.94 | 0.47 | 0.223 | 75.0 | 8 | 0 |
| mock-cheapest-product | 3/3 | 1.30 | 0.45 | 0.221 | 75.9 | 5 | 0 |
| mock-priciest-product | 3/3 | 1.51 | 0.47 | 0.246 | 76.3 | 8 | 0 |
| mock-stock-count | 3/3 | 0.78 | 0.43 | 0.273 | 75.0 | 1 | 0 |
| mock-sign-in-per-digit | 3/3 | 6.81 | 0.79 | 0.279 | 76.5 | 22 | 0 |
| mock-iframe-note | 3/3 | 1.55 | 0.46 | 0.275 | 74.6 | 2 | 0 |
| mock-shadow-dom | 3/3 | 1.50 | 0.47 | 0.301 | 75.2 | 2 | 0 |
| mock-infinite-scroll | 3/3 | 1.34 | 0.55 | 0.285 | 75.7 | 6 | 0 |
| mock-support-portal | 1/3 | 3.24 | 0.72 | 0.286 | 76.0 | 22 | 2 |
| mock-compare-prices | 3/3 | 1.66 | 0.54 | 0.295 | 75.3 | 5 | 0 |
| mock-pagination-exhaustion | 3/3 | 1.40 | 0.54 | 0.278 | 76.1 | 5 | 0 |
| mock-report-total | 3/3 | 1.19 | 0.53 | 0.293 | 75.2 | 2 | 0 |
| mock-drag-card | 3/3 | 1.33 | 0.53 | 0.293 | 75.1 | 3 | 0 |
| mock-new-tab-story | 3/3 | 1.10 | 0.48 | 0.285 | 74.9 | 2 | 0 |
| mock-book-table | 3/3 | 1.59 | 0.53 | 0.317 | 75.6 | 3 | 0 |
| mock-newsletter-gate | 3/3 | 1.51 | 0.51 | 0.288 | 75.1 | 3 | 0 |
| mock-invoice-export | 3/3 | 2.65 | 0.63 | 0.310 | 75.8 | 7 | 0 |
| mock-upload-document | 3/3 | 1.47 | 0.55 | 0.313 | 75.4 | 5 | 0 |
| mock-false-success | 3/3 | 14.21 | 0.63 | 0.306 | 75.6 | 6 | 0 |
| mock-hidden-instruction | 3/3 | 0.81 | 0.48 | 0.300 | 75.1 | 1 | 0 |

## replay, latency recorded (1 runs)

Passed 36/36 task runs. Stability: every run identical to the first, 0 replay misses. Timing-sensitive: mock-support-portal, the page shows its modal 1.2 s after load, and the step it lands on depends on agent speed.

| | median | total | max |
|---|---:|---:|---:|
| wall s | 14.64 | 654.69 | 66.17 |
| agent CPU s | 0.89 | 31.28 | 1.44 |
| startup s | 0.527 | 19.95 | 1.498 |
| peak RSS MiB | 75.5 | - | 76.5 |

| task | pass | wall s | CPU s | startup s | RSS MiB | steps | misses |
|---|---:|---:|---:|---:|---:|---:|---:|
| search-price | 1/1 | 8.67 | 0.67 | 0.409 | 75.3 | 5 | 0 |
| contact-form | 1/1 | 5.91 | 0.47 | 0.228 | 75.4 | 5 | 0 |
| contact-needs-confirmation | 1/1 | 6.55 | 0.72 | 0.521 | 75.1 | 5 | 0 |
| table-extract | 1/1 | 9.74 | 0.39 | 0.225 | 74.5 | 1 | 0 |
| login-wall | 1/1 | 3.01 | 0.54 | 0.368 | 74.4 | 0 | 0 |
| confirm-dialog | 1/1 | 2.95 | 0.54 | 0.328 | 74.6 | 2 | 0 |
| mock-cold-sign-in | 1/1 | 24.64 | 0.80 | 0.328 | 75.8 | 5 | 0 |
| mock-supplied-sign-in-route | 1/1 | 14.13 | 0.74 | 0.414 | 76.0 | 5 | 0 |
| mock-sign-in | 1/1 | 16.01 | 0.80 | 0.453 | 75.6 | 5 | 0 |
| mock-sign-in-two-step | 1/1 | 40.60 | 1.03 | 0.327 | 75.8 | 17 | 0 |
| mock-sign-in-code-given | 1/1 | 28.21 | 1.05 | 0.618 | 76.1 | 7 | 0 |
| mock-sign-out | 1/1 | 12.89 | 0.58 | 0.237 | 75.4 | 5 | 0 |
| mock-basket | 1/1 | 35.44 | 0.80 | 0.235 | 75.9 | 11 | 0 |
| mock-order-validation | 1/1 | 34.69 | 1.44 | 0.648 | 76.1 | 19 | 0 |
| mock-order-pause | 1/1 | 13.60 | 1.20 | 0.575 | 75.4 | 10 | 0 |
| mock-password-change | 1/1 | 16.46 | 1.23 | 0.696 | 75.5 | 9 | 0 |
| mock-password-pause | 1/1 | 11.67 | 0.99 | 0.533 | 75.5 | 8 | 0 |
| mock-cheapest-product | 1/1 | 29.39 | 1.02 | 0.540 | 76.1 | 5 | 0 |
| mock-priciest-product | 1/1 | 25.87 | 1.13 | 1.165 | 76.3 | 8 | 0 |
| mock-stock-count | 1/1 | 9.41 | 0.86 | 0.628 | 74.7 | 1 | 0 |
| mock-sign-in-per-digit | 1/1 | 66.17 | 1.41 | 0.501 | 76.5 | 22 | 0 |
| mock-iframe-note | 1/1 | 5.58 | 0.91 | 0.589 | 75.5 | 2 | 0 |
| mock-shadow-dom | 1/1 | 7.23 | 0.83 | 0.540 | 75.6 | 2 | 0 |
| mock-infinite-scroll | 1/1 | 23.08 | 1.08 | 0.649 | 75.8 | 6 | 0 |
| mock-support-portal | 1/1 | 24.94 | 1.17 | 0.872 | 75.1 | 6 | 0 |
| mock-compare-prices | 1/1 | 15.95 | 0.99 | 0.672 | 75.0 | 5 | 0 |
| mock-pagination-exhaustion | 1/1 | 25.44 | 0.71 | 0.400 | 75.7 | 5 | 0 |
| mock-report-total | 1/1 | 15.15 | 0.95 | 0.863 | 74.9 | 2 | 0 |
| mock-drag-card | 1/1 | 11.21 | 0.95 | 0.909 | 75.5 | 2 | 0 |
| mock-new-tab-story | 1/1 | 6.71 | 0.92 | 1.498 | 75.5 | 2 | 0 |
| mock-book-table | 1/1 | 21.07 | 0.97 | 0.873 | 75.6 | 3 | 0 |
| mock-newsletter-gate | 1/1 | 9.61 | 0.51 | 0.328 | 74.9 | 3 | 0 |
| mock-invoice-export | 1/1 | 23.51 | 0.95 | 0.679 | 76.1 | 7 | 0 |
| mock-upload-document | 1/1 | 11.91 | 0.61 | 0.476 | 75.6 | 5 | 0 |
| mock-false-success | 1/1 | 26.57 | 0.73 | 0.224 | 75.4 | 6 | 0 |
| mock-hidden-instruction | 1/1 | 10.72 | 0.62 | 0.402 | 75.3 | 1 | 0 |

## Footprint

| what | MiB |
|---|---:|
| site-packages, fastbrowse and runtime deps | 12.9 |
| PyInstaller directory | 32.3 |
| npm package, darwin-arm64, unpacked | 50.4 |
