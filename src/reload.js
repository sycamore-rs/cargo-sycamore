// Served only by the development server; never written into dist/.
const script = document.querySelector("script[data-cargo-sycamore-reload]");
let version = script?.dataset.version ?? null;
let socket;
let timer;
let stopped = false;
let retryDelay = 500;

function connect() {
    if (stopped) return;
    const url = new URL("/__sycamore/reload", window.location.href);
    url.protocol = window.location.protocol === "https:" ? "wss:" : "ws:";
    socket = new WebSocket(url);
    socket.addEventListener("open", () => { retryDelay = 500; });
    socket.addEventListener("message", event => {
        if (version !== null && version !== event.data) {
            stopped = true;
            socket.close();
            window.location.reload();
        }
        version = event.data;
    });
    socket.addEventListener("error", () => socket.close());
    socket.addEventListener("close", () => {
        if (!stopped) {
            timer = setTimeout(connect, retryDelay);
            retryDelay = Math.min(retryDelay * 2, 5000);
        }
    });
}

window.addEventListener("pagehide", () => {
    stopped = true;
    clearTimeout(timer);
    socket?.close();
});
window.addEventListener("pageshow", event => {
    if (event.persisted) {
        stopped = false;
        connect();
    }
});
connect();
