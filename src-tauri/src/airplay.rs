//! Penerima AirPlay untuk iPhone/iPad: app tampil sebagai tujuan "Screen Mirroring" di Control Center.
//! Protokolnya ditangani crate `rairplay`; modul ini hanya mengumumkan layanan lewat mDNS dan
//! mengubah paket video menjadi format yang sama dengan jalur Android (header 12 byte + Annex B).

use std::convert::Infallible;
use std::error::Error;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddrV4, SocketAddrV6};
use std::ffi::c_int;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Instant;

use crate::h264::low_latency_sps;
use mdns_sd::{ServiceDaemon, ServiceInfo};
use rairplay::config::{Config, DefaultKeychain, Features, Keychain, MacAddr6, Pairing, Video};
use rairplay::playback::audio::{AudioDevice, AudioPacket, AudioParams, CodecKind};
use rairplay::playback::video::{PacketKind, VideoDevice, VideoPacket, VideoParams};
use rairplay::playback::{ChannelHandle, Device, Stream};
use rairplay::transport::DualStackListenerWithRtspRemap;
use rairplay::ServiceFactory;
use tauri::ipc::{Channel, InvokeResponseBody};

const PORT: u16 = 7000;
const START_CODE: [u8; 4] = [0, 0, 0, 1];

/// Dibagi antara perintah Tauri dan stream AirPlay yang sedang berjalan.
#[derive(Default)]
pub struct AirPlay {
    sink: Mutex<Option<Channel<InvokeResponseBody>>>,
    audio_sink: Mutex<Option<Channel<InvokeResponseBody>>>,
    handle: Mutex<Option<Weak<dyn ChannelHandle>>>,
    pub status: Mutex<String>,
    /// Diagnostik suara: format yang diminta iPhone, lalu jumlah paket masuk dan yang berhasil di-decode.
    pub audio_debug: Mutex<String>,
    audio_in: AtomicUsize,
    audio_ok: AtomicUsize,
    audio_dup: AtomicUsize,
    audio_span: Mutex<Option<(Instant, u32, u32)>>, // (waktu paket pertama, ts RTP pertama, ts RTP terakhir)
    // mDNS berhenti mengumumkan kalau daemon di-drop, jadi disimpan selama app hidup.
    mdns: Mutex<Option<ServiceDaemon>>,
}

impl AirPlay {
    pub fn listen(&self, video: Channel<InvokeResponseBody>, audio: Channel<InvokeResponseBody>) {
        *self.sink.lock().unwrap() = Some(video);
        *self.audio_sink.lock().unwrap() = Some(audio);
    }

    pub fn debug(&self) -> String {
        let span = self.audio_span.lock().unwrap().map(|(t0, first, last)| {
            format!("wall={:.1}s media={:.1}s", t0.elapsed().as_secs_f32(), last.wrapping_sub(first) as f32 / 44100.0)
        });
        format!(
            "{} | paket={} decoded={} dup={} {}",
            self.audio_debug.lock().unwrap(),
            self.audio_in.load(Ordering::Relaxed),
            self.audio_ok.load(Ordering::Relaxed),
            self.audio_dup.load(Ordering::Relaxed),
            span.unwrap_or_default()
        )
    }

    pub fn stop(&self) {
        if let Some(h) = self.handle.lock().unwrap().take().and_then(|w| w.upgrade()) {
            h.close();
        }
    }

    fn send(&self, flags: u8, pts: u64, payload: &[u8]) {
        let mut packet = Vec::with_capacity(12 + payload.len());
        let mut head = (pts & ((1 << 61) - 1)).to_be_bytes();
        head[0] |= flags;
        packet.extend_from_slice(&head);
        packet.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        packet.extend_from_slice(payload);
        if let Some(ch) = self.sink.lock().unwrap().as_ref() {
            let _ = ch.send(InvokeResponseBody::Raw(packet));
        }
    }

    fn end(&self) {
        if let Some(ch) = self.sink.lock().unwrap().as_ref() {
            let _ = ch.send(InvokeResponseBody::Raw(Vec::new()));
        }
    }
}

