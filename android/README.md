# qpwgraph Relay Android client

This directory contains the Android client for the qpwgraph audio relay. It
reuses the Rust relay engine through `crates/pw-graph-relay-sdk` and a JNI
bridge in `crates/pw-graph-relay-android`.

## Build the native library

Install Android Studio, the Android SDK/NDK, Rust Android targets, and
`cargo-ndk`. The first supported ABI is `arm64-v8a`:

```bash
rustup target add aarch64-linux-android
cargo install cargo-ndk
cargo ndk -t arm64-v8a -o android/app/src/main/jniLibs build \
  -p pw-graph-relay-android --release
```

Build additional ABIs only after the corresponding Rust targets and Opus
native build are available. Do not commit generated `.so` files.

## Build the app

Open `android/` in Android Studio, or run the Gradle task from a machine with
the Android SDK configured:

```bash
./gradlew :app:assembleDebug
./gradlew :app:installDebug
```

The app requests microphone permission only for the **Phone → PC** direction,
where the phone captures a microphone or device-playback source. **PC → Phone**
is playback-only on Android. Android 13+ notification permission is requested
for the foreground audio service. Pairing PINs are entered for the current
client or host lifetime and are not persisted; there is no insecure app-wide
default.

The app mirrors the desktop relay panel with two direction tabs:

- **Phone → PC** — connect to a desktop host and send phone audio.
- **PC → Phone** — run a receive-only Android host so the desktop can send
  audio to the phone. The default control port is `48123`, which the desktop
  probes for when it scans USB tether subnets, so keep it unless you have a
  conflict.

Discovery and trusted devices are secondary sections below the active
direction; they are not audio roles.

USB is not a manual link option: the app (like the desktop) auto-detects an
active USB tether, shows it under the tab bar, and `Auto` prefers it. After a
successful PIN pairing, the app stores a per-host credential encrypted with a
non-exportable Android Keystore AES-256-GCM key. Only ciphertext and peer
metadata are kept in `relay.xml`, and the file is excluded from backup/device
transfer. A later discovery result with the same stable peer ID can connect
without another PIN; unknown peers are never auto-connected. Phone → PC
settings provide global trusted auto-connect and a separate Wi-Fi opt-in; USB
is the default background candidate. Use Forget in Trusted devices to revoke a
credential immediately.

## Pair by QR code

The desktop **Phone → PC** tab renders the host's addresses and port and offers
a **Show QR** button while the host runs. The QR carries a
`qpw-relay://host:port?pin=123456` payload. In the Android **Phone → PC** tab,
tap **Scan QR** (camera permission required) to fill in the address and PIN
automatically, then press **Connect**. Plain `host:port` QR codes work too.

## Test over USB tethering

For the zero-configuration cable workflow, enable **USB tethering** on the
Android device and use the USB network address assigned to the phone/Linux
host:

1. Enable Android **USB tethering** and keep the phone unlocked.
2. On Linux, identify the USB/RNDIS interface, usually `usb0`, `rndis0`, or
   an `enx...` address; the desktop relay panel shows the detected link and
   its address automatically.
3. Start the desktop relay host with a PIN; its application default is the
   fixed TCP port `48123`, so the USB scanner can find it. If that port is
   occupied, choose another explicit port and enter that address manually.
4. Keep the preferred link on **Auto**: the relay panel auto-detects the USB
   tether and shows its address (for example `usb0 · 192.168.42.129`), and
   prefers the USB link automatically.
5. In Android, open the **Phone → PC** direction and start discovery from the
   Discovery section — the desktop host is probed over the USB tether directly.
   For the first connection, tap **Connect** and enter the same PIN used by the
   desktop host. The successful pairing is remembered; later USB appearances
   for that same host connect automatically.
6. Confirm the desktop shows a relay session and that the Relay Microphone or
   Relay Speaker node carries audio.

If Linux cannot ping the phone-side USB address, USB tethering is not active.
ADB connectivity does not prove network reachability.

## Relay over an ADB-only cable

