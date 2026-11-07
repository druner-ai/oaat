//! ts-controller — persistent OAAT controller для torrent-streamer.
//!
//! В отличие от `tune-bridge controller --file ...` (который стримит один WAV
//! и выходит), держит ПОСТОЯННОЕ соединение с endpoint (MacBook) и принимает
//! JSON-команды через Unix socket: play / pause / resume / seek / stop /
//! volume / status. Пауза, перемотка и громкость идут нативными сообщениями
//! протокола OAAT (Pause/Seek/VolumeSet), без SIGSTOP/SIGCONT/перезапусков.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use clap::Parser;
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncSeekExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::mpsc;
use tracing::{info, warn};

use oaat_controller::{ClockResponder, ConnectedEndpoint, ControllerConfig};
use oaat_core::format::{AudioFormat, ChannelLayout};
use oaat_core::message::{Pause, TrackMetadata};
use oaat_core::wire::PacketFlags;
use oaat_core::Message;

const SAMPLES_PER_PACKET: usize = 360;
const PLAY_DELAY_NS: u64 = 200_000_000;
const STREAM_ID: &str = "file-stream";

#[derive(Parser)]
#[command(name = "ts-controller")]
struct Args {
    /// OAAT endpoint address
    #[arg(long, default_value = "192.168.31.215:9740")]
    target: SocketAddr,
    /// Unix socket path for commands
    #[arg(long, default_value = "/tmp/ts-controller.sock")]
    socket: String,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "cmd", rename_all = "lowercase")]
enum Command {
    Play {
        path: String,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        artist: Option<String>,
        #[serde(default)]
        album: Option<String>,
    },
    Pause,
    Resume,
    Seek {
        sec: f64,
    },
    Stop,
    Volume {
        level: u8,
    },
    Status,
}

struct Track {
    path: String,
    format: AudioFormat,
    sample_rate: u32,
    channels: u8,
    bits: u8,
    data_offset: u64,
    bytes_per_sample: usize,
    total_samples: u64,
    duration_s: f64,
}

struct Player {
    track: Option<Track>,
    meta: Option<TrackMetadata>, // последние метаданные — для re-arm при seek
    sample_offset: u64,    // следующий sample для ЧТЕНИЯ из файла
    base_offset: u64,      // сэмпл, с которого начинается текущий поток (для заголовка/PTS)
    playing: bool,         // стримим (не paused, не stopped)
    start_ns: u64,         // absolute PTS base (controller clock)
    stream_start: Instant, // real-time база
    base_elapsed_ns: u64,  // сколько "проиграно" до stream_start (для pause/seek)
    seek_pending: bool,    // после seek следующий пакет — FIRST_PACKET
}

fn now_ns() -> u64 {
    oaat_core::time::now_ns()
}

