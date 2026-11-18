# Screen Mirroring

Tampilkan layar HP Android di PC Windows, tanpa command line. Terinspirasi dari [scrcpy](https://github.com/Genymobile/scrcpy).

- Colok HP, klik **Sambungkan**. Bisa lewat kabel USB atau Wi-Fi.
- Hanya mirroring, tanpa kontrol dari PC. Suara ikut diteruskan (Android 11+ dan iPhone).
- Jendela mirror bersih dan mengikuti bentuk layar HP, enak ditangkap OBS (Window Capture, metode "Windows 10 (1903 ke atas)").
- UI bahasa Indonesia dan Inggris.
- iPhone/iPad lewat AirPlay (eksperimental): app muncul sebagai tujuan Screen Mirroring di Control Center.

## Pengembangan

Butuh Node.js, Rust, dan MSVC Build Tools.

Sebelum build pertama, unduh FFmpeg 8.1 versi `win64-lgpl-shared` dari [BtbN/FFmpeg-Builds](https://github.com/BtbN/FFmpeg-Builds/releases) dan ekstrak sehingga ada `src-tauri/vendor/ffmpeg/{bin,include,lib}`. Yang dipakai hanya decoder AAC-nya, untuk suara iPhone.

```
npm install
npm run tauri dev      # jalankan app
npm run tauri build    # buat installer di src-tauri/target/release/bundle/nsis
npm run check          # cek tipe Svelte
cd src-tauri && cargo test
```

Rust (`src-tauri/src/lib.rs`) menjalankan `adb`, menyalakan `scrcpy-server` di HP, dan meneruskan paket H.264 ke webview. Svelte (`src/routes/+page.svelte`) men-decode dengan WebCodecs dan menggambar ke kanvas.

Saat memperbarui `src-tauri/resources/scrcpy-server`, ubah juga `SERVER_VERSION` di `lib.rs`: server menolak client dengan versi berbeda, dan protokolnya bisa berubah antarversi.

## Lisensi

GPL-3.0-or-later, lihat `LICENSE`.

Komponen pihak ketiga yang ikut dibundel di `src-tauri/resources`:

- `scrcpy-server` v5.0.1 dari Genymobile, lisensi Apache 2.0.
- `adb` (Android SDK Platform-Tools) dari Google, lihat `resources/adb/NOTICE.txt`.

Penerima AirPlay memakai crate [`rairplay`](https://github.com/r4v3n6101/rairplay) (GPL-3.0-only), yang ikut mengompilasi kode FairPlay `playfair`. `src-tauri/src/h264.rs` diambil dari [zenlink](https://github.com/zenvyzenshiro/zenlink) (GPL-3.0). Suara iPhone (AAC-ELD) di-decode dengan FFmpeg (LGPL; `avcodec`, `avutil`, `swresample` ikut dibundel). Modul `src-tauri/src/airplay.rs` mengumumkan layanan lewat mDNS dan mengubah video ke format yang sama dengan jalur Android.
