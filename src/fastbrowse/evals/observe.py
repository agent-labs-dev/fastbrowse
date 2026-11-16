"""The viewport a graded page is read at."""

# Every arm that can be given a viewport runs navigation tasks at this one (width, height, scale, not mobile): a cloud
# browser's default is wider and pages reflow, so a pass at another size would not be the same navigation task.
VIEWPORT = {"width": 1120, "height": 780, "deviceScaleFactor": 1, "mobile": False}
