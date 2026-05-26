// Live performance overlay (debug tool). Injected only when Rust set
// window.__TIDALUNAR_PERF__ (env TIDALUNAR_PERF). Listens on the "perf.sample"
// IPC channel for per-CEF-process CPU/RAM from src/debug/perf_monitor.rs and
// draws sparklines, plus a few page-side metrics it can read for free.
//
// The draw loop runs ONLY while the overlay is visible (toggle: F9), so the
// tool never adds frames to what it is measuring when hidden.
import { onIpcEvent } from "../ipc";

interface ProcInfo {
    kind: string;
    pid: number;
    cpu: number;
    mem_mb: number;
}
interface PerfSample {
    cpu_total: number;
    mem_mb_total: number;
    procs: ProcInfo[];
}

const HISTORY = 120; // samples kept per series (~60s at 500ms)
const WIDTH = 360;
const ROW_H = 30;
const ROW_GAP = 16; // leaves room to print the label/value above each graph

class Series {
    private buf: number[] = [];
    push(v: number) {
        this.buf.push(v);
        if (this.buf.length > HISTORY) this.buf.shift();
    }
    get last(): number {
        return this.buf.length ? this.buf[this.buf.length - 1] : 0;
    }
    get peak(): number {
        return this.buf.reduce((m, v) => (v > m ? v : m), 1e-6);
    }
    get mean(): number {
        return this.buf.length ? this.buf.reduce((s, v) => s + v, 0) / this.buf.length : 0;
    }
    get values(): number[] {
        return this.buf;
    }
}

