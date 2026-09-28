"""Credential field selection and submission boundaries."""

import pytest

from fastbrowse.models import Operation
from fastbrowse.page import Control
from fastbrowse.safety import changes_credentials, sets_new_password


def field(label: str, *, sensitive: bool = True, form_id: str | None = "f") -> Control:
    return Control(
        id=label,
        frame_id=None,
        form_id=form_id,
        role="textbox",
        label=label,
        operations=frozenset({Operation.FILL}),
        input_type="password" if sensitive else "text",
        sensitive=sensitive,
    )


def submit(label: str, *, form_id: str | None = "f") -> Control:
    return Control(
        id=label,
        frame_id=None,
        form_id=form_id,
        role="button",
        label=label,
        operations=frozenset({Operation.CLICK}),
    )


@pytest.mark.parametrize(
    "label",
    [
        "New password",
        "Confirm password",
        "Repeat password",
        "Re-enter password",
        "Choose a password",
        "Create password",
    ],
)
def test_a_field_that_takes_a_new_secret_is_recognised(label: str) -> None:
    assert sets_new_password(field(label))


@pytest.mark.parametrize("label", ["Password", "Current password", "Existing password", "Old password"])
def test_a_field_that_takes_an_existing_secret_is_not(label: str) -> None:
    assert not sets_new_password(field(label))


def test_a_field_that_is_not_sensitive_is_never_a_new_secret() -> None:
    assert not sets_new_password(field("Nickname", sensitive=False))


def test_a_change_password_submit_is_a_credential_change() -> None:
    controls = [field("Current password"), field("New password"), submit("Change password")]
    assert changes_credentials(controls, submit("Change password"))


def test_a_submit_worded_like_a_password_change_is_a_credential_change() -> None:
    assert changes_credentials([submit("Reset password")], submit("Reset password"))


def test_an_order_submit_is_not_a_credential_change() -> None:
    controls = [field("Postcode", sensitive=False), submit("Place order")]
    assert not changes_credentials(controls, submit("Place order"))


def test_a_new_password_on_another_form_does_not_carry_over() -> None:
    controls = [field("New password", form_id="change"), submit("Place order", form_id="order")]
    assert not changes_credentials(controls, submit("Place order", form_id="order"))


def test_current_password_context_does_not_make_it_a_replacement() -> None:
    target = field("Current password").model_copy(update={"context": "Set new password"})
    assert not sets_new_password(target)


def test_clicking_a_password_field_does_not_commit_the_form() -> None:
    target = field("New password")
    assert not changes_credentials([target], target)
