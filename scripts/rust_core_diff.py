"""Where the Rust core (WHATWG URL, UTS #46) and the Python modules (urlsplit, IDNA 2003) disagree, by cause.

An opt-in report, not a test: the two are meant to differ. Needs the `fastbrowse._native` module `uv sync` builds.

    uv run --inexact python scripts/rust_core_diff.py [--examples N]

Each disagreement lands in the first bucket whose cause it shows. A bucket's verdict says whether the WHATWG
result is safe for an origin allowlist: it never grants a document at an origin other than the one Chrome gives
it, and a grant entry still names one origin a reader can see.
"""

import argparse
import importlib.util
import os
import random
import re
import sys
from collections import Counter, defaultdict
from collections.abc import Callable
from pathlib import Path
from types import ModuleType

from fastbrowse import _native as native  # ty: ignore[unresolved-import]

SRC = Path(__file__).resolve().parent.parent / "src" / "fastbrowse"


def python_module(name: str) -> ModuleType:
    """The module as Python wrote it, whatever FASTBROWSE_RUST_CORE says."""
    previous = os.environ.pop("FASTBROWSE_RUST_CORE", None)
    try:
        spec = importlib.util.spec_from_file_location(f"_python_{name}", SRC / f"{name}.py")
        assert spec is not None and spec.loader is not None
        module = importlib.util.module_from_spec(spec)
        sys.modules[spec.name] = module
        spec.loader.exec_module(module)
        return module
    finally:
        if previous is not None:
            os.environ["FASTBROWSE_RUST_CORE"] = previous


py_origins = python_module("origins")
py_citations = python_module("citations")


def outcome(fn: Callable[..., object], *args: str) -> object:
    """The value, or the exception type: a refusal's wording is not behavior."""
    try:
        return fn(*args)
    except Exception as exc:
        return type(exc).__name__


SCHEMES = ["http", "https", "HTTPS", "hTtP", "ftp", "", "h+t", "blob:https", "view-source:http", "1http", "ht tp"]
SEPARATORS = ["://", ":/", ":", ":///", "://\\", "//", ":\\\\"]
USERINFO = ["", "u@", "u:p@", "a@b@", "@", "good\\@", "\u00fc@"]
HOSTS = [
    "a.example",
    "A.EXAMPLE",
    "a.example.",
    "a..example",
    ".a.example",
    "a" * 63 + ".example",
    "a" * 64 + ".example",
    "b\u00fccher.example",
    "B\u00dcCHER.example",
    "\u03a3\u0391\u03a3.example",
    "fa\u00df.de",
    "\u0130stanbul.example",
    "\u4f8b\u3048.\u30c6\u30b9\u30c8",
    "b\u00fccher\u3002example",
    "b\u00fccher\uff0eexample\uff61",
    "xn--bcher-kva.example",
    "xn--b\u00fccher.example",
    "ex%41MPLE.com",
    "\uff21\uff22.example",
    "a\u2100b.example",
    "\ufb01x.example",
    "a\u200bb.example",
    "a\u00adb.example",
    "\u0645\u062b\u0627\u0644.example",
    "\u05d0a\u05d1.example",
    "\u05d0\u05d1.example",
    "a\ufffd.example",
    "127.0.0.1",
    "[::1]",
    "[::FFFF:1.2.3.4]",
    "[fe80::1%25eth0]",
    "[fe80::1%]",
    "[1.2.3.4]",
    "[v1.x]",
    "[v.x]",
    "[V1.x]",
    "[::1",
    "::1]",
    "a[::1]",
    "[::1]x",
    "[1:2:3:4:5:6:7:8]",
    "[1:2:3:4:5:6:7:8:9]",
    "[1::2::3]",
    "[::01.2.3.4]",
    "[:1::]",
    "[12345::]",
    "",
    "*.example",
    "exam ple",
    "a\tb",
    "\u0661\u0662.example",
    "\U0001f600.example",
    "\u1e9e.example",
    "\u2c00.example",
    "\ua7cb.example",
]
PORTS = [
    "",
    ":",
    ":0",
    ":80",
    ":443",
    ":0443",
    ":8443",
    ":65535",
    ":65536",
    ":x",
    ":\u0661\u0662",
    "::",
    ":-1",
    ":1" * 3,
]
TAILS = ["", "/", "/a", "?q=1", "#f", "/a?b#c", "/\\x", "/a b", "#:~:text=x", "/%2F"]


