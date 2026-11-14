"""A GTK order form that writes its whole state to a JSON file on every change, so a desktop task is graded exactly.

Run as a program by the desktop benchmark, never imported: `python3 order_form.py STATE APP_ID [--layout alt]`. It
needs the system's PyGObject and GTK 3. `APP_ID` names the window's class, so each run drives a window of its own.
"""

import json
import sys

import gi  # ty: ignore[unresolved-import]

gi.require_version("Gtk", "3.0")
from gi.repository import GLib, Gtk  # noqa: E402  # ty: ignore[unresolved-import]

COUNTRIES = ("United Kingdom", "Ireland", "France", "Germany")


def main(path: str, app_id: str, alt: bool) -> None:
    GLib.set_prgname(app_id)
    window = Gtk.Window(title="Order form")
    window.set_default_size(520, 640)
    grid = Gtk.Grid(row_spacing=8, column_spacing=12, margin=16)

    name = Gtk.Entry()
    email = Gtk.Entry()
    quantity = Gtk.SpinButton.new_with_range(1, 20, 1)
    gift = Gtk.CheckButton(label="Gift wrap")
    small = Gtk.RadioButton.new_with_label(None, "Small")
    medium = Gtk.RadioButton.new_with_label_from_widget(small, "Medium")
    large = Gtk.RadioButton.new_with_label_from_widget(small, "Large")
    country = Gtk.ComboBoxText()
    for option in COUNTRIES:
        country.append_text(option)
    country.set_active(0)
    notes = Gtk.Entry()
    submit = Gtk.Button(label="Place order")
    status = Gtk.Label(label="Not submitted")
    submitted: dict[str, object] = {"value": None}

    def state() -> dict[str, object]:
        size = next(button.get_label() for button in (small, medium, large) if button.get_active())
        return {
            "name": name.get_text(),
            "email": email.get_text(),
            "quantity": quantity.get_value_as_int(),
            "gift_wrap": gift.get_active(),
            "size": size,
            "country": country.get_active_text(),
            "notes": notes.get_text(),
            "submitted": submitted["value"],
        }

    def write(*_: object) -> None:
        with open(path, "w") as out:
            json.dump(state(), out)

    def place(_: object) -> None:
        submitted["value"] = {key: value for key, value in state().items() if key != "submitted"}
        status.set_text(f"Order placed for {name.get_text() or 'nobody'}")
        window.set_title("Order placed")
        write()

    rows = [
        ("Name", name),
        ("Email", email),
        ("Quantity", quantity),
        ("", gift),
        ("Size", small),
        ("", medium),
        ("", large),
        ("Country", country),
        ("Notes", notes),
    ]
    if alt:
        # The same form with its fields in another order and its labels and button reworded.
        rows = [
            ("Email address", email),
            ("Full name", name),
            ("Country", country),
            ("Quantity", quantity),
            ("Size", small),
            ("", medium),
            ("", large),
            ("", gift),
            ("Notes", notes),
        ]
        submit.set_label("Submit order")
    for row, (label, widget) in enumerate(rows):
        if label:
            text = Gtk.Label(label=label, xalign=1)
            text.set_mnemonic_widget(widget)
            grid.attach(text, 0, row, 1, 1)
            if not isinstance(widget, Gtk.ToggleButton):
                # A field is named by its label, as a native form is to a screen reader; a radio keeps its own name.
                widget.get_accessible().set_name(label)
        grid.attach(widget, 1, row, 1, 1)
    grid.attach(submit, 1, len(rows), 1, 1)
    grid.attach(status, 0, len(rows) + 1, 2, 1)

    for entry in (name, email, notes):
        entry.connect("changed", write)
    quantity.connect("value-changed", write)
    gift.connect("toggled", write)
    for button in (small, medium, large):
        button.connect("toggled", write)
    country.connect("changed", write)
    submit.connect("clicked", place)

    window.add(grid)
    window.connect("destroy", Gtk.main_quit)
    window.show_all()
    write()
    Gtk.main()


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2], "--layout" in sys.argv and sys.argv[sys.argv.index("--layout") + 1] == "alt")
