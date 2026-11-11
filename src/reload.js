// Served only by the development server; never written into dist/.
const script = document.querySelector("script[data-cargo-sycamore-reload]");
let version = script?.dataset.version ?? null;
let socket;
let timer;
let stopped = false;
let retryDelay = 500;
let statusHost;
let statusPill;
let statusLabel;

// Keep the development UI isolated from application styles and body rendering.
// It never captures pointer events or changes the page's layout/focus.
function showStatus(kind, message) {
	if (!statusHost) {
		statusHost = document.createElement("cargo-sycamore-status");
		statusHost.hidden = true;
		const shadow = statusHost.attachShadow({ mode: "open" });
		const style = document.createElement("style");
		style.textContent = `
            :host {
                all: initial;
                position: fixed;
                bottom: 16px;
                left: 50%;
                transform: translateX(-50%);
                z-index: 2147483647;
                pointer-events: none;
                max-width: calc(100vw - 32px);
                color-scheme: dark;
            }
            :host([hidden]) { display: none !important; }
            .pill {
                display: flex;
                align-items: center;
                gap: 8px;
                box-sizing: border-box;
                padding: 7px 12px;
                border: 1px solid #52525b;
                border-radius: 999px;
                background: #18181bf2;
                color: #e4e4e7;
                box-shadow: 0 3px 12px #0003;
                font: 12px/1.4 system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
                text-align: center;
            }
            .dot {
                flex: 0 0 6px;
                width: 6px;
                height: 6px;
                border-radius: 50%;
                background: #a1a1aa;
            }
            [data-kind="building"] .dot {
                background: #fbbf24;
                animation: pulse 1.2s ease-in-out infinite;
            }
            [data-kind="error"] {
                border-color: #b91c1c;
                background: #450a0af2;
                color: #fecaca;
            }
            [data-kind="error"] .dot { background: #f87171; }
            @keyframes pulse { 50% { opacity: .35; } }
            @media (prefers-reduced-motion: reduce) {
                .dot { animation: none !important; }
            }
        `;
		statusPill = document.createElement("div");
		statusPill.className = "pill";
		statusPill.setAttribute("role", "status");
		statusPill.setAttribute("aria-live", "polite");
		statusPill.setAttribute("aria-atomic", "true");
		const dot = document.createElement("span");
		dot.className = "dot";
		dot.setAttribute("aria-hidden", "true");
		statusLabel = document.createElement("span");
		statusPill.append(dot, statusLabel);
		shadow.append(style, statusPill);
	}
	if (!statusHost.isConnected) document.documentElement.append(statusHost);
	statusPill.dataset.kind = kind;
	if (statusLabel.textContent !== message) statusLabel.textContent = message;
	statusHost.hidden = false;
}

function hideStatus() {
	if (statusHost) statusHost.hidden = true;
}

function connect() {
	if (stopped || socket) return;
	const url = new URL("/__sycamore/reload", window.location.href);
	url.protocol = window.location.protocol === "https:" ? "wss:" : "ws:";
	const connection = new WebSocket(url);
	socket = connection;
	connection.addEventListener("open", () => {
		if (socket === connection) retryDelay = 500;
	});
	connection.addEventListener("message", event => {
		if (socket !== connection || stopped) return;
		if (event.data === "building") {
			showStatus("building", "Rebuilding… page may be stale");
			return;
		}
		if (event.data === "error") {
			showStatus("error", "Build failed · stale page · check terminal");
			return;
		}
		// Other messages are successful-build version tokens. Never advance
		// this baseline on start/failure, or we could miss the next good build.
		if (version !== null && version !== event.data) {
			showStatus("building", "Reloading…");
			stopped = true;
			connection.close();
			window.location.reload();
		} else {
			hideStatus();
		}
		version = event.data;
	});
	connection.addEventListener("error", () => connection.close());
	connection.addEventListener("close", () => {
		// Ignore late callbacks from an older connection (e.g. bfcache restore).
		if (socket !== connection) return;
		socket = null;
		if (!stopped) {
			// Preserve a known build failure while attempting to reconnect.
			if (statusPill?.dataset.kind !== "error") {
				showStatus("disconnected", "Disconnected · reconnecting…");
			}
			timer = setTimeout(connect, retryDelay);
			retryDelay = Math.min(retryDelay * 2, 5000);
		}
	});
}

window.addEventListener("pagehide", () => {
	stopped = true;
	clearTimeout(timer);
	const connection = socket;
	socket = null;
	connection?.close();
});
window.addEventListener("pageshow", event => {
	if (event.persisted) {
		stopped = false;
		connect();
	}
});
connect();
