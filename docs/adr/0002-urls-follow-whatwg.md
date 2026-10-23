# The Rust core parses URLs as WHATWG and Chrome do, not as CPython does

Origin grants and deep links depend on how a URL is parsed. A Rust port that matched CPython exactly needed
`urlsplit`, IDNA 2003 and Unicode tables generated from one CPython release: 12,000 lines for two small modules.
The core uses the `url` crate (WHATWG, UTS #46) instead. Grants now judge the origin Chrome loads, so
`https://a.example\@evil.example/`, `https://a.exam%70le/` and `https:a.example/` all count as `a.example`,
which is where the browser goes. `faß.de` stays `faß.de` instead of becoming the unrelated domain `fass.de`.

## Considered options

Refusing every URL that is not written in canonical form would have kept the Python test expectations. We chose
to follow the browser, because a grant that disagrees with the page Chrome actually loads is the more likely
source of a bug.
