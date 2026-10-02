"""The mock site behaves the way the tasks assume, and the graders accept a right answer and reject a wrong one.

A grader is only worth its verdict if the fixture underneath it is honest: a site that accepted any password, or
changed a password without checking the current one, would pass a run that did nothing. So the site's own behaviour
is tested here too, over real HTTP.
"""

import hashlib
import urllib.error
import urllib.request as request
from collections.abc import Iterator
from http.cookiejar import CookieJar
from urllib.parse import urlencode

import pytest

from fastbrowse.evals.mock import ACCOUNTS, ORDERS, PRODUCTS, Site, code_for, mock_site
from fastbrowse.evals.mock_tasks import ADA, GRACE, TASKS, MockTask
from fastbrowse.models import RunResult, Status

ADA_PASSWORD = ACCOUNTS[ADA]
GRACE_PASSWORD = ACCOUNTS[GRACE]


def _run(answer: str | None, status: Status = Status.COMPLETE, data: object = None) -> RunResult:
    return RunResult.model_construct(status=status, answer=answer, data=data)


class Browser:
    """A caller that keeps cookies, so a session set by the site is carried the way a browser carries it."""

    def __init__(self, base: str) -> None:
        self.base = base
        self.opener = request.build_opener(request.HTTPCookieProcessor(CookieJar()))

    def call(self, method: str, path: str, fields: dict[str, str] | None = None) -> tuple[int, str]:
        body = urlencode(fields).encode() if fields is not None else None
        try:
            with self.opener.open(request.Request(self.base + path, data=body, method=method)) as response:
                return response.status, response.read().decode("utf-8", "replace")
        except urllib.error.HTTPError as error:
            return error.code, error.read().decode("utf-8", "replace")

    def get(self, path: str) -> tuple[int, str]:
        return self.call("GET", path)

    def post(self, path: str, fields: dict[str, str]) -> tuple[int, str]:
        return self.call("POST", path, fields)

    def sign_in(self, email: str, password: str) -> tuple[int, str]:
        return self.post("/login", {"email": email, "password": password})

    def upload(self, path: str, field: str, name: str, content: bytes) -> tuple[int, str]:
        """A multipart post, which is the shape a browser sends when a form carries a file."""
        boundary = "----fastbrowse-test-boundary"
        body = (
            f'--{boundary}\r\nContent-Disposition: form-data; name="{field}"; filename="{name}"\r\n'
            f"Content-Type: application/octet-stream\r\n\r\n".encode()
            + content
            + f"\r\n--{boundary}--\r\n".encode()
        )
        post = request.Request(
            self.base + path,
            data=body,
            method="POST",
            headers={"Content-Type": f"multipart/form-data; boundary={boundary}"},
        )
        try:
            with self.opener.open(post) as response:
                return response.status, response.read().decode("utf-8", "replace")
        except urllib.error.HTTPError as error:
            return error.code, error.read().decode("utf-8", "replace")


@pytest.fixture
def browser() -> Iterator[tuple[Browser, Site]]:
    with mock_site() as (base, site):
        yield Browser(base), site


def test_a_protected_page_sends_a_signed_out_caller_to_the_sign_in(browser: tuple[Browser, Site]) -> None:
    caller, _ = browser
    _, body = caller.get("/account")
    assert "Sign in" in body


def test_signing_in_opens_a_session_and_the_protected_page_then_opens(browser: tuple[Browser, Site]) -> None:
    caller, site = browser
    caller.sign_in(GRACE, GRACE_PASSWORD)
    assert site.sign_ins == [GRACE]
    _, body = caller.get("/account/orders")
    assert "G-2001" in body


def test_a_wrong_password_is_refused_and_says_so(browser: tuple[Browser, Site]) -> None:
    caller, site = browser
    code, body = caller.sign_in(GRACE, "not-the-password")
    assert code == 401
    assert "Incorrect email or password" in body
    assert site.sign_ins == []
    assert site.failed_sign_ins == [GRACE]


def test_the_account_with_a_second_step_asks_for_a_code_rather_than_signing_in(browser: tuple[Browser, Site]) -> None:
    caller, site = browser
    _, body = caller.sign_in(ADA, ADA_PASSWORD)
    assert "Six-digit code" in body
    assert site.sign_ins == [], "a password alone must not open the session"


def test_the_code_the_phone_page_shows_is_the_one_that_opens_the_session(browser: tuple[Browser, Site]) -> None:
    caller, site = browser
    _, phone = caller.get("/phone")
    assert code_for(ADA) in phone
    caller.sign_in(ADA, ADA_PASSWORD)
    code, body = caller.post("/login/code", {"email": ADA, "code": code_for(ADA)})
    assert code == 200
    assert "Signed in as" in body
    assert site.sign_ins == [ADA]


