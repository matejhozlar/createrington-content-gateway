import { existsSync, readdirSync, readFileSync } from "node:fs";
import { pathToFileURL } from "node:url";
import { git } from "./git.mjs";

const EM_DASH = String.fromCodePoint(0x2014);

export function versionIn(toml, table) {
  let current = "";
  for (const line of toml.split(/\r?\n/)) {
    const header = /^\s*\[([^\]]+)\]\s*$/.exec(line);
    if (header) current = header[1].trim();
    const version = /^\s*version\s*=\s*"([^"]+)"/.exec(line);
    if (current === table && version) return version[1];
  }
  return null;
}

export function lockedVersion(cargoLock, name) {
  const block = cargoLock
    .split(/\r?\n\r?\n/)
    .find((entry) => entry.includes(`\nname = "${name}"\n`));
  return block ? (/\nversion = "([^"]+)"/.exec(block)?.[1] ?? null) : null;
}

export function envNames(configRs) {
  return [...configRs.matchAll(/const \w+_ENV: &str = "([A-Z][A-Z0-9_]*)";/g)]
    .map((match) => match[1])
    .sort();
}

export function inReadmeTable(readme, name) {
  return readme.split(/\r?\n/).some((line) => line.startsWith(`| \`${name}\``));
}

export function linesWith(text, needle) {
  return text
    .split("\n")
    .flatMap((line, index) => (line.includes(needle) ? [index + 1] : []));
}

function read(path) {
  return readFileSync(path, "utf8");
}

function versionProblems() {
  const packageVersion = JSON.parse(read("package.json")).version;
  const cargoVersion = versionIn(read("Cargo.toml"), "workspace.package");
  const lockVersion = lockedVersion(read("Cargo.lock"), "content-gateway");
  const problems = [];
  if (packageVersion !== cargoVersion) {
    problems.push(
      `package.json says ${packageVersion} and [workspace.package] in Cargo.toml says ${cargoVersion}: they are one product version`,
    );
  }
  if (lockVersion !== cargoVersion) {
    problems.push(
      `Cargo.lock holds content-gateway ${lockVersion} while Cargo.toml says ${cargoVersion}: run cargo check to update it`,
    );
  }
  return problems;
}

function emDashProblems() {
  const files = git(["ls-files", "-z"]).split("\0").filter(Boolean);
  return files.flatMap((file) => {
    let text;
    try {
      text = read(file);
    } catch {
      return [];
    }
    return linesWith(text, EM_DASH).map(
      (line) => `${file}:${line} holds an em dash (U+2014)`,
    );
  });
}

function envProblems() {
  const names = envNames(read("crates/server/src/config.rs"));
  if (names.length === 0) {
    return [
      "Found no `const *_ENV: &str` in crates/server/src/config.rs; update scripts/check-repo.mjs to how the server names its variables",
    ];
  }
  const example = read(".env.example");
  const readme = read("README.md");
  return names.flatMap((name) => [
    ...(new RegExp(`\\b${name}\\b`).test(example)
      ? []
      : [`${name} is read by the server but missing from .env.example`]),
    ...(inReadmeTable(readme, name)
      ? []
      : [
          `${name} is read by the server but missing from the README's configuration table`,
        ]),
  ]);
}

export function licenceProblems(root, copies) {
  const normalize = (text) => text.replaceAll("\r\n", "\n");
  return Object.entries(copies)
    .filter(([, text]) => normalize(text) !== normalize(root))
    .map(
      ([path]) =>
        `${path} differs from the root LICENSE: copy the root one over it`,
    );
}

function packageLicenceProblems() {
  const copies = Object.fromEntries(
    readdirSync("packages")
      .map((name) => `packages/${name}/LICENSE`)
      .filter((path) => existsSync(path))
      .map((path) => [path, read(path)]),
  );
  return licenceProblems(read("LICENSE"), copies);
}

if (import.meta.url === pathToFileURL(process.argv[1]).href) {
  const problems = [
    ...versionProblems(),
    ...emDashProblems(),
    ...envProblems(),
    ...packageLicenceProblems(),
  ];
  for (const problem of problems) console.error(problem);
  if (problems.length > 0) process.exit(1);
  console.log(
    "Versions agree, no tracked file holds an em dash, every variable is documented, and every package licence is the root one",
  );
}
