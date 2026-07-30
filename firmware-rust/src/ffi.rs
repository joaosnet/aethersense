#[cfg(feature = "esp-idf")]
use core::ffi::c_void;
#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
use esp_idf_sys as sys;
use std::fmt::Write;
use std::ptr;
#[cfg(feature = "esp-idf")]
use std::sync::atomic::{AtomicU32, Ordering};

use crate::config::DEFAULT_DEVICE_MAC;
use crate::csi::CsiFrame;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureProfile {
    Classic,
    Esp32C5Like,
    Esp32C6Like,
    Unknown,
}

impl CaptureProfile {
    pub fn from_hardware_profile(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "classic" | "esp32" | "wroom" => Self::Classic,
            "c5" | "esp32c5" => Self::Esp32C5Like,
            "c6" | "esp32c6" | "c61" | "esp32c61" => Self::Esp32C6Like,
            _ => Self::Unknown,
        }
    }

    pub fn capture_config(self) -> CsiCaptureConfig {
        match self {
            Self::Classic => CsiCaptureConfig::classic(),
            Self::Esp32C5Like => CsiCaptureConfig::esp32c5(false),
            Self::Esp32C6Like => CsiCaptureConfig::esp32c6(),
            Self::Unknown => CsiCaptureConfig::classic(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CsiCaptureConfig {
    pub profile: CaptureProfile,
    pub lltf_en: bool,
    pub htltf_en: bool,
    pub stbc_htltf2_en: bool,
    pub ltf_merge_en: bool,
    pub channel_filter_en: bool,
    pub manu_scale: bool,
    pub shift: bool,
    pub acquire_csi_legacy: bool,
    pub acquire_csi_force_lltf: bool,
    pub acquire_csi_ht20: bool,
    pub acquire_csi_ht40: bool,
    pub acquire_csi_vht: bool,
    pub acquire_csi_su: bool,
    pub acquire_csi_mu: bool,
    pub acquire_csi_dcm: bool,
    pub acquire_csi_beamformed: bool,
    pub acquire_csi_he_stbc_mode: u8,
    pub val_scale_cfg: u8,
    pub dump_ack_en: bool,
    pub reserved: bool,
    /// v5.5.4 (#18493): habilita o modo de bits da L-LTF — fix da CSI congelada
    /// em 5 GHz no C5. Empacotado no bit 17 do struct versão 3.
    pub lltf_bit_mode: bool,
}

impl CsiCaptureConfig {
    pub fn classic() -> Self {
        Self {
            profile: CaptureProfile::Classic,
            lltf_en: true,
            htltf_en: false,
            stbc_htltf2_en: false,
            ltf_merge_en: true,
            channel_filter_en: true,
            manu_scale: false,
            shift: false,
            acquire_csi_legacy: true,
            acquire_csi_force_lltf: false,
            acquire_csi_ht20: false,
            acquire_csi_ht40: false,
            acquire_csi_vht: false,
            acquire_csi_su: false,
            acquire_csi_mu: false,
            acquire_csi_dcm: false,
            acquire_csi_beamformed: false,
            acquire_csi_he_stbc_mode: 0,
            val_scale_cfg: 0,
            dump_ack_en: false,
            reserved: false,
            lltf_bit_mode: false,
        }
    }

    pub fn esp32c5(force_lltf: bool) -> Self {
        Self {
            profile: CaptureProfile::Esp32C5Like,
            lltf_en: false,
            htltf_en: false,
            stbc_htltf2_en: false,
            ltf_merge_en: false,
            channel_filter_en: false,
            manu_scale: false,
            shift: false,
            acquire_csi_legacy: true,
            acquire_csi_force_lltf: force_lltf,
            acquire_csi_ht20: true,
            acquire_csi_ht40: true,
            acquire_csi_vht: false,
            acquire_csi_su: false,
            acquire_csi_mu: false,
            acquire_csi_dcm: false,
            acquire_csi_beamformed: false,
            acquire_csi_he_stbc_mode: 2,
            val_scale_cfg: 0,
            dump_ack_en: false,
            reserved: false,
            lltf_bit_mode: false,
        }
    }

    pub fn esp32c6() -> Self {
        Self {
            profile: CaptureProfile::Esp32C6Like,
            lltf_en: false,
            htltf_en: false,
            stbc_htltf2_en: false,
            ltf_merge_en: false,
            channel_filter_en: false,
            manu_scale: false,
            shift: false,
            acquire_csi_legacy: true,
            acquire_csi_force_lltf: false,
            acquire_csi_ht20: true,
            acquire_csi_ht40: true,
            acquire_csi_vht: false,
            acquire_csi_su: false,
            acquire_csi_mu: false,
            acquire_csi_dcm: false,
            acquire_csi_beamformed: false,
            acquire_csi_he_stbc_mode: 2,
            val_scale_cfg: 0,
            dump_ack_en: false,
            reserved: false,
            lltf_bit_mode: false,
        }
    }

    /// Liga `lltf_bit_mode` (fix #18493 da CSI congelada em 5 GHz) apenas no C5 e
    /// apenas em 5 GHz. Em 2,4 GHz mantém desligado, preservando a captura e o
    /// formato de dados já validados nessa banda.
    pub fn apply_band(mut self, wifi_band: &str) -> Self {
        let is_5ghz = matches!(
            wifi_band.trim().to_ascii_lowercase().as_str(),
            "5g" | "5ghz"
        );
        self.lltf_bit_mode = is_5ghz && self.profile == CaptureProfile::Esp32C5Like;
        self
    }

    #[allow(dead_code)]
    fn esp32c5_packed_bits(self) -> u32 {
        let mut bits = 0u32;
        bits |= 1 << 0;
        bits |= (self.acquire_csi_legacy as u32) << 1;
        bits |= (self.acquire_csi_force_lltf as u32) << 2;
        bits |= (self.acquire_csi_ht20 as u32) << 3;
        bits |= (self.acquire_csi_ht40 as u32) << 4;
        bits |= (self.acquire_csi_vht as u32) << 5;
        bits |= (self.acquire_csi_su as u32) << 6;
        bits |= (self.acquire_csi_mu as u32) << 7;
        bits |= (self.acquire_csi_dcm as u32) << 8;
        bits |= (self.acquire_csi_beamformed as u32) << 9;
        bits |= ((self.acquire_csi_he_stbc_mode as u32) & 0b11) << 10;
        // Layout do struct versão 3 (ESP-IDF v5.5.4): val_scale com 4 bits (12-15),
        // dump_ack no bit 16 e lltf_bit_mode no bit 17 (fix #18493 da CSI em 5 GHz).
        // O C6 (v5.4.2) deixa esses campos em 0, então o u32 resultante é idêntico
        // ao layout antigo — só o C5 muda (bit 17).
        bits |= ((self.val_scale_cfg as u32) & 0b1111) << 12;
        bits |= (self.dump_ack_en as u32) << 16;
        bits |= (self.lltf_bit_mode as u32) << 17;
        bits
    }

    #[cfg(all(
        feature = "esp-idf",
        target_os = "espidf",
        any(aethersense_esp32_classic, target_arch = "xtensa")
    ))]
    fn to_esp_idf_config(self) -> RawWifiCsiConfig {
        let mut config = unsafe { core::mem::zeroed::<sys::wifi_csi_config_t>() };
        config.lltf_en = self.lltf_en;
        config.htltf_en = self.htltf_en;
        config.stbc_htltf2_en = self.stbc_htltf2_en;
        config.ltf_merge_en = self.ltf_merge_en;
        config.channel_filter_en = self.channel_filter_en;
        config.manu_scale = self.manu_scale;
        config.shift = if self.shift { 1 } else { 0 };
        config.dump_ack_en = self.dump_ack_en;
        RawWifiCsiConfig::Native(config)
    }

    #[cfg(all(
        feature = "esp-idf",
        target_os = "espidf",
        not(any(aethersense_esp32_classic, target_arch = "xtensa"))
    ))]
    fn to_esp_idf_config(self) -> RawWifiCsiConfig {
        RawWifiCsiConfig::Packed(Esp32C5WifiCsiConfig {
            bits: self.esp32c5_packed_bits(),
        })
    }
}

