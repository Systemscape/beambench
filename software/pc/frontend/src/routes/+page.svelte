<script lang="ts">
	import { onMount, onDestroy } from 'svelte';
	import { createWsConnection, type DataPoint, type SystemStatus, type WsEvent } from '$lib/ws';

	let plotDiv: HTMLDivElement;
	let Plotly: typeof import('plotly.js-dist-min');

	let dataPoints: DataPoint[] = $state([]);
	let status: SystemStatus = $state({
		sweeping: false,
		tx_connected: false,
		turntable_connected: false,
		serial_connected: false,
		data_points: 0
	});
	let ports: string[] = $state([]);
	let selectedPort = $state('');
	let connected = $state(false);
	let errorMessage = $state('');

	// Sweep config
	let startDeg = $state(0);
	let stopDeg = $state(360);
	let stepDeg = $state(5);
	let samplesPerAngle = $state(10);

	let ws: ReturnType<typeof createWsConnection> | null = null;

	function handleEvent(event: WsEvent) {
		switch (event.type) {
			case 'DataPoint':
				dataPoints = [
					...dataPoints,
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
				setTimeout(() => {
					errorMessage = '';
				}, 5000);
				break;
		}
	}

	function updatePlot() {
		if (!Plotly || !plotDiv) return;

		const theta = dataPoints.map((d) => d.angle_deg);
		const r = dataPoints.map((d) => d.rssi_dbm);

		const data = [
			{
				type: 'scatterpolar' as const,
				mode: 'lines+markers' as const,
				r,
				theta,
				name: 'RSSI',
				marker: { size: 4 }
			}
		];

		const layout = {
			polar: {
				radialaxis: {
					title: { text: 'RSSI (dBm)' },
					angle: 90,
					tickangle: 90
				},
				angularaxis: {
					direction: 'clockwise' as const,
					period: 360
				}
			},
			showlegend: false,
			margin: { t: 40, b: 40, l: 40, r: 40 }
		};

		Plotly.react(plotDiv, data, layout, { responsive: true });
	}

	async function fetchPorts() {
		try {
			const res = await fetch('/api/ports');
			ports = await res.json();
			if (ports.length > 0 && !selectedPort) {
				selectedPort = ports[0];
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
		dataPoints = [];
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
				<div class="field">
					<label for="port">Serial Port</label>
					<div class="port-row">
						<select id="port" bind:value={selectedPort}>
							{#each ports as port}
								<option value={port}>{port}</option>
							{/each}
						</select>
						<button onclick={fetchPorts}>Refresh</button>
					</div>
				</div>
				{#if status.serial_connected}
					<button onclick={disconnectSerial}>Disconnect</button>
				{:else}
					<button onclick={connectSerial} disabled={!selectedPort}>Connect</button>
				{/if}
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
					{#if status.sweeping}
						<button class="danger" onclick={stopSweep}>Stop</button>
					{:else}
						<button onclick={startSweep} disabled={!status.serial_connected}>
							Start Sweep
						</button>
					{/if}
				</div>
				<p class="info">{dataPoints.length} data points</p>
			</section>

			<section>
				<h2>Export</h2>
				<a href="/api/export/csv" download="beambench.csv">
					<button disabled={dataPoints.length === 0}>Download CSV</button>
				</a>
			</section>
		</div>

		<div class="plot" bind:this={plotDiv}></div>
	</div>
</main>

<style>
	:global(body) {
		margin: 0;
		font-family: system-ui, -apple-system, sans-serif;
		background: #1a1a2e;
		color: #e0e0e0;
	}

	main {
		padding: 1rem;
		max-width: 1400px;
		margin: 0 auto;
	}

	h1 {
		margin: 0 0 1rem;
		font-size: 1.5rem;
		color: #00d4ff;
	}

	h2 {
		margin: 0 0 0.75rem;
		font-size: 1rem;
		color: #aaa;
		text-transform: uppercase;
		letter-spacing: 0.05em;
	}

	.error {
		background: #ff4444;
		color: white;
		padding: 0.5rem 1rem;
		border-radius: 4px;
		margin-bottom: 1rem;
	}

	.layout {
		display: flex;
		gap: 1rem;
		height: calc(100vh - 6rem);
	}

	.sidebar {
		width: 280px;
		flex-shrink: 0;
		display: flex;
		flex-direction: column;
		gap: 1rem;
	}

	section {
		background: #16213e;
		border-radius: 8px;
		padding: 1rem;
	}

	.field {
		margin-bottom: 0.5rem;
	}

	.field label {
		display: block;
		font-size: 0.8rem;
		color: #888;
		margin-bottom: 0.25rem;
	}

	input,
	select {
		width: 100%;
		padding: 0.4rem;
		border: 1px solid #333;
		border-radius: 4px;
		background: #0f3460;
		color: #e0e0e0;
		font-size: 0.9rem;
		box-sizing: border-box;
	}

	.port-row {
		display: flex;
		gap: 0.5rem;
	}

	.port-row select {
		flex: 1;
	}

	.port-row button {
		flex-shrink: 0;
		padding: 0.4rem 0.6rem;
		font-size: 0.8rem;
	}

	button {
		padding: 0.5rem 1rem;
		border: none;
		border-radius: 4px;
		background: #0f3460;
		color: #00d4ff;
		cursor: pointer;
		font-size: 0.9rem;
		width: 100%;
	}

	button:hover:not(:disabled) {
		background: #1a4a8a;
	}

	button:disabled {
		opacity: 0.4;
		cursor: not-allowed;
	}

	button.danger {
		background: #8b0000;
		color: #ff6b6b;
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
		font-size: 0.75rem;
		padding: 0.2rem 0.5rem;
		border-radius: 12px;
		background: #333;
		color: #888;
	}

	.indicator.active {
		background: #004d00;
		color: #00ff00;
	}

	.info {
		font-size: 0.8rem;
		color: #888;
		margin: 0.5rem 0 0;
	}

	.plot {
		flex: 1;
		min-height: 400px;
		background: #16213e;
		border-radius: 8px;
	}

	a {
		text-decoration: none;
	}
</style>
