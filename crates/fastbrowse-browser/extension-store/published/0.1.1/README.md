# The extension package the Chrome Web Store approved

These are the files of the package the Store serves today, version 0.1.1, copied byte for byte from
`crates/fastbrowse-browser/extension/`, which the Extension Store workflow zipped for that upload. The folder has not
changed since, and its manifest `version` is 0.1.1.

They are here as evidence, not as a build input. `tests/store_extension_compat.rs` reads them to assert what a new
user actually gets from the Store, and `extension-store/compat.json` records the gap between that package and what this
tree's runs need. When the Store approves a newer upload, replace this folder with that package's files and update
`compat.json` in the same commit.