#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
#[repr(C)]
#[derive(Clone, Copy)]
struct Esp32C5WifiCsiConfig {
    bits: u32,
}

#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
enum RawWifiCsiConfig {
    #[allow(dead_code)]
    Native(sys::wifi_csi_config_t),
    #[allow(dead_code)]
    Packed(Esp32C5WifiCsiConfig),
}

#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
impl RawWifiCsiConfig {
    fn as_ptr(&self) -> *const sys::wifi_csi_config_t {
        match self {
            Self::Native(config) => config as *const sys::wifi_csi_config_t,
            Self::Packed(config) => {
                config as *const Esp32C5WifiCsiConfig as *const sys::wifi_csi_config_t
            }
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WifiPktRxCtrl {
    pub rssi: i32,
    pub rate: i32,
    pub sig_mode: i32,
    pub mcs: i32,
    pub cwb: i32,
    pub smoothing: i32,
    pub not_sounding: i32,
    pub aggregation: i32,
    pub stbc: i32,
    pub fec_coding: i32,
    pub sgi: i32,
    pub noise_floor: i32,
    pub ampdu_cnt: i32,
    pub channel: i32,
    pub secondary_channel: i32,
    pub timestamp: i64,
    pub ant: i32,
    pub sig_len: i32,
    pub rx_state: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WifiCsiInfo {
    pub mac: [u8; 6],
    pub rx_ctrl: WifiPktRxCtrl,
    pub len: usize,
    pub first_word_invalid: bool,
    pub buf: *const u8,
}

impl Default for WifiCsiInfo {
    fn default() -> Self {
        Self {
            mac: [0; 6],
            rx_ctrl: WifiPktRxCtrl::default(),
            len: 0,
            first_word_invalid: false,
            buf: ptr::null(),
        }
    }
}

#[allow(dead_code)]
pub(crate) trait WifiCsiInfoView {
    fn mac(&self) -> [u8; 6];
    fn rx_ctrl_snapshot(&self) -> WifiPktRxCtrl;
    fn payload_ptr(&self) -> *const i8;
    fn payload_len(&self) -> usize;
    fn first_word_invalid(&self) -> bool;
    fn csi_valid(&self) -> bool {
        true
    }
}

impl WifiCsiInfoView for WifiCsiInfo {
    fn mac(&self) -> [u8; 6] {
        self.mac
    }

    fn rx_ctrl_snapshot(&self) -> WifiPktRxCtrl {
        self.rx_ctrl
    }

    fn payload_ptr(&self) -> *const i8 {
        self.buf as *const i8
    }

    fn payload_len(&self) -> usize {
        self.len
    }

    fn first_word_invalid(&self) -> bool {
        self.first_word_invalid
    }
}

#[cfg(all(
    feature = "esp-idf",
    target_os = "espidf",
    any(aethersense_esp32_classic, target_arch = "xtensa")
))]
impl WifiCsiInfoView for sys::wifi_csi_info_t {
    fn mac(&self) -> [u8; 6] {
        self.mac
    }

