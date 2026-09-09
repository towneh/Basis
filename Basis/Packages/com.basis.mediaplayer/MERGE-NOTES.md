# Merging `developer` into this branch

Working notes for whoever brings `developer` into `feat/media-engine-port`. This branch
replaces the C media player engine with a Rust one, so the merge has one governing rule:
**keep the Rust side, take none of the C engine, and don't silently drop a feature developer
added that the Rust side has not caught up on.** Delete this file once the port lands on
`developer`.

## Merge, not rebase

A rebase stalls on the first port commit, the C-to-Rust engine swap, because it deletes files
`developer` has since edited, and then it would replay every port commit on top of that. A
merge is one pass. Measured against `developer` at the time of writing: the merge conflicts in
about ten files; the shims and `BasisTrustedUrls.cs` auto-merge because the port already
matches `developer` there.

## How to resolve each kind of conflict

- **Media player `.cs` files** (`BasisMediaPlayer`, `BasisMediaPlayerNetworking`,
  `BasisMediaPlayerPanelProvider`, `BasisMediaPlayerAudioTap`, `BasisVideoOutputMath`,
  `BasisSidecarSubtitleEngine`, `BasisVideoFrameBufferPool`): keep the port's Rust version.
- **Watch for unmarked C code.** Git auto-merges some of `developer`'s C-player methods in
  without conflict markers, beside the Rust ones: `ResyncLocal`, `ResyncEveryone`,
  `ReloadInPlace`, and calls to `LoadApprovedUrl`, `Reload` and `ActiveMediaSource`. Delete
  those; they call an API the Rust player does not have. **Compile after the merge** — the
  duplicate-method and missing-symbol errors are how you find the ones you missed.
- **Text and data** (`en.json`, `README.md`, `TESTING.md`, `BasisDefaultTrustedUrls.asset`):
  take both sides. `developer`'s other localisation keys, its VRCDN and RTSP trusted-URL
  entries and its connection-service changes are not C-player code and should come across.

## What `developer` added to the media player, and where the Rust side stands

`developer`'s whole new media-player runtime API since the fork is three methods:

| Developer method | Rust status |
| --- | --- |
| `ResyncEveryone()` | Already on the Rust side. Keep the Rust version, drop developer's. |
| `AddDomain(uri)` trusted-URL scoping | Already on the Rust side, identical. Auto-merges. |
| `LoadApprovedUrl(url)` | Not needed. On the Rust side, URL approval lives in the shims, so `OpenUserUrl` is already the approved-load entry. |

## The one deferred feature

`developer`'s **`ResyncLocal` asks the room for the current state first** and only reloads blind
if no one answers within a timeout (`forcedResyncPending`, `resyncAnswerDeadline`, a
`TickPendingResync` fallback). The Rust side has a simpler local resync that re-opens blind.
This is the only media-player feature intent not yet on the Rust side. It is deferred, not
dropped: to close it, add the request-then-timeout path to the port's existing local resync
button, sending a state request on press and falling back to the current blind re-open from a
tick if nobody answers. It needs the two-client rig to verify.