fn stem(path: &str) -> String {
    std::path::Path::new(path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(path)
        .to_string()
}

fn parse_wav(path: &str) -> Result<Track, String> {
    let file_data = std::fs::read(path).map_err(|e| format!("не читается {path}: {e}"))?;
    let is_wav = file_data.len() > 44 && &file_data[0..4] == b"RIFF" && &file_data[8..12] == b"WAVE";
    if !is_wav {
        return Err(format!("не WAV (только WAV): {path}"));
    }

    let channels = u16::from_le_bytes([file_data[22], file_data[23]]) as u8;
    let sample_rate = u32::from_le_bytes([file_data[24], file_data[25], file_data[26], file_data[27]]);
    let bits_per_sample = u16::from_le_bytes([file_data[34], file_data[35]]) as u8;

    let mut data_offset = 12usize;
    let mut data_len = 0usize;
    while data_offset + 8 < file_data.len() {
        let chunk_id = &file_data[data_offset..data_offset + 4];
        let chunk_size = u32::from_le_bytes([
            file_data[data_offset + 4],
            file_data[data_offset + 5],
            file_data[data_offset + 6],
            file_data[data_offset + 7],
        ]) as usize;
        if chunk_id == b"data" {
            data_offset += 8;
            data_len = chunk_size.min(file_data.len() - data_offset);
            break;
        }
        data_offset += 8 + chunk_size;
        if !chunk_size.is_multiple_of(2) {
            data_offset += 1;
        }
    }
    if data_len == 0 {
        return Err("в WAV нет data chunk".into());
    }

    let bytes_per_sample = (bits_per_sample as usize / 8) * channels as usize;
    if bytes_per_sample == 0 {
        return Err("некорректный WAV (bit depth)".into());
    }
    let total_samples = (data_len / bytes_per_sample) as u64;
    let duration_s = total_samples as f64 / sample_rate as f64;

    let format = match bits_per_sample {
        16 => AudioFormat::PcmS16le,
        24 => AudioFormat::PcmS24le,
        32 => AudioFormat::PcmS32le,
        other => return Err(format!("bit depth не поддержан: {other}")),
    };

    Ok(Track {
        path: path.to_string(),
        format,
        sample_rate,
        channels,
        bits: bits_per_sample,
        data_offset: data_offset as u64,
        bytes_per_sample,
        total_samples,
        duration_s,
    })
}

fn elapsed_ns(p: &Player) -> u64 {
    p.base_elapsed_ns + p.stream_start.elapsed().as_nanos() as u64
}

/// JSON-ответ с ошибкой; экранирует кавычки (сетевые ошибки содержат произвольный текст).
fn json_err(msg: &str) -> String {
    format!("{{\"ok\":false,\"error\":\"{}\"}}", msg.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Подключение к endpoint'у с бесконечными ретраями (стартовое И переподключение).
async fn connect_with_retry(config: &ControllerConfig, target: SocketAddr) -> ConnectedEndpoint {
    loop {
        match ConnectedEndpoint::connect(config, target).await {
            Ok(ep) => return ep,
            Err(e) => {
                warn!("не подключиться к {target}: {e}; повтор через 5с");
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }
}

async fn handle(ep: &mut ConnectedEndpoint, p: &mut Player, cmd: Command) -> String {
    match cmd {
        Command::Play { path, title, artist, album } => match parse_wav(&path) {
            Err(e) => format!("{{\"ok\":false,\"error\":\"{e}\"}}"),
            Ok(track) => {
                if let Err(e) = ep
                    .propose_format(
                        STREAM_ID,
                        track.format,
                        track.sample_rate,
                        track.channels,
                        ChannelLayout::Stereo,
                        track.bits,
                    )
                    .await
                {
                    return json_err(&format!("endpoint недоступен: {e}"));
                }
                let meta = TrackMetadata {
                    title: title.unwrap_or_else(|| stem(&path)),
                    artist: artist.unwrap_or_default(),
                    album: album.unwrap_or_default(),
                    duration_ms: (track.duration_s * 1000.0) as u64,
                    artwork_url: None,
                    format: Some(format!("PCM {}/{}", track.bits, track.sample_rate / 1000)),
                };
                let _ = ep.send_metadata(meta.clone()).await;
                if let Err(e) = ep.send_play(STREAM_ID).await {
                    return json_err(&format!("endpoint недоступен: {e}"));
                }
                let dur = track.duration_s;
                p.track = Some(track);
                p.meta = Some(meta);
                p.sample_offset = 0;
                p.base_offset = 0;
                p.playing = true;
                p.start_ns = now_ns() + PLAY_DELAY_NS;
                p.stream_start = Instant::now();
                p.base_elapsed_ns = 0;
                p.seek_pending = false;
                info!(path, "play");
                format!("{{\"ok\":true,\"state\":\"playing\",\"duration\":{dur}}}")
            }
        },
        Command::Pause => {
            if p.track.is_none() {
                return "{\"ok\":false,\"error\":\"нет трека\"}".into();
            }
            let _ = ep
                .send_message(&Message::Pause(Pause { stream_id: STREAM_ID.into() }))
                .await;
            if p.playing {
                p.base_elapsed_ns = elapsed_ns(p);
                p.playing = false;
            }
            info!("pause");
            "{\"ok\":true,\"state\":\"paused\"}".into()
        }
        Command::Resume => {
            if p.track.is_none() {
                return "{\"ok\":false,\"error\":\"нет трека\"}".into();
            }
            let _ = ep.send_play(STREAM_ID).await;
            if !p.playing {
                p.stream_start = Instant::now();
                p.start_ns = now_ns() + PLAY_DELAY_NS;
                p.playing = true;
            }
            info!("resume");
            "{\"ok\":true,\"state\":\"playing\"}".into()
        }
        Command::Seek { sec } => {
            let Some(t) = &p.track else {
                return "{\"ok\":false,\"error\":\"нет трека\"}".into();
            };
            let (fmt, rate, ch, bits, dur_s) =
                (t.format, t.sample_rate, t.channels, t.bits, t.duration_s);
            let sec = sec.clamp(0.0, dur_s);
            // endpoint (MacBook) не перематывает по Seek-сообщению надёжно →
            // перезапускаем поток с пропущенными сэмплами. ВАЖНО (macOS/Cpal):
            // stop разрушает выходной стрим, и Play без нового propose_format —
            // тихий no-op (пакеты идут, buf=0, звука нет). Поэтому полный
            // re-arm: stop → propose → metadata → play, как при свежем
            // воспроизведении. Для ALSA (NUC) повторный propose безвреден.
            let _ = ep.send_stop(STREAM_ID).await;
            if let Err(e) = ep
                .propose_format(STREAM_ID, fmt, rate, ch, ChannelLayout::Stereo, bits)
                .await
            {
                return json_err(&format!("endpoint недоступен: {e}"));
            }
            if let Some(m) = &p.meta {
                let _ = ep.send_metadata(m.clone()).await;
            }
            p.sample_offset = (sec * rate as f64) as u64;
            // новый поток начинается здесь: заголовок/PTS идут с 0 (rel = 0),
            // а данные читаются с sample_offset (пропущенные сэмплы = позиция sec).
            p.base_offset = p.sample_offset;
            p.base_elapsed_ns = (sec * 1e9) as u64;
            p.stream_start = Instant::now();
            p.start_ns = now_ns() + PLAY_DELAY_NS;
            p.seek_pending = true;
            if p.playing {
                let _ = ep.send_play(STREAM_ID).await;
            }
            let state = if p.playing { "playing" } else { "paused" };
            info!(sec, "seek (перезапуск потока)");
            format!("{{\"ok\":true,\"state\":\"{state}\",\"elapsed\":{sec}}}")
        }
        Command::Stop => {
            if p.track.is_some() {
                let _ = ep.send_stop(STREAM_ID).await;
            }
            p.track = None;
            p.playing = false;
            p.sample_offset = 0;
            info!("stop");
            "{\"ok\":true,\"state\":\"stop\"}".into()
        }
        Command::Volume { level } => {
            let lvl = level.min(100);
            let _ = ep.send_volume(lvl).await;
            info!(lvl, "volume");
            format!("{{\"ok\":true,\"volume\":{lvl}}}")
        }
        Command::Status => match &p.track {
            None => "{\"state\":\"stop\",\"elapsed\":null,\"duration\":null}".into(),
            Some(t) => {
                let state = if p.playing { "playing" } else { "paused" };
                let el = if p.playing { elapsed_ns(p) } else { p.base_elapsed_ns };
                format!(
                    "{{\"state\":\"{state}\",\"elapsed\":{},\"duration\":{}}}",
                    el as f64 / 1e9,
                    t.duration_s
                )
            }
        },
    }
}

async fn send_next_packet(ep: &mut ConnectedEndpoint, p: &mut Player) {
    let Some(t) = &p.track else { return };
    let remaining = t.total_samples.saturating_sub(p.sample_offset);
    if remaining == 0 {
        let rel = p.sample_offset.saturating_sub(p.base_offset);
        let pts = p.start_ns + (rel as f64 / t.sample_rate as f64 * 1e9) as u64;
        let _ = ep.send_audio(1, t.format, pts, rel, &[], PacketFlags::LAST_PACKET).await;
        let _ = ep.send_stop(STREAM_ID).await;
        p.track = None;
        p.playing = false;
        p.sample_offset = 0;
        info!("трек завершён");
        return;
    }

    let chunk_samples = SAMPLES_PER_PACKET.min(remaining as usize);
    let byte_off = t.data_offset + p.sample_offset * t.bytes_per_sample as u64;
    let mut payload = vec![0u8; chunk_samples * t.bytes_per_sample];

    match tokio::fs::File::open(&t.path).await {
        Ok(mut f) => {
            if f.seek(std::io::SeekFrom::Start(byte_off)).await.is_err()
                || f.read_exact(&mut payload).await.is_err()
            {
                warn!("ошибка чтения файла {}", t.path);
                return;
            }
        }
        Err(e) => {
            warn!("не открыть файл {}: {e}", t.path);
            return;
        }
    }

    let rel = p.sample_offset.saturating_sub(p.base_offset);
    let pts = p.start_ns + (rel as f64 / t.sample_rate as f64 * 1e9) as u64;
    let first = p.sample_offset == p.base_offset || p.seek_pending;
    p.seek_pending = false;
    let flags = if first {
        PacketFlags::FIRST_PACKET
    } else {
        PacketFlags::empty()
    };
    if let Err(e) = ep.send_audio(1, t.format, pts, rel, &payload, flags).await {
        warn!("send_audio error: {e}");
    }
    p.sample_offset += chunk_samples as u64;
}

async fn stream_loop(
    mut ep: ConnectedEndpoint,
    mut cmd_rx: mpsc::Receiver<Command>,
    resp_tx: mpsc::Sender<String>,
    config: ControllerConfig,
    target: SocketAddr,
) {
    let mut p = Player {
        track: None,
        meta: None,
        sample_offset: 0,
        base_offset: 0,
        playing: false,
        start_ns: 0,
        stream_start: Instant::now(),
        base_elapsed_ns: 0,
        seek_pending: false,
    };

    loop {
        // endpoint перезапустился (TCP умер): переподключаемся ДО обработки
        // команды, иначе play «пройдёт успешно» в никуда (backend покажет
        // playing, звука нет). Старый поток нежизнеспособен — честный paused.
        if !ep.is_reader_alive() {
            warn!("соединение с endpoint потеряно — переподключение к {target}");
            ep = connect_with_retry(&config, target).await;
            let _ = ep.clock_sync_bootstrap().await;
            p.playing = false;
        }
        if p.playing && p.track.is_some() {
            let t = p.track.as_ref().unwrap();
            let expected =
                Duration::from_nanos((p.sample_offset as f64 / t.sample_rate as f64 * 1e9) as u64);
            let elapsed = Duration::from_nanos(p.base_elapsed_ns) + p.stream_start.elapsed();
            let sleep = expected.saturating_sub(elapsed);

            tokio::select! {
                cmd = cmd_rx.recv() => {
                    match cmd {
                        Some(c) => {
                            let resp = handle(&mut ep, &mut p, c).await;
                            let _ = resp_tx.send(resp).await;
                        }
                        None => break,
                    }
                }
                _ = tokio::time::sleep(sleep) => {
                    send_next_packet(&mut ep, &mut p).await;
                }
            }
        } else {
            // тик 5с: даже без команд регулярно возвращаемся наверх цикла
            // для проверки живости — endpoint может перезапуститься в простое,
            // переподключение должно происходить без внешней команды.
            tokio::select! {
                c = cmd_rx.recv() => match c {
                    Some(c) => {
                        let resp = handle(&mut ep, &mut p, c).await;
                        let _ = resp_tx.send(resp).await;
                    }
                    None => break,
                },
                _ = tokio::time::sleep(Duration::from_secs(5)) => {}
            }
        }
    }
}

async fn spawn_clock_responder() -> u16 {
    match ClockResponder::spawn(
        (std::net::Ipv4Addr::UNSPECIFIED, oaat_core::DEFAULT_CLOCK_PORT).into(),
    )
    .await
    {
        Ok((port, _handle)) => port,
        Err(_) => match ClockResponder::spawn((std::net::Ipv4Addr::UNSPECIFIED, 0).into()).await {
            Ok((port, _handle)) => port,
            Err(e) => {
                warn!("clock responder unavailable: {e}");
                0
            }
        },
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let args = Args::parse();

    let clock_port = spawn_clock_responder().await;
    let config = ControllerConfig {
        controller_id: uuid::Uuid::new_v4().to_string(),
        controller_name: "ts-controller".into(),
        features: vec![],
        clock_port,
        tls: false,
    };
    let mut ep = connect_with_retry(&config, args.target).await;
    info!(target = %args.target, endpoint = %ep.info.endpoint_name, "подключён");
    if let Err(e) = ep.clock_sync_bootstrap().await {
        warn!("clock sync failed: {e}");
    }

    let _ = std::fs::remove_file(&args.socket);
    let listener = match UnixListener::bind(&args.socket) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("не открыть сокет {}: {e}", args.socket);
            std::process::exit(1);
        }
    };

    let (cmd_tx, cmd_rx) = mpsc::channel::<Command>(64);
    let (resp_tx, resp_rx) = mpsc::channel::<String>(64);

    tokio::spawn(async move {
        stream_loop(ep, cmd_rx, resp_tx, config, args.target).await;
    });

    info!(socket = %args.socket, "слушаю команды");
    // Один клиент за раз (backend держит постоянное соединение).
    let cmd_tx = cmd_tx;
    let mut resp_rx = resp_rx;
    loop {
        let (stream, _) = match listener.accept().await {
            Ok(x) => x,
            Err(e) => {
                warn!("accept error: {e}");
                continue;
            }
        };
        info!("клиент подключился");
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        loop {
            line.clear();
            let n = reader.read_line(&mut line).await.unwrap_or(0);
            if n == 0 {
                break;
            }
            let cmd: Command = match serde_json::from_str(line.trim()) {
                Ok(c) => c,
                Err(e) => {
                    let _ = reader
                        .get_mut()
                        .write_all(format!("{{\"error\":\"bad json: {e}\"}}\n").as_bytes())
                        .await;
                    continue;
                }
            };
            if cmd_tx.send(cmd).await.is_err() {
                break;
            }
            match resp_rx.recv().await {
                Some(resp) => {
                    let _ = reader.get_mut().write_all(format!("{resp}\n").as_bytes()).await;
                    let _ = reader.get_mut().flush().await;
                }
                None => break,
            }
        }
        info!("клиент отключился");
    }
}