    fn rx_ctrl_snapshot(&self) -> WifiPktRxCtrl {
        let rx_ctrl = &self.rx_ctrl;
        WifiPktRxCtrl {
            rssi: rx_ctrl.rssi() as i8 as i32,
            rate: rx_ctrl.rate() as i32,
            sig_mode: rx_ctrl.sig_mode() as i32,
            mcs: rx_ctrl.mcs() as i32,
            cwb: rx_ctrl.cwb() as i32,
            smoothing: rx_ctrl.smoothing() as i32,
            not_sounding: rx_ctrl.not_sounding() as i32,
            aggregation: rx_ctrl.aggregation() as i32,
            stbc: rx_ctrl.stbc() as i32,
            fec_coding: rx_ctrl.fec_coding() as i32,
            sgi: rx_ctrl.sgi() as i32,
            noise_floor: rx_ctrl.noise_floor() as i8 as i32,
            ampdu_cnt: rx_ctrl.ampdu_cnt() as i32,
            channel: rx_ctrl.channel() as i32,
            secondary_channel: rx_ctrl.secondary_channel() as i32,
            timestamp: rx_ctrl.timestamp() as i64,
            ant: rx_ctrl.ant() as i32,
            sig_len: rx_ctrl.sig_len() as i32,
            rx_state: rx_ctrl.rx_state() as i32,
        }
    }