/// avcC (ISO 14496-15) -> SPS/PPS berformat Annex B, plus ukuran prefiks panjang NAL.
fn avcc_to_annexb(avcc: &[u8]) -> Option<(Vec<u8>, usize)> {
    let nal_len = (*avcc.get(4)? & 3) as usize + 1;
    let mut out = Vec::new();
    let mut pos = 5;
    for mask in [0x1f, 0xff] {
        let count = *avcc.get(pos)? & mask;
        pos += 1;
        for _ in 0..count {
            let len = u16::from_be_bytes([*avcc.get(pos)?, *avcc.get(pos + 1)?]) as usize;
            pos += 2;
            out.extend_from_slice(&START_CODE);
            // No-op untuk PPS. Tanpa ini decoder hardware menahan tiap frame ~200 ms (terukur).
            out.extend_from_slice(&low_latency_sps(avcc.get(pos..pos + len)?));
            pos += len;
        }
    }
    (!out.is_empty()).then_some((out, nal_len))
}

/// NAL berprefiks panjang -> Annex B. Mengembalikan juga apakah ada IDR (key frame).
fn nals_to_annexb(data: &[u8], nal_len: usize) -> Option<(Vec<u8>, bool)> {
    let mut out = Vec::with_capacity(data.len() + 8);
    let mut key = false;
    let mut pos = 0;
    while pos < data.len() {
        let len = data
            .get(pos..pos + nal_len)?
            .iter()
            .fold(0usize, |n, b| (n << 8) | *b as usize);
        pos += nal_len;
        let nal = data.get(pos..pos + len)?;
        pos += len;
        key |= nal.first().is_some_and(|b| b & 0x1f == 5);
        out.extend_from_slice(&START_CODE);
        out.extend_from_slice(&low_latency_sps(nal)); // SPS bisa juga datang di dalam key frame
    }
    Some((out, key))
}

#[repr(C)]
struct SmAac {
    _opaque: [u8; 0],
}

unsafe extern "C" {
    fn sm_aac_open(asc: *const u8, asc_len: c_int, sample_rate: c_int, channels: c_int) -> *mut SmAac;
    fn sm_aac_decode(
        d: *mut SmAac,
        data: *const u8,
        len: c_int,
        out: *mut f32,
        out_cap: c_int,
        channels: *mut c_int,
        sample_rate: *mut c_int,
    ) -> c_int;
    fn sm_aac_close(d: *mut SmAac);
}

/// Decoder AAC dari FFmpeg (lihat aac.c).
struct Aac(*mut SmAac);

// Pointer hanya dipakai dari balik Mutex di AudioSink, tidak pernah dari dua thread sekaligus.
unsafe impl Send for Aac {}

impl Aac {
    fn open(asc: &[u8], sample_rate: u32, channels: u8) -> Option<Self> {
        let d = unsafe { sm_aac_open(asc.as_ptr(), asc.len() as c_int, sample_rate as c_int, channels as c_int) };
        (!d.is_null()).then_some(Self(d))
    }

    /// Hasil: (sample rate, jumlah kanal, sampel float planar).
    fn decode(&mut self, frame: &[u8], out: &mut [f32]) -> Option<(u32, u32, usize)> {
        let (mut channels, mut rate) = (0, 0);
        let n = unsafe {
            sm_aac_decode(
                self.0,
                frame.as_ptr(),
                frame.len() as c_int,
                out.as_mut_ptr(),
                out.len() as c_int,
                &mut channels,
                &mut rate,
            )
        };
        (n > 0).then_some((rate as u32, channels as u32, n as usize * channels as usize))
    }
}

impl Drop for Aac {
    fn drop(&mut self) {
        unsafe { sm_aac_close(self.0) }
    }
}

/// AudioSpecificConfig untuk format AAC yang dikirim iPhone, dikenali dari jumlah sampel per frame.
fn aac_config(samples_per_frame: u32, sample_rate: u32, channels: u8) -> Option<&'static [u8]> {
    match (samples_per_frame, sample_rate, channels) {
        (480, 44100, 2) => Some(&[0xf8, 0xe8, 0x50, 0x00]), // AAC-ELD, dipakai screen mirroring
        (1024, 44100, 2) => Some(&[0x12, 0x10]),            // AAC-LC
        _ => None,
    }
}

pub struct AudioOut(Arc<AirPlay>);

impl Device for AudioOut {
    type Params = AudioParams;
    type Stream = AudioSink;
    type Error = Infallible;

