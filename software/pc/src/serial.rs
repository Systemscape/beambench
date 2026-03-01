//! Serial communication with the RX board using postcard + COBS framing.

use beambench_protocol::{PcToRx, RxToPc};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio_serial::SerialPortBuilderExt;
use tracing::{debug, error, info, warn};

const BAUD_RATE: u32 = 115_200;
const COBS_BUF_SIZE: usize = 512;

/// Handle to a serial connection. Runs reader/writer tasks in the background.
pub struct SerialHandle {
    pub tx: mpsc::Sender<PcToRx>,
    pub rx: mpsc::Receiver<RxToPc>,
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
            rx: resp_rx,
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
                            accum = accum[zero_pos + 1..].to_vec();
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

/// List available serial ports.
pub fn list_ports() -> Vec<String> {
    tokio_serial::available_ports()
        .unwrap_or_default()
        .into_iter()
        .map(|p| p.port_name)
        .collect()
}
