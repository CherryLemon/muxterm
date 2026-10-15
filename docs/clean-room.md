# Clean-room policy

This project reimplements a *compatible* tmux integration. iTerm2 is GPLv2 and its
implementation is not a source for us. This document is the rule book the
repository is developed under; it is deliberately boring and specific.

## Definition used here

**Clean room** means: no copying or translating another implementation's code, no
reuse of its resources (icons, images, nib files), and no design documents derived
from its internal class structure. Compatibility is built from *public protocols*,
*independent specifications* and *black-box observation* of documented behaviour.

"Written in Rust" means our client and its components are Rust. It does **not**
mean reimplementing tmux, OpenSSH or operating-system interfaces — those are used
as they are.

## Material classification

**Allowed in the implementation:**
* the `tmux(1)` manual page, the public tmux wiki (including the *Control Mode*
  page), tmux's public change log, and terminal control-sequence specifications;
* requirements written by us (this repository's `docs/spec/`);
* reproducible black-box captures recorded by `tools/probe/` (see
  `docs/evidence/`);
* third-party dependencies whose licence has been reviewed
  (`docs/licences.md`).

**Not allowed:**
* iTerm2 source code, or a translation of it;
* design documents reverse-engineered from its internal class structure;
* its test code, icons or other resources;
* feeding any of the above to an AI assistant and committing the result as
  independent work.

**Allowed with an explicit note:** reading *public product documentation* (user
documentation, release notes) to understand expected behaviour. Behaviour learned
that way must be expressed as our own specification text and covered by our own
test — the documentation itself is not copied.

## Evidence we keep

| Evidence | Where |
|---|---|
| Source and version of every compatible behaviour | `docs/spec/*.md`, "Provenance" sections |
| Recorded black-box captures | `docs/evidence/`, `crates/tmux-protocol/tests/fixtures/` |
| The exact command that produced a capture | `tools/probe/probe_control_mode.py` + `manifest.json` next to the capture |
| Independent test records | `cargo test` output in CI, the PoC scenario reports |
| Dependency licences | `docs/licences.md` |
| Contributor provenance statement | `CONTRIBUTING.md` |
| Design decisions | `docs/adr/` |

A behaviour that is not in a specification and not covered by a capture or a test
does not exist as far as this project is concerned.

## Honesty rules

* If a team member has seen the reference implementation, say so in the relevant
  ADR. Do not dress up ordinary independent development as a strict two-team
  isolation that never happened.
* Claims in `docs/spec/compatibility.md` must name the version and the test that
  supports them. "Works with tmux" is not a claim we make.
* When a behaviour is unverified, the code must *degrade explicitly* (report,
  refuse, or mark the state as untrusted) rather than guess.