use std::fmt;

use crate::config::DEFAULT_DEVICE_MAC;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CsiFrame {
    pub seq: u32,
    pub mac: String,
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
    pub len: i32,
    pub first_word_invalid: i32,
    pub payload: Vec<i16>,
}

impl Default for CsiFrame {
    fn default() -> Self {
        Self {
            seq: 0,
            mac: DEFAULT_DEVICE_MAC.to_string(),
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
            timestamp: 0,
            ant: 0,
            sig_len: 66,
            rx_state: 0,
            len: 0,
            first_word_invalid: 0,
            payload: Vec::new(),
        }
    }
}

impl CsiFrame {
    pub fn classic_example(seq: u32) -> Self {
        let payload = vec![10, 20, 30, 40, 50, 60, 70, 80];
        Self {
            seq,
            mac: DEFAULT_DEVICE_MAC.to_string(),
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
            timestamp: 12_345 + seq as i64,
            ant: 0,
            sig_len: 66,
            rx_state: 0,
            len: payload.len() as i32,
            first_word_invalid: 0,
            payload,
        }
    }

    pub fn with_payload(seq: u32, payload: Vec<i16>) -> Self {
        let mut frame = Self::classic_example(seq);
        frame.len = payload.len() as i32;
        frame.payload = payload;
        frame
    }

    pub fn payload_csv(&self) -> String {
        let mut output = String::with_capacity(self.payload.len().saturating_mul(6));
        self.write_payload_csv(&mut output)
            .expect("writing CSI payload to String is infallible");
        output
    }

    pub fn serialize_csv(&self) -> String {
        let mut output = String::new();
        self.serialize_csv_into(&mut output)
            .expect("serializing a test/host CSI frame");
        output
    }

    /// Serialize into a reusable buffer with one fallible reserve. This avoids
    /// allocating one `String` per sample and lets the ESP consumer fail a
    /// single frame gracefully instead of invoking the global OOM handler.
    pub fn serialize_csv_into(
        &self,
        output: &mut String,
    ) -> Result<(), std::collections::TryReserveError> {
        output.clear();
        output.try_reserve(256usize.saturating_add(self.payload.len().saturating_mul(7)))?;
        self.write_csv(output)
            .expect("writing CSI CSV to String is infallible");
        Ok(())
    }

    fn write_payload_csv<W: fmt::Write>(&self, output: &mut W) -> fmt::Result {
        for (index, value) in self.payload.iter().enumerate() {
            if index > 0 {
                output.write_char(',')?;
            }
            write!(output, "{value}")?;
        }
        Ok(())
    }

    fn write_csv<W: fmt::Write>(&self, output: &mut W) -> fmt::Result {
        write!(
            output,
            "CSI_DATA,{seq},{mac},{rssi},{rate},{sig_mode},{mcs},{cwb},{smoothing},{not_sounding},{aggregation},{stbc},{fec_coding},{sgi},{noise_floor},{ampdu_cnt},{channel},{secondary_channel},{timestamp},{ant},{sig_len},{rx_state},{len},{first_word_invalid},\"[",
            seq = self.seq,
            mac = self.mac,
            rssi = self.rssi,
            rate = self.rate,
            sig_mode = self.sig_mode,
            mcs = self.mcs,
            cwb = self.cwb,
            smoothing = self.smoothing,
            not_sounding = self.not_sounding,
            aggregation = self.aggregation,
            stbc = self.stbc,
            fec_coding = self.fec_coding,
            sgi = self.sgi,
            noise_floor = self.noise_floor,
            ampdu_cnt = self.ampdu_cnt,
            channel = self.channel,
            secondary_channel = self.secondary_channel,
            timestamp = self.timestamp,
            ant = self.ant,
            sig_len = self.sig_len,
            rx_state = self.rx_state,
            len = self.len,
            first_word_invalid = self.first_word_invalid,
        )?;
        self.write_payload_csv(output)?;
        output.write_str("]\"")
    }
}

impl fmt::Display for CsiFrame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.write_csv(f)
    }
}

pub fn sample_frame(seq: u32) -> CsiFrame {
    CsiFrame::classic_example(seq)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serialize_csv_matches_backend_contract() {
        let frame = CsiFrame::classic_example(7);
        assert_eq!(
            frame.serialize_csv(),
            "CSI_DATA,7,aa:bb:cc:dd:ee:ff,-45,11,1,0,0,0,0,0,0,0,0,-90,0,6,0,12352,0,66,0,8,0,\"[10,20,30,40,50,60,70,80]\""
        );
    }

    #[test]
    fn with_payload_updates_length() {
        let frame = CsiFrame::with_payload(1, vec![1, 2, 3, 4]);
        assert_eq!(frame.len, 4);
        assert_eq!(frame.payload_csv(), "1,2,3,4");
    }
}
