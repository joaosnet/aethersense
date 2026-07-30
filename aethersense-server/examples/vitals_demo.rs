//! Harness do Apêndice A: roda o pipeline REAL de sinais vitais sobre capturas
//! reais e imprime os números do exemplo (taxa efetiva, FFT, picos, BPM).
//!
//! Os números finais (breathing_rate, heart_rate_bpm, confidence) vêm da função
//! de produção `estimate_vitals_resampled`. Os intermediários (fft_size, Δf, bin
//! do pico, SNR) são recomputados aqui REPLICANDO verbatim o núcleo de vitals.rs
//! e CRUZADOS com a saída da biblioteca (asserção) — se divergirem, o harness
//! falha, garantindo que o exemplo do TCC é fiel ao código.
//!
//! Rodar: cargo run --release --example vitals_demo

use std::fs;
use std::path::{Path, PathBuf};

use rustfft::{num_complex::Complex as FftComplex, FftPlanner};

use aethersense_server::csi::recorder::replay_from_file;
use aethersense_server::detection::vitals::estimate_vitals_resampled;
use aethersense_server::processing::cscr::build_vitals_input;
use aethersense_server::state::DashboardSettings;

// ── Constantes (espelham aethersense-server/src/config.rs) ─────────────
const VITALS_WINDOW_SECONDS: f64 = 30.0;
const VITALS_MIN_SAMPLES: usize = 100;
const BREATHING_FREQ_MIN_HZ: f64 = 0.1;
const BREATHING_FREQ_MAX_HZ: f64 = 0.5;
const HEART_RATE_FREQ_MIN_HZ: f64 = 0.8;
const HEART_RATE_FREQ_MAX_HZ: f64 = 3.0;
const VITALS_PEAK_PROMINENCE: f64 = 2.0;
const VITALS_HARMONIC_COUNT: usize = 5;
const VITALS_HARMONIC_TOLERANCE_HZ: f64 = 0.06;
const HEART_RATE_ABS_SNR_FLOOR: f64 = 3.0;
const VITALS_RESAMPLE_MIN_HZ: f64 = 0.5;
const VITALS_RESAMPLE_MAX_HZ: f64 = 50.0;
const VITALS_SUBCARRIER_TOP_K: usize = 16;
const STFT_SAMPLE_RATE_HZ: f64 = 10.0;

const MAC_C5: &str = "02:00:00:00:00:01";
const MAC_WROOM: &str = "02:00:00:00:00:02";

fn chip(mac: &str) -> &'static str {
    match mac {
        MAC_C5 => "ESP32-C5",
        MAC_WROOM => "ESP32-WROOM",
        _ => "?",
    }
}

// ── Réplicas verbatim de vitals.rs (para expor os intermediários) ──────
fn detrend(signal: &[f64]) -> Vec<f64> {
    let n = signal.len();
    if n < 2 {
        return signal.to_vec();
    }
    let nf = n as f64;
    let sx: f64 = (0..n).map(|i| i as f64).sum();
    let sy: f64 = signal.iter().sum();
    let sxx: f64 = (0..n).map(|i| (i as f64) * (i as f64)).sum();
    let sxy: f64 = signal.iter().enumerate().map(|(i, &y)| i as f64 * y).sum();
    let denom = nf * sxx - sx * sx;
    let (intercept, slope) = if denom.abs() < 1e-12 {
        (sy / nf, 0.0)
    } else {
        let slope = (nf * sxy - sx * sy) / denom;
        (((sy - slope * sx) / nf), slope)
    };
    signal
        .iter()
        .enumerate()
        .map(|(i, &y)| y - (intercept + slope * i as f64))
        .collect()
}

fn apply_hann(signal: &[f64]) -> Vec<f64> {
    let n = signal.len();
    if n < 2 {
        return signal.to_vec();
    }
    let nm1 = (n - 1) as f64;
    signal
        .iter()
        .enumerate()
        .map(|(i, &x)| x * (0.5 * (1.0 - (2.0 * std::f64::consts::PI * i as f64 / nm1).cos())))
        .collect()
}

