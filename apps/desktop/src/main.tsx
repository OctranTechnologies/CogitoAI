import ReactDOM from "react-dom/client";
import { isTauri } from "@tauri-apps/api/core";
import App from "./App";
import { listenRuntimeConnectionStates } from "./lib/rpc";
import { useDesktopStore } from "./store";
// xterm ships its own stylesheet and must be imported explicitly by the host app.
import "@xterm/xterm/css/xterm.css";
import "./styles.css";

async function mount() {
  if (isTauri()) {
    await listenRuntimeConnectionStates((event) => {
      useDesktopStore.getState().setRuntimeConnectionState(event.state);
    }).catch(() => undefined);
  }
  ReactDOM.createRoot(document.getElementById("root")!).render(<App />);
}

void mount();