    fn payload_ptr(&self) -> *const i8 {
        self.buf as *const i8
    }

    fn payload_len(&self) -> usize {
        self.len as usize
    }

    fn first_word_invalid(&self) -> bool {
        self.first_word_invalid
    }

    fn csi_valid(&self) -> bool {
        self.rx_ctrl.rx_state() == 0
    }
}

#[cfg(all(
    feature = "esp-idf",
    target_os = "espidf",
    not(any(aethersense_esp32_classic, target_arch = "xtensa"))
))]
impl WifiCsiInfoView for sys::wifi_csi_info_t {
    fn mac(&self) -> [u8; 6] {
        self.mac
    }

    fn rx_ctrl_snapshot(&self) -> WifiPktRxCtrl {
        let rx_ctrl = &self.rx_ctrl;
        WifiPktRxCtrl {
            rssi: rx_ctrl.rssi() as i8 as i32,
            rate: rx_ctrl.rate() as i32,
            sig_mode: 0,
            mcs: 0,
            cwb: 0,
            smoothing: 0,
            not_sounding: 0,
            aggregation: 0,
            stbc: 0,
            fec_coding: 0,
            sgi: 0,
            noise_floor: rx_ctrl.noise_floor() as i8 as i32,
            ampdu_cnt: 0,
            channel: rx_ctrl.channel() as i32,
            secondary_channel: 0,
            timestamp: rx_ctrl.timestamp() as i64,
            ant: 0,
            sig_len: rx_ctrl.sig_len() as i32,
            rx_state: rx_ctrl.rx_state() as i32,
        }
    }

    fn payload_ptr(&self) -> *const i8 {
        self.buf as *const i8
    }

    fn payload_len(&self) -> usize {
        self.len as usize
    }

    fn first_word_invalid(&self) -> bool {
        self.first_word_invalid
    }

    fn csi_valid(&self) -> bool {
        self.rx_ctrl.rx_state() == 0
    }
}

pub fn format_mac(mac: &[u8; 6]) -> String {
    let mut formatted = String::with_capacity(17);
    for (index, byte) in mac.iter().enumerate() {
        if index > 0 {
            formatted.push(':');
        }
        write!(&mut formatted, "{:02x}", byte).expect("formatting MAC address");
    }
    formatted
}

fn stable_capture_mac(context: &EspIdfCallbackContext) -> String {
    context
        .device_mac
        .map(|mac| format_mac(&mac))
        .unwrap_or_else(|| DEFAULT_DEVICE_MAC.to_string())
}