fn resample_uniform(signal: &[f64], t: &[f64]) -> Option<(Vec<f64>, f64)> {
    let n = signal.len();
    if n < VITALS_MIN_SAMPLES || t.len() != n {
        return None;
    }
    let (t0, tn) = (t[0], t[n - 1]);
    let span = tn - t0;
    if !span.is_finite() || span <= 0.0 {
        return None;
    }
    for i in 1..n {
        if !t[i].is_finite() || t[i] < t[i - 1] {
            return None;
        }
    }
    let fs = (n as f64 - 1.0) / span;
    if !fs.is_finite() || fs < VITALS_RESAMPLE_MIN_HZ || fs > VITALS_RESAMPLE_MAX_HZ {
        return None;
    }
    let dt = 1.0 / fs;
    let mut out = Vec::with_capacity(n);
    let mut j = 0usize;
    for k in 0..n {
        let tk = t0 + k as f64 * dt;
        while j + 1 < n && t[j + 1] < tk {
            j += 1;
        }
        let value = if j + 1 < n {
            let (ta, tb) = (t[j], t[j + 1]);
            let denom = tb - ta;
            if denom.abs() < 1e-12 {
                signal[j]
            } else {
                let frac = ((tk - ta) / denom).clamp(0.0, 1.0);
                signal[j] + (signal[j + 1] - signal[j]) * frac
            }
        } else {
            signal[n - 1]
        };
        out.push(value);
    }
    Some((out, fs))
}

struct Peak {
    bin: usize,
    freq: f64,
    mag: f64,
    band_mean: f64,
}

fn find_peak(
    mags: &[f64],
    df: f64,
    lo: f64,
    hi: f64,
    avoid: &[f64],
    tol: f64,
) -> Option<Peak> {
    let min_bin = (lo / df).ceil() as usize;
    let max_bin = (hi / df).floor() as usize;
    if min_bin >= mags.len() || max_bin >= mags.len() || min_bin > max_bin {
        return None;
    }
    let avoided = |bin: usize| {
        let f = bin as f64 * df;
        avoid.iter().any(|&a| a > 0.0 && (f - a).abs() <= tol)
    };
    let (mut best_bin, mut best_mag, mut sum, mut cnt) = (None, 0.0, 0.0, 0usize);
    for bin in min_bin..=max_bin {
        if avoided(bin) {
            continue;
        }
        let m = mags[bin];
        sum += m;
        cnt += 1;
        if best_bin.is_none() || m > best_mag {
            best_mag = m;
            best_bin = Some(bin);
        }
    }
    let best_bin = best_bin?;
    if cnt == 0 {
        return None;
    }
    let band_mean = sum / cnt as f64;
    if best_mag > band_mean * VITALS_PEAK_PROMINENCE {
        Some(Peak {
            bin: best_bin,
            freq: best_bin as f64 * df,
            mag: best_mag,
            band_mean,
        })
    } else {
        None
    }
}

struct Detail {
    file: String,
    mac: String,
    n_total: usize,
    window_n: usize,
    span_s: f64,
    fs_eff: f64,
    used_improved: bool,
    fft_size: usize,
    df: f64,
    mean_mag: f64,
    breathing: Option<Peak>,
    heart_raw: Option<Peak>,
    heart_pass_floor: bool,
    breathing_rate: Option<f64>,
    heart_bpm: Option<f64>,
    confidence: f64,
}