ADB debugging can carry relay audio when the app's transport is set to **ADB
forwarding**. This mode uses two authenticated TCP streams (control plus a
length-framed encrypted audio stream), so it does not need UDP or USB
tethering. It is explicit rather than discoverable: set the target to
`127.0.0.1:48123` and create the matching ADB tunnel before connecting.

For **Phone → PC**, where the Android client connects to a desktop host, run on
the desktop:

```bash
adb reverse tcp:48123 tcp:48123
```

For **PC → Phone**, where the desktop client connects to an Android host, run
on the desktop:

```bash
adb forward tcp:48123 tcp:48123
```

Select **ADB forwarding** in the active direction's advanced settings, use
`127.0.0.1:48123`, and pair once with the host PIN. Keep the host on port
`48123` or use the same explicit port in the ADB command and target. ADB
forwarding does not provide peer discovery; QR and automatic USB discovery
still require a network link.

If ADB is selected but `127.0.0.1:48123` refuses the connection, create the
matching rule and retry. The app reports this as an ADB forwarding diagnostic,
not as a bad PIN. The secondary audio TCP stream reconnects independently from
the control stream with a fresh authenticated challenge/proof; removing and
recreating the forwarding rule should therefore restore audio without a new
PIN pairing.

## Use

1. Start the desktop qpwgraph-rs application with the default `relay` feature.
2. For Phone → PC, open the desktop relay panel, select the direction, set a
   six-digit PIN, and start the host.
3. In Android's **Phone → PC** tab, enter the desktop `host:port` and PIN, scan
   the host's QR code, or find it in the Discovery section.
4. Press **Connect**. For PC → Phone, select that direction on both devices,
   start the Android host, then connect from the desktop's PC → Phone tab.

Manual address entry remains supported as a fallback. The relay protocol uses
TCP control and UDP audio, so both devices must be able to reach each other
on the local network. VPNs, guest Wi-Fi isolation, and firewalls can block
the connection.

The app never auto-connects to an arbitrary discovered peer. The first pairing
requires the current PIN and explicit user approval; subsequent automatic
connections are limited to the stored credential and stable identity of that
same peer. USB tethering uses TCP plus encrypted UDP. ADB forwarding uses the
explicit TCP audio mode described above.

### Physical-device validation checklist

For Phone → PC over USB tether: start the host, pair once,
confirm trusted credential creation, enable USB tethering, confirm the same
stable peer is discovered and reconnects without a PIN, verify audio, disable
USB, and verify the intended Wi-Fi resume/failover behavior.

For a live Wi-Fi → USB transition: establish audio, plug in USB and enable
tethering, verify whether policy keeps Wi-Fi until failure or the product's
authenticated migration path moves immediately, and confirm the status reports
the actual link.

For ADB: enable USB debugging, create the correct reverse/forward rule, select
ADB in the chosen direction, connect to `127.0.0.1:48123`, pair, verify
one-way audio, delete the forwarding rule, observe `Reconnecting audio`,
recreate it, and verify audio returns without PIN pairing. Also exercise
service death, process death, restart, retained trusted credentials, and
direction switching for stale handles.

The stable installation ID and encrypted trusted bearer credentials live in
Android's private `relay` preferences. The file is excluded from cloud backup
and device transfer so a restored copy cannot impersonate the original
installation. Deleting a trusted credential does not regenerate the stable ID.

## Troubleshooting

- **No microphone audio:** grant `RECORD_AUDIO`; check Android's active input
  route and verify the app is not muted by system privacy controls.
- **No connection:** use the host's actual TCP port, verify the PIN, and test
  LAN reachability without guest-network isolation.
- **Connected but silent:** ensure the selected direction matches the peer,
  keep the app's foreground notification active, and check the desktop graph's
  Relay Microphone/Relay Speaker virtual nodes.
- **Discovery:** mDNS is optional; while discovery runs, the desktop also
  probes USB tether subnets directly (mDNS often does not cross a USB
  tether). Manual `host:port` remains supported when neither works.
