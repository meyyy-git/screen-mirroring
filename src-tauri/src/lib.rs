mod airplay;
mod h264;

use std::io::Read;
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tauri::ipc::{Channel, InvokeResponseBody};
use tauri::path::BaseDirectory;
use tauri::{AppHandle, Manager, State};

// Harus persis sama dengan versi file resources/scrcpy-server, server menolak versi lain.
const SERVER_VERSION: &str = "5.0.1";
const DEVICE_JAR: &str = "/data/local/tmp/scrcpy-server.jar";
const CODEC_H264: u32 = 0x6832_3634;
const CODEC_OPUS: u32 = 0x6f70_7573;
const MAX_PACKET: usize = 16 * 1024 * 1024;

static ADB_PATH: OnceLock<PathBuf> = OnceLock::new();

#[derive(Default)]
struct Mirror(Mutex<Option<Child>>);

#[derive(Serialize, PartialEq, Debug)]
struct Device {
    serial: String,
    state: String,
    model: String,
}

/// adb bawaan app; kalau tidak ada (mis. saat test), pakai adb dari PATH.
fn adb() -> Command {
    let mut c = match ADB_PATH.get() {
        Some(p) if p.exists() => Command::new(p),
        _ => Command::new("adb"),
    };
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW: tanpa ini muncul jendela konsol sekilas
    }
    c
}

fn adb_out(args: &[&str]) -> Result<String, String> {
    let o = adb()
        .args(args)
        .output()
        .map_err(|e| format!("adb: {e}"))?;
    let stdout = String::from_utf8_lossy(&o.stdout).into_owned();
    if !o.status.success() {
        let stderr = String::from_utf8_lossy(&o.stderr);
        let msg = if stderr.trim().is_empty() { stdout.trim() } else { stderr.trim() };
        return Err(msg.to_string());
    }
    Ok(stdout)
}

fn parse_devices(out: &str) -> Vec<Device> {
    out.lines()
        .skip(1)
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let serial = parts.next()?.to_string();
            let state = parts.next()?.to_string();
            let model = parts
                .find_map(|p| p.strip_prefix("model:"))
                .unwrap_or("")
                .replace('_', " ");
            Some(Device { serial, state, model })
        })
        .collect()
}

/// Ambil IPv4 dari keluaran `ip -f inet addr show wlan0`.
fn parse_wlan_ip(out: &str) -> Option<String> {
    let rest = out.split("inet ").nth(1)?;
    let ip = rest.split(['/', ' ']).next()?;
    ip.parse::<std::net::Ipv4Addr>().ok().map(|i| i.to_string())
}

/// Alamat dari pengguna diteruskan ke adb sebagai argumen, jadi tolak apa pun yang bukan host:port.
fn valid_addr(addr: &str) -> bool {
    match addr.rsplit_once(':') {
        Some((host, port)) => {
            !host.is_empty()
                && !host.starts_with('-')
                && host.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
                && port.parse::<u16>().is_ok()
        }
        None => false,
    }
}

#[tauri::command(async)]
fn list_devices() -> Result<Vec<Device>, String> {
    Ok(parse_devices(&adb_out(&["devices", "-l"])?))
}

// adb connect/pair keluar dengan kode 0 walau gagal, jadi hasilnya dinilai dari teksnya.
fn connect(addr: &str) -> Result<String, String> {
    let out = adb_out(&["connect", addr])?;
    if out.contains("connected to") {
        Ok(addr.to_string())
    } else {
        Err(out.trim().to_string())
    }
}

#[tauri::command(async)]
fn adb_connect(addr: String) -> Result<String, String> {
    let addr = addr.trim();
    if !valid_addr(addr) {
        return Err("Format alamat harus IP:port".into());
    }
    connect(addr)
}

#[tauri::command(async)]
fn adb_pair(addr: String, code: String) -> Result<(), String> {
    let (addr, code) = (addr.trim(), code.trim());
    if !valid_addr(addr) || code.is_empty() || !code.chars().all(|c| c.is_ascii_digit()) {
        return Err("Format alamat harus IP:port dan kode harus angka".into());
    }
    let out = adb_out(&["pair", addr, code])?;
    if out.contains("Successfully paired") {
        Ok(())
    } else {
        Err(out.trim().to_string())
    }
}