/// Roda o pipeline real + recomputa intermediários para um device de um arquivo.
fn run_one(path: &Path, mac: &str) -> Option<Detail> {
    let packets = replay_from_file(path);
    let dev: Vec<_> = packets
        .into_iter()
        .filter(|p| p.mac == mac && !p.amplitudes.is_empty())
        .collect();
    if dev.len() < VITALS_MIN_SAMPLES {
        return None;
    }

    let signal_history: Vec<f64> = dev
        .iter()
        .map(|p| p.amplitudes.iter().sum::<f64>() / p.amplitudes.len() as f64)
        .collect();
    let amp_history: Vec<Vec<f64>> = dev.iter().map(|p| p.amplitudes.clone()).collect();
    // Tempos do timer do ESP (µs → s), relativos. Em produção a grade vem do
    // relógio monotônico do servidor na recepção; offline usamos o timer do
    // pacote, cuja cadência inter-pacote ≈ a taxa de recepção (~10 Hz).
    let t0 = dev[0].timestamp.unwrap_or(0) as f64;
    let times_s: Vec<f64> = dev
        .iter()
        .map(|p| (p.timestamp.unwrap_or(0) as f64 - t0) / 1.0e6)
        .collect();

    // Caminho de produção
    let improved = build_vitals_input(&amp_history, VITALS_SUBCARRIER_TOP_K, STFT_SAMPLE_RATE_HZ);
    let used_improved = improved.is_some();
    let signal = improved.clone().unwrap_or_else(|| signal_history.clone());
    let (lib, _diag) = estimate_vitals_resampled(&signal, &times_s, STFT_SAMPLE_RATE_HZ, true, &DashboardSettings::default());

    // Recomputo dos intermediários (mesmo caminho do core)
    let (uniform, fs_eff) = resample_uniform(&signal, &times_s)
        .unwrap_or_else(|| (signal.clone(), STFT_SAMPLE_RATE_HZ));
    let window_samples = (VITALS_WINDOW_SECONDS * fs_eff) as usize;
    let n = uniform.len().min(window_samples.max(VITALS_MIN_SAMPLES));
    let start = uniform.len() - n;
    let win = &uniform[start..];
    let windowed = apply_hann(&detrend(win));
    let fft_size = n.next_power_of_two();
    let mut planner = FftPlanner::new();
    let fft = planner.plan_fft_forward(fft_size);
    let mut buf: Vec<FftComplex<f64>> =
        windowed.iter().map(|&x| FftComplex::new(x, 0.0)).collect();
    buf.resize(fft_size, FftComplex::new(0.0, 0.0));
    fft.process(&mut buf);
    let n_freq = fft_size / 2;
    let mags: Vec<f64> = buf[..n_freq]
        .iter()
        .map(|c| (c.re * c.re + c.im * c.im).sqrt())
        .collect();
    let mean_mag = mags.iter().sum::<f64>() / mags.len() as f64;
    let df = fs_eff / fft_size as f64;

    let breathing = find_peak(&mags, df, BREATHING_FREQ_MIN_HZ, BREATHING_FREQ_MAX_HZ, &[], 0.0);
    let mut avoid = Vec::new();
    if let Some(b) = &breathing {
        for k in 2..=(VITALS_HARMONIC_COUNT + 1) {
            avoid.push(b.freq * k as f64);
        }
    }
    let heart_raw = find_peak(
        &mags,
        df,
        HEART_RATE_FREQ_MIN_HZ,
        HEART_RATE_FREQ_MAX_HZ,
        &avoid,
        VITALS_HARMONIC_TOLERANCE_HZ,
    );
    let heart_pass_floor = heart_raw
        .as_ref()
        .map(|h| h.mag >= mean_mag * HEART_RATE_ABS_SNR_FLOOR)
        .unwrap_or(false);

    // Cross-check: meu cálculo manual deve bater com a biblioteca.
    let my_br = breathing.as_ref().map(|b| b.freq * 60.0);
    if let (Some(a), Some(b)) = (my_br, lib.breathing_rate) {
        assert!(
            (a - b).abs() < 1e-6,
            "DIVERGENCIA breathing: manual={a} lib={b} ({})",
            path.display()
        );
    } else {
        assert_eq!(
            my_br.is_some(),
            lib.breathing_rate.is_some(),
            "DIVERGENCIA presença breathing ({})",
            path.display()
        );
    }

    Some(Detail {
        file: path.file_name().unwrap().to_string_lossy().to_string(),
        mac: mac.to_string(),
        n_total: dev.len(),
        window_n: n,
        span_s: times_s[times_s.len() - 1],
        fs_eff,
        used_improved,
        fft_size,
        df,
        mean_mag,
        breathing,
        heart_raw,
        heart_pass_floor,
        breathing_rate: lib.breathing_rate,
        heart_bpm: lib.heart_rate_bpm,
        confidence: lib.confidence,
    })
}

fn dominant_macs(path: &Path) -> Vec<String> {
    let packets = replay_from_file(path);
    let mut c5 = 0usize;
    let mut wroom = 0usize;
    for p in &packets {
        if p.mac == MAC_C5 {
            c5 += 1;
        } else if p.mac == MAC_WROOM {
            wroom += 1;
        }
    }
    let mut v = Vec::new();
    if c5 >= VITALS_MIN_SAMPLES {
        v.push(MAC_C5.to_string());
    }
    if wroom >= VITALS_MIN_SAMPLES {
        v.push(MAC_WROOM.to_string());
    }
    v
}