#[allow(dead_code)]
pub(crate) unsafe fn frame_from_wifi_csi_info<T: WifiCsiInfoView>(
    seq: u32,
    info: &T,
) -> Option<CsiFrame> {
    if !info.csi_valid() || info.payload_ptr().is_null() || info.payload_len() == 0 {
        return None;
    }

    // This callback runs in the high-priority Wi-Fi task. An infallible
    // `collect::<Vec<_>>()` aborts the whole device when another peripheral
    // (notably SDSPI/FAT during mount) temporarily fragments the internal
    // heap. Reserve fallibly and drop only this frame on memory pressure.
    let payload_len = info.payload_len();
    let mut samples = Vec::new();
    if samples.try_reserve_exact(payload_len).is_err() {
        return None;
    }
    samples.extend(
        std::slice::from_raw_parts(info.payload_ptr(), payload_len)
            .iter()
            .map(|&value| value as i16),
    );

    let rx_ctrl = info.rx_ctrl_snapshot();

    Some(CsiFrame {
        seq,
        mac: format_mac(&info.mac()),
        rssi: rx_ctrl.rssi,
        rate: rx_ctrl.rate,
        sig_mode: rx_ctrl.sig_mode,
        mcs: rx_ctrl.mcs,
        cwb: rx_ctrl.cwb,
        smoothing: rx_ctrl.smoothing,
        not_sounding: rx_ctrl.not_sounding,
        aggregation: rx_ctrl.aggregation,
        stbc: rx_ctrl.stbc,
        fec_coding: rx_ctrl.fec_coding,
        sgi: rx_ctrl.sgi,
        noise_floor: rx_ctrl.noise_floor,
        ampdu_cnt: rx_ctrl.ampdu_cnt,
        channel: rx_ctrl.channel,
        secondary_channel: rx_ctrl.secondary_channel,
        timestamp: rx_ctrl.timestamp,
        ant: rx_ctrl.ant,
        sig_len: rx_ctrl.sig_len,
        rx_state: rx_ctrl.rx_state,
        len: samples.len() as i32,
        first_word_invalid: if info.first_word_invalid() { 1 } else { 0 },
        payload: samples,
    })
}

#[cfg(feature = "esp-idf")]
pub struct EspIdfCallbackContext {
    ingress: crate::runtime::PacketIngress,
    seq: AtomicU32,
    capture_config: CsiCaptureConfig,
    ap_bssid: Option<[u8; 6]>,
    device_mac: Option<[u8; 6]>,
}

#[cfg(feature = "esp-idf")]
pub struct EspIdfSourceHandle {
    #[cfg(all(feature = "esp-idf", target_os = "espidf"))]
    _wifi: crate::wifi::WifiConnection,
    _context: Box<EspIdfCallbackContext>,
}

#[cfg(feature = "esp-idf")]
#[cfg(all(target_os = "espidf"))]
pub type EspIdfWifiCsiCallback = unsafe extern "C" fn(*mut c_void, *mut sys::wifi_csi_info_t);

#[cfg(feature = "esp-idf")]
#[cfg(not(target_os = "espidf"))]
pub type EspIdfWifiCsiCallback = unsafe extern "C" fn(*mut c_void, *const WifiCsiInfo);

#[cfg(feature = "esp-idf")]
#[derive(Clone, Copy, Debug)]
pub struct EspIdfCallbackBinding {
    pub context: *mut c_void,
    pub callback: EspIdfWifiCsiCallback,
    pub capture_config: CsiCaptureConfig,
}

#[cfg(feature = "esp-idf")]
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
enum EspIdfInstallError {
    EspError { stage: &'static str, code: i32 },
}

#[cfg(feature = "esp-idf")]
impl std::fmt::Display for EspIdfInstallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EspError { stage, code } => write!(f, "{} failed with esp_err_t {}", stage, code),
        }
    }
}

#[cfg(feature = "esp-idf")]
impl std::error::Error for EspIdfInstallError {}

#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
fn check_esp_err(stage: &'static str, code: i32) -> Result<(), EspIdfInstallError> {
    if code == 0 {
        Ok(())
    } else {
        Err(EspIdfInstallError::EspError { stage, code })
    }
}

