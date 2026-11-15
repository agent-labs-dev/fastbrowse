# The extension package the Chrome Web Store approved

These are the files of the page the Store serves today, copied byte for byte out of the extension that fastbrowse
0.10.0 writes with `fastbrowse extension path` (PyPI `fastbrowse==0.10.0`, self-reported `fastbrowse 0.10.0`). That
package is the approved 0.1.0 upload: manifest `version` 0.1.0, a 35-command `allowed-methods.json`, and a `hello`
that returns its identity without a `methods` inventory.

They are here as evidence, not as a build input. `tests/store_extension_compat.rs` reads them to assert what a new
user actually gets from the Store, and `extension-store/compat.json` records the gap between that package and what this
tree's runs need. When the Store approves a newer upload, replace this folder with that package's files and update
`compat.json` in the same commit.