    async fn create(&self, _id: u64, params: AudioParams, _: Weak<dyn ChannelHandle>) -> Result<AudioSink, Infallible> {
        let codec = params.codec;
        // shortcut: hanya AAC (suara screen mirroring). ALAC/PCM dari pemutaran musik AirPlay diabaikan.
        let decoder = matches!(codec.kind, CodecKind::Aac)
            .then(|| aac_config(params.samples_per_frame, codec.sample_rate, codec.channels))
            .flatten()
            .and_then(|asc| Aac::open(asc, codec.sample_rate, codec.channels));
        *self.0.audio_debug.lock().unwrap() = format!(
            "{:?} spf={} rate={} ch={} decoder={}",
            codec.kind, params.samples_per_frame, codec.sample_rate, codec.channels, decoder.is_some()
        );
        *self.0.audio_span.lock().unwrap() = None;
        Ok(AudioSink { app: self.0.clone(), decoder: Mutex::new(decoder.map(|d| (d, None))) })
    }
}

impl AudioDevice for AudioOut {
    // shortcut: tombol volume iPhone diabaikan, atur volume dari PC; teruskan ke webview kalau diminta.
    fn get_volume(&self) -> f32 {
        0.0
    }

    fn set_volume(&self, _value: f32) {}
}

pub struct AudioSink {
    app: Arc<AirPlay>,
    decoder: Mutex<Option<(Aac, Option<u16>)>>, // decoder + nomor urut RTP terakhir yang di-decode
}

impl Stream for AudioSink {
    type Content = AudioPacket;

    fn on_data(&self, packet: AudioPacket) {
        let Some(frame) = packet.rtp.get(AudioPacket::HEADER_LEN..).filter(|f| !f.is_empty()) else { return };
        let seq = u16::from_be_bytes([packet.rtp[2], packet.rtp[3]]);
        let ts = u32::from_be_bytes(packet.rtp[4..8].try_into().unwrap());
        self.app.audio_in.fetch_add(1, Ordering::Relaxed);
        let mut guard = self.decoder.lock().unwrap();
        let Some((decoder, last_seq)) = guard.as_mut() else { return };
        // AAC bergantung pada frame sebelumnya, jadi paket kembar atau terlambat (kiriman ulang) merusak suara.
        if last_seq.is_some_and(|last| seq.wrapping_sub(last) == 0 || seq.wrapping_sub(last) > 0x8000) {
            self.app.audio_dup.fetch_add(1, Ordering::Relaxed);
            return;
        }
        *last_seq = Some(seq);
        let mut span = self.app.audio_span.lock().unwrap();
        *span = Some(span.map_or((Instant::now(), ts, ts), |(t0, first, _)| (t0, first, ts)));
        drop(span);
        let mut pcm = [0f32; 2 * 2048];
        if let Some((rate, channels, len)) = decoder.decode(frame, &mut pcm) {
            self.app.audio_ok.fetch_add(1, Ordering::Relaxed);
            // Pesan ke webview: sample rate, jumlah kanal, cap waktu RTP (semua u32 LE), lalu float planar LE.
            let mut msg = Vec::with_capacity(12 + len * 4);
            msg.extend_from_slice(&rate.to_le_bytes());
            msg.extend_from_slice(&channels.to_le_bytes());
            msg.extend_from_slice(&ts.to_le_bytes());
            pcm[..len].iter().for_each(|s| msg.extend_from_slice(&s.to_le_bytes()));
            if let Some(ch) = self.app.audio_sink.lock().unwrap().as_ref() {
                let _ = ch.send(InvokeResponseBody::Raw(msg));
            }
        }
    }

    fn on_ok(self) {}

    fn on_err(self, _err: Box<dyn Error>) {}
}

pub struct Sink(Arc<AirPlay>);

impl Device for Sink {
    type Params = VideoParams;
    type Stream = VideoSink;
    type Error = Infallible;

    async fn create(
        &self,
        _id: u64,
        _params: VideoParams,
        handle: Weak<dyn ChannelHandle>,
    ) -> Result<VideoSink, Infallible> {
        *self.0.handle.lock().unwrap() = Some(handle);
        // Paket "sesi" (bit tertinggi) memberi tahu webview bahwa iPhone mulai mirroring.
        self.0.send(0x80, 0, &[]);
        Ok(VideoSink {
            app: self.0.clone(),
            nal_len: AtomicUsize::new(4),
            started: Instant::now(),
        })
    }
}