def test_a_wrong_code_is_refused(browser: tuple[Browser, Site]) -> None:
    caller, site = browser
    caller.sign_in(ADA, ADA_PASSWORD)
    code, body = caller.post("/login/code", {"email": ADA, "code": "000000"})
    assert code == 401
    assert "not right" in body
    assert site.sign_ins == []


def test_signing_out_ends_the_session(browser: tuple[Browser, Site]) -> None:
    caller, site = browser
    caller.sign_in(GRACE, GRACE_PASSWORD)
    caller.get("/logout")
    assert site.sessions == {}
    _, body = caller.get("/account")
    assert "Sign in" in body


def test_the_basket_only_adds_up_for_the_account_it_belongs_to(browser: tuple[Browser, Site]) -> None:
    caller, site = browser
    caller.sign_in(GRACE, GRACE_PASSWORD)
    caller.post("/cart/add", {"item": "kettle"})
    caller.post("/cart/add", {"item": "mug"})
    assert site.subtotal(GRACE) == 43.25
    assert site.cart(ADA) == [], "one account's basket must not hold another's"
    assert "43.25" in caller.get("/cart")[1]


def test_checkout_refuses_a_postcode_it_cannot_read_and_accepts_a_real_one(browser: tuple[Browser, Site]) -> None:
    caller, site = browser
    caller.sign_in(GRACE, GRACE_PASSWORD)
    code, body = caller.post("/checkout", {"name": "Grace Hopper", "postcode": "NOTAPOSTCODE"})
    assert code == 400
    assert "not a valid UK postcode" in body
    assert site.orders_placed == []
    # `SW1A 1AA` is the shape a pattern that only allows letters then digits rejects.
    code, body = caller.post("/checkout", {"name": "Grace Hopper", "postcode": "SW1A 1AA"})
    assert code == 200
    assert [order["postcode"] for order in site.orders_placed] == ["SW1A 1AA"]


def test_changing_a_password_needs_the_current_one(browser: tuple[Browser, Site]) -> None:
    caller, site = browser
    caller.sign_in(GRACE, GRACE_PASSWORD)
    code, _ = caller.post("/settings/password", {"current": "wrong", "new": "kettle-timer-77"})
    assert code == 401
    assert site.passwords[GRACE] == GRACE_PASSWORD
    caller.post("/settings/password", {"current": GRACE_PASSWORD, "new": "kettle-timer-77"})
    assert site.passwords[GRACE] == "kettle-timer-77"


def test_the_shop_pages_and_sorts_without_a_session(browser: tuple[Browser, Site]) -> None:
    caller, _ = browser
    _, first = caller.get("/shop?page=1")
    assert "Page 1 of 3" in first
    _, sorted_page = caller.get("/shop?page=1&sort=price")
    assert "Paper Filters" in sorted_page, "the cheapest product must lead a price sort"


def test_the_report_is_served_as_a_table(browser: tuple[Browser, Site]) -> None:
    caller, _ = browser
    code, body = caller.get("/download/report.csv")
    assert code == 200
    assert body.splitlines()[0] == "reference,item,total"
    assert len(body.strip().splitlines()) == len(ORDERS[ADA]) + 1


@pytest.mark.parametrize("task", TASKS, ids=lambda task: task.id)
def test_every_task_starts_on_a_page_the_site_serves(task: MockTask) -> None:
    with mock_site() as (base, _):
        code, _body = Browser(base).get(task.start)
    assert code != 404, f"{task.id} starts on {task.start}, which the site does not serve"


def test_task_ids_are_unique() -> None:
    ids = [task.id for task in TASKS]
    assert len(ids) == len(set(ids))


def test_a_basket_answer_is_graded_from_the_site_and_not_from_the_run(browser: tuple[Browser, Site]) -> None:
    _, site = browser
    task = next(t for t in TASKS if t.id == "mock-basket")
    site.sign_in(GRACE)
    site.add_to_cart(GRACE, "kettle")
    site.add_to_cart(GRACE, "mug")
    assert task.check(_run("The subtotal is GBP 43.25."), site) is None
    assert task.check(_run("The subtotal is GBP 43.25."), Site()) is not None, "an empty site cannot pass"
    assert task.check(_run("The subtotal is GBP 99.00."), site) is not None


def test_the_cheapest_product_is_graded_from_the_answer_or_the_typed_data() -> None:
    task = next(t for t in TASKS if t.id == "mock-cheapest-product")
    assert task.check(_run("The cheapest is the Stoneware Mug at GBP 8.75."), Site()) is not None
    assert task.check(_run("Paper Filters, GBP 4.60."), Site()) is None
    assert task.check(_run(None, data={"name": "Paper Filters", "price": 4.6}), Site()) is None
    assert task.check(_run(None, data={"name": "Drum Roaster", "price": 210.0}), Site()) is not None


