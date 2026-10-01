"""The mock site behaves the way the tasks assume, and the graders accept a right answer and reject a wrong one.

A grader is only worth its verdict if the fixture underneath it is honest: a site that accepted any password, or
changed a password without checking the current one, would pass a run that did nothing. So the site's own behaviour
is tested here too, over real HTTP.
"""

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


def test_the_board_offers_a_draggable_card_and_two_drop_columns(browser: tuple[Browser, Site]) -> None:
    caller, _ = browser
    code, body = caller.get("/board")
    assert code == 200
    assert "draggable='true'" in body
    assert "data-column='To Do'" in body and "data-column='Done'" in body
    assert "Card A" in body


def test_a_card_dropped_on_a_column_is_recorded_by_the_site(browser: tuple[Browser, Site]) -> None:
    caller, site = browser
    caller.get("/board")
    caller.post("/board/drop", {"card": "Card A", "column": "Done"})
    assert site.drops == [{"card": "Card A", "column": "Done"}]


def test_the_drag_grader_needs_the_card_on_done_and_a_finished_run() -> None:
    task = next(t for t in TASKS if t.id == "mock-drag-card")
    site = Site()
    assert task.check(_run("I moved the card."), site) is not None, "a run that moved nothing cannot pass"
    site.record_drop({"card": "Card A", "column": "To Do"})
    assert task.check(_run("I moved the card."), site) is not None, "the wrong column cannot pass"
    site.record_drop({"card": "Card A", "column": "Done"})
    assert task.check(_run("I moved the card."), site) is None
    assert task.check(_run(None, status=Status.NEEDS_CONFIRMATION), site) is not None
