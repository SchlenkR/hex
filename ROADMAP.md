# Roadmap

HEX ships the macOS product loop: configurable hold-to-talk local dictation,
optional context-aware OpenCode post-processing, a Metal HUD, bounded paste workers, settings, and a signed GPUI
app bundle. Streaming commands are a disabled-by-default experimental opt-in.
An x86_64 Arch/i3 X11 beta now provides hotkey dictation, automatic paste, a
GPUI shell, and signed user-local updates; it does not yet include commands.
Native wlroots-compatible Wayland and Nix packaging are implemented
and awaiting supported-host validation before release.
The authenticated local transcription service now implements discovery,
direct-child embedding, model preparation, and bounded host-audio transcription;
`@kitlangton/hex@0.3.0` publishes ready transcribers through Promise and Effect
entrypoints. Local Electron capture and native transcription have been exercised;
signed helper packaging and a signed first-consumer integration remain. Follow the
[embedded desktop delivery plan](docs/plans/typescript-sdk.md); Apple Silicon
macOS is the proposed first target, not a cross-platform availability promise.

## Validate The macOS Release

Runtime logs now live in Application Support, first-run setup installs the
selected dictation model, and the release flow signs, notarizes,
staples, Gatekeeper-assesses, and publishes Sparkle updates through R2.

The quarantined public DMG and a genuine automatic 2.0.0-to-2.0.1 Sparkle update
have been validated on the development Mac. Before broadly sharing it, validate
the DMG from a clean macOS account, including every TCC permission, model
installation, and source-free launch. Preserve the stable bundle identity that
owns Microphone, Accessibility, Input Monitoring, and Automation permissions.

## Finish Settings And First-Run Health

Microphone selection, idle-safe live switching, unavailable-device fallback,
sound-effect volume, application/browser-host processing modes, and the native
`SMAppService` login-item toggle now ship. Runtime settings changes project live
and persist atomically; launch-at-login remains owned by macOS rather than the
settings file. A persistent menu-bar item keeps Settings, Paste Last, updates,
and Quit reachable when the Dock icon is hidden. Settings now surfaces revoked
Microphone, Accessibility, or Input Monitoring access after onboarding, and
supported transcription runtimes expose automatic language detection. Modes own
corrections, optional OpenCode rewriting, and ordered text transformations;
stage failures preserve the preceding pipeline output. Do not add a generalized
processor framework without a concrete need beyond this shipped sequence.

Keep Swift and Rust separate. Prefer the old app's existing Sparkle update window
with a website-only informational item to install the new app manually, with no
data import or automatic replacement.
Follow [`docs/plans/swift-app-handoff.md`](docs/plans/swift-app-handoff.md).
The new app uses identity `com.kitlangton.hex2`, display name `Hex`, and filename
`Hex.app`, while retaining the Rust `voice-control` data root. Use Kit's approved
Developer ID signing team and validate the new identity before release. The legacy
S3 feed must never install a Rust payload. Publish
the updated Rust artifact first, then the separately validated legacy feed notice.
No Swift rebuild should be necessary for the notice.

## Validate The Linux Beta

Run a genuine signed update between two published versions on the target
Arch/i3 machine. Verify exact artifact validation, atomic activation, restart,
and retained-version rollback. Smoke-test installation, model download, the CLI
microphone override, hotkey rebinding, locked capture, cancellation, automatic
paste, service autostart, client reconnect, and Settings closing without stopping
accepted work. Linux no longer uses a tray.

Keep app-managed updates limited to the user-local direct-install layout. A
future Arch package must leave updates to the package manager. Nix owns its
packaged installation and optional Home Manager service. Validate the Wayland
beta on a compatible compositor with physical keyboard reconnect, microphone,
foreground paste, click-through HUD, and independent service/client shutdown. See
[`docs/plans/linux.md`](docs/plans/linux.md). Add commands, context, or
broader Wayland support only as explicit later capability slices.

Converge the separate macOS and Linux GPUI roots on one capability-driven
product shell without merging their lifecycle or platform behavior. Follow
[`docs/plans/shared-desktop-ui.md`](docs/plans/shared-desktop-ui.md); preserve
both settings formats and delete the duplicate Linux render tree only after the
shared Settings and Activity slices work on both hosts.

## Harden The Real-Time Boundary

Append-only observation serialization and flushing now run on a bounded writer
instead of the semantic coordinator. Replaceable partial transcript and context
observations may be dropped under pressure; completed transcripts, state
transitions, command outcomes, and failures retain bounded backpressure.