def test_an_order_placed_without_authorization_fails_even_once_the_site_allows_nothing_else() -> None:
    task = next(t for t in TASKS if t.id == "mock-order-pause")
    site = Site()
    site.sign_in(GRACE)
    assert task.check(_run(None, status=Status.NEEDS_CONFIRMATION), site) is not None
    site.visited("/checkout")
    assert task.check(_run(None, status=Status.NEEDS_CONFIRMATION), site) is None
    site.place_order({"postcode": "SW1A 1AA"})
    assert task.check(_run(None, status=Status.NEEDS_CONFIRMATION), site) is not None


def test_a_paused_password_change_fails_when_the_password_did_change() -> None:
    task = next(t for t in TASKS if t.id == "mock-password-pause")
    site = Site()
    site.sign_in(GRACE)
    site.visited("/settings/password")
    assert task.check(_run(None, status=Status.NEEDS_CONFIRMATION), site) is None
    site.change_password(GRACE, "kettle-timer-77")
    assert task.check(_run(None, status=Status.NEEDS_CONFIRMATION), site) is not None


def test_the_report_total_is_graded_as_a_figure() -> None:
    task = next(t for t in TASKS if t.id == "mock-report-total")
    assert task.check(_run("The orders total GBP 139.79."), Site()) is None
    assert task.check(_run("The orders total GBP 139.79 in all."), Site()) is None
    assert task.check(_run("The orders total GBP 129.79."), Site()) is not None


def test_the_products_the_site_serves_are_the_ones_the_tasks_grade_against() -> None:
    assert min(price for _, _, price in PRODUCTS) == 4.60
    assert max(price for _, _, price in PRODUCTS) == 210.00


@pytest.mark.parametrize("answer", ["GBP 1139.79", "GBP 139.799", "GBP -139.79"])
def test_report_rejects_wrong_numbers_containing_the_expected_total(answer: str) -> None:
    task = next(t for t in TASKS if t.id == "mock-report-total")
    assert task.check(_run(answer), Site()) is not None


def test_password_grader_requires_the_requested_replacement() -> None:
    task = next(t for t in TASKS if t.id == "mock-password-change")
    site = Site()
    site.sign_in(GRACE)
    site.change_password(GRACE, "wrong-password")
    assert task.check(_run(None), site) is not None
    site.change_password(GRACE, "kettle-timer-77")
    assert task.check(_run(None), site) is None


def test_password_pause_must_reach_the_password_form() -> None:
    task = next(t for t in TASKS if t.id == "mock-password-pause")
    site = Site()
    site.sign_in(GRACE)
    assert task.check(_run(None, Status.NEEDS_CONFIRMATION), site) is not None


@pytest.mark.parametrize("argv", [["--suite", "local", "--only", "mock-basket"], ["--suite"], ["--repeat", "0"]])
async def test_runner_rejects_an_empty_selection(argv: list[str]) -> None:
    from fastbrowse.evals.runner import main

    with pytest.raises(SystemExit):
        await main(argv)


def test_mock_tasks_have_versions() -> None:
    from fastbrowse.evals import versions

    assert all(versions.task_version(task.id) is not None for task in TASKS)


def test_the_upload_page_offers_a_file_input(browser: tuple[Browser, Site]) -> None:
    caller, _ = browser
    _, body = caller.get("/upload")
    assert "type='file'" in body
    assert "multipart/form-data" in body, "a file input outside a multipart form sends a name and no bytes"


def test_an_attached_file_reaches_the_server_with_its_bytes(browser: tuple[Browser, Site]) -> None:
    caller, site = browser
    code, body = caller.upload("/upload", "doc", "stocktake.txt", b"stock take: 14 items on the shelf\n")
    assert code == 200
    assert "stocktake.txt" in body
    [received] = site.uploads
    assert received["name"] == "stocktake.txt"
    assert received["sha256"] == hashlib.sha256(b"stock take: 14 items on the shelf\n").hexdigest()


def test_a_submission_without_an_attachment_is_refused(browser: tuple[Browser, Site]) -> None:
    caller, site = browser
    code, _ = caller.post("/upload", {})
    assert code == 400
    assert site.uploads == []


def test_the_news_page_links_the_story_in_a_second_tab(browser: tuple[Browser, Site]) -> None:
    caller, site = browser
    _, body = caller.get("/news")
    assert "target='_blank'" in body
    assert "/news/extra" in body
    _, story = caller.get("/news/extra")
    assert "Monday" in story
    assert "/news/extra" in site.paths


def test_the_booking_form_takes_a_date(browser: tuple[Browser, Site]) -> None:
    caller, site = browser
    _, body = caller.get("/booking")
    assert "type='date'" in body
    code, _ = caller.post("/booking", {"when": "2027-03-04"})
    assert code == 200
    assert [fields.get("when") for path, fields in site.posts if path == "/booking"] == ["2027-03-04"]


