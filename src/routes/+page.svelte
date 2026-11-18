<script lang="ts">
  import { invoke, Channel } from "@tauri-apps/api/core";
  import { getCurrentWindow } from "@tauri-apps/api/window";
  import { LogicalSize } from "@tauri-apps/api/dpi";
  import { onMount } from "svelte";
  import { langs, initialLang, type Lang } from "$lib/i18n";

  type Device = { serial: string; state: string; model: string };

  const win = getCurrentWindow();
  const IPHONE = "iphone"; // nilai `active` saat sumbernya AirPlay, bukan serial adb

  let lang = $state<Lang>(initialLang());
  const t = $derived(langs[lang]);
  let devices = $state<Device[]>([]);
  let airStatus = $state("");
  let error = $state("");
  let notice = $state("");
  let busy = $state("");
  let active = $state("");
  let hasFrame = $state(false);
  let slow = $state(false);
  let controls = $state(true);
  let fullscreen = $state(false);
  let muted = $state(false);
  let pairAddr = $state("");
  let pairCode = $state("");
  let connectAddr = $state("");
  let canvas = $state<HTMLCanvasElement>();
  let decoder: VideoDecoder | null = null;
  let audio = $state.raw<{ ctx: AudioContext; gain: GainNode; dec?: AudioDecoder } | null>(null);
  let homeSize: LogicalSize | null = null;
  let session = 0;
  let hideTimer = 0;
  let slowTimer = 0;

  const isWifi = (serial: string) => serial.includes(":") || serial.includes("._adb");

  function setLang(l: Lang) {
    lang = l;
    localStorage.setItem("lang", l);
  }

  async function refresh() {
    if (active) return;
    try {
      devices = await invoke<Device[]>("list_devices");
    } catch (e) {
      error = String(e);
    }
    airStatus = await invoke<string>("airplay_status");
  }

  onMount(() => {
    // iPhone memulai mirroring dari sisinya sendiri, jadi kanal ini terpasang sepanjang app hidup.
    const air = new Channel<ArrayBuffer>();
    let airHandler: ((msg: ArrayBuffer) => void) | null = null;
    let airAudioHandler: ((msg: ArrayBuffer) => void) | null = null;
    air.onmessage = (msg) => {
      const p = new Uint8Array(msg);
      if (p.length && p[0] & 0x80) {
        if (active && active !== IPHONE) return; // sedang mirror Android, jangan direbut
        error = notice = "";
        hasFrame = slow = false;
        active = IPHONE;
        showControls();
        clearTimeout(slowTimer);
        slowTimer = window.setTimeout(() => (slow = true), 5000);
        airHandler = makeHandler();
        closeAudio();
        airAudioHandler = makePcmHandler();
        return;
      }
      if (active === IPHONE) airHandler?.(msg);
    };
    const airAudio = new Channel<ArrayBuffer>();
    airAudio.onmessage = (msg) => {
      if (active === IPHONE) airAudioHandler?.(msg);
    };
    invoke("airplay_listen", { onPacket: air, onAudio: airAudio });

    refresh();
    const timer = setInterval(refresh, 2000);
    return () => clearInterval(timer);
  });

  async function run(key: string, fn: () => Promise<void>) {
    busy = key;
    error = notice = "";
    try {
      await fn();
    } catch (e) {
      error = String(e);
    }
    busy = "";
    refresh();
  }

  const toWifi = (serial: string) =>
    run(serial, async () => {
      await invoke("wifi_from_usb", { serial });
      notice = t.toWifiDone;
    });
  const pair = () =>
    run("pair", async () => {
      await invoke("adb_pair", { addr: pairAddr, code: pairCode });
      notice = t.pairOk;
    });
  const connectWifi = () => run("connect", () => invoke("adb_connect", { addr: connectAddr }));

  // "avc1.PPCCLL" diambil dari 3 byte setelah header NAL SPS, supaya cocok dengan encoder HP mana pun.
  function codecFromSps(config: Uint8Array): string {
    let i = 0;
    while (i + 3 < config.length && !(config[i] === 0 && config[i + 1] === 0 && config[i + 2] === 1)) i++;
    const hex = [...config.subarray(i + 4, i + 7)].map((b) => b.toString(16).padStart(2, "0")).join("");
    return `avc1.${hex}`;
  }

  // Jendela mengikuti bentuk layar HP supaya tangkapan OBS tidak punya bilah hitam.
  async function fitWindow(w: number, h: number) {
    if (await win.isFullscreen()) return;
    const size = (await win.innerSize()).toLogical(await win.scaleFactor());
    homeSize ??= size;
    // Sisi panjang jendela selalu sama, supaya rotasi bolak-balik tidak membuat jendela menyusut.
    const long = homeSize.height;
    const next =
      h >= w
        ? new LogicalSize(Math.round((long * w) / h), long)
        : new LogicalSize(long, Math.round((long * h) / w));
    await win.setSize(next);
  }

  function makeHandler() {
    const ctx = canvas!.getContext("2d")!;
    let config: Uint8Array | null = null;
    let waitKey = true;
    let fitted = "";
    // Thread pembaca sesi lama masih bisa mengirim paket sesudah sesi baru dimulai; abaikan semuanya.
    const mine = ++session;
    if (decoder && decoder.state !== "closed") decoder.close();

    const dec = (decoder = new VideoDecoder({
      output(frame) {
        // Rotasi HP muncul sebagai frame dengan ukuran baru.
        if (canvas!.width !== frame.displayWidth || canvas!.height !== frame.displayHeight) {
          canvas!.width = frame.displayWidth;
          canvas!.height = frame.displayHeight;
        }
        // Dicek per sesi, bukan dari ukuran kanvas: kanvas masih menyimpan ukuran sesi sebelumnya.
        if (fitted !== `${frame.displayWidth}x${frame.displayHeight}`) {
          fitted = `${frame.displayWidth}x${frame.displayHeight}`;
          fitWindow(frame.displayWidth, frame.displayHeight);
        }
        ctx.drawImage(frame, 0, 0);
        frame.close();
        hasFrame = true;
      },
      error(e) {
        if (mine === session) error = `Decoder: ${e.message}`;
      },
    }));

    return (msg: ArrayBuffer) => {
      if (mine !== session) return;
      const p = new Uint8Array(msg);
      if (p.length === 0) {
        if (active) {
          // iPhone berhenti mirroring dari sisinya adalah hal biasa, bukan sambungan putus.
          const lost = active !== IPHONE;
          stop();
          if (lost) error = t.ended;
        }
        return;
      }
      if (p[0] & 0x80) return; // paket sesi: ukuran sudah didapat dari frame hasil decode
      if (dec.state === "closed") return;

      const data = p.subarray(12);
      if (p[0] & 0x40) {
        // Paket config (SPS/PPS): dikirim ulang tiap HP dirotasi.
        config = data.slice();
        dec.configure({ codec: codecFromSps(data), optimizeForLatency: true });
        waitKey = true;
        return;
      }

      const key = (p[0] & 0x20) !== 0;
      if (waitKey && !key) return;
      waitKey = false;

      let chunk = data;
      if (config) {
        // Annex B: SPS/PPS harus menempel di depan key frame pertama.
        chunk = new Uint8Array(config.length + data.length);
        chunk.set(config);
        chunk.set(data, config.length);
        config = null;
      }
      const pts = new DataView(p.buffer, p.byteOffset).getBigUint64(0) & ((1n << 61n) - 1n);
      dec.decode(new EncodedVideoChunk({ type: key ? "key" : "delta", timestamp: Number(pts), data: chunk }));
    };
  }

  // Pemutar bersama untuk suara Android dan iPhone.
  function makePlayer() {
    const TARGET = 0.08; // panjang antrean yang dituju, dalam detik
    const ctx = new AudioContext({ latencyHint: "interactive" });
    const gain = ctx.createGain();
    gain.gain.value = muted ? 0 : 1;
    gain.connect(ctx.destination);
    let anchor = 0; // waktu AudioContext untuk cap waktu 0
    let last = -1;
    let next = 0;
    let queued: AudioBufferSourceNode[] = [];

    const restart = () => {
      queued.forEach((src) => src.stop());
      queued = [];
    };

    /**
     * Dengan `ts` (detik, jam pengirim): diputar tepat pada cap waktunya. Dipakai iPhone, yang cap waktunya
     * presisi per sampel. Tanpa `ts`: disambung rapat dengan potongan sebelumnya. Dipakai Android, yang cap
     * waktunya terukur maju lebih cepat dari isi suaranya (60,3 s cap waktu untuk 60,06 s suara).
     */
    const play = (channels: number, frames: number, rate: number, fill: (out: Float32Array<ArrayBuffer>, c: number) => void, ts?: number) => {
      const now = ctx.currentTime;
      const buf = ctx.createBuffer(channels, frames, rate);
      for (let c = 0; c < channels; c++) fill(buf.getChannelData(c), c);
      const src = ctx.createBufferSource();
      src.buffer = buf;
      src.connect(gain);

      if (ts !== undefined) {
        const lead = anchor + ts - now;
        // Awal stream, lompatan cap waktu (jeda/geser), terlambat, atau antrean memanjang: mulai lagi dari sekarang.
        if (last < 0 || ts < last || ts > last + 10 || lead < 0 || lead > 0.5) {
          restart();
          anchor = now + TARGET - ts;
        }
        last = ts;
        src.start(anchor + ts);
      } else {
        let lead = next - now;
        if (lead < 0.02 || lead > 1) {
          restart(); // awal stream, antrean habis, atau tertinggal terlalu jauh untuk dikejar
          next = now + TARGET;
          lead = TARGET;
        }
        // Jam HP dan PC tidak persis sama dan paket kadang datang bergerombol. Kecepatan putar digeser
        // sedikit (tak terdengar) supaya antrean kembali ke TARGET, alih-alih membuang potongan suara.
        src.playbackRate.value = Math.min(1.05, Math.max(0.97, 1 + (lead - TARGET) * 0.25));
        src.start(next);
        next += buf.duration / src.playbackRate.value;
      }
      queued.push(src);
      src.onended = () => (queued = queued.filter((q) => q !== src));
    };
    return { ctx, gain, play };
  }

  // Android: Opus dari scrcpy, di-decode webview. Selama mirroring, speaker HP sendiri senyap.
  function makeAudioHandler() {
    const mine = session;
    const player = makePlayer();
    const dec = new AudioDecoder({
      output(data) {
        player.play(data.numberOfChannels, data.numberOfFrames, data.sampleRate, (out, c) =>
          data.copyTo(out, { planeIndex: c, format: "f32-planar" }),
        );
        data.close();
      },
      error(e) {
        if (mine === session) error = `Audio: ${e.message}`;
      },
    });
    dec.configure({ codec: "opus", sampleRate: 48000, numberOfChannels: 2 });
    audio = { ...player, dec };

    return (msg: ArrayBuffer) => {
      if (mine !== session || dec.state !== "configured") return;
      const p = new Uint8Array(msg);
      if (p.length <= 12 || p[0] & 0x40) return; // paket config (OpusHead) tidak dibutuhkan decoder
      const pts = new DataView(p.buffer, p.byteOffset).getBigUint64(0) & ((1n << 61n) - 1n);
      dec.decode(new EncodedAudioChunk({ type: "key", timestamp: Number(pts), data: p.subarray(12) }));
    };
  }

  // iPhone: AAC sudah di-decode di Rust. Pesan = sample rate, jumlah kanal, cap waktu RTP (u32), lalu float planar.
  function makePcmHandler() {
    const mine = session;
    const player = makePlayer();
    audio = player;

    return (msg: ArrayBuffer) => {
      if (mine !== session) return;
      const bytes = msg instanceof ArrayBuffer ? msg : new Uint8Array(msg).buffer;
      if (bytes.byteLength <= 12 || bytes.byteLength % 4) return;
      const head = new DataView(bytes);
      const [rate, channels, ts] = [head.getUint32(0, true), head.getUint32(4, true), head.getUint32(8, true)];
      const pcm = new Float32Array(bytes, 12);
      const frames = pcm.length / channels;
      if (!channels || channels > 2 || !Number.isInteger(frames) || rate < 8000 || rate > 96000) return;
      player.play(channels, frames, rate, (out, c) => out.set(pcm.subarray(c * frames, (c + 1) * frames)), ts / rate);
    };
  }

  function closeAudio() {
    if (!audio) return;
    if (audio.dec && audio.dec.state !== "closed") audio.dec.close();
    audio.ctx.close();
    audio = null;
  }

  function toggleMute() {
    muted = !muted;
    if (audio) audio.gain.gain.value = muted ? 0 : 1;
  }

  async function start(serial: string) {
    error = notice = "";
    hasFrame = slow = false;
    active = serial;
    showControls();
    const onPacket = new Channel<ArrayBuffer>();
    onPacket.onmessage = makeHandler();
    const onAudio = new Channel<ArrayBuffer>();
    onAudio.onmessage = makeAudioHandler();
    try {
      await invoke("start_mirror", { serial, onPacket, onAudio });
      // HP dengan layar mati tidak mengirim gambar sama sekali, jadi beri petunjuk alih-alih layar hitam.
      slowTimer = window.setTimeout(() => (slow = true), 3000);
    } catch (e) {
      await stop();
      error = String(e);
    }
  }

  async function stop() {
    clearTimeout(slowTimer);
    session++;
    if (active === IPHONE) invoke("airplay_stop");
    closeAudio();
    if (decoder && decoder.state !== "closed") decoder.close();
    decoder = null;
    active = "";
    if (fullscreen) await toggleFullscreen();
    // Halaman utama butuh lebar semula, bukan lebar sempit mengikuti layar HP.
    if (homeSize) await win.setSize(homeSize);
    homeSize = null;
    await invoke("stop_mirror");
    refresh();
  }

  async function toggleFullscreen() {
    fullscreen = !fullscreen;
    await win.setFullscreen(fullscreen);
  }

  function showControls() {
    controls = true;
    clearTimeout(hideTimer);
    hideTimer = window.setTimeout(() => (controls = false), 2500);
  }

  function onKey(e: KeyboardEvent) {
    if (!active) return;
    if (e.key === "F11" || (e.key === "Escape" && fullscreen)) {
      e.preventDefault();
      toggleFullscreen();
    }
  }
