import { spawnSync } from "node:child_process";

const candidates = [["uv"], ["python", "-m", "uv"], ["python3", "-m", "uv"]];

const uv = candidates.find(
  ([command, ...prefix]) =>
    spawnSync(command, [...prefix, "--version"], { stdio: "ignore" }).status ===
    0,
);
if (!uv) {
  console.error(
    "uv is neither on PATH nor reachable as `python -m uv`. Install it: https://docs.astral.sh/uv/",
  );
  process.exit(1);
}

const [command, ...prefix] = uv;
const result = spawnSync(command, [...prefix, ...process.argv.slice(2)], {
  stdio: "inherit",
});
process.exit(result.status ?? 1);
