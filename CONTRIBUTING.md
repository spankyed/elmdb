# Contributing to elmdb

## The contributor license agreement

Before a pull request from you can be merged, you sign [elmdb's Contributor License Agreement](CLA.md) once. When you
open your first pull request, a bot comments asking you to; you sign by posting this comment in the pull request:

```text
I have read the CLA Document and I hereby sign the CLA
```

The pull request's `CLAAssistant` check then passes, for this pull request and every later one. Your signature is
recorded with your GitHub account in `signatures/version1/cla.json` on the repository's `cla-signatures` branch.

The agreement grants licenses and transfers nothing: you keep the copyright in what you contribute. It gives the
maintainer the right to license your contribution under other terms as well, and promises that it stays available
under the license elmdb has when it is accepted, MIT OR Apache-2.0. Read [CLA.md](CLA.md) for the terms.

If someone else, such as your employer, has rights in what you write, make sure you may contribute it before signing:
the agreement asks you to say so.

A company whose employees contribute can sign [elmdb's Corporate Contributor License Agreement](CLA-CORPORATE.md)
once instead. Someone authorized to sign for it opens an issue titled "Corporate CLA: <company>", and signing is
arranged privately from there. The GitHub accounts it lists are then recorded in `.github/cla-corporate.json`, and
their pull requests pass the check without an individual signature.

## Making a change

Read [`docs/architecture/design.md`](docs/architecture/design.md) first, and the document that owns the part you are
changing; [`docs/README.md`](docs/README.md) says what each holds. [CLAUDE.md](CLAUDE.md) lists the checks a change
must pass and the repository's rules: its vocabulary, how errors and comments are written, and that a new check is
shown failing once. Before you push, run them in one command:

```bash
cargo xtask pre-push
```

Commit messages are [conventional](https://www.conventionalcommits.org) (`fix(engine): …`), one logical change each.
Open a pull request as a draft while you iterate, and mark it ready for review when it's done: that starts the full
tier of CI, which the merge waits on with the rest.
