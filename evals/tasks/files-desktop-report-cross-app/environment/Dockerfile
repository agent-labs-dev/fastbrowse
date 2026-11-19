# Trusted-verify image: the checks read the saved ODF files with Python's standard library and the PDF with
# pdftotext, so nothing here needs LibreOffice. The oracle's files were built in the desktop image's LibreOffice
# (solution/build_artifacts.py), so what the checks read is what a trial saves.
FROM python:3.13-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends poppler-utils \
 && rm -rf /var/lib/apt/lists/*
ENV PYTHONDONTWRITEBYTECODE=1
WORKDIR /workspace