</script>

<svelte:window onkeydown={onKey} />

{#snippet icon(name: "usb" | "wifi")}
  <svg viewBox="0 0 24 24" width="20" height="20" fill="none" stroke="currentColor" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
    {#if name === "wifi"}
      <path d="M2.5 9a14 14 0 0 1 19 0M5.8 12.6a9.2 9.2 0 0 1 12.4 0M9 16.1a4.4 4.4 0 0 1 6 0" />
      <circle cx="12" cy="19.2" r="0.6" fill="currentColor" />
    {:else}
      <path d="M12 21V8M9 8h6V3H9zM7 13v2a3 3 0 0 0 3 3h4a3 3 0 0 0 3-3v-2" />
    {/if}
  </svg>
{/snippet}

{#if active}
  <!-- svelte-ignore a11y_no_static_element_interactions -->
  <div class="stage" onmousemove={showControls} ondblclick={toggleFullscreen}>
    {#if !hasFrame}<p class="hint" role="status">{slow ? t.screenOff : t.connecting}</p>{/if}
    <div class="controls" class:show={controls}>
      {#if audio}
        <button class="quiet" aria-pressed={muted} onclick={toggleMute} onfocus={showControls}>{muted ? t.unmute : t.mute}</button>
      {/if}
      <button class="quiet" onclick={toggleFullscreen} onfocus={showControls}>{fullscreen ? t.exitFullscreen : t.fullscreen}</button>
      <button class="danger" onclick={stop} onfocus={showControls}>{t.disconnect}</button>
    </div>
  </div>
{:else}
  <main>
    <header>
      <div>
        <h1>Screen Mirroring</h1>
        <p class="muted">{t.subtitle}</p>
      </div>
      <div class="lang" role="group" aria-label="Bahasa / Language">
        <button class:on={lang === "id"} aria-pressed={lang === "id"} onclick={() => setLang("id")}>ID</button>
        <button class:on={lang === "en"} aria-pressed={lang === "en"} onclick={() => setLang("en")}>EN</button>
      </div>
    </header>

    {#if error}<p class="banner error" role="alert">{error}</p>{/if}
    {#if notice}<p class="banner notice" role="status">{notice}</p>{/if}

    <h2>{t.devices}<span class="live" title={t.scanning}></span></h2>
    {#each devices as d (d.serial)}
      <div class="device">
        <span class="badge">{@render icon(isWifi(d.serial) ? "wifi" : "usb")}</span>
        <div class="who">
          <strong>{d.model || d.serial}</strong>
          <span class="muted">{isWifi(d.serial) ? t.viaWifi : t.viaUsb}</span>
        </div>
        {#if d.state === "device"}
          <div class="actions">
            {#if !isWifi(d.serial)}
              <button class="quiet" title={t.toWifiHint} disabled={!!busy} onclick={() => toWifi(d.serial)}>
                {busy === d.serial ? t.working : t.toWifi}
              </button>
            {/if}
            <button disabled={!!busy} onclick={() => start(d.serial)}>{t.connect}</button>
          </div>
        {:else}
          <p class="needs">{d.state === "unauthorized" ? t.unauthorized : d.state === "offline" ? t.offline : d.state}</p>
        {/if}
      </div>
    {:else}
      <p class="empty">{t.noDevice}</p>
    {/each}

    <details open={devices.length === 0}>
      <summary>{t.guideTitle}</summary>
      <ol>
        <li>{t.guide1}</li>
        <li>{t.guide2}</li>
        <li>{t.guide3}</li>
      </ol>
    </details>

    <details>
      <summary>{t.wifiTitle}</summary>
      <p class="muted">{t.wifiIntro}</p>
      <form onsubmit={(e) => (e.preventDefault(), pair())}>
        <label>{t.pairAddr}<input bind:value={pairAddr} placeholder="192.168.1.23:37123" autocomplete="off" spellcheck="false" required /></label>
        <label>{t.pairCode}<input bind:value={pairCode} inputmode="numeric" placeholder="123456" autocomplete="off" required /></label>
        <button class="quiet" disabled={!!busy}>{busy === "pair" ? t.working : t.pair}</button>
      </form>
      <form onsubmit={(e) => (e.preventDefault(), connectWifi())}>
        <label>{t.connectAddr}<input bind:value={connectAddr} placeholder="192.168.1.23:40555" autocomplete="off" spellcheck="false" required /></label>
        <button class="quiet" disabled={!!busy}>{busy === "connect" ? t.working : t.connect}</button>
      </form>
    </details>

    <details>
      <summary>{t.obsTitle}</summary>
      <p>{t.obsTip}</p>
    </details>

    <h2>iPhone / iPad</h2>
    <div class="empty">
      {#if airStatus.startsWith("ok:")}
        <p>{t.iphoneReady} <strong class="name">{airStatus.slice(3)}</strong></p>
        <p class="muted note">{t.iphoneNote}</p>
      {:else if airStatus.startsWith("error:")}
        <p class="needs">{t.iphoneError} {airStatus.slice(6)}</p>
      {:else}
        <p>{t.iphoneWait}</p>
      {/if}
    </div>
  </main>
{/if}
<canvas bind:this={canvas} hidden={!active}></canvas>

<style>
  :global(:root) {
    --bg: #0e1014;
    --surface: #171a21;
    --raised: #232832;
    --raised-hover: #2c323e;
    --line: #2b303b;
    --text: #eceef2;
    --muted: #a3a9b4;
    --accent: #2563eb;
    --accent-hover: #3b74f0;
    --focus: #93c5fd;
    --warn: #fbbf24;
    --danger: #dc2626;
    --ease: cubic-bezier(0.16, 1, 0.3, 1);
    color-scheme: dark;
    scrollbar-color: var(--line) transparent;
  }
  :global(body) {
    margin: 0;
    font-family: "Segoe UI Variable Text", "Segoe UI", system-ui, sans-serif;
    background: var(--bg);
    color: var(--text);
    line-height: 1.5;
  }
  :global(::selection) {
    background: var(--accent);
    color: #fff;
  }
  main {
    max-width: 600px;
    margin: 0 auto;
    padding: 28px 24px 32px;
  }
  header {
    display: flex;
    justify-content: space-between;
    align-items: start;
    gap: 16px;
  }
  h1 {
    margin: 0;
    font-size: 1.6rem;
    font-weight: 650;
    letter-spacing: -0.02em;
    line-height: 1.2;
  }
  h2 {
    display: flex;
    align-items: center;
    gap: 8px;
    margin: 32px 0 10px;
    font-size: 0.95rem;
    font-weight: 600;
  }
  p {
    margin: 0;
  }
  .muted {
    color: var(--muted);
  }
  .name {
    color: var(--text);
  }
  .note {
    margin-top: 8px;
    font-size: 0.875rem;
  }

  /* Titik berdenyut: tanda bahwa app terus mencari HP, tanpa tombol "refresh". */
  .live {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: #4ade80;
    animation: pulse 2s ease-in-out infinite;
  }
  @keyframes pulse {
    50% {
      opacity: 0.3;
    }
  }

  .device,
  .empty,
  details {
    background: var(--surface);
    border: 1px solid var(--line);
    border-radius: 12px;
  }
  .device {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 12px 14px;
    padding: 14px 16px;
    margin-bottom: 8px;
    transition:
      opacity 0.35s var(--ease),
      translate 0.35s var(--ease);
  }
  @starting-style {
    .device {
      opacity: 0;
      translate: 0 6px;
    }
  }
  .badge {
    display: grid;
    place-items: center;
    width: 40px;
    height: 40px;
    border-radius: 10px;
    background: var(--raised);
    color: var(--focus);
  }
  .who {
    flex: 1 1 140px;
    min-width: 0;
  }
  .who strong {
    display: block;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .who .muted {
    font-size: 0.875rem;
  }
  .needs {
    flex: 1 1 100%;
    color: var(--warn);
    font-size: 0.9rem;
  }
  .actions {
    display: flex;
    gap: 8px;
  }
  .empty {
    padding: 16px;
    color: var(--muted);
  }

  button {
    padding: 9px 16px;
    border: 0;
    border-radius: 8px;
    background: var(--accent);
    color: #fff;
    font: inherit;
    font-weight: 600;
    cursor: pointer;
    transition:
      background 0.15s,
      scale 0.15s var(--ease);
  }
  button:hover:not(:disabled) {
    background: var(--accent-hover);
  }
  button:active:not(:disabled) {
    scale: 0.97;
  }
  button:disabled {
    opacity: 0.5;
    cursor: default;
  }
  button:focus-visible,
  input:focus-visible,
  summary:focus-visible {
    outline: 2px solid var(--focus);
    outline-offset: 2px;
  }
  .quiet,
  .lang button {
    background: var(--raised);
    color: var(--text);
    font-weight: 500;
  }
  .quiet:hover:not(:disabled),
  .lang button:hover {
    background: var(--raised-hover);
  }
  .danger {
    background: var(--danger);
  }
  .danger:hover:not(:disabled) {
    background: #e23c3c;
  }
  .lang {
    display: flex;
    padding: 3px;
    gap: 2px;
    background: var(--surface);
    border: 1px solid var(--line);
    border-radius: 10px;
  }
  .lang button {
    padding: 4px 10px;
    border-radius: 7px;
    background: transparent;
    color: var(--muted);
    font-size: 0.85rem;
  }
  .lang .on,
  .lang .on:hover {
    background: var(--raised-hover);
    color: var(--text);
  }

  details {
    margin-top: 10px;
    padding: 0 16px;
  }
  summary {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    padding: 14px 0;
    border-radius: 6px;
    font-weight: 600;
    cursor: pointer;
    list-style: none;
  }
  summary::after {
    content: "";
    flex: none;
    width: 8px;
    height: 8px;
    margin-right: 4px;
    border-right: 2px solid var(--muted);
    border-bottom: 2px solid var(--muted);
    rotate: 45deg;
    transition: rotate 0.2s var(--ease);
  }
  details[open] summary::after {
    rotate: -135deg;
  }
  details > :last-child:not(summary) {
    margin-bottom: 16px;
  }
  ol {
    margin: 0;
    padding: 0;
    list-style: none;
    counter-reset: step;
  }
  li {
    position: relative;
    padding-left: 38px;
    margin-bottom: 14px;
    max-width: 65ch;
    counter-increment: step;
  }
  li::before {
    content: counter(step);
    position: absolute;
    left: 0;
    top: 0;
    display: grid;
    place-items: center;
    width: 26px;
    height: 26px;
    border-radius: 50%;
    background: var(--raised);
    font-size: 0.85rem;
    font-weight: 600;
    font-variant-numeric: tabular-nums;
  }
  form {
    display: flex;
    flex-wrap: wrap;
    align-items: end;
    gap: 10px;
    margin-top: 14px;
  }
  label {
    flex: 1 1 180px;
    font-size: 0.85rem;
    color: var(--muted);
  }
  input {
    display: block;
    box-sizing: border-box;
    width: 100%;
    margin-top: 4px;
    padding: 8px 10px;
    border: 1px solid var(--line);
    border-radius: 8px;
    background: var(--bg);
    color: var(--text);
    caret-color: var(--focus);
    font: inherit;
    font-variant-numeric: tabular-nums;
  }
  input::placeholder {
    color: #7d8490;
  }
  .banner {
    padding: 10px 14px;
    border-radius: 10px;
    margin-top: 16px;
  }
  .error {
    background: #3a1a1a;
    color: #fca5a5;
  }
  .notice {
    background: #12301e;
    color: #86efac;
  }

  /* Tampilan mirror: hanya gambar HP di atas hitam polos, supaya bersih saat ditangkap OBS. */
  canvas {
    position: fixed;
    inset: 0;
    width: 100vw;
    height: 100vh;
    object-fit: contain;
    background: #000;
  }
  .stage {
    position: fixed;
    inset: 0;
    z-index: 1;
  }
  .hint {
    position: absolute;
    inset: 50% 16px auto;
    translate: 0 -50%;
    text-align: center;
    color: var(--muted);
  }
  .controls {
    position: absolute;
    top: 12px;
    right: 12px;
    display: flex;
    gap: 6px;
    padding: 6px;
    border-radius: 12px;
    background: var(--surface);
    box-shadow: 0 6px 20px rgb(0 0 0 / 0.5);
    opacity: 0;
    translate: 0 -4px;
    transition:
      opacity 0.25s var(--ease),
      translate 0.25s var(--ease);
  }
  .controls.show,
  .controls:focus-within {
    opacity: 1;
    translate: 0;
  }
  .controls button {
    padding: 6px 12px;
    font-size: 0.875rem;
    white-space: nowrap;
  }
  @media (prefers-reduced-motion: reduce) {
    .controls,
    .device,
    button,
    summary::after {
      transition: none;
    }
    .live {
      animation: none;
    }
  }
</style>
