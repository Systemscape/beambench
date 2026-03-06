/** WebSocket client for communicating with the Axum backend. */

/** A single RSSI measurement at a given turntable angle. */
export type DataPoint = {
    /** Turntable angle in degrees. */
    angle_deg: number;
    /** Averaged RSSI reading in dBm (negative values; closer to 0 = stronger). */
    rssi_dbm: number;
    /** Number of raw RSSI samples averaged into this reading. */
    sample_count: number;
};

/** Snapshot of the system's connection and sweep state, sent periodically by the backend. */
export type SystemStatus = {
    /** Whether a sweep is currently in progress. */
    sweeping: boolean;
    /** Whether the TX (transmitter) ESP32 is paired via ESP-NOW. */
    tx_connected: boolean;
    /** Whether the RX (receiver) ESP32 is paired via ESP-NOW. */
    rx_connected: boolean;
    /** Whether the turntable stepper ESP32 is paired via ESP-NOW. */
    turntable_connected: boolean;
    /** Whether the PC is connected to the Bridge over serial. */
    serial_connected: boolean;
    /** Total data points collected in the current session. */
    data_points: number;
};

/** A serial port available on the host machine. */
export type PortInfo = {
    /** OS device path (e.g. `/dev/ttyUSB0` or `COM3`). */
    name: string;
    /** Human-readable description from the USB descriptor. */
    description: string;
};

/**
 * Server-to-client WebSocket events.
 *
 * The backend pushes these as JSON over the `/ws` endpoint.
 */
export type WsEvent =
    | {
          type: 'DataPoint';
          angle_deg: number;
          rssi_dbm: number;
          sample_count: number;
      }
    | { type: 'SweepComplete' }
    | { type: 'HomeComplete' }
    | {
          type: 'Status';
          sweeping: boolean;
          tx_connected: boolean;
          rx_connected: boolean;
          turntable_connected: boolean;
          serial_connected: boolean;
          data_points: number;
      }
    | { type: 'Error'; message: string }
    | { type: 'Log'; message: string };

/**
 * Client-to-server WebSocket commands.
 *
 * Sent as JSON to the `/ws` endpoint to control the measurement system.
 */
export type WsCommand =
    | {
          type: 'StartSweep';
          start_deg: number;
          stop_deg: number;
          step_deg: number;
          samples_per_angle: number;
      }
    | {
          type: 'ConfigureTx';
          channel: number;
          tx_power_dbm: number;
          packet_rate_hz: number;
      }
    | { type: 'Stop' }
    | { type: 'QueryStatus' }
    | { type: 'ListPorts' }
    | { type: 'Connect'; port: string }
    | { type: 'Disconnect' }
    | { type: 'ExportCsv' }
    | { type: 'ReturnHome' };

export function createWsConnection(
    onEvent: (event: WsEvent) => void,
    onOpen?: () => void,
    onClose?: () => void
) {
    const protocol = window.location.protocol === 'https:' ? 'wss:' : 'ws:';
    const url = `${protocol}//${window.location.host}/ws`;

    let ws: WebSocket | null = null;
    let reconnectTimer: ReturnType<typeof setTimeout> | null = null;

    function connect() {
        ws = new WebSocket(url);

        ws.onopen = () => {
            onOpen?.();
        };

        ws.onmessage = (event) => {
            try {
                const data = JSON.parse(event.data) as WsEvent;
                onEvent(data);
            } catch (e) {
                console.error('Failed to parse WS message:', e);
            }
        };

        ws.onclose = () => {
            onClose?.();
            reconnectTimer = setTimeout(connect, 2000);
        };

        ws.onerror = () => {
            ws?.close();
        };
    }

    connect();

    return {
        send(cmd: WsCommand) {
            if (ws?.readyState === WebSocket.OPEN) {
                ws.send(JSON.stringify(cmd));
            }
        },
        close() {
            if (reconnectTimer) clearTimeout(reconnectTimer);
            ws?.close();
        }
    };
}
