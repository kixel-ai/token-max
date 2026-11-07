# Everything CI runs.
check:
    bun install --frozen-lockfile
    bunx --no-install prettier --check .
    bun run check
    bun run build
    bun test ./test
    node --test dist/test/*.test.js
    quest check

build:
    bun run build

run *args:
    bun src/cli.ts {{args}}
