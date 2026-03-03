<script lang="ts">
	import { onMount, onDestroy } from 'svelte';
	import { createWsConnection, type DataPoint, type PortInfo, type SystemStatus, type WsEvent } from '$lib/ws';

	let plotDiv: HTMLDivElement;
	let Plotly: typeof import('plotly.js-dist-min');

	type Measurement = {
		id: number;
		name: string;
		data: DataPoint[];
		color: string;
		visible: boolean;
	};

	const COLORS = ['#fff', '#4fc3f7', '#ff8a65', '#81c784', '#ce93d8', '#fff176'];

	let measurements: Measurement[] = $state([]);
	let activeData: DataPoint[] = $state([]);
	let nextId = $state(1);

	let status: SystemStatus = $state({
		sweeping: false,
		tx_connected: false,
		turntable_connected: false,
		serial_connected: false,
		data_points: 0
	});
	let ports: PortInfo[] = $state([]);
	let selectedPort = $state('');
	let connected = $state(false);
	let errorMessage = $state('');
	let logEntries: { ts: string; msg: string }[] = $state([]);
	let logDiv: HTMLDivElement;

	// Plot settings
	let dynamicRangeDb = $state(40);

	// Sweep config
	let startDeg = $state(0);
	let stopDeg = $state(360);
	let stepDeg = $state(5);
	let samplesPerAngle = $state(10);

	let ws: ReturnType<typeof createWsConnection> | null = null;

	function addLog(msg: string) {
		const ts = new Date().toLocaleTimeString();
		logEntries = [...logEntries, { ts, msg }];
		if (logEntries.length > 200) {
			logEntries = logEntries.slice(-200);
		}
		// Auto-scroll to bottom
		requestAnimationFrame(() => {
			if (logDiv) logDiv.scrollTop = logDiv.scrollHeight;
		});
	}

	function archiveActive() {
		if (activeData.length === 0) return;
		measurements = [
			...measurements,
			{
				id: nextId,
				name: `Sweep ${nextId}`,
				data: activeData,
				color: COLORS[(nextId - 1) % COLORS.length],
				visible: true
			}
		];
		nextId++;
		activeData = [];
	}

	function handleEvent(event: WsEvent) {
		switch (event.type) {
			case 'DataPoint':
				activeData = [
					...activeData,
					{
						angle_deg: event.angle_deg,
						rssi_dbm: event.rssi_dbm,
						sample_count: event.sample_count
					}
				];
				updatePlot();
				break;
			case 'SweepComplete':
				status = { ...status, sweeping: false };
				archiveActive();
				updatePlot();
				addLog('Sweep complete');
				break;
			case 'Status':
				status = {
					sweeping: event.sweeping,
					tx_connected: event.tx_connected,
					turntable_connected: event.turntable_connected,
					serial_connected: event.serial_connected,
					data_points: event.data_points
				};
				break;
			case 'Error':
				errorMessage = event.message;
				addLog(`Error: ${event.message}`);
				setTimeout(() => {
					errorMessage = '';
				}, 5000);
				break;
			case 'Log':
				addLog(event.message);
				break;
		}
	}

	$effect(() => {
		// Track reactive deps to re-render plot
		dynamicRangeDb;
		measurements.map((m) => m.visible);
		updatePlot();
	});

	function updatePlot() {
		if (!Plotly || !plotDiv) return;

		// Collect all visible data for normalization
		const allVisible: DataPoint[] = [];
		for (const m of measurements) {
			if (m.visible) allVisible.push(...m.data);
		}
		allVisible.push(...activeData);

		let radialaxis: Record<string, unknown> = {
			title: { text: 'RSSI (dBm)', font: { color: '#888' } },
			angle: 90,
			tickangle: 90,
			gridcolor: '#2a2a2a',
			linecolor: '#333',
			tickfont: { color: '#666' }
		};

		let floor = 0;
		if (allVisible.length > 0) {
			const maxRssi = Math.max(...allVisible.map((d) => d.rssi_dbm));
			floor = maxRssi - dynamicRangeDb;

			const tickCount = 5;
			const tickStep = dynamicRangeDb / tickCount;
			const tickvals = Array.from({ length: tickCount + 1 }, (_, i) => i * tickStep);
			const ticktext = tickvals.map((v) => `${Math.round(floor + v)}`);

			radialaxis = {
				...radialaxis,
				range: [0, dynamicRangeDb],
				tickvals,
				ticktext
			};
		}

		// Build one trace per visible measurement + active
		const traces: Record<string, unknown>[] = [];

		for (const m of measurements) {
			if (!m.visible) continue;
			traces.push({
				type: 'scatterpolar' as const,
				mode: 'lines+markers' as const,
				r: m.data.map((d) => Math.max(0, d.rssi_dbm - floor)),
				theta: m.data.map((d) => d.angle_deg),
				name: m.name,
				line: { color: m.color, width: 1.5 },
				marker: { size: 3, color: m.color }
			});
		}

		if (activeData.length > 0) {
			const activeColor = COLORS[(nextId - 1) % COLORS.length];
			traces.push({
				type: 'scatterpolar' as const,
				mode: 'lines+markers' as const,
				r: activeData.map((d) => Math.max(0, d.rssi_dbm - floor)),
				theta: activeData.map((d) => d.angle_deg),
				name: `Sweep ${nextId} (active)`,
				line: { color: activeColor, width: 2 },
				marker: { size: 4, color: activeColor }
			});
		}

		const layout = {
			polar: {
				bgcolor: '#1a1a1a',
				radialaxis,
				angularaxis: {
					direction: 'clockwise' as const,
					period: 360,
					gridcolor: '#2a2a2a',
					linecolor: '#333',
					tickfont: { color: '#666' }
				}
			},
			showlegend: traces.length > 1,
			legend: { font: { color: '#888' } },
			paper_bgcolor: '#1a1a1a',
			plot_bgcolor: '#1a1a1a',
			margin: { t: 40, b: 40, l: 40, r: 40 }
		};

		Plotly.react(plotDiv, traces, layout, { responsive: true });
	}

	async function fetchPorts() {
		try {
			const res = await fetch('/api/ports');
			ports = await res.json();
			if (ports.length > 0 && !selectedPort) {
				selectedPort = ports[0].name;
			}
		} catch {
			errorMessage = 'Failed to fetch serial ports';
		}
	}

	function connectSerial() {
		if (selectedPort) {
			ws?.send({ type: 'Connect', port: selectedPort });
		}
	}

	function disconnectSerial() {
		ws?.send({ type: 'Disconnect' });
	}

	function startSweep() {
		archiveActive();
		ws?.send({
			type: 'StartSweep',
			start_deg: startDeg,
			stop_deg: stopDeg,
			step_deg: stepDeg,
			samples_per_angle: samplesPerAngle
		});
	}

	function stopSweep() {
		ws?.send({ type: 'Stop' });
	}

	function returnHome() {
		ws?.send({ type: 'ReturnHome' });
	}

	function toggleMeasurement(id: number) {
		measurements = measurements.map((m) =>
			m.id === id ? { ...m, visible: !m.visible } : m
		);
		updatePlot();
	}

	function deleteMeasurement(id: number) {
		measurements = measurements.filter((m) => m.id !== id);
		updatePlot();
	}

	function clearAllMeasurements() {
		measurements = [];
		updatePlot();
	}

	onMount(async () => {
		Plotly = await import('plotly.js-dist-min');
		updatePlot();

		ws = createWsConnection(
			handleEvent,
			() => {
				connected = true;
			},
			() => {
				connected = false;
			}
		);

		await fetchPorts();
	});

	onDestroy(() => {
		ws?.close();
	});
