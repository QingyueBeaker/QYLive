import { invoke } from "@tauri-apps/api/core";

const canvas = document.getElementById("c") as HTMLCanvasElement;
const ctx = canvas.getContext("2d", { alpha: false })!;
const hint = document.getElementById("hint") as HTMLDivElement;

let lastSeq = -1;
let frameTimer: number | null = null;

/** 与后端预览帧率上限对齐的拉取间隔（毫秒） */
let intervalMs = 33;

async function tick() {
  // 窗口最小化 / 隐藏时不拉取，直接省掉 IPC 与绘制
  if (!document.hidden) {
    try {
      const raw = (await invoke("preview_frame")) as ArrayBuffer;
      if (raw && raw.byteLength > 12) {
        const head = new DataView(raw);
        const seq = Number(head.getBigUint64(0, true));
        const w = head.getUint16(8, true);
        const h = head.getUint16(10, true);
        if (seq !== lastSeq && w > 1 && h > 1) {
          lastSeq = seq;
          if (canvas.width !== w || canvas.height !== h) {
            canvas.width = w;
            canvas.height = h;
          }
          const pixels = new Uint8ClampedArray(raw, 12);
          ctx.putImageData(new ImageData(pixels, w, h), 0, 0);
          hint.style.display = "none";
        }
      }
    } catch {
      /* 未开播时静默等待 */
    }
  }
  frameTimer = window.setTimeout(tick, intervalMs);
}

window.addEventListener("beforeunload", () => {
  if (frameTimer !== null) window.clearTimeout(frameTimer);
});

void tick();