impl VideoDevice for Sink {}

pub struct VideoSink {
    app: Arc<AirPlay>,
    nal_len: AtomicUsize,
    started: Instant,
}

impl Stream for VideoSink {
    type Content = VideoPacket;

    fn on_data(&self, packet: VideoPacket) {
        match packet.kind {
            PacketKind::AvcC => {
                if let Some((config, nal_len)) = avcc_to_annexb(&packet.payload) {
                    self.nal_len.store(nal_len, Ordering::Relaxed);
                    self.app.send(0x40, 0, &config);
                }
            }
            PacketKind::Payload => {
                let nal_len = self.nal_len.load(Ordering::Relaxed);
                if let Some((data, key)) = nals_to_annexb(&packet.payload, nal_len) {
                    let pts = self.started.elapsed().as_micros() as u64;
                    self.app.send(if key { 0x20 } else { 0 }, pts, &data);
                }
            }
            // shortcut: HEVC tidak diiklankan (tanpa ScreenMultiCodec), tambahkan kalau iPhone mengirim HvcC
            _ => {}
        }
    }

    fn on_ok(self) {
        self.app.end();
    }

    fn on_err(self, _err: Box<dyn Error>) {
        self.app.end();
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Nama PC, dibatasi ke ASCII karena nama non-ASCII bermasalah di daftar AirPlay.
fn device_name() -> String {
    let pc: String = std::env::var("COMPUTERNAME")
        .unwrap_or_default()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect();
    if pc.is_empty() { "Screen Mirroring".into() } else { format!("Screen Mirroring ({pc})") }
}

/// MAC tetap per PC (diturunkan dari nama), supaya iPhone mengenali penerima yang sama tiap kali.
fn device_mac(name: &str) -> [u8; 6] {
    let mut mac: [u8; 6] = [0x02, 0, 0, 0, 0, 0]; // 0x02 = alamat lokal, bukan milik vendor mana pun
    for (i, b) in name.bytes().enumerate() {
        mac[1 + i % 5] = mac[1 + i % 5].wrapping_mul(31).wrapping_add(b);
    }
    mac
}

fn advertise(name: &str, mac: [u8; 6], features: u64, pubkey: &[u8]) -> Result<ServiceDaemon, mdns_sd::Error> {
    let daemon = ServiceDaemon::new()?;
    let host = format!("{}.local.", hex(&mac));
    let device_id = mac.map(|b| format!("{b:02X}")).join(":");
    let feat = format!("0x{:X},0x{:X}", features & 0xffff_ffff, features >> 32);
    let pk = hex(pubkey);

    // Kumpulan TXT penerima mirroring "legacy" (sama dengan yang diumumkan UxPlay).
    let airplay: &[(&str, &str)] = &[
        ("deviceid", &device_id),
        ("features", &feat),
        ("flags", "0x4"),
        ("model", "AppleTV3,2"),
        ("pk", &pk),
        ("pi", "2e388006-13ba-4041-9a67-25dd4a43d536"),
        ("srcvers", "220.68"),
        ("vv", "2"),
        ("pw", "false"),
    ];
    let raop: &[(&str, &str)] = &[
        ("ch", "2"),
        ("cn", "0,1,2,3"),
        ("da", "true"),
        ("et", "0,3,5"),
        ("vv", "2"),
        ("ft", &feat),
        ("am", "AppleTV3,2"),
        ("md", "0,1,2"),
        ("rhd", "5.6.0.0"),
        ("pw", "false"),
        ("sr", "44100"),
        ("ss", "16"),
        ("sv", "false"),
        ("tp", "UDP"),
        ("txtvers", "1"),
        ("sf", "0x4"),
        ("vs", "220.68"),
        ("vn", "65537"),
        ("pk", &pk),
    ];
    daemon.register(
        ServiceInfo::new("_airplay._tcp.local.", name, &host, "", PORT, airplay)?.enable_addr_auto(),
    )?;
    daemon.register(
        ServiceInfo::new("_raop._tcp.local.", &format!("{}@{name}", hex(&mac).to_uppercase()), &host, "", PORT, raop)?
            .enable_addr_auto(),
    )?;
    Ok(daemon)
}

/// Dipanggil sekali saat app mulai. Kegagalan (mis. port 7000 terpakai) dicatat di `status`, app tetap jalan.
pub async fn serve(state: Arc<AirPlay>) {
    let name = device_name();
    let mac = device_mac(&name);
    // shortcut: DefaultKeychain memakai kunci identitas bawaan yang sama di semua PC dan tidak menyimpan
    // perangkat tepercaya; ganti dengan Keychain persisten sebelum fitur iPhone dianggap siap rilis.
    let keychain = DefaultKeychain::default();
    let pubkey = keychain.pubkey().to_vec();
    // Mode audio tersangga (AirPlay 2) tidak diiklankan: app memutar suara real-time, tanpa penyangga panjang.
    // Terukur di iPhone: suara mirroring tetap datang sebagai AAC-LC 1024 sampel, tiap paket dikirim 3 kali.
    let features = Features::default() - Features::BufferedAudio - Features::PTPClock;

    let config = Arc::new(Config {
        mac_addr: MacAddr6::from(mac),
        features,
        model: "AppleTV3,2".into(),
        name: name.clone(),
        keychain,
        audio: rairplay::config::Audio { device: AudioOut(state.clone()), buf_size: 1024 * 1024 },
        video: Video { device: Sink(state.clone()), width: 1920, height: 1080, fps: 30, buf_size: 4 * 1024 * 1024 },
        manufacturer: "Screen Mirroring".into(),
        fw_version: env!("CARGO_PKG_VERSION").into(),
        pin: None,
        pairing: Pairing::Legacy,
    });

    let result: Result<(), Box<dyn Error>> = async {
        let listener = DualStackListenerWithRtspRemap::bind(
            SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, PORT),
            SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, PORT, 0, 0),
        )?;
        *state.mdns.lock().unwrap() = Some(advertise(&name, mac, features.bits(), &pubkey)?);
        *state.status.lock().unwrap() = format!("ok:{name}");
        axum::serve(listener, ServiceFactory::new(config)).await?;
        Ok(())
    }
    .await;