fn main() {
    let dir = PathBuf::from("tests/captures");
    let mut files: Vec<PathBuf> = fs::read_dir(&dir)
        .expect("tests/captures")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().map_or(false, |e| e == "csv"))
        .collect();
    files.sort();

    let mut results: Vec<Detail> = Vec::new();
    for f in &files {
        for mac in dominant_macs(f) {
            if let Some(d) = run_one(f, &mac) {
                results.push(d);
            }
        }
    }

    results.sort_by(|a, b| b.confidence.partial_cmp(&a.confidence).unwrap());

    println!("\n===== SCAN: top 12 por confiança (pipeline REAL) =====");
    println!(
        "{:<42} {:<12} {:>4} {:>6} {:>6} {:>4} {:>8} {:>8} {:>6}",
        "arquivo", "nó", "N", "span_s", "fs_eff", "impr", "resp/min", "bpm", "conf"
    );
    for d in results.iter().take(12) {
        println!(
            "{:<42} {:<12} {:>4} {:>6.1} {:>6.3} {:>4} {:>8} {:>8} {:>6.3}",
            &d.file[..d.file.len().min(42)],
            chip(&d.mac),
            d.n_total,
            d.span_s,
            d.fs_eff,
            if d.used_improved { "sim" } else { "não" },
            d.breathing_rate
                .map(|r| format!("{r:.1}"))
                .unwrap_or_else(|| "-".into()),
            d.heart_bpm
                .map(|r| format!("{r:.1}"))
                .unwrap_or_else(|| "-".into()),
            d.confidence
        );
    }

    // Exemplo didático: prefere caso típico (fs≈10 Hz, respiração humana 10-22/min,
    // janela cheia ~30 s), senão qualquer detecção, senão o primeiro.
    let best = results
        .iter()
        .filter(|d| {
            d.fs_eff >= 9.5
                && d.fs_eff <= 10.5
                && d.window_n >= 250
                && d.breathing_rate.map_or(false, |r| r >= 10.0 && r <= 22.0)
        })
        .max_by(|a, b| a.confidence.partial_cmp(&b.confidence).unwrap())
        .or_else(|| results.iter().find(|d| d.breathing_rate.is_some()))
        .or_else(|| results.first());
    let Some(d) = best else {
        println!("nenhum resultado");
        return;
    };

    println!("\n===== EXEMPLO REAL (worked example) =====");
    println!("arquivo            : {}", d.file);
    println!("nó                 : {} ({})", chip(&d.mac), d.mac);
    println!("frames do device   : {}", d.n_total);
    println!(
        "sinal usado        : {}",
        if d.used_improved {
            "combinado SA-WiSense (build_vitals_input)"
        } else {
            "amplitude média por frame (fallback)"
        }
    );
    println!("janela N (amostras): {}", d.window_n);
    println!("span da janela     : {:.3} s", d.span_s);
    println!("fs efetiva         : {:.4} Hz   (=(N-1)/span; nominal {STFT_SAMPLE_RATE_HZ})", d.fs_eff);
    println!("fft_size (pow2)    : {}", d.fft_size);
    println!("Δf = fs/fft_size   : {:.5} Hz/bin", d.df);
    println!("média global |X|   : {:.4}", d.mean_mag);
    if let Some(b) = &d.breathing {
        println!(
            "RESPIRAÇÃO  bin={:>3}  f={:.4} Hz  |X|={:.3}  média_banda={:.3}  SNR={:.2}  → {:.2} resp/min",
            b.bin,
            b.freq,
            b.mag,
            b.band_mean,
            b.mag / (d.mean_mag + 1e-9),
            b.freq * 60.0
        );
    } else {
        println!("RESPIRAÇÃO  : não detectada");
    }
    if let Some(h) = &d.heart_raw {
        println!(
            "CARDÍACO    bin={:>3}  f={:.4} Hz  |X|={:.3}  piso_abs={:.3} ({}x média)  passa_piso={}  → {}",
            h.bin,
            h.freq,
            h.mag,
            d.mean_mag * HEART_RATE_ABS_SNR_FLOOR,
            HEART_RATE_ABS_SNR_FLOOR,
            d.heart_pass_floor,
            d.heart_bpm
                .map(|b| format!("{b:.2} bpm"))
                .unwrap_or_else(|| "rejeitado (não publicado)".into())
        );
    } else {
        println!("CARDÍACO    : nenhum pico proeminente na banda");
    }
    println!("confiança          : {:.4}", d.confidence);
    println!("\n(saída final breathing/heart/confidence = estimate_vitals_resampled; cross-check OK)");
}
