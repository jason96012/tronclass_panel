/* QR_AI_SR_V1（fix93）：AI 超分 worker —— 在背景執行緒跑 Swin2SR（×2），主執行緒的預覽不會卡。
 *
 * 為什麼要 worker：模型推論在手機上要數百毫秒到數秒，跑在主執行緒會讓預覽畫面凍住
 * （使用者正在對準，畫面一凍就會以為壞了）。放進 worker 之後，預覽與掃描完全不受影響。
 *
 * 檔案：
 *   /scanner/sr/ort.mjs              onnxruntime-web 1.17.3（WASM 後端、單執行緒、不需要 SharedArrayBuffer）
 *   /scanner/sr/ort-wasm-simd.wasm   ORT 的 WASM 執行檔（10MB）
 *   /scanner/sr/model.onnx           Swin2SR-lightweight ×2（uint8 量化，6.8MB；Apache-2.0）
 *
 * 協定：
 *   收到 { type:"upscale", w, h, buf }  buf＝RGBA Uint8ClampedArray（w×h，已由主執行緒縮到上限內）
 *   回覆 { type:"result", w:2w, h:2h, buf }（RGBA，transferable）
 *   失敗 { type:"error", message }
 */
const ORT_URL = "/scanner/sr/ort.mjs";
const MODEL_URL = "/scanner/sr/model.onnx";
const WASM_PATH = "/scanner/sr/";
const WIN = 8;            /* Swin2SR 的 window size：輸入尺寸要是 8 的倍數（不足補邊） */
const SCALE = 2;

let ort = null;
let session = null;
let loading = null;

async function ensureSession() {
  if (session) return session;
  if (!loading) {
    loading = (async () => {
      ort = await import(ORT_URL);
      ort.env.wasm.wasmPaths = WASM_PATH;
      ort.env.wasm.numThreads = 1;      /* 單執行緒：不必 SharedArrayBuffer（部署端不必加 COOP/COEP） */
      ort.env.wasm.simd = true;
      ort.env.logLevel = "error";
      session = await ort.InferenceSession.create(MODEL_URL, {
        executionProviders: ["wasm"],
        graphOptimizationLevel: "all",
      });
      return session;
    })().catch((err) => {
      loading = null;
      throw err;
    });
  }
  return loading;
}

async function upscale(msg) {
  const s = await ensureSession();
  const w = msg.w | 0;
  const h = msg.h | 0;
  if (w < 8 || h < 8) throw new Error("裁切太小");
  const src = msg.buf instanceof Uint8ClampedArray ? msg.buf
    : (msg.buf instanceof Uint8Array ? new Uint8ClampedArray(msg.buf.buffer, msg.buf.byteOffset, msg.buf.byteLength)
      : new Uint8ClampedArray(msg.buf));
  const ph = (WIN - (h % WIN)) % WIN;
  const pw = (WIN - (w % WIN)) % WIN;
  const H = h + ph;
  const W = w + pw;
  /* RGBA → CHW float32（0–1）；補邊用邊緣像素（避免補黑造成假邊界）。 */
  const data = new Float32Array(3 * H * W);
  for (let y = 0; y < H; y++) {
    const sy = y < h ? y : h - 1;
    for (let x = 0; x < W; x++) {
      const sx = x < w ? x : w - 1;
      const i = (sy * w + sx) * 4;
      const o = y * W + x;
      data[o] = src[i] / 255;
      data[H * W + o] = src[i + 1] / 255;
      data[2 * H * W + o] = src[i + 2] / 255;
    }
  }
  const tensor = new ort.Tensor("float32", data, [1, 3, H, W]);
  const out = await s.run({ pixel_values: tensor });
  const y = out[Object.keys(out)[0]];
  const dims = y.dims;                 /* [1,3,2H,2W] */
  const OW = dims[3];
  const OH = dims[2];
  const f = y.data;
  const ow = w * SCALE;
  const oh = h * SCALE;
  const rgba = new Uint8ClampedArray(ow * oh * 4);
  const plane = OH * OW;
  for (let yy = 0; yy < oh; yy++) {
    for (let xx = 0; xx < ow; xx++) {
      const si = yy * OW + xx;
      const di = (yy * ow + xx) * 4;
      rgba[di] = f[si] * 255;
      rgba[di + 1] = f[plane + si] * 255;
      rgba[di + 2] = f[2 * plane + si] * 255;
      rgba[di + 3] = 255;
    }
  }
  return { w: ow, h: oh, buf: rgba };
}

self.onmessage = async (ev) => {
  const msg = ev.data || {};
  if (msg.type === "warmup") {                 /* 先把 runtime 與模型載好（可選） */
    try { await ensureSession(); self.postMessage({ type: "ready" }); }
    catch (err) { self.postMessage({ type: "error", message: String((err && err.message) || err).slice(0, 200) }); }
    return;
  }
  if (msg.type !== "upscale") return;
  try {
    const r = await upscale(msg);
    self.postMessage({ type: "result", w: r.w, h: r.h, buf: r.buf }, [r.buf.buffer]);
  } catch (err) {
    self.postMessage({ type: "error", message: String((err && err.message) || err).slice(0, 200) });
  }
};