The authoritative dictation timeline now drains timestamped CoreAudio on its own
thread; Moonshine receives a duration-bounded, generation-safe projection that
may reset without affecting active capture. Physically smoke-test delayed
shortcut boundaries, custom modifier and key chords, lock behavior,
cancellation, foreground insertion, and both reserved paste shortcuts.

The first installed timestamped-timeline build regressed physical dictation:
shortcut input arrived, but long captures discarded and one accepted job reached
inference with zero milliseconds of audio. At the HID tap, physical timestamps
arrived as raw Mach ticks; on the development Mac's `125/3` timebase, treating
them as nanoseconds made a three-second hold appear to last roughly 72 ms.
The 2.1.7 move to annotated-session taps retained that conversion even though
timestamps at this boundary are already nanoseconds. This stretched short taps
by 41.67 times, breaking double-tap lock and admitting accidental short captures.
A simultaneous passive HID/annotated probe confirmed the different units for
real modifier events. Annotated-session timestamps now enter the audio timeline
without scaling; callback-level regressions cover lock, short-tap discard, and
exact delayed release trimming. Keep physical shortcuts, assistive modifier and
key chords, non-empty captures, and immediate HUD onset in the release smoke
checks; changing tap location requires revalidating the clock as well as routing.

Version 2.1.11 also recovers blocked modifier gestures after missed releases and
navigation-key metadata. Recovery requires a neutral keyboard and emits no
capture actions. Keep the callback regressions for delayed releases and Fn
evidence alongside physical Option-hold checks; a physical Fn/Globe smoke test
and general cross-tap delivery-order validation remain outstanding.

After these reliability fixes, evaluate an explicit press-once-to-start,
press-again-to-finish option, especially for key chords and standalone function
keys. Keep it distinct from hold-to-dictate and double-tap-only activation;
do not add new shortcut modes to the regression release.

Track the remaining shortcut requests explicitly:

- [#82](https://github.com/anomalyco/hex/issues/82): menu-bar Pause/Resume must
  remain deferred for now; quitting HEX is sufficient for the current scope.
  If revisited, it must
  suspend capture shortcuts and command activation, cancel only active capture,
  retain accepted output, and require key release before resuming. Test event-tap
  suppression as well as the recognition state. App exclusions are a later slice.
- [#77](https://github.com/anomalyco/hex/issues/77): single-tap hands-free
  activation needs its own hold/lock, chord-discard, and cold-microphone checks.
- [#67](https://github.com/anomalyco/hex/issues/67): standalone keypad/character
  bindings need explicit key-consumption, cancellation, and rebinding semantics.
- [#37](https://github.com/anomalyco/hex/issues/37): remaining Caps Lock-to-Control
  behavior needs physical remapping evidence. Side-less flags and actual Right Control flags are distinct.

On-demand rewrite [PR #63](https://github.com/anomalyco/hex/pull/63) remains under
review: publish the shared last transcript only after successful paste, require
explicit shortcut opt-in, prove generation while automatic processing is off at
the generation boundary, and reuse the isolated native keyboard test harness.
Do not pre-publish an uncommitted transcript and attempt to repair it afterward.

The microphone and dictation model remain warm by default. The shipped
`Release when idle` microphone option opens the device on shortcut press, removes
pre-roll, and adds first-capture latency. It is mutually exclusive with Commands
through confirmed settings transitions. `Do nothing` is the default
recording-audio behavior. Idle-sleep prevention is automatic and scoped to
intentional dictation rather than persisted as a user setting.

Add end-of-speech detection only if it preserves explicit Send, Cancel, and
locked-capture controls.

## Make Diagnostics Incremental

Ratatui and GPUI now share a bounded incremental event reader with session and
partial-write handling, and observation writes run on a bounded background
writer. Add heartbeat or process-liveness evidence so a crash cannot leave an
old `Listening` event looking live.

Add an inspectable audit that separates capture failures, recognition errors,
sleeping or discarded utterances, context mismatches, resolution misses, queue
pressure, and action failures. Recommendations may propose command-taxonomy
changes but must not mutate compiled configuration automatically.

Keep installed-application discovery lazy. Opening Settings must not recursively
scan application bundles or rasterize their icons; start that work only after
the user opens `Add application`, and verify ordinary startup requests no
Downloads, Calendar, or other unrelated TCC access.

## Deliberate Deferrals

Keep protected lifecycle commands and typed captures in compiled Rust. Ordinary
literal commands and dictation control phrases already use the explicit
TypeScript user config. Do not generalize that config into a plugin framework.

Do not generalize the concrete macOS and Linux X11 adapters into a platform
framework. Add seams only when a second implemented adapter requires one.
Follow the remaining capability contracts and exit criteria in
[`docs/plans/linux.md`](docs/plans/linux.md).