</script>

<main>
	<h1>Beambench</h1>

	{#if errorMessage}
		<div class="error">{errorMessage}</div>
	{/if}

	<div class="layout">
		<div class="sidebar">
			<section>
				<h2>Connection</h2>
				{#if ports.length > 0}
					<div class="field">
						<select id="port" bind:value={selectedPort}>
							{#each ports as port}
								<option value={port.name}>
								{port.name}{port.description ? ` \u2014 ${port.description}` : ''}
							</option>
							{/each}
						</select>
					</div>
				{:else}
					<div class="field">
						<input
							id="port"
							bind:value={selectedPort}
							placeholder="tcp://127.0.0.1:9876"
						/>
					</div>
				{/if}
				<div class="connect-row">
					{#if status.serial_connected}
						<button onclick={disconnectSerial}>Disconnect</button>
					{:else}
						<button onclick={connectSerial} disabled={!selectedPort}>Connect</button>
					{/if}
					<button class="secondary" onclick={fetchPorts}>Refresh</button>
				</div>
				<div class="status-indicators">
					<span class="indicator" class:active={status.serial_connected}>Serial</span>
					<span class="indicator" class:active={status.tx_connected}>TX</span>
					<span class="indicator" class:active={status.turntable_connected}>Turntable</span>
				</div>
			</section>

			<section>
				<h2>Sweep Configuration</h2>
				<div class="field">
					<label for="start">Start angle (deg)</label>
					<input id="start" type="number" bind:value={startDeg} />
				</div>
				<div class="field">
					<label for="stop">Stop angle (deg)</label>
					<input id="stop" type="number" bind:value={stopDeg} />
				</div>
				<div class="field">
					<label for="step">Step size (deg)</label>
					<input id="step" type="number" bind:value={stepDeg} min="0.1" step="0.5" />
				</div>
				<div class="field">
					<label for="samples">Samples per angle</label>
					<input id="samples" type="number" bind:value={samplesPerAngle} min="1" />
				</div>
				<div class="button-row">
					<button onclick={startSweep} disabled={!status.serial_connected || status.sweeping}>
						Start Sweep
					</button>
				</div>
				<div class="button-row connect-row">
					<button class="danger" onclick={stopSweep} disabled={!status.serial_connected}>
						Stop
					</button>
					<button class="secondary" onclick={returnHome} disabled={!status.serial_connected || status.sweeping}>
						Home
					</button>
				</div>
				<p class="info">{activeData.length} data points</p>
			</section>

			<section>
				<h2>Plot</h2>
				<div class="field">
					<label for="dynrange">Dynamic range (dB)</label>
					<input id="dynrange" type="number" bind:value={dynamicRangeDb} min="10" max="80" step="5" />
				</div>
			</section>

			{#if measurements.length > 0}
				<section class="measurements-section">
					<div class="measurements-header">
						<h2>Measurements</h2>
						<button class="small-btn danger" onclick={clearAllMeasurements}>Clear All</button>
					</div>
					<div class="measurements-list">
						{#each measurements as m (m.id)}
							<div class="measurement-row">
								<label class="measurement-toggle">
									<input type="checkbox" checked={m.visible} onchange={() => toggleMeasurement(m.id)} />
									<span class="color-dot" style="background: {m.color}"></span>
									{m.name}
								</label>
								<button class="icon-btn" onclick={() => deleteMeasurement(m.id)}>&times;</button>
							</div>
						{/each}
					</div>
				</section>
			{/if}

			<section>
				<h2>Export</h2>
				<a href="/api/export/csv" download="beambench.csv">
					<button disabled={activeData.length === 0 && measurements.length === 0}>Download CSV</button>
				</a>
			</section>

			<section class="log-section">
				<h2>Log</h2>
				<div class="log" bind:this={logDiv}>
					{#each logEntries as entry}
						<div class="log-entry"><span class="log-ts">{entry.ts}</span> {entry.msg}</div>
					{/each}
				</div>
			</section>
		</div>

		<div class="plot" bind:this={plotDiv}></div>
	</div>
</main>

<style>
	:global(body) {
		margin: 0;
		font-family: system-ui, -apple-system, sans-serif;
		background: #111;
		color: #ddd;
	}

	main {
		padding: 1.5rem;
		max-width: 1400px;
		margin: 0 auto;
	}

	h1 {
		margin: 0 0 1.25rem;
		font-size: 1.4rem;
		font-weight: 600;
		color: #fff;
	}

	h2 {
		margin: 0 0 0.75rem;
		font-size: 0.75rem;
		font-weight: 600;
		color: #888;
		text-transform: uppercase;
		letter-spacing: 0.08em;
	}

	.error {
		background: #d32f2f;
		color: white;
		padding: 0.5rem 1rem;
		border-radius: 4px;
		margin-bottom: 1rem;
		font-size: 0.85rem;
	}

	.layout {
		display: flex;
		gap: 1.5rem;
		height: calc(100vh - 6rem);
	}

	.sidebar {
		width: 300px;
		flex-shrink: 0;
		display: flex;
		flex-direction: column;
		gap: 1rem;
	}

	section {
		background: #1a1a1a;
		border: 1px solid #2a2a2a;
		border-radius: 6px;
		padding: 1rem;
	}

	.field {
		margin-bottom: 0.6rem;
	}

	.field label {
		display: block;
		font-size: 0.8rem;
		color: #777;
		margin-bottom: 0.25rem;
	}

	input,
	select {
		width: 100%;
		padding: 0.45rem 0.5rem;
		border: 1px solid #333;
		border-radius: 4px;
		background: #222;
		color: #ddd;
		font-size: 0.85rem;
		box-sizing: border-box;
	}

	input:focus {
		outline: none;
		border-color: #555;
	}

	.connect-row {
		display: flex;
		gap: 0.5rem;
		margin-top: 0.25rem;
	}

	.connect-row button {
		flex: 1;
	}

	button {
		padding: 0.5rem 1rem;
		border: 1px solid #444;
		border-radius: 4px;
		background: #222;
		color: #ddd;
		cursor: pointer;
		font-size: 0.85rem;
		width: 100%;
	}

	button:hover:not(:disabled) {
		background: #333;
		border-color: #555;
	}

	button:disabled {
		opacity: 0.3;
		cursor: not-allowed;
	}

	button.secondary {
		color: #888;
		border-color: #333;
	}

	button.danger {
		background: #222;
		color: #e53935;
		border-color: #e53935;
	}

	button.danger:hover {
		background: #2a1010;
	}

	.button-row {
		margin-top: 0.5rem;
	}

	.status-indicators {
		display: flex;
		gap: 0.5rem;
		margin-top: 0.75rem;
	}

	.indicator {
		font-size: 0.7rem;
		padding: 0.2rem 0.5rem;
		border-radius: 3px;
		background: #222;
		border: 1px solid #333;
		color: #555;
	}

	.indicator.active {
		background: #111;
		border-color: #4caf50;
		color: #4caf50;
	}

	.info {
		font-size: 0.8rem;
		color: #666;
		margin: 0.5rem 0 0;
	}

	.plot {
		flex: 1;
		min-height: 400px;
		background: #1a1a1a;
		border: 1px solid #2a2a2a;
		border-radius: 6px;
	}

	a {
		text-decoration: none;
	}

	.log-section {
		flex: 1;
		display: flex;
		flex-direction: column;
		min-height: 0;
	}

	.log {
		flex: 1;
		overflow-y: auto;
		font-family: monospace;
		font-size: 0.75rem;
		line-height: 1.5;
		color: #999;
		min-height: 80px;
		max-height: 200px;
	}

	.log-entry {
		white-space: pre-wrap;
		word-break: break-word;
	}

	.log-ts {
		color: #555;
	}

	.measurements-header {
		display: flex;
		align-items: center;
		justify-content: space-between;
		margin-bottom: 0.5rem;
	}

	.measurements-header h2 {
		margin: 0;
	}

	.measurements-list {
		max-height: 150px;
		overflow-y: auto;
	}

	.measurement-row {
		display: flex;
		align-items: center;
		justify-content: space-between;
		padding: 0.25rem 0;
	}

	.measurement-toggle {
		display: flex;
		align-items: center;
		gap: 0.4rem;
		font-size: 0.8rem;
		color: #bbb;
		cursor: pointer;
	}

	.measurement-toggle input[type='checkbox'] {
		width: auto;
		margin: 0;
	}

	.color-dot {
		display: inline-block;
		width: 8px;
		height: 8px;
		border-radius: 50%;
		flex-shrink: 0;
	}

	.small-btn {
		padding: 0.2rem 0.5rem;
		font-size: 0.7rem;
		width: auto;
	}

	.icon-btn {
		padding: 0.1rem 0.4rem;
		font-size: 1rem;
		width: auto;
		line-height: 1;
		color: #666;
		border: none;
		background: transparent;
	}

	.icon-btn:hover {
		color: #e53935;
		background: transparent;
	}
</style>
