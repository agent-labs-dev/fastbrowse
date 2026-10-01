"""A small stateful site for evals that need a login, a session and a multi-step flow.

`local.py` serves static pages and records submissions, which covers a form on a public page. It cannot cover the
shape most real work has: a sign-in that sets a session, pages that redirect to that sign-in when it is absent, a
second step after the password, and an action that only becomes reachable once signed in.

This module serves instead, from one process, with no dependencies beyond the standard library:

- form sign-in with a rejection that has to be read and corrected
- a second step asking for a code shown on another page, so the run has to go and find it
- a session cookie, and protected pages that redirect to the sign-in without one
- sign-out, and signing in again as somebody else
- a cart, and a checkout whose postcode is validated server-side
- a password change, an irreversible action reachable only when signed in
- a paginated, sortable product list
- a page whose contents are written by script rather than served as markup
- a downloadable file

Every request is recorded, so a grader reads what the site actually received rather than what the run says it did.

The authenticator code is fixed per account rather than moving every thirty seconds: the same time-based code is
what a real site would use, but a code that rolls over mid-run turns a correct run into a flaky failure, and the
skill under test is finding a value on one page and typing it into another, not clock arithmetic.
"""

import base64
import hmac
import json
import re
import struct
import threading
import time
from collections.abc import Callable, Generator, Mapping
from contextlib import contextmanager
from http import HTTPStatus
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qsl, quote, urlsplit

from fastbrowse.adapters.local_chrome import free_port

PASSWORD_KEY = b"fastbrowse-mock-site"

ACCOUNTS: Mapping[str, str] = {
    "ada@example.com": "correct-horse",
    "grace@example.com": "battery-staple",
    "katherine@example.com": "hidden-figures",
}

# The account that asks for a code after the password, in one box.
NEEDS_CODE = "ada@example.com"

# The account whose code is typed one digit per box, which is what a real authenticator screen asks for.
NEEDS_PER_DIGIT_CODE = "katherine@example.com"

PRODUCTS: tuple[tuple[str, str, float], ...] = (
    ("kettle", "Blue Kettle", 34.50),
    ("toaster", "Chrome Toaster", 41.00),
    ("mug", "Stoneware Mug", 8.75),
    ("grinder", "Burr Grinder", 89.99),
    ("scale", "Kitchen Scale", 19.20),
    ("thermometer", "Milk Thermometer", 12.40),
    ("press", "French Press", 27.00),
    ("filter", "Paper Filters", 4.60),
    ("beans", "House Beans", 11.50),
    ("roaster", "Drum Roaster", 210.00),
    ("jug", "Serving Jug", 15.30),
    ("tray", "Drip Tray", 9.95),
    ("cloth", "Bar Cloth", 6.20),
    ("tap", "Water Tap", 52.75),
)

ORDERS: Mapping[str, tuple[tuple[str, str, float], ...]] = {
    "ada@example.com": (
        ("A-1001", "Blue Kettle", 34.50),
        ("A-1002", "Burr Grinder", 89.99),
        ("A-1003", "Serving Jug", 15.30),
    ),
    "grace@example.com": (("G-2001", "Paper Filters", 4.60),),
}

PER_PAGE = 6

# The feed appends a batch at a time until every item is shown, the way a load-more list does.
FEED_TOTAL = 60
FEED_BATCH = 20

# Two shops, one product: the comparison is which page holds the cheaper price.
COMPARISON: Mapping[str, tuple[str, float]] = {
    "a": ("Clerkenwell Coffee", 84.99),
    "b": ("Bermondsey Beans", 92.50),
}


def _feed_item(n: int) -> str:
    return f"Feed item {n}"


def code_for(email: str) -> str:
    """A six-digit code derived from the account, so it is stable across runs and machines."""
    digest = hmac.new(PASSWORD_KEY, email.encode(), "sha256").digest()
    return f"{int.from_bytes(digest[:4], 'big') % 1_000_000:06d}"


