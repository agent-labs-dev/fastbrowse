"""The viewport a graded page is read at."""

# The viewport jev-ultrafast's pinned browser.py sets in its constructor (width, height, scale, not mobile).
# Fastbrowse's navigation runs get the same one, since a cloud browser's default is wider and pages reflow.
VIEWPORT = {"width": 1120, "height": 780, "deviceScaleFactor": 1, "mobile": False}
