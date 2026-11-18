# Testing

- Write the failing test first. A bug fix keeps a test that fails without the fix.
- Tests drive real code paths with only the network faked. Mock upstreams use loopback listeners and shapes copied from recorded or documented responses.
- Tests never touch the owner's home directory. Every test binary includes `tests/support/hermetic.rs`, which points HOME, the XDG and platform directories, `DUOLANE_CONFIG_DIR`, `DUOLANE_STATE_DIR`, and `CODEX_HOME` at a per-process temporary directory before `main` runs. A new test binary must include it.
- A process test sets the environment of its child explicitly.
- Override `HOME` only for the test process, never for Git commands, because Git reads the commit identity and signing key from it.
- Name a test after the behavior it proves.
- Security fixes include the hostile case and the legitimate case that must keep working.
- Live checks against real Claude or ChatGPT accounts run only with the owner's approval for that run. Mocks do not prove upstream behavior, so state which claims stay unverified.
