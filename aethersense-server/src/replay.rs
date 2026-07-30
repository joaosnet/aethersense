use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::csi::packet::CsiPacket;
use crate::csi::recorder::replay_from_file;
use crate::state::AppState;

pub const REPLAY_INTERVAL_MS: u64 = 100;
pub const DEFAULT_REPLAY_DIR: &str = "tests/captures";

pub fn replay_dir_from_env() -> PathBuf {
    std::env::var("AETHERSENSE_REPLAY_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(DEFAULT_REPLAY_DIR))
}

pub fn find_latest_m6_capture(dir: impl AsRef<Path>) -> Option<PathBuf> {
    let mut candidates = std::fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file()
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .map(|name| name.ends_with("_m6_benchmark.csv"))
                    .unwrap_or(false)
        })
        .collect::<Vec<_>>();

    candidates.sort_by(|left, right| {
        let left_name = left
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or_default();
        let right_name = right
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or_default();
        right_name.cmp(left_name)
    });

    candidates.into_iter().next()
}

pub fn load_default_capture(dir: impl AsRef<Path>) -> Result<(PathBuf, Vec<CsiPacket>), String> {
    let path = find_latest_m6_capture(dir.as_ref()).ok_or_else(|| {
        format!(
            "Nenhum arquivo *_m6_benchmark.csv encontrado em {}",
            dir.as_ref().display()
        )
    })?;
    let packets = replay_from_file(&path);

    if packets.is_empty() {
        return Err(format!(
            "Arquivo {} nao contem pacotes CSI validos",
            path.display()
        ));
    }

    Ok((path, packets))
}

pub fn display_name(path: &Path) -> String {
    path.file_name()
        .and_then(|value| value.to_str())
        .map(ToString::to_string)
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

pub fn replay_response(stream: &crate::state::StreamStatus, default_file: Option<String>) -> Value {
    json!({
        "ok": true,
        "stream": stream,
        "default_file": default_file,
    })
}

pub fn spawn_replay_loop(
    state: AppState,
    packets: Vec<CsiPacket>,
    generation: u64,
    replay_file: String,
) {
    if packets.is_empty() {
        // Defensive: avoid index-out-of-bounds / divide-by-zero on `% len()`.
        return;
    }

    tokio::spawn(async move {
        let interval = tokio::time::Duration::from_millis(REPLAY_INTERVAL_MS);
        let mut packet_index = 0usize;

        loop {
            let packet = packets[packet_index].clone();
            packet_index = (packet_index + 1) % packets.len();

            {
                let mut inner = state.write().await;
                if !inner.running
                    || !inner.stream.replay_active
                    || inner.replay_generation != generation
                {
                    break;
                }

                inner.ingest_replay_packet(packet, replay_file.clone());
            }

            tokio::time::sleep(interval).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "aethersense_replay_{}_{}_{}",
            name,
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system time before epoch")
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).expect("temp dir can be created");
        path
    }

    #[test]
    fn find_latest_m6_capture_prefers_newest_timestamp_name() {
        let dir = temp_dir("selection");
        std::fs::write(dir.join("20260510T100000_m6_benchmark.csv"), "")
            .expect("fixture can be written");
        std::fs::write(dir.join("20260511T100000_m4_geo_c5_wroom.csv"), "")
            .expect("fixture can be written");
        std::fs::write(dir.join("20260511T090000_m6_benchmark.csv"), "")
            .expect("fixture can be written");

        let selected = find_latest_m6_capture(&dir).expect("m6 capture should be selected");
        assert_eq!(
            selected.file_name().and_then(|value| value.to_str()),
            Some("20260511T090000_m6_benchmark.csv")
        );

        let _ = std::fs::remove_dir_all(dir);
    }
}
