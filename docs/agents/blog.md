# Blog posts

`blog/` holds the posts the site publishes at https://www.fastbrowse.ai/blog/, one Markdown file each with YAML
frontmatter: `title`, `description`, `topic`, `published`, `updated`, `verifiedAgainst` and `verifiedOn`. The site
fetches this directory at the commit it builds from, so a post ships by merging here, and `verifiedAgainst` is the
released version it was checked against, which is the version the post shows a reader.

A post that names a flag, a status, a field or a symbol is a claim about this tree, so each one is bound in
`drift.lock` to the files that carry its claims and `drift check` runs on every pull request. When it fails, read
the post against the change, fix the words, then re-stamp the post and bump its dates in the same commit:

```sh
drift link blog/<slug>.md --doc-is-still-accurate   # after the words are correct again
```