/// HP yang sedang tercolok USB dipindah ke Wi-Fi tanpa kode pairing. Mengembalikan serial barunya.
#[tauri::command(async)]
fn wifi_from_usb(serial: String) -> Result<String, String> {
    let ip = parse_wlan_ip(&adb_out(&["-s", &serial, "shell", "ip", "-f", "inet", "addr", "show", "wlan0"])?)
        .ok_or("HP belum tersambung ke Wi-Fi")?;
    adb_out(&["-s", &serial, "tcpip", "5555"])?;
    let addr = format!("{ip}:5555");
    // adbd di HP restart setelah perintah tcpip, jadi sambungan pertama bisa terlalu cepat.
    let mut last = String::new();
    for _ in 0..10 {
        std::thread::sleep(Duration::from_millis(500));
        match connect(&addr) {
            Ok(a) => return Ok(a),
            Err(e) => last = e,
        }
    }
    Err(last)
}

fn kill(child: Option<Child>) {
    if let Some(mut c) = child {
        let _ = c.kill();
        let _ = c.wait();
    }
}

#[tauri::command]
fn stop_mirror(mirror: State<Mirror>) {
    kill(mirror.0.lock().unwrap().take());
}

/// Meneruskan paket scrcpy dari satu socket ke webview sampai socket ditutup.
/// Tiap pesan = header 12 byte dari scrcpy (+ payload untuk paket media).
fn pump(mut sock: TcpStream, out: &Channel<InvokeResponseBody>) {
    loop {
        let mut packet = vec![0u8; 12];
        if sock.read_exact(&mut packet).is_err() {
            return;
        }
        // Bit tertinggi = paket sesi (ukuran video), tanpa payload.
        if packet[0] & 0x80 == 0 {
            let size = u32::from_be_bytes(packet[8..12].try_into().unwrap()) as usize;
            if size > MAX_PACKET {
                return;
            }
            packet.resize(12 + size, 0);
            if sock.read_exact(&mut packet[12..]).is_err() {
                return;
            }
        }
        if out.send(InvokeResponseBody::Raw(packet)).is_err() {
            return;
        }
    }
}