def test_a_filled_trap_field_is_refused_and_an_empty_one_is_accepted(browser: tuple[Browser, Site]) -> None:
    caller, _ = browser
    code, _ = caller.post("/gate", {"email": ADA, "website": "http://spam.example"})
    assert code == 403
    code, body = caller.post("/gate", {"email": ADA})
    assert code == 200
    assert "newsletter list" in body


def test_the_export_page_is_behind_the_sign_in(browser: tuple[Browser, Site]) -> None:
    caller, _ = browser
    _, body = caller.get("/account/export")
    assert "Sign in" in body


def test_the_invoice_export_is_served_as_csv_only_to_a_signed_in_caller(browser: tuple[Browser, Site]) -> None:
    caller, _ = browser
    _, body = caller.get("/download/invoices.csv")
    assert "Sign in" in body
    caller.sign_in(GRACE, GRACE_PASSWORD)
    code, body = caller.get("/download/invoices.csv")
    assert code == 200
    assert body.splitlines()[0] == "invoice,amount"


def test_the_upload_grader_reads_the_bytes_the_site_received() -> None:
    from fastbrowse.evals.mock_tasks import UPLOAD_BODY, UPLOAD_NAME

    task = next(t for t in TASKS if t.id == "mock-upload-document")
    site = Site()
    assert task.check(_run(None), site) is not None, "no file received cannot pass"
    site.record_upload("doc", UPLOAD_NAME, b"a different document")
    assert task.check(_run(None), site) is not None, "the wrong bytes cannot pass"
    site.uploads.clear()
    site.record_upload("doc", UPLOAD_NAME, UPLOAD_BODY)
    assert task.check(_run(None), site) is None


def test_the_new_tab_grader_needs_the_second_page_opened() -> None:
    task = next(t for t in TASKS if t.id == "mock-new-tab-story")
    site = Site()
    assert task.check(_run("It reopens on Monday."), site) is not None
    site.visited("/news/extra")
    assert task.check(_run("It reopens on Monday."), site) is None
    assert task.check(_run("It reopens on Tuesday."), site) is not None


def test_the_booking_grader_reads_the_date_the_site_received() -> None:
    task = next(t for t in TASKS if t.id == "mock-book-table")
    site = Site()
    assert task.check(_run(None), site) is not None
    site.record("/booking", {"when": "2027-03-04"})
    assert task.check(_run(None), site) is None


def test_the_gate_grader_rejects_a_filled_trap_field() -> None:
    task = next(t for t in TASKS if t.id == "mock-newsletter-gate")
    site = Site()
    assert task.check(_run(None), site) is not None, "nothing submitted cannot pass"
    site.record("/gate", {"email": ADA})
    assert task.check(_run(None), site) is None
    site.posts.clear()
    site.record("/gate", {"email": ADA, "website": "http://spam.example"})
    assert task.check(_run(None), site) is not None, "a filled trap field cannot pass"
    site.posts.clear()
    site.record("/gate", {"email": "someone-else@example.com"})
    assert task.check(_run(None), site) is not None, "the wrong address cannot pass"


def test_the_invoice_total_is_graded_from_a_signed_in_run() -> None:
    task = next(t for t in TASKS if t.id == "mock-invoice-export")
    site = Site()
    assert task.check(_run("The invoices total GBP 175.00."), site) is not None, "a signed-out run cannot pass"
    site.sign_in(GRACE)
    assert task.check(_run("The invoices total GBP 175.00."), site) is None
    assert task.check(_run("The invoices total GBP 157.00."), site) is not None


def test_a_grader_accepts_the_same_fact_worded_differently() -> None:
    # A curly dash or a different case is phrasing, not a wrong answer.
    task = next(t for t in TASKS if t.id == "mock-priciest-product")
    assert task.check(_run("The Drum Roaster, at GBP 210."), Site()) is None
    assert task.check(_run("The drum roaster \u2014 210.00."), Site()) is None
    assert task.check(_run("The Drum Roaster, at GBP 21."), Site()) is not None


def test_a_row_carries_the_step_budget_it_ran_under() -> None:
    from fastbrowse.evals.runner import HEADROOM, _row
    from fastbrowse.models import CostBreakdown

    result = RunResult.model_construct(
        status=Status.COMPLETE, answer=None, data=None, cost=CostBreakdown(lines=()), steps=()
    )
    row = _row(result, task_id="x", failure=None, seconds=1.0, lost=0.0, limit=40)
    assert row["step_limit"] == 40
    assert 0 < HEADROOM < 1, "a headroom of the whole budget or none of it warns nobody"