class Site:
    """Everything the site remembers: sessions, what was sent, and what it did as a result."""

    def __init__(self) -> None:
        self._lock = threading.Lock()
        self.sessions: dict[str, str] = {}
        self.passwords: dict[str, str] = dict(ACCOUNTS)
        self.posts: list[tuple[str, dict[str, str]]] = []
        self.paths: list[str] = []
        self.sign_ins: list[str] = []
        self.failed_sign_ins: list[str] = []
        self.code_attempts: list[str] = []
        self.carts: dict[str, list[str]] = {}
        self.orders_placed: list[dict[str, str]] = []
        self.password_changes: list[tuple[str, str]] = []
        self.drops: list[dict[str, str]] = []
        """Cards dropped on a column, each recorded the way a page's own drag posts its result."""

    def record(self, path: str, fields: dict[str, str]) -> None:
        with self._lock:
            self.posts.append((path, fields))

    def visited(self, path: str) -> None:
        with self._lock:
            self.paths.append(path)

    def sign_in(self, email: str) -> str:
        """Open a session for `email` and return its id."""
        sid = base64.urlsafe_b64encode(struct.pack(">Q", time.monotonic_ns())).decode().rstrip("=")
        with self._lock:
            self.sessions[sid] = email
            self.sign_ins.append(email)
        return sid

    def who(self, sid: str | None) -> str | None:
        if not sid:
            return None
        with self._lock:
            return self.sessions.get(sid)

    def sign_out(self, sid: str | None) -> None:
        with self._lock:
            self.sessions.pop(sid or "", None)

    def cart(self, email: str) -> list[str]:
        with self._lock:
            return list(self.carts.get(email, []))

    def add_to_cart(self, email: str, item: str) -> None:
        with self._lock:
            self.carts.setdefault(email, []).append(item)

    def remove_from_cart(self, email: str, item: str) -> None:
        with self._lock:
            held = self.carts.setdefault(email, [])
            if item in held:
                held.remove(item)

    def subtotal(self, email: str) -> float:
        by_id = {pid: price for pid, _, price in PRODUCTS}
        return round(sum(by_id[item] for item in self.cart(email) if item in by_id), 2)

    def place_order(self, fields: dict[str, str]) -> None:
        with self._lock:
            self.orders_placed.append(fields)

    def change_password(self, email: str, new: str) -> None:
        with self._lock:
            self.passwords[email] = new
            self.password_changes.append((email, new))

    def note_code_attempt(self, code: str) -> None:
        with self._lock:
            self.code_attempts.append(code)

    def record_drop(self, fields: dict[str, str]) -> None:
        with self._lock:
            self.drops.append(fields)


def _page(title: str, body: str) -> bytes:
    links = (("/", "Home"), ("/shop", "Shop"), ("/account", "Account"), ("/phone", "Phone"))
    nav = "<nav>" + " | ".join(f'<a href="{href}">{name}</a>' for href, name in links) + "</nav>"
    return (
        f"<!doctype html><html lang='en'><head><meta charset='utf-8'><title>{title}</title></head>"
        f"<body><h1>{title}</h1>{nav}<main>{body}</main></body></html>"
    ).encode()


def _form(action: str, fields: str, button: str, hidden: Mapping[str, str] = {}) -> str:
    concealed = "".join(f"<input type='hidden' name='{k}' value='{v}'>" for k, v in hidden.items())
    return f"<form action='{action}' method='post'>{concealed}{fields}<button type='submit'>{button}</button></form>"


