# ALSA and AES67 playback troubleshooting

This guide covers librespot feeding an ALSA PCM provided by the
[AES67 Linux daemon](https://github.com/bondagit/aes67-linux-daemon) and its
RAVENNA kernel driver. Keep the configured audio endpoint, channel mapping and
network sample rate consistent when comparing playback backends.

## Identify the playback path

On Linux, the `rodio` backend normally uses Rodio, then CPAL, then ALSA. The
separate `alsa` backend uses librespot's native ALSA implementation. Seeing
`cpal host: Alsa` in a Rodio log does not mean the native backend is selected.

List the backends compiled into the binary:

```bash
librespot --backend '?'
```

To build this checkout with the native ALSA backend in addition to its default
features, install the [ALSA development dependencies](../COMPILING.md#audio-library-dependencies)
and run:

```bash
cargo build --release --locked --features alsa-backend
```

Explicitly select `--backend alsa` when running that binary; adding the feature
does not change the default backend. Preserve any custom TLS or discovery
feature selection used by your existing build.

After stopping the existing playback instance, list compatible ALSA devices:

```bash
target/release/librespot --backend alsa --device '?'
```

Then use the actual ALSA PCM name, retaining the other options from the existing
service:

```bash
target/release/librespot --backend alsa --format S16 --device 'spotify_aes67'
```

Here `spotify_aes67` is an example configuration name, not a built-in device.
A CPAL display name may differ from an ALSA PCM identifier. Device enumeration
can omit a raw PCM while another process owns it.

## Keep the AES67 sample rate fixed when required

Librespot supplies stereo audio at 44,100 Hz. The native ALSA backend requests
that rate; Rodio can resample when its selected output configuration uses a
different rate. If the AES67 session must run at 48,000 Hz, configure conversion
before the hardware PCM rather than relying on the daemon's initial rate.

`plughw:` alone does not force 48,000 Hz: the RAVENNA device supports 44,100 Hz,
so it can accept that rate without conversion.

For a **single stereo playback client on hardware channels 0 and 1**, an
illustrative ALSA definition is:

```text
pcm.spotify_aes67 {
    type plug
    slave {
        pcm "hw:CARD=RAVENNA,DEV=0"
        rate 48000
        channels 2
        format S16_LE
    }
}
```

Add an appropriate definition to the account's ALSA configuration or the
system configuration, using the real card name. Preserve existing definitions.
If the installation already uses `dmix`, a multi-channel route, or another
shared PCM, retain that topology and fix the intended slave rate there. This
example opens the raw playback PCM exclusively and is not a multi-client mixer.

The ALSA sample format and the daemon's RTP source encoding are separate
settings. Changing S16 to another format does not repair a timestamp or stream
recovery failure.

After playback starts, inspect the **hardware** PCM's `hw_params` as described
below. Confirm its actual rate, channels, format, period size and buffer size;
do not infer them solely from the client command line. Backends can negotiate
different period and buffer sizes even when they use the same named PCM. Keep
period sizes compatible with the driver's configured audio frame scheme.

## Timestamp errors followed by POLLERR

Affected CPAL versions can report:

```text
get_htstamp `...` was earlier than get_trigger_htstamp `...`
`alsa::poll()` returned POLLERR
```

These compare ALSA status and trigger timestamps, not RTP packet timestamps.
The trigger timestamp can change on start, stop, pause or suspend. A stale
audio-position timestamp after a state transition does not by itself prove a
PTP clock jump.

In CPAL 0.16.0, a failed timestamp calculation skips that callback's audio
write. The old POLLERR path also bypasses underrun recovery and can repeatedly
report the same condition. Upstream fixes include
[CPAL #1116](https://github.com/RustAudio/cpal/pull/1116) and
[CPAL #1122](https://github.com/RustAudio/cpal/pull/1122); the latter includes
shutdown-pipe fixes needed when allowing the worker to exit on a fatal error.
See the [CPAL 0.18.0 changelog](https://github.com/RustAudio/cpal/blob/v0.18.0/CHANGELOG.md)
for the released changes.

The native ALSA backend is a useful comparison because it bypasses that CPAL
path. It cannot restore a missing PTP clock or fix a kernel-driver failure.
Reducing logging alone cannot restore playback progress.

## Capture the failure before restarting

Record the running binary and kernel version:

```bash
librespot --version
uname -r
cat /sys/module/MergingRavennaALSA/version
modinfo MergingRavennaALSA
```

The sysfs version describes the loaded module. `modinfo` describes the module
file on disk, which can differ after an update. If building from source, record
both the daemon and kernel-driver commits independently.

From the exact librespot checkout used for the binary, inspect dependencies:

```bash
cargo tree --locked -i cpal
cargo tree --locked -i rodio
cargo tree --locked -d
```

If Cargo reports multiple CPAL versions, qualify the query with the version
(for example `-i cpal@0.16.0`) and identify the copy under Rodio. A direct
dependency upgrade alone does not replace Rodio's transitive dependency.
Crates.io patches in an application workspace apply at that workspace root;
an application embedding librespot must verify its own resolution.

Find the RAVENNA card:

```bash
cat /proc/asound/cards
```

In the following commands, replace `N` with its card number. These examples use
device 0, playback, substream 0. Read `status` several times while the fault is
active, before closing librespot:

```bash
cat /proc/asound/cardN/pcm0p/sub0/status
cat /proc/asound/cardN/pcm0p/sub0/hw_params
cat /proc/asound/cardN/pcm0p/sub0/sw_params
sudo fuser -v /dev/snd/pcmCND0p
```

For card 2 the device node is `/dev/snd/pcmC2D0p`. The RAVENNA driver exposes one
raw playback substream. A failed stream can still own it until its PCM handle
is closed; a second raw open returning `EBUSY` does not establish a driver-global
failure. Mixing plugins can change which process owns the hardware PCM.

| Observation | Next check |
| --- | --- |
| Persistent `XRUN` with repeated POLLERR | Whether the client reaches prepare/recovery and resumes writes. |
| `RUNNING` with stationary `hw_ptr` | PTP lock, audio ticks and driver start state. |
| Advancing `hw_ptr` with stationary `appl_ptr` | Whether the client is still supplying samples. |
| Librespot owns the PCM and another open returns `EBUSY` | Whether failed-stream teardown releases the original handle. |
| A rate change or PTP transition precedes the first error | Correlate the daemon and kernel logs with that transition. |

Preserve the exact second-open error, the ALSA configuration, negotiated PCM
parameters, and the daemon/kernel logs around the **first** failure. The later
repeated error messages often describe the persistent state rather than its
original trigger.

## Validate a recovery change

Compare backends with the intended hardware rate and routing held constant.
Record negotiated buffer differences. Check repeated playback start/stop,
underrun recovery, failed-stream teardown and reopening. In a test setup, also
check PTP loss and restoration. Successful recovery should not require
unloading the driver, and permanently failed recovery must release the PCM
instead of waiting indefinitely or flooding logs.

Hardware validation is necessary: unit tests and the ALSA `null` PCM cannot
simulate RAVENNA's PTP-driven hardware pointer. The daemon project's test scripts
can stop services and reload its kernel module; run them in a test environment
or an appropriate maintenance window.