#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
unsafe fn install_promiscuous_filter() -> Result<(), EspIdfInstallError> {
    let promiscuous_filter = sys::wifi_promiscuous_filter_t {
        filter_mask: sys::WIFI_PROMIS_FILTER_MASK_DATA | sys::WIFI_PROMIS_FILTER_MASK_MGMT,
    };

    check_esp_err(
        "esp_wifi_set_promiscuous_filter",
        sys::esp_wifi_set_promiscuous_filter(&promiscuous_filter),
    )
}

#[cfg(feature = "esp-idf")]
impl EspIdfCallbackBinding {
    #[allow(dead_code)]
    fn install(&self) -> Result<(), EspIdfInstallError> {
        #[cfg(all(feature = "esp-idf", target_os = "espidf"))]
        {
            unsafe {
                let csi_config = self.capture_config.to_esp_idf_config();

                check_esp_err(
                    "esp_wifi_set_promiscuous",
                    sys::esp_wifi_set_promiscuous(true),
                )?;
                install_promiscuous_filter()?;
                check_esp_err(
                    "esp_wifi_set_csi_config",
                    sys::esp_wifi_set_csi_config(csi_config.as_ptr()),
                )?;
                check_esp_err(
                    "esp_wifi_set_csi_rx_cb",
                    sys::esp_wifi_set_csi_rx_cb(Some(self.callback), self.context),
                )?;
            }

            return Ok(());
        }

        #[cfg(not(all(feature = "esp-idf", target_os = "espidf")))]
        {
            let _ = self;
            Ok(())
        }
    }
}

#[cfg(feature = "esp-idf")]
impl EspIdfSourceHandle {
    #[cfg(all(feature = "esp-idf", target_os = "espidf"))]
    pub fn new(
        ingress: crate::runtime::PacketIngress,
        capture_config: CsiCaptureConfig,
        wifi_connection: crate::wifi::WifiConnection,
    ) -> Self {
        let ap_bssid = if capture_config.profile == CaptureProfile::Classic {
            Some(wifi_connection.ap_bssid())
        } else {
            None
        };
        let device_mac = wifi_connection.station_mac();
        let handle = Self {
            _wifi: wifi_connection,
            _context: Box::new(EspIdfCallbackContext {
                ingress,
                seq: AtomicU32::new(0),
                capture_config,
                ap_bssid,
                device_mac: Some(device_mac),
            }),
        };

        let binding = handle.binding();
        if let Err(err) = binding.install() {
            panic!("failed to install ESP-IDF CSI callback: {}", err);
        }

        // Capture is intentionally left disabled. `main` starts it only after
        // SDSPI/FAT and the optional accessory workers complete their startup
        // handshakes, avoiding concurrent CSI payload allocation at the heap's
        // highest-pressure point.

        handle
    }

    #[cfg(not(target_os = "espidf"))]
    pub fn new(ingress: crate::runtime::PacketIngress, capture_config: CsiCaptureConfig) -> Self {
        let handle = Self {
            _context: Box::new(EspIdfCallbackContext {
                ingress,
                seq: AtomicU32::new(0),
                capture_config,
                ap_bssid: None,
                device_mac: None,
            }),
        };

        #[cfg(all(feature = "esp-idf", target_os = "espidf"))]
        {
            let binding = handle.binding();
            if let Err(err) = binding.install() {
                panic!("failed to install ESP-IDF CSI callback: {}", err);
            }
        }

        handle
    }

    pub fn as_ptr(&self) -> *mut c_void {
        self._context.as_ref() as *const EspIdfCallbackContext as *mut c_void
    }

    pub fn capture_config(&self) -> CsiCaptureConfig {
        self._context.capture_config
    }

    pub fn binding(&self) -> EspIdfCallbackBinding {
        EspIdfCallbackBinding {
            context: self.as_ptr(),
            callback: wifi_csi_rx_cb,
            capture_config: self.capture_config(),
        }
    }

