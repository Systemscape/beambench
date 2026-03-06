//! Serial communication with the RX board using postcard + COBS framing.

use std::sync::Arc;

use beambench_protocol::{PcToRx, RxToPc};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Mutex, mpsc};
use tokio_serial::SerialPortBuilderExt;
use tracing::{debug, error, info, warn};

/// Baud rate for the USB-serial connection to the RX board.
const BAUD_RATE: u32 = 115_200;

/// Buffer size for COBS frame encoding/decoding.
const COBS_BUF_SIZE: usize = 512;

/// Maximum accumulation buffer size before resetting. Guards against unbounded
/// growth if COBS delimiters are missing (e.g. noise on the line).
const MAX_ACCUM_SIZE: usize = 1024;

/// Handle to an active serial (or TCP) connection to the RX board.
///
/// Spawns background reader/writer tasks that bridge between typed channels
/// and the raw COBS-framed byte stream. Drop or call [`close()`](Self::close)
/// to shut down both tasks.
pub struct SerialHandle {
    pub tx: mpsc::Sender<PcToRx>,
    pub rx: Arc<Mutex<mpsc::Receiver<RxToPc>>>,
    cancel: tokio::sync::watch::Sender<bool>,
}

impl SerialHandle {
    /// Open a serial port and spawn reader/writer tasks.
    pub async fn open(port_name: &str) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let port = tokio_serial::new(port_name, BAUD_RATE).open_native_async()?;
        let (reader, writer) = tokio::io::split(port);

        let (cmd_tx, cmd_rx) = mpsc::channel::<PcToRx>(32);
        let (resp_tx, resp_rx) = mpsc::channel::<RxToPc>(64);
        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);

        tokio::spawn(writer_task(writer, cmd_rx, cancel_rx.clone()));
        tokio::spawn(reader_task(reader, resp_tx, cancel_rx));

        info!("Serial port {} opened at {} baud", port_name, BAUD_RATE);

        Ok(Self {
            tx: cmd_tx,
            rx: Arc::new(Mutex::new(resp_rx)),
            cancel: cancel_tx,
        })
    }

    /// Open a TCP connection (e.g. to rx-sim) and spawn reader/writer tasks.
    pub async fn open_tcp(addr: &str) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let stream = tokio::net::TcpStream::connect(addr).await?;
        let (reader, writer) = tokio::io::split(stream);

        let (cmd_tx, cmd_rx) = mpsc::channel::<PcToRx>(32);
        let (resp_tx, resp_rx) = mpsc::channel::<RxToPc>(64);
        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);

        tokio::spawn(writer_task(writer, cmd_rx, cancel_rx.clone()));
        tokio::spawn(reader_task(reader, resp_tx, cancel_rx));

        info!("TCP connection to {} established", addr);

        Ok(Self {
            tx: cmd_tx,
            rx: Arc::new(Mutex::new(resp_rx)),
            cancel: cancel_tx,
        })
    }

    /// Close the serial connection.
    pub fn close(self) {
        let _ = self.cancel.send(true);
    }
}

async fn writer_task<W: AsyncWriteExt + Unpin>(
    mut writer: W,
    mut commands: mpsc::Receiver<PcToRx>,
    mut cancel: tokio::sync::watch::Receiver<bool>,
) {
    let mut buf = [0u8; COBS_BUF_SIZE];
    loop {
        tokio::select! {
            Some(cmd) = commands.recv() => {
                match beambench_protocol::serialize_cobs(&cmd, &mut buf) {
                    Ok(len) => {
                        if let Err(e) = writer.write_all(&buf[..len]).await {
                            error!("Serial write error: {}", e);
                            return;
                        }
                        debug!("Sent command: {:?}", cmd);
                    }
                    Err(e) => {
                        error!("Failed to serialize command: {:?}", e);
                    }
                }
            }
            _ = cancel.changed() => {
                info!("Serial writer shutting down");
                return;
            }
        }
    }
}

