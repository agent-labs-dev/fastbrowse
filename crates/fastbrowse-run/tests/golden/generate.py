"""Write the blocks Python's fetch reader makes of each fixture page, for `tests/fetch.rs` to match.

    uv run python crates/fastbrowse-run/tests/golden/generate.py [OUT] [HTML_FILES...]

With no arguments it reads every HTML fixture in the repository and a set of edge cases and writes `fetch.json` here.
Given files, it reads those instead and writes OUT, which `FASTBROWSE_FETCH_GOLDEN=OUT cargo test -p fastbrowse-run
--test fetch` compares against: a way to check real pages without committing them.
"""

import json
import subprocess
import sys
from pathlib import Path

from fastbrowse.fetch import _blocks

HERE = Path(__file__).parent
ROOT = HERE.parents[3]
URL = "https://shop.example.com/a/page.html"

EDGES = {
    "unclosed-tag": "<p>kept</p><p>dropped <a href='x",
    "unclosed-comment": "<p>kept</p><!-- never closed <p>gone</p>",
    "bogus-end": "<p>a</ p>b</>c</ 3>d</p>",
    "raw-text": "<script>if (a<b) document.write('</div>')</script ><p>after</p><xmp><b>shown</b></xmp>",
    "title-escapes": "<title>A &amp; <b>B</b></title><p>x</p>",
    "references": "<p>&copy 2026 &amp;&amp &notit; &notin; &#x41;&#66;&#0;&#x110000; &#128; &unknown;</p>",
    "attribute-references": "<a href='/q?a=1&amp;b=2&ampc=3&amp=4&lt;'>q</a>",
    "self-closing": "<p>a<br/>b<div/>c</p><h2/>d",
    "hidden": "<div hidden>no</div><p style='DISPLAY : none'>no</p><p style='visibility:hidden'>no</p><p>yes</p>",
    "hidden-void": "<p>yes</p><input hidden><p>after a hidden void element</p>",
    "nested-table": (
        "<table><tr><th>a</th><td>b<table><tr><td>c</td></tr></table></td></tr><tr><td>x|y</td></tr></table>"
    ),
    "table-no-header": "<h3>T</h3><table><tr><td>1</td><td><img alt=' flag  '></td></tr></table>",
    "headings": "<h1>One</h1><p>a</p><h3>Three</h3><p>b</p><h2>Two</h2><p>c</p><h2></h2><p>d</p>",
    "pre": "<pre>\n\n  line one\n    line two  \n\n</pre><pre>   </pre>",
    "links": "<p>see <a href='/x'>x</a></p><a href='https://other.example/y'> whole </a><a href=''>empty</a>",
    "whitespace": "<p>  a \t b\r\n\n\n\n c \u00a0 d  </p>",
    "uppercase": "<DIV CLASS=gone>x</DIV><STYLE>.GONE{display:none}</STYLE><P CLASS=GONE>hidden</P>",
    "attributes-odd": '<p class="a"title=\'t\'data-x = 1 / >odd</p><p =x>eq</p><p "q">quoted</p>',
    "lone-lt": "<p>1 < 2 and 3 <3 4 <</p>",
    "cdata": "<p>a<![CDATA[ x > y ]]>b</p><!doctype html><?pi x?>",
    "style-in-hidden": "<div hidden><style>.k{display:none}</style></div><p class=k>k</p><p>after</p>",
    "list-in-block": "<div>Before<ul></ul>After</div><plaintext><b>kept</b></plaintext>tail",
    "unicode": "<h1>Caf\u00e9 \u2615</h1><p>na\u00efve \u2014 \u201cquoted\u201d \U0001d518</p>",
}


def case(name: str, html: str) -> dict:
    title, blocks = _blocks(html, URL)
    return {
        "name": name,
        "url": URL,
        "html": html,
        "title": title,
        "blocks": [
            {"kind": b.kind.value, "text": b.text, "heading_path": list(b.heading_path), "href": b.href} for b in blocks
        ],
    }


def _name(path: Path) -> str:
    return str(path.relative_to(ROOT)) if path.is_relative_to(ROOT) else path.name


def main() -> None:
    if len(sys.argv) > 2:
        out = Path(sys.argv[1])
        files = [Path(p) for p in sys.argv[2:]]
    else:
        out = HERE / "fetch.json"
        listed = subprocess.run(["git", "ls-files", "*.html"], cwd=ROOT, capture_output=True, text=True, check=True)
        files = [ROOT / p for p in sorted(listed.stdout.split())]
    cases = [case(_name(f), f.read_text("utf-8", errors="replace")) for f in files]
    if len(sys.argv) <= 2:
        cases += [case(name, html) for name, html in EDGES.items()]
    out.write_text(json.dumps(cases, indent=1, ensure_ascii=True) + "\n", encoding="utf-8")
    print(f"wrote {len(cases)} cases to {out}")


if __name__ == "__main__":
    main()
