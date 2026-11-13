# Spork

**Neither a spoon nor a fork: just a better Git client.**

Spork is a free, open-source Git client for **macOS, Windows and Linux**. It's built for people who love what
polished desktop clients like Fork and GitKraken offer (a clear commit graph, painless staging, safe history
editing) but want it open, cross-platform, and owned by its community instead of a license key.

> 🚧 **Spork is in early development.** It isn't ready for daily use yet. Today it opens a repository, shows its
> branches, remotes and tags, draws the full commit graph with commit details and diffs, refreshes itself, and lets
> you stage files and commit. Everything else below is being built in the open, step by step. Star or watch the repo to follow along, and see [Status & roadmap](#status--roadmap).

## What Spork is

- **An open-source alternative to Fork and GitKraken.** The same kind of fast, visual, keyboard-friendly Git
  workflow, without a subscription or a paywall.
- **Open source, always.** Spork's code is public and will stay public, for everyone: individuals, teams and
  companies. No paid tier, no feature locked behind an account, no telemetry.
- **Cross-platform for real.** One Rust codebase, native builds for macOS, Windows and Linux, with the same
  features on every OS.
- **Independent.** Spork is written from scratch. It doesn't contain code from any other Git client and is not
  affiliated with Fork, GitKraken, or any other product.

## Why another Git client?

Today the nicest Git GUIs are closed source, paid, and often missing one of the three major platforms.
The open-source ones tend to be slower or harder to use. Spork aims to close that gap: commercial-grade UX
in an open project that anyone can use, audit, fix and extend.

## Features (the plan)

Each item has a detailed spec in [`docs/specs/`](docs/specs) and is built in the order of the roadmap.

| Area | What you'll get |
|---|---|
| History | Fast commit graph for very large repos, search by message/author/content, file history, blame, reflog view |
| Working copy | Stage files, hunks or **single lines**, discard safely, amend, commit templates, signing |
| Branching | Create / checkout / rename / delete / reset, drag-and-drop merge & rebase, ahead/behind counters |
| History editing | Merge, rebase, **interactive rebase** (reorder, squash, fixup, reword, drop), cherry-pick, revert, with undo |
| Conflicts | Built-in **3-way merge** editor, or your favourite external tool |
| Remotes | Fetch / pull / push with progress, force-with-lease by default, multiple remotes |
| More git | Stashes & snapshots, tags, submodules, worktrees, git-flow, Git LFS, bisect, patches |
| Hosting | GitHub, GitLab, Bitbucket and Gitea accounts, clone from your repo list, pull requests |
| Power tools | Custom commands, bug-tracker links, repository statistics, quick launch, workspaces |
| **AI (optional)** | Commit messages, code review, PR descriptions, conflict suggestions, using **your own provider** (Claude, any OpenAI-compatible API, or a local Ollama model). Off by default, you can see exactly what is sent, and it never runs git on its own. |
| Look & feel | Light and dark themes that follow your system (JetBrains-inspired palette), keyboard shortcuts for everything |

## The stack

| Layer | Technology | Why |
|---|---|---|
| Language | [Rust](https://www.rust-lang.org) | fast, memory-safe, one codebase for every OS |
| UI | [GPUI](https://www.gpui.rs) via [GPUI Kit](https://gpui-kit.com) | GPU-rendered at 120 fps, from the team behind the Zed editor, with 75+ components (virtual lists, trees, diff and code editor, docking, charts) |
| Git (reads) | [gitoxide](https://github.com/GitoxideLabs/gitoxide) (planned) | in-process reads for speed on huge repositories |
| Git (writes & network) | the real `git` CLI | your hooks, credential helpers, SSH keys, signing and LFS work exactly as in your terminal |
| Platform | OS keychain, native notifications, file watching | credentials never stored in plain text |

Design rule: the UI never runs git directly. Every action goes through one tested operations layer, which keeps
behaviour predictable and testable without a window. Details: [docs/00-overview.md](docs/00-overview.md).

## Status & roadmap

| Phase | Scope | State |
|---|---|---|
| P0 Foundations | git runner, error handling, repo view with branches/remotes/tags, themes, auto-refresh, CI on 3 OSes | ✓ done |
| **P1 Read-only client** | commit graph ✓, commit details ✓, diffs ✓, blame, file history, search | **in progress** |
| **P2 Daily driver** | staging ✓ (line-level next), commit ✓, amend ✓, fetch/pull/push ✓, branches, credentials | **in progress** |
| P3 History editing | merge, rebase, interactive rebase, conflict resolver, cherry-pick, stash, tags | |
| P4 Breadth | clone, submodules, worktrees, git-flow, LFS, image diffs, workspaces | |
| P5 Integrations | hosting accounts & pull requests, custom commands, statistics | |
| P6 AI | the optional AI features above | |
| P7 1.0 | installers for every OS, auto-update, docs site | |

## Build from source

Requirements: Rust (stable) and git ≥ 2.38.
- **macOS:** Xcode (for Metal).
- **Linux:** the [GPUI system libraries](https://github.com/zed-industries/zed/blob/main/docs/src/development/linux.md).
- **Windows:** the MSVC build tools.

Open a repository with Spork:

```bash
cargo run -p spork -- /path/to/a/repo
```

Run the tests:

```bash
cargo test --workspace
```

## Project layout

```
crates/spork-git   git runner, error classification, refs and commit log (tested against real git)
crates/spork-graph commit-graph lane layout (pure, no I/O)
bins/spork         the desktop app (GPUI Kit), themes in bins/spork/assets/themes
docs/              architecture overview, 16 feature specs, AI and open-source policy
scripts/           helpers for writing behaviour specs
```

## Contributing

Spork is built in the open and contributions of every size are welcome: code, specs, testing on your OS, design,
docs, or simply opening an issue with what you need from a Git client.

1. Pick a module in [`docs/specs/`](docs/specs) (each lists its open questions and tests).
2. Open an issue or draft PR to say what you're working on.
3. Follow [CONTRIBUTING.md](CONTRIBUTING.md): Conventional Commit messages, one branch per change, and a DCO sign-off (`git commit -s`).

**Clean-room rule:** Spork is an independent implementation. Don't submit code, pseudo-code or text derived
from decompiling or disassembling proprietary software. Specs describe observable behaviour only, and code is
written from the specs. This keeps Spork safe to use and to build on for everyone.
More in [docs/20-open-source.md](docs/20-open-source.md).

## License

Spork is free software, licensed under the **[GNU General Public License v3.0 or later](LICENSE)**.
Anyone can use, study, change and share it, and every distributed version, including modified ones, must stay
open source under the same terms. That's how Spork stays open for everyone, for good.

*Spork is not affiliated with or endorsed by Fork, GitKraken, or any other Git client. Product names belong to their owners.*