def generated_urls(rng: random.Random, count: int) -> list[tuple[str]]:
    urls: list[tuple[str]] = [
        (s + sep + u + h + p + t,)
        for s in SCHEMES[:2]
        for sep in SEPARATORS
        for u in USERINFO
        for h in HOSTS[:8]
        for p in PORTS[:3]
        for t in TAILS[:2]
    ]
    alphabet = "aZ09.:/@[]%#?\\*- \t\u00fc\u03a3\u3002\u2100\u200b\ud800"
    for _ in range(count):
        if rng.random() < 0.8:
            url = "".join(rng.choice(pool) for pool in (SCHEMES, SEPARATORS, USERINFO, HOSTS, PORTS, TAILS))
        else:
            url = "https://" + "".join(rng.choice(alphabet) for _ in range(rng.randint(0, 12)))
        if rng.random() < 0.1:
            i = rng.randint(0, len(url))
            url = url[:i] + rng.choice(" \t\n\x00\x7f\\*") + url[i:]
        if rng.random() < 0.1:
            url = rng.choice(" \x01\t") + url
        urls.append((url,))
    return urls


MARKS = [
    (0x300, 0x36F),
    (0x8D0, 0x8FF),
    (0x1AB0, 0x1AFF),
    (0x1DC0, 0x1DFF),
    (0x20D0, 0x20FF),
    (0x1100, 0x11FF),
    (0xAC00, 0xD7A3),
    (0x11099, 0x110BA),
]


def random_host_char(rng: random.Random) -> str:
    while True:
        c = rng.choice([rng.randint(0x80, 0x3000), rng.randint(0x80, 0x10FFFF), rng.randint(*rng.choice(MARKS))])
        if not 0xD800 <= c <= 0xDFFF:
            return chr(c)


WORDS = [
    "a",
    "a-b",
    "word,",
    "100%",
    "#1",
    "/",
    "(yes?)",
    "caf\u00e9",
    "\u6771\u4eac",
    "~_.",
    "\U0001f600",
    "x" * 40,
    "\ud800",
    "\udfff",
    "&",
    "=",
    "+",
    "'\"",
    "\u00e9\u0301",
]
SPACES = [" ", "  ", "\n", "\t", "\u00a0", "\u3000", "\x1c", "\x1f", "\x85", "\u2028", "\u200b", "\x0b"]
LINKS = [
    "https://example.test",
    "https://example.test/a?x=1&y=2",
    "https://example.test#",
    "https://example.test/#section",
    "https://example.test/#section:~:text=old&text=other",
    "https://e.test/#a#b",
    "https://e.test/#:~:",
    "",
    "https://e.test/\ud800#\udc00",
]


DEVIATIONS = {chr(0xDF), chr(0x3C2), chr(0x200C), chr(0x200D)}
SURROGATE = re.compile("[" + chr(0xD800) + "-" + chr(0xDFFF) + "]")
IPV4_NUMBER = re.compile(r"//(?:[^/@]*@)?(0x[0-9a-f]*|[0-9]+)(\.(0x[0-9a-f]*|[0-9]+)){0,3}\.?(?:[:/?#]|$)", re.I)

# Bucket, the test that puts a case in it, and whether the WHATWG result is safe for an origin allowlist.
URL_BUCKETS: list[tuple[str, Callable[[str], bool], str]] = [
    ("lone surrogate", lambda u: bool(SURROGATE.search(u)), "safe: U+FFFD in a host is refused, elsewhere ignored"),
    ("backslash read as /", lambda u: "\\" in u, "safe: Chrome reads it the same; entries refuse it"),
    (
        "C0, space or DEL stripped",
        lambda u: any(c <= " " or c == chr(0x7F) for c in u),
        "safe: Chrome strips the same; entries refuse it",
    ),
    ("IPv6 scope id", lambda u: re.search(r"\[[^\]]*%", u) is not None, "safe: refused, never granted"),
    ("IPvFuture [v1.x]", lambda u: re.search(r"\[[vV]", u) is not None, "safe: refused, never granted"),
    ("IPv6 bracket rules and serialization", lambda u: "[" in u or "]" in u, "safe: canonical form on both sides"),
    (
        "percent-encoded host",
        lambda u: "%" in u.split("//", 1)[-1].split("/", 1)[0],
        "safe: the decoded host is the one Chrome loads",
    ),
    (
        "UTS #46 deviation (sharp s stays sharp s)",
        lambda u: bool(DEVIATIONS & set(u)),
        "safe: IDNA 2003 named another domain",
    ),
    ("xn-- label checks", lambda u: "xn--" in u.lower(), "safe: invalid punycode is refused"),
    ("IDNA 2003 vs UTS #46, other", lambda u: not u.isascii(), "safe: UTS #46 is Chrome's host mapping"),
    ("IPv4 number forms", lambda u: IPV4_NUMBER.search(u) is not None, "safe: serialized as Chrome does"),
    ("empty label or trailing dot", lambda u: ".." in u or "//." in u or "@." in u, "safe: the host Chrome resolves"),
    (
        "not exactly two slashes after the scheme",
        lambda u: re.match(r"^[a-zA-Z][\w+.-]*:(?!//[^/])", u) is not None,
        "safe: Chrome reads it so; entries refuse it",
    ),
    (
        "DNS length not checked",
        lambda u: re.search(r"[^./@:\[\]]{64}", u) is not None,
        "safe: WHATWG and Chrome resolve it; DNS refuses it",
    ),
    ("port", lambda u: re.search(r":[^/@\[\]]*(/|$)", u.split("//", 1)[-1]) is not None, "safe: refused or normalized"),
    ("other", lambda u: True, "review"),
]