    /// Temporarily gates CSI production while memory-heavy peripherals are
    /// initialized. The callback/configuration remain installed, so capture
    /// can resume without reconnecting Wi-Fi or moving its context pointer.
    pub fn set_capture_enabled(&self, enabled: bool) -> Result<(), i32> {
        #[cfg(all(feature = "esp-idf", target_os = "espidf"))]
        unsafe {
            let code = sys::esp_wifi_set_csi(enabled);
            if code == 0 {
                Ok(())
            } else {
                Err(code)
            }
        }

        #[cfg(not(all(feature = "esp-idf", target_os = "espidf")))]
        {
            let _ = enabled;
            Ok(())
        }
    }
}

#[cfg(feature = "esp-idf")]
impl EspIdfCallbackContext {
    fn next_seq(&self) -> u32 {
        self.seq.fetch_add(1, Ordering::Relaxed)
    }

    fn callback_decimation(&self) -> u32 {
        // Override por env de build: decimar MENOS eleva a taxa de CSI e, logo, o
        // Nyquist do batimento cardíaco (que precisa de ~>=2x ~2 Hz; com decimação
        // ×4 sobre ~? Hz a margem some). Padrão por perfil mantém a estabilidade
        // já validada; defina AETHERSENSE_CSI_DECIMATION=1|2 para sessões de vitais.
        if let Some(value) =
            option_env!("AETHERSENSE_CSI_DECIMATION").and_then(|raw| raw.trim().parse::<u32>().ok())
        {
            if value >= 1 {
                return value;
            }
        }
        match self.capture_config.profile {
            CaptureProfile::Classic => 1,
            CaptureProfile::Esp32C5Like | CaptureProfile::Esp32C6Like => 4,
            CaptureProfile::Unknown => 1,
        }
    }
}

#[cfg(feature = "esp-idf")]
unsafe fn handle_wifi_csi_info<T: WifiCsiInfoView>(context: &EspIdfCallbackContext, info: &T) {
    if let Some(expected_bssid) = context.ap_bssid {
        if info.mac() != expected_bssid {
            return;
        }
    }

    let seq = context.next_seq();
    let decimation = context.callback_decimation();
    if decimation > 1 && seq % decimation != 0 {
        return;
    }

    if let Some(mut frame) = frame_from_wifi_csi_info(seq, info) {
        frame.mac = stable_capture_mac(context);
        let _ = context.ingress.try_push(frame);
    }
}

#[cfg(all(feature = "esp-idf", target_os = "espidf"))]
pub unsafe extern "C" fn wifi_csi_rx_cb(ctx: *mut c_void, info: *mut sys::wifi_csi_info_t) {
    if ctx.is_null() || info.is_null() {
        return;
    }

    let context = &*(ctx as *const EspIdfCallbackContext);
    handle_wifi_csi_info(context, &*info);
}

