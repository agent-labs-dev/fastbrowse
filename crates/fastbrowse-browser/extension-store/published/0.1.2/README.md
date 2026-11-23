# The extension package the Chrome Web Store approved

These are the files of the package the Store serves today, version 0.1.2, unpacked byte for byte from the archive
`scripts/extension_store.py build` produces for that upload (sha256
`3ece3ba43984873fd7c4b63bf36e5c6a7b441ba59cad8dc878755b09672d878e`, the digest recorded by the
[0.1.2 submission](https://github.com/agent-labs-dev/fastbrowse/actions/runs/45055859775)). The builder drops the manifest's `key`, so this manifest has none.

They are here as evidence, not as a build input. `tests/store_extension_compat.rs` reads them to assert what a new
user actually gets from the Store, and `extension-store/compat.json` records the gap between that package and what this
tree's runs need. When the Store approves a newer upload, replace this folder with that package's files and update
`compat.json` in the same commit.
