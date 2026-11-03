// Writes openapi.json from the server's own description of its routes. The
// client's generated types follow in the pnpm script that calls this.
import { execFileSync } from "node:child_process";
import { writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

const root = fileURLToPath(new URL("..", import.meta.url));
const document = execFileSync(
  "cargo",
  ["run", "--quiet", "--locked", "--bin", "content-gateway", "--", "--openapi"],
  { cwd: root, encoding: "utf8", stdio: ["ignore", "pipe", "inherit"] },
).replace(/\r\n/g, "\n");

writeFileSync(new URL("../openapi.json", import.meta.url), document);
console.log("Wrote openapi.json");
