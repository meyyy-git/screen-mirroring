fn main() {
    // FFmpeg (LGPL, shared) diunduh ke vendor/ffmpeg, lihat README. Hanya decoder AAC-nya yang dipakai.
    println!("cargo:rerun-if-changed=src/aac.c");
    cc::Build::new().file("src/aac.c").include("vendor/ffmpeg/include").compile("sm_aac");
    println!("cargo:rustc-link-search=native={}/vendor/ffmpeg/lib", env!("CARGO_MANIFEST_DIR"));
    println!("cargo:rustc-link-lib=dylib=avcodec");
    println!("cargo:rustc-link-lib=dylib=avutil");

    tauri_build::build()
}
