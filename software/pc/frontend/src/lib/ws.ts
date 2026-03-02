// WebSocket client for communicating with the Axum backend.

export type DataPoint = {
	angle_deg: number;
	rssi_dbm: number;
	sample_count: number;
};

export type SystemStatus = {
	sweeping: boolean;
	tx_connected: boolean;
	turntable_connected: boolean;
	serial_connected: boolean;
	data_points: number;
};

export type PortInfo = {
	name: string;
	description: string;
};

export type WsEvent =
	| { type: 'DataPoint'; angle_deg: number; rssi_dbm: number; sample_count: number }
	| { type: 'SweepComplete' }
	| { type: 'Status'; sweeping: boolean; tx_connected: boolean; turntable_connected: boolean; serial_connected: boolean; data_points: number }
	| { type: 'Error'; message: string }
	| { type: 'Log'; message: string };

export type WsCommand =
	| { type: 'StartSweep'; start_deg: number; stop_deg: number; step_deg: number; samples_per_angle: number }
	| { type: 'ConfigureTx'; channel: number; tx_power_dbm: number; packet_rate_hz: number }
	| { type: 'Stop' }
	| { type: 'QueryStatus' }
	| { type: 'ListPorts' }
	| { type: 'Connect'; port: string }
	| { type: 'Disconnect' }
	| { type: 'ExportCsv' };

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
