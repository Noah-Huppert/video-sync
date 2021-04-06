SRC := $(wildcard src/**/*.ts)
SRC_MAIN := src/main.ts

DENO_OPTS := --config ./tsconfig.json --unstable --allow-read --allow-net

run: ${SRC}
	deno run ${DENO_OPTS} ${SRC_MAIN}
