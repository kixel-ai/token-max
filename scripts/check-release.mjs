import assert from "node:assert/strict";
import { readFile, readdir } from "node:fs/promises";

const pkg = JSON.parse(await readFile("package.json", "utf8"));
assert.match(
  process.env.RELEASE_VERSION ?? "",
  /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/,
);
assert.equal(
  process.env.RELEASE_VERSION,
  pkg.version,
  "release input must match the single package version",
);
assert.notEqual(
  pkg.version,
  "0.0.0",
  "never publish the development placeholder",
);
assert.notEqual(
  pkg.private,
  true,
  "m0 must remove the private guard for the complete release",
);
assert.equal(
  process.env.GITHUB_REF,
  "refs/heads/main",
  "publish only from main",
);
assert.equal(
  process.env.GITHUB_REF_PROTECTED,
  "true",
  "main must be protected",
);
assert.equal(process.env.GITHUB_REPOSITORY, "kixelated/token-max");
const children = (await readdir("quest/m0")).filter(
  (name) => name !== "README.md",
);
assert.deepEqual(
  children,
  [],
  "m0 owns first publication after every child merges",
);

// A missing environment would otherwise be created without protection by Actions.
const api = async (path) => {
  const response = await fetch(
    `https://api.github.com/repos/kixelated/token-max/${path}`,
    {
      headers: {
        Authorization: `Bearer ${process.env.GH_TOKEN}`,
        Accept: "application/vnd.github+json",
        "X-GitHub-Api-Version": "2022-11-28",
      },
    },
  );
  assert.ok(
    response.ok,
    `release protection lookup failed: ${response.status}`,
  );
  return response.json();
};
const environment = await api("environments/npm");
assert.equal(
  environment.deployment_branch_policy?.custom_branch_policies,
  true,
  "npm environment must allow only main",
);
const policies = await api("environments/npm/deployment-branch-policies");
assert.deepEqual(
  policies.branch_policies.map(({ name, type }) => ({ name, type })),
  [{ name: "main", type: "branch" }],
);
console.log(
  `Protected release preparation allowed for ${pkg.name}@${pkg.version}`,
);