async fn reader_task<R: AsyncReadExt + Unpin>(
    mut reader: R,
    responses: mpsc::Sender<RxToPc>,
    mut cancel: tokio::sync::watch::Receiver<bool>,
) {
    let mut raw_buf = [0u8; COBS_BUF_SIZE];
    let mut accum = Vec::with_capacity(COBS_BUF_SIZE);

    loop {
        tokio::select! {
            result = reader.read(&mut raw_buf) => {
                match result {
                    Ok(0) => {
                        warn!("Serial port closed (EOF)");
                        return;
                    }
                    Ok(n) => {
                        accum.extend_from_slice(&raw_buf[..n]);

                        // Guard against unbounded accumulation (e.g. noise without delimiters).
                        if accum.len() > MAX_ACCUM_SIZE {
                            warn!("Accumulation buffer exceeded {} bytes, resetting", MAX_ACCUM_SIZE);
                            accum.clear();
                            continue;
                        }

                        // COBS frames are delimited by 0x00
                        while let Some(zero_pos) = accum.iter().position(|&b| b == 0) {
                            let frame = &accum[..zero_pos + 1];
                            if frame.len() > 1 {
                                match postcard::from_bytes_cobs(&mut frame.to_vec()) {
                                    Ok(msg) => {
                                        if responses.send(msg).await.is_err() {
                                            return;
                                        }
                                    }
                                    Err(e) => {
                                        warn!("Failed to decode COBS frame: {:?}", e);
                                    }
                                }
                            }
                            accum.drain(..=zero_pos);
                        }
                    }
                    Err(e) => {
                        error!("Serial read error: {}", e);
                        return;
                    }
                }
            }
            _ = cancel.changed() => {
                info!("Serial reader shutting down");
                return;
            }
        }
    }
}

/// Default address for the rx-sim simulator.
const SIM_ADDR: &str = "127.0.0.1:9876";

/// List available serial ports with descriptions. Also probes the default
/// rx-sim TCP address and includes it if reachable.
pub async fn list_ports() -> Vec<crate::PortInfo> {
    let mut ports: Vec<crate::PortInfo> = tokio_serial::available_ports()
        .unwrap_or_default()
        .into_iter()
        .map(|p| {
            let description = match &p.port_type {
                tokio_serial::SerialPortType::UsbPort(usb) => {
                    let mut parts = Vec::new();
                    if let Some(product) = &usb.product {
                        parts.push(product.clone());
                    } else if let Some(manufacturer) = &usb.manufacturer {
                        parts.push(manufacturer.clone());
                    } else {
                        parts.push(format!("USB {:04x}:{:04x}", usb.vid, usb.pid));
                    }
                    if let Some(sn) = &usb.serial_number {
                        parts.push(format!("[{}]", sn));
                    }
                    parts.join(" ")
                }
                tokio_serial::SerialPortType::BluetoothPort => "Bluetooth".to_string(),
                tokio_serial::SerialPortType::PciPort => "PCI".to_string(),
                _ => String::new(),
            };
            crate::PortInfo {
                name: p.port_name,
                description,
            }
        })
        .collect();

    // Quick probe: can we connect to the simulator?
    if tokio::time::timeout(
        std::time::Duration::from_millis(100),
        tokio::net::TcpStream::connect(SIM_ADDR),
    )
    .await
    .is_ok_and(|r| r.is_ok())
    {
        ports.insert(
            0,
            crate::PortInfo {
                name: format!("tcp://{}", SIM_ADDR),
                description: "RX Simulator".to_string(),
            },
        );
    }

    ports
}

#[cfg(test)]
mod tests {
    use super::*;
    use beambench_protocol::{MAX_MSG_SIZE, RxToPc};

    /// Encode an RxToPc message as a COBS frame (ready to write to a stream).
    fn encode_cobs_frame(msg: &RxToPc) -> Vec<u8> {
        let mut buf = [0u8; MAX_MSG_SIZE];
        let len = beambench_protocol::serialize_cobs(msg, &mut buf).unwrap();
        buf[..len].to_vec()
    }