export function initPerfOverlay(): void {
    if ((window as { __perfOverlay?: boolean }).__perfOverlay) return; // idempotent
    (window as { __perfOverlay?: boolean }).__perfOverlay = true;

    const rows: Array<{ s: Series; color: string; label: string; unit: string }> = [
        { s: new Series(), color: "#ff9f40", label: "CPU", unit: "%" },
        { s: new Series(), color: "#4fc3f7", label: "RAM", unit: "MB" },
        { s: new Series(), color: "#ba68c8", label: "JS heap", unit: "MB" },
        { s: new Series(), color: "#fff176", label: "Listeners", unit: "" },
        { s: new Series(), color: "#f06292", label: "Recalcs", unit: "/s" },
        { s: new Series(), color: "#81c784", label: "FPS", unit: "" },
        { s: new Series(), color: "#e57373", label: "DOM", unit: " nodes" },
    ];
    const [cpu, ram, heap, listeners, recalcs, fps, dom] = rows.map((r) => r.s);
    let topProc = "";

    const height = HISTORY > 0 ? rows.length * (ROW_H + ROW_GAP) + ROW_GAP + 16 : 0;
    const dpr = window.devicePixelRatio || 1;
    const canvas = document.createElement("canvas");
    canvas.width = WIDTH * dpr;
    canvas.height = height * dpr;
    Object.assign(canvas.style, {
        position: "fixed",
        top: "8px",
        left: "8px",
        width: `${WIDTH}px`,
        height: `${height}px`,
        zIndex: "2147483647",
        pointerEvents: "none",
        borderRadius: "6px",
        boxShadow: "0 2px 12px rgba(0,0,0,0.5)",
    });
    const ctx = canvas.getContext("2d");
    if (!ctx) return;
    ctx.scale(dpr, dpr);
    ctx.font = "11px monospace";
    document.documentElement.appendChild(canvas);

    onIpcEvent("perf.sample", (s: PerfSample) => {
        cpu.push(s.cpu_total);
        ram.push(s.mem_mb_total);
        const top = (s.procs || [])[0];
        topProc = top ? `${top.kind} ${top.cpu.toFixed(0)}% / ${top.mem_mb.toFixed(0)}MB` : "";
        // Page-side metrics, sampled at the same 500ms cadence (cheap):
        const mem = (performance as { memory?: { usedJSHeapSize: number } }).memory;
        if (mem) heap.push(mem.usedJSHeapSize / 1048576);
        dom.push(document.getElementsByTagName("*").length);
    });

    // Engine metrics from CDP (src/debug/perf_observer.rs): listeners is a gauge;
    // recalc_total is cumulative, so derive a per-second rate from its delta.
    let prevRecalc = 0;
    let prevEngineT = 0;
    onIpcEvent("perf.engine", (e: { listeners: number; recalc_total: number }) => {
        listeners.push(e.listeners);
        const now = performance.now();
        if (prevEngineT) {
            const dt = (now - prevEngineT) / 1000;
            if (dt > 0) recalcs.push(Math.max(0, (e.recalc_total - prevRecalc) / dt));
        }
        prevRecalc = e.recalc_total;
        prevEngineT = now;
    });

    let visible = false; // hidden by default; F9 reveals it
    let frames = 0;
    let fpsClock = performance.now();

    const fmt = (v: number) => (v < 10 ? v.toFixed(1) : v.toFixed(0));

    const drawRow = (y: number, r: (typeof rows)[number]) => {
        ctx.strokeStyle = "rgba(255,255,255,0.12)";
        ctx.strokeRect(8, y, WIDTH - 16, ROW_H);
        const vals = r.s.values;
        const peak = r.s.peak;
        ctx.strokeStyle = r.color;
        ctx.beginPath();
        vals.forEach((v, i) => {
            const px = 8 + (i / (HISTORY - 1)) * (WIDTH - 16);
            const py = y + ROW_H - (v / peak) * ROW_H;
            if (i === 0) ctx.moveTo(px, py);
            else ctx.lineTo(px, py);
        });
        ctx.stroke();
        // Average over the visible window, drawn as a dashed baseline.
        const mean = r.s.mean;
        const my = y + ROW_H - (mean / peak) * ROW_H;
        ctx.strokeStyle = "rgba(255,255,255,0.3)";
        ctx.setLineDash([3, 3]);
        ctx.beginPath();
        ctx.moveTo(8, my);
        ctx.lineTo(WIDTH - 8, my);
        ctx.stroke();
        ctx.setLineDash([]);
        // Label + current + window average, printed above the graph so the text
        // never sits on top of the line.
        ctx.fillStyle = r.color;
        ctx.fillText(`${r.label} ${fmt(r.s.last)}${r.unit}`, 10, y - 4);
        ctx.fillStyle = "rgba(255,255,255,0.45)";
        ctx.fillText(`avg ${fmt(mean)}`, WIDTH - 70, y - 4);
    };

    const draw = () => {
        ctx.clearRect(0, 0, WIDTH, height);
        ctx.fillStyle = "rgba(10,12,16,0.92)";
        ctx.fillRect(0, 0, WIDTH, height);
        rows.forEach((r, i) => drawRow(ROW_GAP + i * (ROW_H + ROW_GAP), r));
        ctx.fillStyle = "rgba(255,255,255,0.6)";
        ctx.fillText(`hot: ${topProc}`, 12, height - 5);
    };

    const loop = (t: number) => {
        if (!visible) return;
        frames++;
        if (t - fpsClock >= 500) {
            fps.push((frames * 1000) / (t - fpsClock));
            frames = 0;
            fpsClock = t;
        }
        draw();
        requestAnimationFrame(loop);
    };
    // Start hidden so the 60fps draw loop never runs (and never perturbs the
    // measurement) until explicitly toggled on with F9. Data still accumulates
    // via the IPC handlers above so the graph has history when revealed.
    canvas.style.display = "none";

    window.addEventListener("keydown", (e) => {
        if (e.key !== "F9") return;
        e.preventDefault();
        visible = !visible;
        canvas.style.display = visible ? "block" : "none";
        if (visible) {
            fpsClock = performance.now();
            frames = 0;
            requestAnimationFrame(loop);
        }
    });
}
