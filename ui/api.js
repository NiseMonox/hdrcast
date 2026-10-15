// The bridge to the Rust side. Inside the app this is Tauri's IPC; opened in a plain browser (for working on the
// design) it falls back to mock.js, which plays a believable backend.

const tauri = window.__TAURI__;
export const isTauri = !!tauri;

let mockPromise = null;
function mock() {
  if (!mockPromise) mockPromise = import('./mock.js');
  return mockPromise;
}

export async function invoke(cmd, args) {
  if (isTauri) return tauri.core.invoke(cmd, args);
  return (await mock()).invoke(cmd, args);
}

/** Subscribes to a backend event; returns a function that unsubscribes. */
export function listen(event, handler) {
  let off = null;
  let cancelled = false;
  const ready = isTauri
    ? tauri.event.listen(event, (e) => handler(e.payload))
    : mock().then((m) => m.listen(event, handler));
  ready.then((unlisten) => {
    if (cancelled) unlisten();
    else off = unlisten;
  });
  return () => {
    cancelled = true;
    if (off) off();
  };
}

export const win = {
  minimize: () => isTauri && tauri.window.getCurrentWindow().minimize(),
  toggleMaximize: () => isTauri && tauri.window.getCurrentWindow().toggleMaximize(),
  close: () => (isTauri ? tauri.window.getCurrentWindow().close() : undefined),
  // The window is created hidden and shown once the first frame is drawn, so it never flashes white.
  show: () => (isTauri ? tauri.window.getCurrentWindow().show() : undefined),
};