    if let Err(e) = result {
        *state.status.lock().unwrap() = format!("error:{e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_avcc_and_nals_to_annexb() {
        // avcC: version 1, profil 64 00 2a, panjang NAL 4 byte, 1 SPS (3 byte), 1 PPS (2 byte)
        let avcc = [1, 0x64, 0, 0x2a, 0xff, 0xe1, 0, 3, 0x67, 0x64, 0, 1, 0, 2, 0x68, 0xee];
        let (config, nal_len) = avcc_to_annexb(&avcc).unwrap();
        assert_eq!(nal_len, 4);
        assert_eq!(config, [0, 0, 0, 1, 0x67, 0x64, 0, 0, 0, 0, 1, 0x68, 0xee]);
        assert!(avcc_to_annexb(&avcc[..9]).is_none());

        let idr = [0, 0, 0, 2, 0x65, 0xaa, 0, 0, 0, 1, 0x06];
        assert_eq!(nals_to_annexb(&idr, 4).unwrap(), (vec![0, 0, 0, 1, 0x65, 0xaa, 0, 0, 0, 1, 0x06], true));
        assert!(!nals_to_annexb(&[0, 0, 0, 1, 0x41], 4).unwrap().1);
        assert!(nals_to_annexb(&[0, 0, 0, 9, 0x41], 4).is_none()); // panjang melebihi data
    }

    #[test]
    fn ffmpeg_opens_the_aac_profiles_iphone_sends() {
        // Membuktikan build FFmpeg yang dibundel punya AAC-ELD, dan data rusak tidak membuat crash.
        let mut eld = Aac::open(aac_config(480, 44100, 2).unwrap(), 44100, 2).expect("AAC-ELD");
        assert!(Aac::open(aac_config(1024, 44100, 2).unwrap(), 44100, 2).is_some());
        assert!(aac_config(352, 44100, 2).is_none());
        let mut pcm = [0f32; 4096];
        assert!(eld.decode(&[0xff; 64], &mut pcm).is_none_or(|(rate, ch, len)| rate == 44100 && ch == 2 && len == 960));
    }
}