    #[tokio::test]
    async fn reader_decodes_single_frame() {
        let msg = RxToPc::SweepComplete;
        let frame = encode_cobs_frame(&msg);

        let (resp_tx, mut resp_rx) = mpsc::channel::<RxToPc>(16);
        let (_cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);

        // DuplexStream: write to `writer`, reader_task reads from `reader`
        let (mut writer, reader) = tokio::io::duplex(1024);

        tokio::spawn(reader_task(reader, resp_tx, cancel_rx));

        writer.write_all(&frame).await.unwrap();
        drop(writer); // EOF signals reader to stop

        let decoded = resp_rx.recv().await.expect("should receive decoded message");
        assert_eq!(decoded, msg);
    }

    #[tokio::test]
    async fn reader_handles_fragmented_frames() {
        let msg = RxToPc::DataPoint {
            angle_deg: 45.0,
            rssi_dbm: -30.0,
            sample_count: 10,
        };
        let frame = encode_cobs_frame(&msg);
        assert!(frame.len() > 2, "frame should be long enough to split");

        let (resp_tx, mut resp_rx) = mpsc::channel::<RxToPc>(16);
        let (_cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);

        let (mut writer, reader) = tokio::io::duplex(1024);

        tokio::spawn(reader_task(reader, resp_tx, cancel_rx));

        // Write frame in two fragments
        let mid = frame.len() / 2;
        writer.write_all(&frame[..mid]).await.unwrap();
        tokio::task::yield_now().await;
        writer.write_all(&frame[mid..]).await.unwrap();
        drop(writer);

        let decoded = resp_rx.recv().await.expect("should reassemble fragmented frame");
        assert_eq!(decoded, msg);
    }

    #[tokio::test]
    async fn reader_resets_on_oversized_accumulation() {
        // Write >MAX_ACCUM_SIZE bytes without a COBS delimiter, then a valid frame.
        let (resp_tx, mut resp_rx) = mpsc::channel::<RxToPc>(16);
        let (_cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);

        let (mut writer, reader) = tokio::io::duplex(4096);

        tokio::spawn(reader_task(reader, resp_tx, cancel_rx));

        // Write garbage that exceeds MAX_ACCUM_SIZE (no zero delimiter).
        let garbage = vec![0xAA; super::MAX_ACCUM_SIZE + 100];
        writer.write_all(&garbage).await.unwrap();
        tokio::task::yield_now().await;

        // Now write a valid frame — reader should have reset and decode this.
        let msg = RxToPc::SweepComplete;
        let frame = encode_cobs_frame(&msg);
        writer.write_all(&frame).await.unwrap();
        drop(writer);

        let decoded = resp_rx.recv().await.expect("should decode frame after reset");
        assert_eq!(decoded, msg);
    }

    #[tokio::test]
    async fn reader_handles_multiple_frames_in_one_read() {
        let msg1 = RxToPc::SweepComplete;
        let msg2 = RxToPc::DataPoint {
            angle_deg: 90.0,
            rssi_dbm: -50.0,
            sample_count: 5,
        };
        let mut combined = encode_cobs_frame(&msg1);
        combined.extend_from_slice(&encode_cobs_frame(&msg2));

        let (resp_tx, mut resp_rx) = mpsc::channel::<RxToPc>(16);
        let (_cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);

        let (mut writer, reader) = tokio::io::duplex(1024);

        tokio::spawn(reader_task(reader, resp_tx, cancel_rx));

        writer.write_all(&combined).await.unwrap();
        drop(writer);

        let decoded1 = resp_rx.recv().await.expect("should decode first frame");
        let decoded2 = resp_rx.recv().await.expect("should decode second frame");
        assert_eq!(decoded1, msg1);
        assert_eq!(decoded2, msg2);
    }
}
