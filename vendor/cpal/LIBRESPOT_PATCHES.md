# CPAL 0.18.2 ALSA recovery hardening

This directory starts from the complete published `cpal-0.18.2.crate`, downloaded
from <https://static.crates.io/crates/cpal/cpal-0.18.2.crate>.

- Crate SHA-256: `6f02e8d0327b42d3e2e4ab2119af397344eb9fc54a34bf0ddeaa1277af8681f1`
- Upstream source commit: `e1612d5d98152f8dc2a62e1b51ef7cbf4f7f26b7`
- Upstream repository: <https://github.com/RustAudio/cpal>
- License: Apache-2.0, retained in `LICENSE`.
- Published standalone `Cargo.lock` SHA-256: `fd326c05edf7a1117e1ca18f167737c72abbe75b0f2169946ca042ec3aa3a47e`

Upstream manifests, public API, other backends, the standalone lockfile and
`rustfmt.toml` are unchanged. Runtime changes are confined to
`src/host/alsa/mod.rs` and the new `src/host/alsa/recovery.rs`. The `.gitignore`
override keeps the published lockfile visible to Git for standalone `--locked`
tests. This file records our changes; `.cargo_vcs_info.json` identifies the
original upstream source, not the modified tree. Two trailing spaces in upstream
`SECURITY.md` are removed for whitespace checks.

## Relationship to the dependency upgrade

The original recovery PR used CPAL 0.16 with Rodio 0.21. This port builds on
librespot's coordinated upgrade to Rodio 0.23 and CPAL 0.18.2. Both direct and
Rodio dependencies resolve to this one patched CPAL 0.18.2.

CPAL 0.18.2 already contains creation-relative timestamps, ALSA state-based
POLLERR recovery, a retained shutdown-pipe reader, rounded poll timeouts,
non-probing device enumeration, a start latch and equilibrium-prefilled period
buffers. Those implementations are retained. The obsolete CPAL 0.16 marker
warning suppression is no longer needed.

The remaining recovery guarantees are adapted to the current implementation:

1. Hardware timestamps still use the system or link-synchronized clock when
   available. Invalid or backwards values permanently switch to a continuous
   `Instant` fallback. Callbacks and `Stream::now()` share the same clock state;
   changing origins cannot make either timeline move backwards. Timestamp and
   delay metadata cannot prevent the data callback.
2. ALSA state is checked before blocking, on timeout and after readiness, so a
   suspended PCM does not depend on a fresh poll event to retry resume. POLLERR
   reaches ALSA availability/state classification. Unexplained POLLERR and
   invalid availability become terminal errors instead of ready-loop spam.
3. Transient errors, zero transfers, xruns and suspend recovery use a 10 ms
   backoff and at most 100 recovery attempts without a positive frame transfer.
   A successful prepare/resume alone does not reset that budget. Capture is
   restarted after prepare; upstream's can-resume and already-running handling
   is preserved.
4. Successful internal recovery is silent. A terminal failure notifies the
   owner once and stops the worker. Budget exhaustion reports
   `ErrorKind::StreamInvalidated`. Any terminal path whose underlying error
   would look like a nonfatal `Xrun`, `RealtimeDenied` or `DeviceChanged` is
   normalized to `StreamInvalidated`, including failed prepare/start. The
   startup real-time-promotion notification remains nonfatal.
5. A partial output write keeps its unwritten suffix across subsequent poll
   cycles and EAGAIN without replaying the accepted prefix. Each cycle makes
   at most one write, avoiding an uninterruptible zero-write retry loop. An
   xrun discards its interrupted period because ALSA resets its buffer. Capture
   likewise reads once per cycle and delivers exactly the frames it received.

The root `[patch.crates-io]` applies to this checkout and Git installations.
Consumers embedding librespot must apply an equivalent patch at their own
Cargo workspace root. Publishing librespot crates does not automatically
patch their consumers' CPAL dependencies.

## Verification

From the repository root with ALSA development headers installed:

```sh
cargo test --manifest-path vendor/cpal/Cargo.toml --locked --lib
cargo test --manifest-path vendor/cpal/Cargo.toml --locked --lib -- --include-ignored
cargo fmt --manifest-path vendor/cpal/Cargo.toml -- --check
cargo clippy --manifest-path vendor/cpal/Cargo.toml --locked --lib --tests -- -D warnings
cargo tree --locked -i cpal
cargo build --locked
```

The vendor suite passes on Rust 1.87 and Rust 1.100, including strict Clippy
without lint exemptions. Its 30 tests cover upstream configuration/timestamp
behavior, stale or invalid hardware timestamps, continuity, POLLERR
classification, recovery bounds, short writes/EAGAIN, zero writes, fatal error
classification and the shutdown pipe. Four opt-in ALSA null PCM tests also
exercise the start latch and repeated playback/drop/reopen, one terminal
notification followed by safe drop, capture restart, and recovery exhaustion
despite repeated successful prepare calls with no frame transfers.

Linux CI runs those null tests without physical hardware. The standalone lock
file is retained so the same suite can run from a fresh Git checkout. Null PCM
testing does not reproduce the AES67/PTP-driven hardware pointer: validate
normal playback, rapid stop/start, underruns and PTP loss/restoration on the
target installation.

## Removal

Remove the vendor directory and root patch when a released CPAL version shared
by Rodio and librespot supplies equivalent bounded recovery, pending-write and
clock-fallback guarantees. Regenerate the lockfile and use `cargo tree -i cpal`
to confirm there is no second unpatched version, then repeat target-device
recovery tests. Upgrading to 0.18.2 alone does not supply all of these guarantees.
