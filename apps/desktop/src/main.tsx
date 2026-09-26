import ReactDOM from "react-dom/client";
import App from "./App";
// xterm ships its own stylesheet and must be imported explicitly by the host app.
import "@xterm/xterm/css/xterm.css";
import "./styles.css";

ReactDOM.createRoot(document.getElementById("root")!).render(<App />);
