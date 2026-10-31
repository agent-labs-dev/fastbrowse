"""Work out each held-out task's answer from the sites themselves, without fastbrowse. Run with python3 -I."""

import collections
import html
import json
import re
import time
import urllib.error
import urllib.request

AGENT = {"User-Agent": "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 Chrome/130 Safari/537.36"}


def get(url: str) -> str:
    for attempt in range(5):
        try:
            request = urllib.request.Request(url, headers=AGENT)
            return urllib.request.urlopen(request, timeout=30).read().decode("utf8", "ignore")
        except urllib.error.HTTPError:
            time.sleep(10 * (attempt + 1))
    raise SystemExit(f"could not fetch {url}")


def products() -> dict:
    items = []
    for page in range(1, 6):
        body = get(f"https://web-scraping.dev/products?page={page}")
        cards = body.split('class="row product')[1:]
        for card in cards:
            name = re.search(r'/product/\d+[^"]*"[^>]*>([^<]+)<', card).group(1).strip()
            price = float(re.search(r'class="price[^>]*>\s*\$?([\d.]+)', card).group(1))
            items.append((html.unescape(name), price))
    return {"records": len(items), "items": items}


def tablets() -> dict:
    """The load more page holds every record in the JSON its button reveals from."""
    body = get("https://webscraper.io/test-sites/e-commerce/more/computers/tablets")
    items = json.loads(html.unescape(re.search(r"data-items='([^']+)'", body).group(1)))
    return {"records": len(items), "items": [(i["title"], float(i["price"])) for i in items]}


def shop() -> dict:
    items = []
    for page in range(1, 13):
        body = get(f"https://www.scrapingcourse.com/ecommerce/page/{page}/")
        for card in body.split('<li data-products="item"')[1:]:
            name = re.search(r"<h2[^>]*>([^<]+)</h2>", card).group(1)
            price = re.search(r"</span>([\d.]+)</bdi>", card).group(1)
            items.append((html.unescape(name).strip(), float(price)))
    return {"records": len(items), "items": items}


def django() -> dict:
    titles = []
    for page in range(1, 6):
        body = get(f"https://www.djangoproject.com/weblog/2023/?page={page}")
        titles += [
            html.unescape(t).strip() for t in re.findall(r'<h2>\s*<a href="[^"]+/weblog/2023/[^"]+">([^<]+)</a>', body)
        ]
    return {"records": len(titles), "titles": titles}


def gutenberg(shelf: int) -> dict:
    books, start = [], 1
    while True:
        body = get(f"https://www.gutenberg.org/ebooks/bookshelf/{shelf}?start_index={start}")
        found = re.findall(r'class="title">([^<]+)</span>\s*(?:<span class="subtitle">([^<]*)</span>)?', body)
        rows = [
            (html.unescape(t), html.unescape(a))
            for t, a in found
            if t not in ("Sort Alphabetically", "Sort by Release Date", "Sort by Popularity")
        ]
        rows = [r for r in rows if not r[0].startswith(("Sort ", "Previous", "Next"))]
        books += rows
        if f"start_index={start + 25}" not in body:
            return {"records": len(books), "books": books}
        start += 25


def datatable() -> dict:
    body = get("https://datatables.net/examples/basic_init/zero_configuration")
    rows = re.findall(r"<tr>\s*<td>([^<]+)</td>\s*<td>([^<]+)</td>\s*<td>([^<]+)</td>\s*<td>(\d+)</td>", body)
    return {"records": len(rows), "by_office": collections.Counter(r[2] for r in rows), "rows": rows}


if __name__ == "__main__":
    report = {
        "tablets": tablets(),
        "products": products(),
        "shop": shop(),
        "django": django(),
        "gutenberg": gutenberg(5),
        "datatable": datatable(),
    }
    answers = {
        "shop-numbered": sum(p >= 75 for _, p in report["shop"]["items"]),
        "products-numbered": sum(p > 50 for _, p in report["products"]["items"]),
        "tablets-load-more": sum(p > 200 for _, p in report["tablets"]["items"]),
        "django-next": sum("security release" in t.lower() for t in report["django"]["titles"]),
        "gutenberg-next": sum("Rohlfs" in a for _, a in report["gutenberg"]["books"]),
        "datatable-client-pages": report["datatable"]["by_office"]["London"],
    }
    print(json.dumps({"answers": answers, "evidence": report}, indent=1))
