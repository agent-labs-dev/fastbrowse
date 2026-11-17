# The extension package the Chrome Web Store approved

These are the files of the package the Store serves today, version 0.1.1, unpacked byte for byte from the archive
`scripts/extension_store.py build` produces for that upload (sha256
`ea95c4fd5a08bae451ebd8bdbc2b69eafa5228f00e47a0e1818289174f2947fc`, the digest every Extension Store workflow run since
the 0.1.1 submission recorded). The builder drops the manifest's `key`, so this manifest has none.

They are here as evidence, not as a build input. `tests/store_extension_compat.rs` reads them to assert what a new
user actually gets from the Store, and `extension-store/compat.json` records the gap between that package and what this
tree's runs need. When the Store approves a newer upload, replace this folder with that package's files and update
`compat.json` in the same commit.
