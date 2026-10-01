import { spawnSync } from "node:child_process";
import { chmodSync, copyFileSync, mkdirSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const scriptDirectory = dirname(fileURLToPath(import.meta.url));
const desktopDirectory = resolve(scriptDirectory, "..");
const repositoryRoot = resolve(desktopDirectory, "../..");
const development = process.argv.includes("--dev");
const target =
  process.env.TAURI_ENV_TARGET_TRIPLE || process.env.CARGO_BUILD_TARGET || hostTriple();
const extension = process.platform === "win32" ? ".exe" : "";
const binaryName = `cogito-harness-runtime${extension}`;
const cargoTarget = process.env.TAURI_ENV_TARGET_TRIPLE || process.env.CARGO_BUILD_TARGET;
const targetRoot = process.env.CARGO_TARGET_DIR
  ? resolve(repositoryRoot, process.env.CARGO_TARGET_DIR)
  : join(repositoryRoot, "target");

runCargo(target, development, cargoTarget);

const profileDirectory = development ? "debug" : "release";
const builtBinary = join(
  targetRoot,
  ...(cargoTarget ? [cargoTarget] : []),
  profileDirectory,
  binaryName,
);
const sidecarDirectory = join(desktopDirectory, "src-tauri", "binaries");
const sidecarPath = join(
  sidecarDirectory,
  `cogito-harness-runtime-${target}${extension}`,
);

mkdirSync(sidecarDirectory, { recursive: true });
copyFileSync(builtBinary, sidecarPath);
if (process.platform !== "win32") chmodSync(sidecarPath, 0o755);
console.log(`Prepared Tauri runtime sidecar: ${sidecarPath}`);

function hostTriple() {
  const result = spawnSync("rustc", ["-vV"], { encoding: "utf8" });
  if (result.status !== 0) {
    throw new Error(`could not determine Rust host target: ${result.stderr}`);
  }
  const match = result.stdout.match(/^host: (.+)$/m);
  if (!match) throw new Error("rustc -vV did not report its host target");
  return match[1].trim();
}

function runCargo(targetTriple, isDevelopment, cargoTargetTriple) {
  const args = [
      "build",
      "--manifest-path",
      join(repositoryRoot, "Cargo.toml"),
      "-p",
      "harness-rpc",
      "--bin",
      "cogito-harness-runtime",
  ];
  if (!isDevelopment) args.push("--release");
  if (cargoTargetTriple) args.push("--target", targetTriple);
  const result = spawnSync(
    "cargo",
    args,
    { cwd: repositoryRoot, stdio: "inherit" },
  );
  if (result.error) throw result.error;
  if (result.status !== 0) process.exit(result.status ?? 1);
}
