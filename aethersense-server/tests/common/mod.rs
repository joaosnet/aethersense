use aethersense_server::csi::packet::CsiPacket;
use aethersense_server::csi::recorder;
use std::fs;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

static LIVE_CAPTURE_MUTEX: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

struct LiveCaptureLease {
    path: PathBuf,
}

impl Drop for LiveCaptureLease {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn live_capture_mutex() -> &'static tokio::sync::Mutex<()> {
    LIVE_CAPTURE_MUTEX.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// Serializa o consumidor do tap tanto entre testes do mesmo binário quanto
/// entre processos `cargo test` concorrentes. O arquivo contém apenas PID e
/// porta; nenhum CSI é persistido.
async fn acquire_live_capture_lease(port: u16) -> Result<LiveCaptureLease, String> {
    let timeout_seconds = std::env::var("AETHERSENSE_LIVE_CAPTURE_LOCK_TIMEOUT_SECONDS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(180)
        .max(10);
    let timeout = Duration::from_secs(timeout_seconds);
    let stale_after = Duration::from_secs(timeout_seconds.saturating_mul(2).max(300));
    let path = std::env::temp_dir().join(format!("aethersense-live-tap-{port}.lock"));
    let started_at = Instant::now();

    loop {
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                let _ = writeln!(file, "pid={} port={}", std::process::id(), port);
                return Ok(LiveCaptureLease { path });
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let stale = fs::metadata(&path)
                    .and_then(|metadata| metadata.modified())
                    .and_then(|modified| modified.elapsed().map_err(std::io::Error::other))
                    .map(|age| age >= stale_after)
                    .unwrap_or(false);
                if stale {
                    let _ = fs::remove_file(&path);
                    continue;
                }
                if started_at.elapsed() >= timeout {
                    return Err(format!(
                        "tap UDP {port} permaneceu ocupado por outro teste durante {timeout_seconds}s"
                    ));
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(error) => {
                return Err(format!(
                    "não foi possível adquirir o mutex do tap UDP {port}: {error}"
                ));
            }
        }
    }
}

fn env_flag(name: &str) -> bool {
    std::env::var(name)
        .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

pub fn require_live_capture() -> bool {
    env_flag("AETHERSENSE_REQUIRE_LIVE_CAPTURE")
}

/// Captura o tap UDP somente por opt-in e persiste os pacotes apenas quando
/// `AETHERSENSE_SAVE_LIVE_CAPTURE=true` for definido explicitamente.
#[allow(dead_code)]
pub async fn capture_and_save(
    milestone: &str,
    test_name: &str,
    min_packets: usize,
) -> Vec<CsiPacket> {
    // Nunca disputar a porta 5005 com o relay de produção no Windows. O relay
    // duplica os datagramas para este tap, enquanto continua encaminhando o
    // fluxo principal ao backend Docker em 5006.
    let port = std::env::var("AETHERSENSE_TEST_UDP_PORT")
        .ok()
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(5007);
    let duration = if min_packets >= 500 { 60 } else { 2 };
    let require_live = require_live_capture();
    let try_live = require_live || env_flag("AETHERSENSE_TRY_LIVE_CAPTURE");
    let save_live = env_flag("AETHERSENSE_SAVE_LIVE_CAPTURE");

    if let Some(replay_capture) = std::env::var_os("AETHERSENSE_REPLAY_CAPTURE") {
        assert!(
            !require_live,
            "AETHERSENSE_REPLAY_CAPTURE e AETHERSENSE_REQUIRE_LIVE_CAPTURE=true sao incompatíveis: escolha replay explícito ou captura UDP ao vivo."
        );

        let path = PathBuf::from(replay_capture);
        assert!(
            path.is_file(),
            "AETHERSENSE_REPLAY_CAPTURE deve apontar para um arquivo de captura existente: {:?}",
            path
        );

        let packets = recorder::replay_from_file(&path);
        assert!(
            !packets.is_empty(),
            "AETHERSENSE_REPLAY_CAPTURE nao contem pacotes CSI_DATA validos: {:?}",
            path
        );
        println!(
            "📂 Usando {} pacotes do REPLAY EXPLÍCITO: {:?}",
            packets.len(),
            path
        );
        return packets;
    }

    let mut packets = if try_live {
        let _in_process_guard = live_capture_mutex().lock().await;
        match acquire_live_capture_lease(port).await {
            Ok(_cross_process_lease) => recorder::capture_live(port, duration, min_packets).await,
            Err(error) if require_live => {
                panic!("Captura live obrigatória não adquiriu o tap UDP: {error}")
            }
            Err(error) => {
                println!("⚠️ Captura live opcional ignorada: {error}");
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };

    if !packets.is_empty() {
        if require_live && packets.len() < min_packets {
            panic!(
                "Captura live obrigatoria recebeu {} de {} pacotes minimos via UDP.",
                packets.len(),
                min_packets
            );
        }

        if save_live {
            let timestamp = chrono::Local::now().format("%Y%m%dT%H%M%S");
            let path = PathBuf::from(format!(
                "tests/captures/{}_{}_{}.csv",
                timestamp, milestone, test_name
            ));
            recorder::save_capture(&packets, &path);
            println!(
                "💾 Captura ao vivo persistida explicitamente em {:?}.",
                path
            );
        } else {
            println!("🔒 Captura usada somente em memória; nenhum CSI foi gravado.");
        }
        println!(
            "📡 Usando {} pacotes REAIS recebidos via UDP.",
            packets.len()
        );
    } else {
        if require_live {
            panic!("Nenhum pacote real recebido via UDP e AETHERSENSE_REQUIRE_LIVE_CAPTURE=true.");
        }

        if try_live {
            println!("⚠️ Nenhum pacote recebido no tap UDP {}. Iniciando fallback para arquivos .csv locais...", port);
        } else {
            println!("📂 Captura UDP ao vivo desativada por padrão; usando replay local explícito/fallback.");
        }

        let dir = PathBuf::from("tests/captures");
        let mut fallback_candidates = Vec::new();
        if let Ok(entries) = fs::read_dir(&dir) {
            for entry in entries.filter_map(Result::ok) {
                let path = entry.path();
                if path.extension().map_or(false, |ext| ext == "csv") {
                    fallback_candidates.push(path);
                }
            }
        }

        fallback_candidates.sort_by(|left, right| {
            let score = |path: &PathBuf| {
                let name = path
                    .file_name()
                    .and_then(|value| value.to_str())
                    .unwrap_or_default();
                let mut score = 0;
                if name.contains(&format!("_{milestone}_")) {
                    score += 100;
                }
                if name.contains(test_name) {
                    score += 50;
                }
                score
            };
            let file_len = |path: &PathBuf| {
                fs::metadata(path)
                    .map(|metadata| metadata.len())
                    .unwrap_or(0)
            };

            score(right)
                .cmp(&score(left))
                .then_with(|| file_len(right).cmp(&file_len(left)))
        });

        let mut best_under_min: Option<(PathBuf, Vec<CsiPacket>)> = None;
        for path in fallback_candidates {
            let candidate_packets = recorder::replay_from_file(&path);
            if candidate_packets.len() >= min_packets {
                best_under_min = Some((path, candidate_packets));
                break;
            }

            if best_under_min
                .as_ref()
                .map(|(_, current_packets)| candidate_packets.len() > current_packets.len())
                .unwrap_or(true)
            {
                best_under_min = Some((path, candidate_packets));
            }
        }

        if let Some((path, loaded_packets)) = best_under_min {
            println!("📂 Carregando dados do arquivo: {:?}", path);
            packets = loaded_packets;

            // Limit to min_packets if we loaded a huge file, or just use all
            if packets.len() > min_packets * 2 {
                packets.truncate(min_packets * 2);
            }
        } else {
            println!("❌ Nenhum arquivo CSV encontrado para fallback.");
        }
    }

    packets
}

/// Macro for printing test summaries, following the planned output format
#[macro_export]
macro_rules! print_summary {
    ($id:expr, $desc:expr, $packets:expr, $params:expr) => {
        println!("\n═══════════════════════════════════════════════════════════");
        println!("  ✅ {}", $id);
        println!("───────────────────────────────────────────────────────────");

        let desc: &str = $desc;
        let mut first = true;
        for line in desc.lines() {
            if first {
                 println!("  📋 {}", line);
                 first = false;
            } else {
                 println!("     {}", line);
            }
        }

        let packets_len = $packets.len();
        println!("  📡 Pacotes capturados: {}", packets_len);

        if packets_len > 0 {
            let rssis: Vec<i32> = $packets.iter().filter_map(|p| p.rssi).collect();
            if !rssis.is_empty() {
                let min = rssis.iter().min().unwrap();
                let max = rssis.iter().max().unwrap();
                let avg = (rssis.iter().sum::<i32>() as f32) / (rssis.len() as f32);
                println!("  📶 RSSI: min={}, max={}, média={:.1} dBm", min, max, avg);
            }

            let sc_count = $packets[0].complex_samples.len();
            println!("  📊 Subportadoras por pacote: {}", sc_count);
        }

        for p in $params {
            println!("  ⚙️  {}", p);
        }

        println!("═══════════════════════════════════════════════════════════\n");
    };
}
