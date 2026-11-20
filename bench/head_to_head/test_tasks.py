"""The graders on answers shaped like the ones each arm writes: `pytest bench/head_to_head/test_tasks.py`."""

from tasks import TASKS, grade

BY_ID = {task.id: task for task in TASKS}
AUTHORS = BY_ID["stretch-quotes-top-authors"]


def test_citation_number_and_url_do_not_pass_as_the_value() -> None:
    answer = "Albert Einstein has 9 [10](<https://quotes.toscrape.com/page/10/#x>), J.K. Rowling 9, Marilyn Monroe 7."
    assert "10" in str(grade(AUTHORS, ended=True, answer=answer, final_url=None))


def test_a_value_belongs_to_the_name_before_it() -> None:
    answer = "J.K. Rowling 10, Albert Einstein 9, Marilyn Monroe 7"
    assert grade(AUTHORS, ended=True, answer=answer, final_url=None) is not None


def test_bold_and_citations_around_right_values_pass() -> None:
    answer = "**Albert Einstein**: 10 quotes [1](<https://a.example/1>); **J.K. Rowling**: 9; **Marilyn Monroe**: 7."
    assert grade(AUTHORS, ended=True, answer=answer, final_url=None) is None


def test_checkout_needs_the_final_page() -> None:
    task = BY_ID["saucedemo-checkout"]
    assert grade(task, ended=True, answer="$43.18", final_url="https://www.saucedemo.com/checkout-step-two.html")
    assert (
        grade(task, ended=True, answer="$43.18", final_url="https://www.saucedemo.com/checkout-complete.html") is None
    )


def test_a_right_answer_from_a_run_that_did_not_end_normally_fails() -> None:
    assert grade(BY_ID["dynamic-loading"], ended=False, answer="Hello World!", final_url=None) is not None