/// Menjalankan scrcpy-server di HP lalu meneruskan paket video dan audio ke webview.
/// Pesan kosong di kanal video = stream berakhir.
#[tauri::command(async)]
fn start_mirror(
    app: AppHandle,
    mirror: State<Mirror>,
    serial: String,
    on_packet: Channel<InvokeResponseBody>,
    on_audio: Channel<InvokeResponseBody>,
) -> Result<(), String> {
    kill(mirror.0.lock().unwrap().take());

    let jar = app
        .path()
        .resolve("resources/scrcpy-server", BaseDirectory::Resource)
        .map_err(|e| e.to_string())?;
    adb_out(&["-s", &serial, "push", &jar.to_string_lossy(), DEVICE_JAR])?;

    let scid = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .subsec_nanos()
        & 0x7fff_ffff;
    let port_arg = adb_out(&[
        "-s",
        &serial,
        "forward",
        "tcp:0",
        &format!("localabstract:scrcpy_{scid:08x}"),
    ])?;
    let port: u16 = port_arg
        .trim()
        .parse()
        .map_err(|_| "adb forward tidak mengembalikan nomor port".to_string())?;

    let mut child = adb()
        .args(["-s", &serial, "shell"])
        .arg(format!("CLASSPATH={DEVICE_JAR}"))
        .args(["app_process", "/", "com.genymobile.scrcpy.Server", SERVER_VERSION])
        .arg(format!("scid={scid:08x}"))
        .args([
            "tunnel_forward=true",
            "control=false",
            "send_device_meta=false",
            "max_size=1920",
        ])
        .stdin(Stdio::null())
        .spawn()
        .map_err(|e| format!("gagal menjalankan server: {e}"))?;

    // Lewat adb forward, connect() selalu berhasil walau server belum siap;
    // byte dummy dari server adalah tanda bahwa ia benar-benar mendengarkan.
    let mut sock = None;
    for _ in 0..50 {
        if let Ok(mut s) = TcpStream::connect(("127.0.0.1", port)) {
            let mut dummy = [0u8; 1];
            if s.read_exact(&mut dummy).is_ok() {
                sock = Some(s);
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    // Server menunggu socket kedua (audio) sebelum mulai mengirim apa pun, jadi sambungkan sekarang.
    let audio = sock.as_ref().and_then(|_| TcpStream::connect(("127.0.0.1", port)).ok());
    // Server sudah menerima sambungan (atau gagal), forward tidak dipakai lagi.
    let _ = adb_out(&["-s", &serial, "forward", "--remove", &format!("tcp:{port}")]);

    let Some(mut sock) = sock else {
        kill(Some(child));
        return Err("server di HP tidak merespons dalam 5 detik".into());
    };
    let _ = sock.set_nodelay(true);

    let mut codec = [0u8; 4];
    if sock.read_exact(&mut codec).is_err() || u32::from_be_bytes(codec) != CODEC_H264 {
        let _ = child.kill();
        return Err("server tidak mengirim stream H.264".into());
    }

    *mirror.0.lock().unwrap() = Some(child);

    std::thread::spawn(move || {
        pump(sock, &on_packet);
        let _ = on_packet.send(InvokeResponseBody::Raw(Vec::new()));
    });
    if let Some(mut audio) = audio {
        std::thread::spawn(move || {
            // Android 10 ke bawah tidak bisa menangkap suara: server mengirim id 0 dan mirroring lanjut tanpa audio.
            let mut codec = [0u8; 4];
            if audio.read_exact(&mut codec).is_ok() && u32::from_be_bytes(codec) == CODEC_OPUS {
                pump(audio, &on_audio);
            }
        });
    }
    Ok(())
}

/// Webview mendaftar sekali saat mulai; paket iPhone bisa datang kapan saja sesudahnya.
#[tauri::command]
fn airplay_listen(
    airplay: State<Arc<airplay::AirPlay>>,
    on_packet: Channel<InvokeResponseBody>,
    on_audio: Channel<InvokeResponseBody>,
) {
    airplay.listen(on_packet, on_audio);
}

#[tauri::command]
fn airplay_stop(airplay: State<Arc<airplay::AirPlay>>) {
    airplay.stop();
}

#[tauri::command]
fn airplay_debug(airplay: State<Arc<airplay::AirPlay>>) -> String {
    airplay.debug()
}

/// "ok:<nama penerima>", "error:<pesan>", atau kosong selama masih menyiapkan.
#[tauri::command]
fn airplay_status(airplay: State<Arc<airplay::AirPlay>>) -> String {
    airplay.status.lock().unwrap().clone()
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .manage(Mirror::default())
        .setup(|app| {
            if let Ok(p) = app.path().resolve("resources/adb/adb.exe", BaseDirectory::Resource) {
                let _ = ADB_PATH.set(p);
            }
            let airplay = Arc::new(airplay::AirPlay::default());
            app.manage(airplay.clone());
            let key_path = app.path().app_data_dir()?.join("airplay.key");
            tauri::async_runtime::spawn(airplay::serve(airplay, key_path));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            list_devices,
            adb_connect,
            adb_pair,
            wifi_from_usb,
            start_mirror,
            stop_mirror,
            airplay_listen,
            airplay_stop,
            airplay_status,
            airplay_debug
        ])
        .build(tauri::generate_context!())
        .expect("error while running tauri application")
        .run(|_, event| {
            // Server adb bawaan terus hidup setelah app ditutup dan mengunci filenya sendiri,
            // sehingga installer versi berikutnya tidak bisa menimpanya.
            if let tauri::RunEvent::Exit = event {
                let _ = adb().arg("kill-server").output();
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_adb_devices_output() {
        let out = "List of devices attached\n\
                   R58M123ABC             device product:a52q model:SM_A525F device:a52q transport_id:1\n\
                   192.168.1.7:5555       unauthorized transport_id:2\n\n";
        assert_eq!(
            parse_devices(out),
            vec![
                Device { serial: "R58M123ABC".into(), state: "device".into(), model: "SM A525F".into() },
                Device { serial: "192.168.1.7:5555".into(), state: "unauthorized".into(), model: "".into() },
            ]
        );
    }

    #[test]
    fn parses_wlan_ip() {
        let out = "24: wlan0: <BROADCAST,MULTICAST,UP,LOWER_UP> mtu 1500 qdisc mq state UP\n    inet 192.168.1.23/24 brd 192.168.1.255 scope global wlan0\n";
        assert_eq!(parse_wlan_ip(out), Some("192.168.1.23".into()));
        assert_eq!(parse_wlan_ip("Device \"wlan0\" does not exist."), None);
    }

    #[test]
    fn validates_addresses() {
        assert!(valid_addr("192.168.1.23:5555"));
        assert!(!valid_addr("192.168.1.23"));
        assert!(!valid_addr("-s:5555"));
        assert!(!valid_addr("a b:5555"));
        assert!(!valid_addr("host:99999"));
    }
}