TEXT_BUCKETS: list[tuple[str, Callable[[str, str], bool], str]] = [
    ("lone surrogate", lambda url, text: bool(SURROGATE.search(url + text)), "not security: U+FFFD instead"),
    (
        "U+001C..U+001F are not White_Space",
        lambda url, text: any(chr(c) in text for c in range(0x1C, 0x20)),
        "not security: kept in the word, encoded",
    ),
    ("other", lambda url, text: True, "review"),
]


def direction(python: object, rust: object) -> str:
    if isinstance(python, str) and python.endswith("Error"):
        return "rust accepts" if not (isinstance(rust, str) and rust.endswith("Error")) else "both refuse"
    if python is None:
        return "rust accepts"
    if rust is None or (isinstance(rust, str) and rust.endswith("Error")):
        return "rust refuses"
    return "both accept, differ"


def report(title: str, rows: list[tuple[str, str, tuple[str, ...], object, object]], verdicts: dict[str, str], n: int):
    print(f"\n## {title}: {len(rows)} disagreements")
    grouped: dict[str, list] = defaultdict(list)
    for bucket, how, args, a, b in rows:
        grouped[bucket].append((how, args, a, b))
    for bucket, items in sorted(grouped.items(), key=lambda kv: -len(kv[1])):
        ways = Counter(how for how, *_ in items)
        print(f"- {bucket}: {len(items)} ({', '.join(f'{k} {v}' for k, v in ways.most_common())}). {verdicts[bucket]}")
        for _, args, a, b in items[:n]:
            print(f"    {args if len(args) > 1 else args[0]!a}: python={a!a} rust={b!a}")


def url_corpus() -> list[str]:
    urls = [u for (u,) in generated_urls(random.Random(1), 20_000)]
    rng = random.Random(2)
    for _ in range(50_000):
        label = "".join(
            random_host_char(rng) if rng.random() < 0.6 else rng.choice("aZ-0") for _ in range(rng.randint(1, 6))
        )
        urls.append(f"https://{label}.example/")
    return urls


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--examples", type=int, default=3)
    args = parser.parse_args()
    urls = url_corpus()
    print(f"# Python (urlsplit, IDNA 2003) vs Rust (WHATWG, UTS #46) on {len(urls)} URLs")
    url_verdicts = {name: verdict for name, _, verdict in URL_BUCKETS}
    for name, python, rust in [
        ("origin_of", py_origins.origin_of, native.origin_of),
        ("parse_origin", py_origins.parse_origin, native.parse_origin),
    ]:
        rows = []
        for url in urls:
            a, b = outcome(python, url), outcome(rust, url)
            if a != b:
                bucket = next(n for n, test, _ in URL_BUCKETS if test(url))
                rows.append((bucket, direction(a, b), (url,), a, b))
        report(name, rows, url_verdicts, args.examples)
    rng = random.Random(3)
    rows = []
    for _ in range(20_000):
        n = rng.choice([0, 1, 2, 5, 10, 11, 12, 30])
        text = "".join(rng.choice(WORDS) + rng.choice(SPACES) for _ in range(n))
        link = rng.choice(LINKS)
        a, b = outcome(py_citations.text_fragment, link, text), outcome(native.text_fragment, link, text)
        if a != b:
            bucket = next(n for n, test, _ in TEXT_BUCKETS if test(link, text))
            rows.append((bucket, direction(a, b), (link, text), a, b))
    report("text_fragment on 20000 quotes", rows, {n: v for n, _, v in TEXT_BUCKETS}, args.examples)


if __name__ == "__main__":
    main()
