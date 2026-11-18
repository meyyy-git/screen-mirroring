# Screen Mirroring

Show your phone screen on a Windows PC, with sound, without touching a command line. Works with Android and iPhone/iPad. Inspired by [scrcpy](https://github.com/Genymobile/scrcpy).

**[Download the latest installer](https://github.com/meyyy-git/screen-mirroring/releases/latest)** (Windows 10/11, 64-bit)

## Features

- **Android** over a USB cable or Wi-Fi: plug in the phone and click **Connect**.
- **iPhone / iPad** over AirPlay (experimental): the app shows up as a Screen Mirroring target in Control Center.
- Sound is forwarded along with the picture (Android 11+ and iPhone).
- The mirror window is clean and follows the shape of the phone screen, so it captures well in OBS.
- Mirroring only: the phone cannot be controlled from the PC.
- Interface in English and Indonesian.

## Using it

### Android

1. On the phone, enable Developer options, then turn on **USB debugging**. The app shows a step-by-step guide.
2. Plug the phone into the PC and tap **Allow** on the phone.
3. Click **Connect**. To go wireless, click **Switch to Wi-Fi** and unplug the cable.

While mirroring, sound plays on the PC and the phone's own speaker is silent.

### iPhone / iPad

1. Put the iPhone and the PC on the same network.
2. On the iPhone, open Control Center → **Screen Mirroring** and pick **Screen Mirroring (your PC name)**.

If the PC does not show up, set the network to **Private** in Windows and allow the app through the firewall.

For a wired connection, plug in the iPhone and turn on **Personal Hotspot**. AirPlay then runs over the cable. This needs Apple's USB driver, which Windows installs together with iTunes or Apple Devices.

### OBS

Add a **Window Capture** source, pick the **Screen Mirroring** window, and use the **Windows 10 (1903 and up)** capture method. The on-screen buttons hide themselves when the mouse is idle.

## Good to know

- The installer is not code-signed, so Windows SmartScreen shows an "unknown publisher" warning. Choose **More info → Run anyway**.
- Closing the app also stops the `adb` server, which disconnects other tools that are using `adb`.
- The AirPlay receiver listens on TCP port 7000. If another program uses that port, the iPhone section shows an error and Android keeps working.
- Each installation generates its own AirPlay identity key on first run and stores it in the app data folder (`%APPDATA%\com.screenmirroring.app\airplay.key`).
- Only screen-mirroring audio is handled for iPhone. Playing music over AirPlay without mirroring is ignored, and the iPhone volume buttons do not change the PC volume.

## Development

Requires Node.js, Rust, and the MSVC Build Tools.

Before the first build, download the FFmpeg 8.1 `win64-lgpl-shared` build from [BtbN/FFmpeg-Builds](https://github.com/BtbN/FFmpeg-Builds/releases) and extract it so that `src-tauri/vendor/ffmpeg/{bin,include,lib}` exist. Only its AAC decoder is used, for iPhone audio.

```
npm install
npm run tauri dev      # run the app
npm run tauri build    # build the installer into src-tauri/target/release/bundle/nsis
npm run check          # Svelte type check
cd src-tauri && cargo test   # needs vendor/ffmpeg/bin on PATH
```

Close the app before building: a running `adb` server locks its own files in `target/`.

### How it works

- `src-tauri/src/lib.rs` runs `adb`, starts `scrcpy-server` on the phone, and forwards H.264 and Opus packets to the webview.
- `src-tauri/src/airplay.rs` advertises the AirPlay receiver over mDNS, converts iPhone video to the same packet format as the Android path, and decodes iPhone audio (AAC) through FFmpeg (`aac.c`).
- `src-tauri/src/h264.rs` rewrites the SPS so hardware decoders do not hold frames back. Without it, iPhone video was measured about 200 ms late.
- `src/routes/+page.svelte` decodes video with WebCodecs, draws it to a canvas, and plays audio through Web Audio.

When updating `src-tauri/resources/scrcpy-server`, also change `SERVER_VERSION` in `lib.rs`. The server rejects a client with a different version, and the protocol can change between versions.

## License

GPL-3.0-or-later, see `LICENSE`.

Third-party components:

- `scrcpy-server` v5.0.1 by Genymobile, Apache 2.0 (`src-tauri/resources/scrcpy-LICENSE.txt`), bundled.
- `adb` (Android SDK Platform-Tools) by Google, see `src-tauri/resources/adb/NOTICE.txt`, bundled.
- [FFmpeg](https://ffmpeg.org) (`avcodec`, `avutil`, `swresample`), LGPL, bundled in the installer.
- [`rairplay`](https://github.com/r4v3n6101/rairplay), GPL-3.0-only, the AirPlay receiver. It compiles the `playfair` FairPlay code, whose legal status is unclear.
- `src-tauri/src/h264.rs` is taken from [zenlink](https://github.com/zenvyzenshiro/zenlink), GPL-3.0.
