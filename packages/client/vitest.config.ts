import { defineConfig } from "vitest/config";

export default defineConfig({
  test: {
    projects: [
      { extends: true, test: { name: "zod 4" } },
      {
        extends: true,
        test: {
          name: "zod 3.25",
          include: ["tests/curseforge.test.ts", "tests/modrinth.test.ts"],
        },
        resolve: { alias: { zod: "zod3" } },
      },
    ],
  },
});