#[cfg(all(feature = "esp-idf", not(target_os = "espidf")))]
pub unsafe extern "C" fn wifi_csi_rx_cb(ctx: *mut c_void, info: *const WifiCsiInfo) {
    if ctx.is_null() || info.is_null() {
        return;
    }

    let context = &*(ctx as *const EspIdfCallbackContext);
    handle_wifi_csi_info(context, &*info);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_rx_ctrl() -> WifiPktRxCtrl {
        WifiPktRxCtrl {
            rssi: -45,
            rate: 11,
            sig_mode: 1,
            mcs: 0,
            cwb: 0,
            smoothing: 0,
            not_sounding: 0,
            aggregation: 0,
            stbc: 0,
            fec_coding: 0,
            sgi: 0,
            noise_floor: -90,
            ampdu_cnt: 0,
            channel: 6,
            secondary_channel: 0,
            timestamp: 12_348,
            ant: 0,
            sig_len: 66,
            rx_state: 0,
        }
    }

    #[test]
    fn format_mac_uses_lowercase_hex() {
        assert_eq!(
            format_mac(&[0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]),
            "aa:bb:cc:dd:ee:ff"
        );
    }

    #[test]
    fn hardware_profile_maps_to_expected_capture_config() {
        assert_eq!(
            CaptureProfile::from_hardware_profile("classic"),
            CaptureProfile::Classic
        );
        assert_eq!(
            CaptureProfile::from_hardware_profile("ESP32C5"),
            CaptureProfile::Esp32C5Like
        );
        assert_eq!(
            CaptureProfile::from_hardware_profile("esp32c6"),
            CaptureProfile::Esp32C6Like
        );
        assert_eq!(
            CaptureProfile::from_hardware_profile("something-else"),
            CaptureProfile::Unknown
        );
    }

    #[test]
    fn classic_profile_uses_classic_capture_config() {
        let config = CaptureProfile::Classic.capture_config();
        assert!(config.lltf_en);
        assert!(!config.htltf_en);
        assert!(!config.stbc_htltf2_en);
        assert!(config.ltf_merge_en);
        assert!(config.channel_filter_en);
        assert!(!config.manu_scale);
        assert!(!config.shift);
        assert!(config.acquire_csi_legacy);
        assert!(!config.acquire_csi_force_lltf);
        assert_eq!(config.profile, CaptureProfile::Classic);
    }

    #[test]
    fn esp32c5_profile_enables_ht40_and_he_stbc_sampling() {
        let config = CaptureProfile::Esp32C5Like.capture_config();

        assert!(config.acquire_csi_legacy);
        assert!(config.acquire_csi_ht20);
        assert!(config.acquire_csi_ht40);
        assert_eq!(config.acquire_csi_he_stbc_mode, 2);
        assert_eq!(config.esp32c5_packed_bits() & (1 << 4), 1 << 4);
        assert_eq!(config.esp32c5_packed_bits() & (0b11 << 10), 0b10 << 10);
        // lltf_bit_mode é condicional à banda: DESLIGADO por padrão (2,4 GHz).
        assert_eq!(config.esp32c5_packed_bits() & (1 << 17), 0);
        // LIGADO só no C5 em 5 GHz (fix #18493 da CSI congelada — bit 17, layout v5.5.4).
        let c5_5g = CaptureProfile::Esp32C5Like
            .capture_config()
            .apply_band("5ghz");
        assert_eq!(c5_5g.esp32c5_packed_bits() & (1 << 17), 1 << 17);
        // 2,4 GHz mantém desligado (não altera a captura que já funciona).
        let c5_24 = CaptureProfile::Esp32C5Like
            .capture_config()
            .apply_band("2.4ghz");
        assert_eq!(c5_24.esp32c5_packed_bits() & (1 << 17), 0);
        // lltf não liga em outros perfis, mesmo em 5 GHz.
        assert!(
            !CaptureProfile::Classic
                .capture_config()
                .apply_band("5ghz")
                .lltf_bit_mode
        );
    }

    #[cfg(feature = "esp-idf")]
    #[test]
    fn binding_exposes_callback_and_context() {
        let (ingress, _egress) = crate::runtime::packet_bus(1);
        let handle = EspIdfSourceHandle::new(ingress, CaptureProfile::Classic.capture_config());
        let binding = handle.binding();

        assert!(!binding.context.is_null());
        assert_eq!(binding.capture_config.profile, CaptureProfile::Classic);
    }

    #[test]
    fn frame_from_wifi_csi_info_matches_contract() {
        let payload = [10_u8, 20, 30, 40, 50, 60, 70, 80];
        let info = WifiCsiInfo {
            mac: [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff],
            rx_ctrl: test_rx_ctrl(),
            len: payload.len(),
            first_word_invalid: false,
            buf: payload.as_ptr(),
        };

        let frame = unsafe { frame_from_wifi_csi_info(7, &info) }.expect("frame");
        assert_eq!(
            frame.serialize_csv(),
            "CSI_DATA,7,aa:bb:cc:dd:ee:ff,-45,11,1,0,0,0,0,0,0,0,0,-90,0,6,0,12348,0,66,0,8,0,\"[10,20,30,40,50,60,70,80]\""
        );
    }
}
