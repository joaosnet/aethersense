use std::io;
use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};

use crate::config::FirmwareConfig;
use crate::csi::CsiFrame;

const TRANSIENT_SEND_RETRIES: usize = 8;
const TRANSIENT_SEND_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(5);

fn is_transient_send_error(error: &io::Error) -> bool {
    matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted)
        || matches!(error.raw_os_error(), Some(11 | 12 | 105 | 10_055))
}

pub struct UdpTransmitter {
    socket: UdpSocket,
    destination: SocketAddrV4,
}

impl UdpTransmitter {
    pub fn from_config(config: &FirmwareConfig) -> io::Result<Self> {
        Self::from_destination(config.udp_destination())
    }

    pub fn from_destination(destination: SocketAddrV4) -> io::Result<Self> {
        let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0))?;
        socket.set_broadcast(true)?;

        Ok(Self {
            socket,
            destination,
        })
    }

    pub fn send_line(&self, line: &str) -> io::Result<usize> {
        let mut retries = 0;
        loop {
            match self.socket.send_to(line.as_bytes(), self.destination) {
                Ok(written) => return Ok(written),
                Err(error)
                    if retries < TRANSIENT_SEND_RETRIES && is_transient_send_error(&error) =>
                {
                    retries += 1;
                    // lwIP reports ENOMEM while a Wi-Fi TX buffer is still in
                    // flight. A short bounded wait preserves the frame without
                    // increasing any resident queue or touching the CSI callback.
                    std::thread::sleep(TRANSIENT_SEND_RETRY_DELAY);
                }
                Err(error) => return Err(error),
            }
        }
    }

    pub fn send_frame(&self, frame: &CsiFrame) -> io::Result<usize> {
        self.send_line(&frame.serialize_csv())
    }

    pub fn destination(&self) -> SocketAddrV4 {
        self.destination
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lwip_memory_pressure_is_classified_as_transient() {
        assert!(is_transient_send_error(&io::Error::from_raw_os_error(12)));
        assert!(is_transient_send_error(&io::Error::from_raw_os_error(105)));
        assert!(!is_transient_send_error(&io::Error::from_raw_os_error(22)));
    }
}
