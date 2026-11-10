# AGENTS.md

## Commit messages

- Use [Conventional Commits](https://www.conventionalcommits.org/): `<type>: <subject>`
  - Types: `feat`, `fix`, `refactor`, `style`, `docs`, `test`, `chore`
- Write a single subject line only. No body.
- Write the subject in English, lowercase, imperative mood, without a trailing period.
- For a release commit, use `chore(release): prepare for vX.Y.Z`. git-cliff leaves this commit out of the changelog.

Examples:

```
feat: add select tool
fix: keep the selection after rotating
style: format with rustfmt
chore(release): prepare for v0.2.0
```
