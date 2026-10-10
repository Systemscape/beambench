/** WebSocket client for communicating with the Axum backend. */

// Types are generated from the Rust definitions in src/lib.rs (`just gen-types`).
import type { WsCommand, WsEvent } from './bindings';
export type {
    DataPoint,
    PortInfo,
    SystemStatus,
    WsCommand,
    WsEvent
} from './bindings';

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
