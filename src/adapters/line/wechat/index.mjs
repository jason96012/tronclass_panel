async function importOpenCV() {
  const cv = await import('./wasm.mjs').then((r) => r.cv);
  await cv.ready;
  const qrcode_detector = await loadModels(cv);
  return {
    cv,
    qrcode_detector
  };
}
let _promise;
async function getOpenCV() {
  if (!_promise)
    _promise = importOpenCV();
  return _promise;
}
async function ready() {
  await getOpenCV();
}
async function scan(input, options = {}) {
  const { cv, qrcode_detector } = await getOpenCV();
  let inputImage, points_vec, res, points, dst;
  try {
    inputImage = cv.imread(input, cv.IMREAD_GRAYSCALE);
    points_vec = new cv.MatVector();
    res = qrcode_detector.detectAndDecode(inputImage, points_vec);
    // Detected corners need not be axis-aligned or ordered along the
    // top-left/bottom-right diagonal (rotated and perspective QR codes).
    // A missing point vector must not suppress an otherwise valid decoded text.
    let rect;
    try {
      if (typeof points_vec.size !== "function" || points_vec.size() > 0) points = points_vec.get(0);
      if (points) {
        let minX = Infinity, minY = Infinity, maxX = -Infinity, maxY = -Infinity;
        for (let i = 0; i < 4; i++) {
          const x = points.floatAt(i * 2), y = points.floatAt(i * 2 + 1);
          if (!Number.isFinite(x) || !Number.isFinite(y)) throw new Error("invalid corners");
          minX = Math.min(minX, x); maxX = Math.max(maxX, x);
          minY = Math.min(minY, y); maxY = Math.max(maxY, y);
        }
        if (maxX > minX && maxY > minY) rect = { x: minX, y: minY, width: maxX - minX, height: maxY - minY };
      }
    } catch (_) { rect = undefined; } // keep decoded text even if coordinates are unavailable
    let rectCanvas;
    if (rect && options.includeRectCanvas) {
      rectCanvas = document.createElement("canvas");
      dst = inputImage.roi(new cv.Rect(rect.x, rect.y, rect.width, rect.height));
      cv.imshow(rectCanvas, dst);
    }
    const text = typeof res.size === "function" && res.size() === 0 ? "" : res.get(0);
    return { text, rect, rectCanvas };
  } finally {
    // OpenCV.js objects own WASM allocations; release them even if detection,
    // rectangle extraction or rendering throws. Results contain only JS data.
    for (const obj of [dst, points, res, points_vec, inputImage]) {
      try { if (obj && typeof obj.delete === "function") obj.delete(); } catch (_) { /* best effort */ }
    }
  }
}

async function loadModels(cv) {
  const models = await import('./wasm.mjs');
  cv.FS_createDataFile("/", "detect.prototxt", models.detect_prototxt, true, false, false);
  cv.FS_createDataFile("/", "detect.caffemodel", models.detect_caffemodel, true, false, false);
  cv.FS_createDataFile("/", "sr.prototxt", models.sr_prototxt, true, false, false);
  cv.FS_createDataFile("/", "sr.caffemodel", models.sr_caffemodel, true, false, false);
  const qrcode_detector = new cv.wechat_qrcode_WeChatQRCode(
    "detect.prototxt",
    "detect.caffemodel",
    "sr.prototxt",
    "sr.caffemodel"
  );
  return qrcode_detector;
}

export { ready, scan };