class _Handler(BaseHTTPRequestHandler):
    site: Site
    protocol_version = "HTTP/1.1"

    def log_message(self, format: str, *args: object) -> None:
        pass

    def _body(self) -> dict[str, str]:
        length = int(self.headers.get("Content-Length") or 0)
        return dict(parse_qsl(self.rfile.read(length).decode("utf-8", "replace")))

    def _sid(self) -> str | None:
        for part in (self.headers.get("Cookie") or "").split(";"):
            name, _, value = part.strip().partition("=")
            if name == "sid":
                return value or None
        return None

    def _send(self, status: HTTPStatus, body: bytes, type: str = "text/html; charset=utf-8") -> None:
        self.send_response(status)
        self.send_header("Content-Type", type)
        self.send_header("Cache-Control", "no-store")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _html(self, title: str, body: str, status: HTTPStatus = HTTPStatus.OK) -> None:
        self._send(status, _page(title, body))

    def _redirect(self, where: str, sid: str | None = None) -> None:
        self.send_response(HTTPStatus.SEE_OTHER)
        self.send_header("Location", where)
        if sid:
            self.send_header("Set-Cookie", f"sid={sid}; Path=/; HttpOnly")
        self.send_header("Content-Length", "0")
        self.end_headers()

    def _protected(self) -> str | None:
        """The signed-in account, or a redirect to the sign-in page and None."""
        email = self.site.who(self._sid())
        if email is None:
            here = urlsplit(self.path).path
            self._redirect(f"/login?next={quote(here)}")
        return email

    # ---- pages ---------------------------------------------------------------------------------------------

    def do_GET(self) -> None:
        parsed = urlsplit(self.path)
        path, query = parsed.path, dict(parse_qsl(parsed.query))
        self.site.visited(path)
        route = {
            "/": self._home,
            "/shop": self._shop,
            "/login": self._login_form,
            "/login/code": self._code_form,
            "/logout": self._logout,
            "/account": self._account,
            "/account/orders": self._orders,
            "/cart": self._cart,
            "/checkout": self._checkout_form,
            "/settings/password": self._password_form,
            "/phone": self._phone,
            "/reports": self._reports,
            "/widgets": self._widgets,
            "/widgets/note": self._widgets_note,
            "/feed": self._feed,
            "/api/feed.json": self._feed_json,
            "/portal": self._portal,
            "/compare": self._compare_index,
            "/login/otp": self._otp_page,
            "/js": self._scripted,
            "/api/products.json": self._products_json,
            "/download/report.csv": self._report,
            "/board": self._board,
        }.get(path)
        if route is None:
            if path.startswith("/product/"):
                self._product(path.removeprefix("/product/"))
            elif path.startswith("/compare/"):
                self._compare(path.removeprefix("/compare/"))
            else:
                self._send(HTTPStatus.NOT_FOUND, b"<h1>Not found</h1>")
            return
        route(query)

    def do_POST(self) -> None:
        path = urlsplit(self.path).path
        fields = self._body()
        self.site.record(path, fields)
        route = {
            "/login": self._login_post,
            "/login/code": self._code_post,
            "/cart/add": self._add_post,
            "/cart/remove": self._remove_post,
            "/checkout": self._checkout_post,
            "/settings/password": self._password_post,
            "/login/otp": self._otp_post,
            "/widgets": self._widgets_post,
            "/widgets/shadow": self._shadow_post,
            "/portal": self._portal_post,
            "/board/drop": self._board_drop,
        }.get(path)
        if route is None:
            self._send(HTTPStatus.NOT_FOUND, b"<h1>Not found</h1>")
            return
        route(fields)

    def _home(self, _: dict[str, str]) -> None:
        self._html(
            "Mock Site",
            "<p>A small shop with an account area.</p><ul>"
            "<li><a href='/shop'>Browse the shop</a></li>"
            "<li><a href='/account'>Your account</a></li></ul>",
        )

    def _shop(self, query: dict[str, str]) -> None:
        page = max(1, int(query.get("page", "1") or 1))
        rows = list(PRODUCTS)
        if query.get("sort") == "price":
            rows.sort(key=lambda row: row[2])
        elif query.get("sort") == "price-desc":
            rows.sort(key=lambda row: row[2], reverse=True)
        pages = max(1, -(-len(rows) // PER_PAGE))
        here = rows[(page - 1) * PER_PAGE : page * PER_PAGE]
        items = "".join(f"<li><a href='/product/{pid}'>{name}</a> - GBP {price:.2f}</li>" for pid, name, price in here)
        links = " ".join(f"<a href='/shop?page={n}&sort={query.get('sort', '')}'>{n}</a>" for n in range(1, pages + 1))
        self._html(
            "Shop",
            f"<p>Page {page} of {pages}. Sort by <a href='/shop?page=1&sort=price'>price</a> or "
            f"<a href='/shop?page=1&sort=price-desc'>price descending</a>.</p><ul>{items}</ul><p>Pages: {links}</p>",
        )

    def _product(self, pid: str) -> None:
        found = next((row for row in PRODUCTS if row[0] == pid), None)
        if found is None:
            self._send(HTTPStatus.NOT_FOUND, b"<h1>Not found</h1>")
            return
        _, name, price = found
        self._html(
            name,
            f"<p>Price: GBP {price:.2f}</p>" + _form("/cart/add", "", "Add to basket", hidden={"item": pid}),
        )

    def _login_form(self, query: dict[str, str]) -> None:
        back = query.get("next", "")
        self._html(
            "Sign in",
            "<p>Sign in to your account.</p>"
            + _form(
                "/login",
                "<p><label for='email'>Email</label> <input id='email' name='email' type='email'></p>"
                "<p><label for='password'>Password</label> <input id='password' name='password' type='password'></p>",
                "Sign in",
                hidden={"next": back},
            ),
        )

    def _login_post(self, fields: dict[str, str]) -> None:
        email = (fields.get("email") or "").strip().lower()
        password = fields.get("password") or ""
        if self.site.passwords.get(email) != password:
            self.site.failed_sign_ins.append(email)
            self._html(
                "Sign in",
                "<p><strong>Incorrect email or password.</strong> Please try again.</p>"
                + _form(
                    "/login",
                    "<p><label for='email'>Email</label> <input id='email' name='email' type='email'></p>"
                    "<p><label for='password'>Password</label> "
                    "<input id='password' name='password' type='password'></p>",
                    "Sign in",
                    hidden={"next": fields.get("next", "")},
                ),
                HTTPStatus.UNAUTHORIZED,
            )
            return
        if email == NEEDS_CODE:
            self._html(
                "Two-step sign-in",
                f"<p>A code is needed for {email}.</p>"
                + _form(
                    "/login/code",
                    "<p><label for='code'>Six-digit code</label> "
                    "<input id='code' name='code' type='text' inputmode='numeric'></p>",
                    "Continue",
                    hidden={"email": email, "next": fields.get("next", "")},
                ),
            )
            return
        if email == NEEDS_PER_DIGIT_CODE:
            self._html("Two-step sign-in", self._otp_form(email, fields.get("next", "")))
            return
        sid = self.site.sign_in(email)
        self._redirect(fields.get("next") or "/account", sid)

    def _otp_form(self, email: str, back: str) -> str:
        """A code typed one digit per box, which is what a real one-time-password screen asks for."""
        boxes = "".join(
            f"<input name='d{n}' maxlength='1' size='1' inputmode='numeric' aria-label='Digit {n}'>"
            for n in range(1, 7)
        )
        return _form(
            "/login/otp",
            f"<p>Enter the six-digit code for {email}, one digit per box.</p><p>{boxes}</p>",
            "Continue",
            hidden={"email": email, "next": back},
        )

    def _otp_page(self, query: dict[str, str]) -> None:
        self._html("Two-step sign-in", self._otp_form(query.get("email", ""), query.get("next", "")))

    def _otp_post(self, fields: dict[str, str]) -> None:
        email = (fields.get("email") or "").strip().lower()
        code = "".join((fields.get(f"d{n}") or "").strip() for n in range(1, 7))
        self.site.note_code_attempt(code)
        if code != code_for(email):
            self._html(
                "Two-step sign-in",
                "<p><strong>That code is not right.</strong></p>" + self._otp_form(email, fields.get("next", "")),
                HTTPStatus.UNAUTHORIZED,
            )
            return
        sid = self.site.sign_in(email)
        self._redirect(fields.get("next") or "/account", sid)

    def _code_form(self, query: dict[str, str]) -> None:
        self._html(
            "Two-step sign-in",
            "<p>Enter the six-digit code from your authenticator.</p>"
            + _form(
                "/login/code",
                "<p><label for='code'>Six-digit code</label> "
                "<input id='code' name='code' type='text' inputmode='numeric'></p>",
                "Continue",
                hidden={"email": query.get("email", ""), "next": query.get("next", "")},
            ),
        )

    def _code_post(self, fields: dict[str, str]) -> None:
        email = (fields.get("email") or "").strip().lower()
        code = (fields.get("code") or "").strip()
        self.site.note_code_attempt(code)
        if code != code_for(email):
            self._html(
                "Two-step sign-in",
                "<p><strong>That code is not right.</strong></p>"
                + _form(
                    "/login/code",
                    "<p><label for='code'>Six-digit code</label> "
                    "<input id='code' name='code' type='text' inputmode='numeric'></p>",
                    "Continue",
                    hidden={"email": email, "next": fields.get("next", "")},
                ),
                HTTPStatus.UNAUTHORIZED,
            )
            return
        sid = self.site.sign_in(email)
        self._redirect(fields.get("next") or "/account", sid)

    def _logout(self, _: dict[str, str]) -> None:
        self.site.sign_out(self._sid())
        self._redirect("/")

    def _account(self, _: dict[str, str]) -> None:
        email = self._protected()
        if email is None:
            return
        self._html(
            "Your account",
            f"<p>Signed in as {email}.</p><ul>"
            "<li><a href='/account/orders'>Your orders</a></li>"
            "<li><a href='/cart'>Your basket</a></li>"
            "<li><a href='/settings/password'>Change your password</a></li>"
            "<li><a href='/logout'>Sign out</a></li></ul>",
        )

    def _orders(self, _: dict[str, str]) -> None:
        email = self._protected()
        if email is None:
            return
        rows = "".join(
            f"<tr><td>{ref}</td><td>{name}</td><td>GBP {total:.2f}</td></tr>"
            for ref, name, total in ORDERS.get(email, ())
        )
        self._html(
            "Your orders",
            f"<table><tr><th>Reference</th><th>Item</th><th>Total</th></tr>{rows}</table>",
        )

    def _cart(self, _: dict[str, str]) -> None:
        email = self._protected()
        if email is None:
            return
        by_id = {pid: name for pid, name, _ in PRODUCTS}
        held = self.site.cart(email)
        items = "".join(
            f"<li>{by_id.get(item, item)} {_form('/cart/remove', '', 'Remove', hidden={'item': item})}</li>"
            for item in held
        )
        self._html(
            "Your basket",
            f"<ul>{items or '<li>Nothing yet.</li>'}</ul>"
            f"<p>Subtotal: GBP {self.site.subtotal(email):.2f}</p>"
            "<p><a href='/checkout'>Go to checkout</a></p>",
        )

    def _add_post(self, fields: dict[str, str]) -> None:
        email = self._protected()
        if email is None:
            return
        self.site.add_to_cart(email, fields.get("item", ""))
        self._redirect("/cart")

    def _remove_post(self, fields: dict[str, str]) -> None:
        email = self._protected()
        if email is None:
            return
        self.site.remove_from_cart(email, fields.get("item", ""))
        self._redirect("/cart")

    def _checkout_form(self, _: dict[str, str]) -> None:
        email = self._protected()
        if email is None:
            return
        self._html(
            "Checkout",
            f"<p>Basket total GBP {self.site.subtotal(email):.2f}.</p>"
            + _form(
                "/checkout",
                "<p><label for='name'>Full name</label> <input id='name' name='name'></p>"
                "<p><label for='postcode'>Postcode</label> <input id='postcode' name='postcode'></p>",
                "Place order",
            ),
        )

    def _checkout_post(self, fields: dict[str, str]) -> None:
        email = self._protected()
        if email is None:
            return
        name = (fields.get("name") or "").strip()
        postcode = (fields.get("postcode") or "").strip().upper()
        problems = []
        if not name:
            problems.append("A full name is required.")
        if not _looks_like_postcode(postcode):
            problems.append("That postcode is not a valid UK postcode.")
        if problems:
            self._html(
                "Checkout",
                "".join(f"<p><strong>{problem}</strong></p>" for problem in problems)
                + _form(
                    "/checkout",
                    "<p><label for='name'>Full name</label> "
                    f"<input id='name' name='name' value='{name}'></p>"
                    "<p><label for='postcode'>Postcode</label> "
                    f"<input id='postcode' name='postcode' value='{postcode}'></p>",
                    "Place order",
                ),
                HTTPStatus.BAD_REQUEST,
            )
            return
        self.site.place_order(
            {"email": email, "name": name, "postcode": postcode, "total": f"{self.site.subtotal(email):.2f}"}
        )
        self._html("Order placed", "<p>Thank you. Your order has been placed.</p><p>Your basket is now empty.</p>")

    def _password_form(self, _: dict[str, str]) -> None:
        if self._protected() is None:
            return
        self._html(
            "Change your password",
            "<p>Changing your password signs you out everywhere.</p>"
            + _form(
                "/settings/password",
                "<p><label for='current'>Current password</label> "
                "<input id='current' name='current' type='password'></p>"
                "<p><label for='new'>New password</label> <input id='new' name='new' type='password'></p>",
                "Change password",
            ),
        )

    def _password_post(self, fields: dict[str, str]) -> None:
        email = self._protected()
        if email is None:
            return
        current = fields.get("current") or ""
        new = fields.get("new") or ""
        if self.site.passwords.get(email) != current:
            self._html(
                "Change your password",
                "<p><strong>That is not your current password.</strong></p>",
                HTTPStatus.UNAUTHORIZED,
            )
            return
        self.site.change_password(email, new)
        self._html("Password changed", "<p>Your password has been changed.</p>")

    def _phone(self, _: dict[str, str]) -> None:
        self._html(
            "Authenticator",
            f"<p>ada@example.com: <strong>{code_for('ada@example.com')}</strong></p>"
            f"<p>grace@example.com: <strong>{code_for('grace@example.com')}</strong></p>"
            f"<p>katherine@example.com: <strong>{code_for('katherine@example.com')}</strong></p>",
        )

    def _reports(self, _: dict[str, str]) -> None:
        self._html("Reports", "<p>Download the <a href='/download/report.csv'>orders report</a>.</p>")

    def _widgets(self, _: dict[str, str]) -> None:
        """Two controls outside the main document: one in a frame, one in a shadow root."""
        self._html(
            "Widgets",
            "<p>The note panel and the reference form are not part of this document.</p>"
            "<iframe id='note-panel' src='/widgets/note' title='Note panel' width='320' height='140'></iframe>"
            "<div id='shadow-host'></div>"
            "<script>"
            "const host = document.getElementById('shadow-host');"
            "const root = host.attachShadow({mode: 'open'});"
            "root.innerHTML = \"<form action='/widgets/shadow' method='post'>"
            "<p><label for='ref'>Reference</label> <input id='ref' name='ref'></p>"
            "<button type='submit'>Confirm reference</button></form>\";"
            "</script>",
        )

    def _widgets_note(self, _: dict[str, str]) -> None:
        self._html(
            "Note panel",
            _form("/widgets", "<p><label for='note'>Note</label> <input id='note' name='note'></p>", "Save note"),
        )

    def _widgets_post(self, fields: dict[str, str]) -> None:
        self._html("Note saved", f"<p>Saved {fields.get('note', '')!r}.</p>")

    def _shadow_post(self, fields: dict[str, str]) -> None:
        self._html("Reference confirmed", f"<p>Reference {fields.get('ref', '')!r} confirmed.</p>")

    def _feed(self, _: dict[str, str]) -> None:
        """A list that grows a batch at a time on scroll or on the button, with an explicit end state."""
        first = "".join(f"<li>{_feed_item(n)}</li>" for n in range(1, FEED_BATCH + 1))
        self._html(
            "Feed",
            f"<p>Showing <span id='count'>{FEED_BATCH}</span> of {FEED_TOTAL} items.</p>"
            f"<ul id='items'>{first}</ul>"
            "<p id='end'></p>"
            "<p><button id='more' type='button'>Load more</button></p>"
            "<script>"
            f"let shown = {FEED_BATCH}; const total = {FEED_TOTAL};"
            "async function more() {"
            "  if (shown >= total) return;"
            "  const response = await fetch('/api/feed.json?after=' + shown);"
            "  const batch = await response.json();"
            "  const list = document.getElementById('items');"
            "  for (const item of batch.items) {"
            "    const li = document.createElement('li'); li.textContent = item; list.appendChild(li);"
            "  }"
            "  shown = batch.shown;"
            "  document.getElementById('count').textContent = shown;"
            "  if (shown >= total) {"
            "    document.getElementById('end').textContent = 'End of list. ' + total + ' items.';"
            "    document.getElementById('more').disabled = true;"
            "  }"
            "}"
            "document.getElementById('more').addEventListener('click', more);"
            "window.addEventListener('scroll', () => {"
            "  if (window.innerHeight + window.scrollY >= document.body.offsetHeight - 200) more();"
            "});"
            "</script>",
        )

    def _feed_json(self, query: dict[str, str]) -> None:
        after = int(query.get("after", "0") or 0)
        shown = min(FEED_TOTAL, after + FEED_BATCH)
        items = [_feed_item(n) for n in range(after + 1, shown + 1)]
        self._send(HTTPStatus.OK, json.dumps({"items": items, "shown": shown}).encode(), "application/json")

    def _portal(self, _: dict[str, str]) -> None:
        """A consent banner on arrival and a modal a beat later, with the form hidden until both are gone."""
        self._html(
            "Support portal",
            "<div id='consent' style='position:fixed;left:0;right:0;bottom:0;background:#111;color:#fff;padding:1rem'>"
            "This site uses cookies. <button id='accept' type='button'>Accept all</button></div>"
            "<div id='modal' style='display:none;position:fixed;inset:0;background:rgba(0,0,0,.6)'>"
            "<div style='margin:20vh auto;width:20rem;background:#fff;padding:1rem'>"
            "<p>A moment of your time?</p><button id='dismiss' type='button'>Dismiss</button></div></div>"
            "<form action='/portal' method='post' id='request'>"
            "<p><label for='topic'>Request</label> <input id='topic' name='topic'></p>"
            "<button type='submit'>Send request</button></form>"
            "<script>"
            "document.getElementById('accept').addEventListener('click', () => "
            "{ document.getElementById('consent').remove(); });"
            "setTimeout(() => { document.getElementById('modal').style.display = 'block'; }, 1200);"
            "document.getElementById('dismiss').addEventListener('click', () => "
            "{ document.getElementById('modal').remove(); });"
            "</script>",
        )

    def _portal_post(self, fields: dict[str, str]) -> None:
        self._html("Request sent", f"<p>We have your request: {fields.get('topic', '')!r}.</p>")

    def _compare_index(self, _: dict[str, str]) -> None:
        self._html(
            "Compare",
            "<p>Two shops sell the same grinder.</p><ul>"
            "<li><a href='/compare/a'>Shop A</a></li><li><a href='/compare/b'>Shop B</a></li></ul>",
        )

    def _compare(self, which: str) -> None:
        found = COMPARISON.get(which)
        if found is None:
            self._send(HTTPStatus.NOT_FOUND, b"<h1>Not found</h1>")
            return
        shop, price = found
        self._html(shop, f"<p>Burr Grinder - GBP {price:.2f}</p>")

    def _scripted(self, _: dict[str, str]) -> None:
        self._send(
            HTTPStatus.OK,
            _page(
                "Stock report",
                "<p>Loading...</p><div id='out'></div><script>"
                "fetch('/api/products.json').then(r => r.json()).then(rows => {"
                "document.getElementById('out').textContent = 'In stock: ' + rows.length + ' products';"
                "});</script>",
            ),
        )

    def _products_json(self, _: dict[str, str]) -> None:
        self._send(HTTPStatus.OK, json.dumps(PRODUCTS).encode(), "application/json")

    def _report(self, _: dict[str, str]) -> None:
        body = "reference,item,total\n" + "".join(
            f"{ref},{name},{total:.2f}\n" for ref, name, total in ORDERS["ada@example.com"]
        )
        self._send(HTTPStatus.OK, body.encode(), "text/csv")

    def _board(self, _: dict[str, str]) -> None:
        """A card `draggable="true"` and two columns carrying `ondrop`, the shapes the snapshot offers as a
        drag source and a drop target. Dropping posts the card and its column, so the site, not the run's own
        report, says where the card went."""
        body = (
            "<p>Drag the card into the column it belongs in.</p>"
            "<div class='board'>"
            "<div class='column' data-column='To Do' ondragover='allow(event)' ondrop='drop(event)'>"
            "<h2>To Do</h2>"
            "<div class='card' draggable='true' data-card='Card A' ondragstart='drag(event)'>Card A</div>"
            "</div>"
            "<div class='column' data-column='Done' ondragover='allow(event)' ondrop='drop(event)'>"
            "<h2>Done</h2>"
            "</div>"
            "</div>"
            "<script>"
            "function allow(ev){ ev.preventDefault(); }"
            "function drag(ev){ ev.dataTransfer.setData('text/plain', ev.target.dataset.card); }"
            "function drop(ev){ ev.preventDefault();"
            " const card = ev.dataTransfer.getData('text/plain');"
            " const column = ev.currentTarget.dataset.column;"
            " const body = 'card=' + encodeURIComponent(card) + '&column=' + encodeURIComponent(column);"
            " fetch('/board/drop', {method: 'POST',"
            " headers: {'Content-Type': 'application/x-www-form-urlencoded'}, body: body}); }"
            "</script>"
        )
        self._html("Board", body)

    def _board_drop(self, fields: dict[str, str]) -> None:
        self.site.record_drop(fields)
        self._send(HTTPStatus.OK, b"dropped")


def _looks_like_postcode(value: str) -> bool:
    """The shapes a UK postcode takes: one or two letters, a digit, an optional letter, a space, then three more.

    `SW1A 1AA` is why the optional letter is there: without it the pattern rejects a real postcode, and the site
    would fail a run that filled the form exactly as asked.
    """
    return bool(re.fullmatch(r"[A-Z]{1,2}\d[A-Z\d]? \d[A-Z]{2}", value))


@contextmanager
def mock_server() -> Generator[tuple[str, Callable[[], Site]]]:
    """Keep an address alive while sequential attempts each receive fresh fixture state."""
    handler = type("Handler", (_Handler,), {"site": Site()})
    server = ThreadingHTTPServer(("127.0.0.1", free_port()), handler)

    def fresh() -> Site:
        site = Site()
        handler.site = site
        return site

    threading.Thread(target=server.serve_forever, daemon=True).start()
    try:
        yield f"http://127.0.0.1:{server.server_port}", fresh
    finally:
        server.shutdown()
        server.server_close()


@contextmanager
def mock_site() -> Generator[tuple[str, Site]]:
    """Serve one fresh site and close its server after the attempt."""
    with mock_server() as (base, fresh):
        yield base, fresh()
