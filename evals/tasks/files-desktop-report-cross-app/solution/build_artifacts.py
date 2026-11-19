"""Builds the oracle's real LibreOffice artifacts and the mutants' from `sales.csv`, through LibreOffice's UNO.

Run it in an image with the same LibreOffice as the desktop box (Arch Linux, libreoffice-fresh) from a directory holding
`sales.csv`: `python3 build_artifacts.py OUT`. It writes `OUT/sales.ods`, `OUT/report.odt` and `OUT/report.pdf`, the
correct deliverables, and under `OUT/mutants/` one broken variant per mutation in `tests/mutations.yaml`. A report's chart
is the Calc chart copied and pasted into Writer, which is how an agent in the apps gets one.
"""

import sys
from pathlib import Path

import uno  # noqa: F401  (registers the com.sun.star modules)
from com.sun.star.awt import Rectangle
from com.sun.star.beans import PropertyValue
from com.sun.star.table import CellRangeAddress
from com.sun.star.text.ControlCharacter import PARAGRAPH_BREAK

sys.path.insert(0, str(Path(__file__).parent))
from lo import Office  # noqa: E402  (the UNO connection helper beside this file)

CSV = Path("sales.csv").resolve()
HEADING = "Regional Sales Report"
REVENUE, UNITS = 2, 1  # columns of sales.csv


def pv(name, value):
    return PropertyValue(Name=name, Value=value)


def url(path):
    return Path(path).resolve().as_uri()


def calc_with_chart(office, column, chart=True):
    """sales.csv in Calc with a bar chart of `column` by region, or no chart."""
    calc = office.load(url(CSV), pv("FilterName", "Text - txt - csv (StarCalc)"), pv("FilterOptions", "44,34,76,1,"))
    if chart:
        sheet = calc.Sheets.getByIndex(0)

        def span(col):
            return CellRangeAddress(Sheet=0, StartColumn=col, EndColumn=col, StartRow=0, EndRow=5)

        sheet.Charts.addNewByName("Chart", Rectangle(500, 8000, 14000, 8000), (span(0), span(column)), True, True)
    return calc


def save(doc, path, filter_name):
    Path(path).parent.mkdir(parents=True, exist_ok=True)
    doc.storeToURL(url(path), (pv("FilterName", filter_name),))


def report(office, calc, total, odt, pdf):
    """A report holding the heading and the total, and the chart of `calc` when it has one."""
    doc = office.load("private:factory/swriter")
    text = doc.Text
    cursor = text.createTextCursor()
    cursor.ParaStyleName = "Heading 1"
    text.insertString(cursor, HEADING, False)
    text.insertControlCharacter(cursor, PARAGRAPH_BREAK, False)
    cursor.ParaStyleName = "Text body"
    text.insertString(cursor, f"Total revenue: {total}", False)
    text.insertControlCharacter(cursor, PARAGRAPH_BREAK, False)
    page = calc.Sheets.getByIndex(0).DrawPage
    if page.Count:
        calc.CurrentController.select(page.getByIndex(0))
        office.dispatch(calc, ".uno:Copy")
        doc.CurrentController.getViewCursor().gotoEnd(False)
        office.dispatch(doc, ".uno:Paste")
    save(doc, odt, "writer8")
    save(doc, pdf, "writer_pdf_Export")
    doc.close(True)


def main(out):
    out = Path(out)
    office = Office()
    try:
        for column, total, chart, name in (
            (REVENUE, "75,025", True, ""),
            (REVENUE, "74,925", True, "wrong-total"),
            (REVENUE, "75,025", False, "no-chart"),
            (UNITS, "75,025", True, "wrong-column"),
            (REVENUE, "$75,025.00", True, "equivalent-format"),
        ):
            calc = calc_with_chart(office, column, chart)
            where = out / "mutants" / name if name else out
            if not name or name == "wrong-column":
                save(calc, where / "sales.ods", "calc8")
            report(office, calc, total, where / "report.odt", where / "report.pdf")
            calc.close(True)
        calc = calc_with_chart(office, REVENUE, chart=False)
        save(calc, out / "mutants" / "no-ods-chart" / "sales.ods", "calc8")
        calc.close(True)
    finally:
        office.close()


main(sys.argv[1])
