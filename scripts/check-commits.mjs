import { pathToFileURL } from "node:url";
import { fail, git } from "./git.mjs";

const types = ["feat", "fix", "chore", "refactor", "docs", "test", "perf"];
const scopes = ["server", "client"];
const shape = new RegExp(
  `^(${types.join("|")})(\\((${scopes.join("|")})\\))?: `,
);

export function subjectProblem(subject) {
  const prefix = shape.exec(subject);
  if (!prefix) {
    return `is not type(scope): description, with a type of ${types.join(", ")} and a scope of ${scopes.join(" or ")} or none`;
  }
  const description = subject.slice(prefix[0].length);
  if (description.trim() === "") return "has no description";
  if (/^[A-Z]/.test(description)) return "starts its description in uppercase";
  if (description.endsWith(".")) return "ends with a period";
  return null;
}

if (import.meta.url === pathToFileURL(process.argv[1]).href) {
  const base = process.argv[2];
  if (!base) fail("Usage: node scripts/check-commits.mjs <base ref>");
  const commits = git(["log", "--no-merges", "--format=%h %s", `${base}..HEAD`])
    .split("\n")
    .filter(Boolean);
  const problems = commits.flatMap((line) => {
    const problem = subjectProblem(line.slice(line.indexOf(" ") + 1));
    return problem ? [`${line}\n  ${problem}`] : [];
  });
  if (problems.length > 0) {
    fail(
      `These commit subjects break the format in the README's Contributing section:\n${problems.join("\n")}\nReword them (git rebase -i ${base}) and push again.`,
    );
  }
  console.log(`${commits.length} commit subject(s) follow the format`);
}
