import { readFileSync, writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import openapiTS, { astToString } from "openapi-typescript";
import prettier from "prettier";

const document = new URL("../../../openapi.json", import.meta.url);
const generated = fileURLToPath(
  new URL("../src/generated/openapi.ts", import.meta.url),
);
const check = process.argv.includes("--check");

const ast = await openapiTS(document, { alphabetize: true });
const config = await prettier.resolveConfig(generated);
const fresh = await prettier.format(astToString(ast), {
  ...config,
  filepath: generated,
});

if (!check) {
  writeFileSync(generated, fresh);
  console.log("Wrote src/generated/openapi.ts");
} else {
  let committed = "";
  try {
    committed = readFileSync(generated, "utf8").replace(/\r\n/g, "\n");
  } catch {
    committed = "";
  }
  if (committed !== fresh) {
    console.error(
      "src/generated/openapi.ts is stale, run: pnpm openapi (or pnpm --filter @createrington/content-gateway-client generate)",
    );
    process.exitCode = 1;
  } else {
    console.log("src/generated/openapi.ts is up to date");
  }
}
