#!/usr/bin/env bash
# Builds the sample projects an eval run exercises. Kept separate from capture
# so the same workspace can be reused across runs.
set -euo pipefail
root="${1:?workspace root}"
# `--warm` additionally populates the dependency caches the samples need. It is
# run once at image build time so an eval run does not pay for, or depend on,
# resolving a Maven repository or a package registry.
warm="${2:-}"
mkdir -p "$root"
cd "$root"

# Rust
mkdir -p rustdemo/src
cat > rustdemo/Cargo.toml <<'EOF'
[package]
name = "rustdemo"
version = "0.1.0"
edition = "2021"

# Dependencies exist so `cargo build` emits the volume of `Compiling` lines a
# real project does. A single-crate sample would understate the rule.
[dependencies]
regex = "1"
serde_json = "1"
EOF
cat > rustdemo/src/main.rs <<'EOF'
fn unused_helper() {}
fn main() {
    let unused = 1;
    println!("hello");
}
EOF
cat > rustdemo/src/lib.rs <<'EOF'
pub fn add(a: i32, b: i32) -> i32 { a + b }
#[cfg(test)]
mod tests {
    #[test]
    fn adds() { assert_eq!(super::add(1, 2), 3); }
    #[test]
    fn also_adds() { assert_eq!(super::add(2, 2), 4); }
}
EOF

# Go
mkdir -p godemo
cat > godemo/go.mod <<'EOF'
module godemo

go 1.22
EOF
cat > godemo/main.go <<'EOF'
package main

import "fmt"

func main() { fmt.Println("hello") }
EOF
cat > godemo/main_test.go <<'EOF'
package main

import "testing"

func TestOne(t *testing.T) {}
func TestTwo(t *testing.T) {}
EOF

# Go, multi-package. `go test ./...` emits one result line per package, so a
# single-package sample would not show the volume the rule exists to compact.
mkdir -p gomulti
cat > gomulti/go.mod <<'EOF'
module gomulti

go 1.22
EOF
for i in 1 2 3 4 5 6 7 8; do
  mkdir -p "gomulti/pkg$i"
  printf 'package pkg%s\n\nfunc F() int { return %s }\n' "$i" "$i" > "gomulti/pkg$i/a.go"
  printf 'package pkg%s\n\nimport "testing"\n\nfunc TestF(t *testing.T) {\n\tif F() != %s {\n\t\tt.Fatal("unexpected")\n\t}\n}\n' \
    "$i" "$i" > "gomulti/pkg$i/a_test.go"
done

# Java / Maven
mkdir -p mvndemo/src/main/java/com/example
cat > mvndemo/pom.xml <<'EOF'
<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <groupId>com.example</groupId>
  <artifactId>mvndemo</artifactId>
  <version>1.0.0</version>
  <packaging>jar</packaging>
  <properties>
    <maven.compiler.source>17</maven.compiler.source>
    <maven.compiler.target>17</maven.compiler.target>
    <project.build.sourceEncoding>UTF-8</project.build.sourceEncoding>
  </properties>
</project>
EOF
cat > mvndemo/src/main/java/com/example/App.java <<'EOF'
package com.example;

public class App {
    public static void main(String[] args) {
        System.out.println("hello");
    }
}
EOF

# Docker Compose. Services are image-and-command only: the eval may target a
# remote daemon, where a bind mount would resolve on the daemon's filesystem.
mkdir -p composedemo
cat > composedemo/compose.yaml <<'EOF'
services:
  one:
    image: alpine:3.20
    command: echo "service one"
  two:
    image: alpine:3.20
    command: echo "service two"
EOF

# Archive sample with enough members that `tar -v` shows its per-file volume.
mkdir -p tardemo/files
for i in $(seq 1 200); do echo "file $i" > "tardemo/files/f$i.txt"; done

# JavaScript test runners. Jest and Vitest each get their own directory so the
# two discovery conventions cannot collide.
mkdir -p jestdemo/__tests__
cat > jestdemo/package.json <<'EOF'
{ "name": "jestdemo", "version": "1.0.0", "private": true }
EOF
cat > jestdemo/sum.js <<'EOF'
module.exports = function sum(a, b) { return a + b; };
EOF
cat > jestdemo/__tests__/sum.test.js <<'EOF'
const sum = require("../sum");

test("adds", () => { expect(sum(1, 2)).toBe(3); });
test("adds again", () => { expect(sum(2, 2)).toBe(4); });
EOF

mkdir -p vitestdemo
cat > vitestdemo/package.json <<'EOF'
{ "name": "vitestdemo", "version": "1.0.0", "private": true, "type": "module" }
EOF
cat > vitestdemo/sum.js <<'EOF'
export function sum(a, b) { return a + b; }
EOF
cat > vitestdemo/sum.test.js <<'EOF'
import { expect, test } from "vitest";
import { sum } from "./sum.js";

test("adds", () => { expect(sum(1, 2)).toBe(3); });
test("adds again", () => { expect(sum(2, 2)).toBe(4); });
EOF

# Python
mkdir -p pydemo
cat > pydemo/test_sample.py <<'EOF'
def test_one():
    assert 1 == 1

def test_two():
    assert 2 == 2

def test_three():
    assert 3 == 3
EOF

# Node / TypeScript / ESLint
mkdir -p nodedemo/src
cat > nodedemo/package.json <<'EOF'
{ "name": "nodedemo", "version": "1.0.0", "private": true }
EOF
cat > nodedemo/tsconfig.json <<'EOF'
{ "compilerOptions": { "strict": true, "noEmit": true, "target": "ES2020" } }
EOF
cat > nodedemo/src/index.ts <<'EOF'
const value: number = "not a number";
export function greet(name: string) { return `hi ${name}`; }
EOF
# Both config formats are written so the sample works whichever ESLint major
# the image provides.
cat > nodedemo/eslint.config.mjs <<'EOF'
export default [{ files: ["**/*.js"], rules: { "no-unused-vars": "warn" } }];
EOF
cat > nodedemo/.eslintrc.json <<'EOF'
{
  "env": { "es2021": true },
  "parserOptions": { "ecmaVersion": 2021, "sourceType": "module" },
  "rules": { "no-unused-vars": "warn" }
}
EOF
cat > nodedemo/src/app.js <<'EOF'
const unusedVariable = 1;
export function run() { return 2; }
EOF

# Docker
mkdir -p dockerdemo
cat > dockerdemo/Dockerfile <<'EOF'
FROM ubuntu:24.04
RUN echo "step one"
RUN echo "step two"
RUN echo "step three"
EOF

# Git. Recreated rather than updated so a re-run over a warmed workspace
# reproduces the same status output instead of failing on an empty commit.
rm -rf gitdemo
mkdir -p gitdemo
cd gitdemo
git init -q .
git config user.email eval@example.com
git config user.name Eval
echo one > a.txt
git add a.txt
git commit -qm "first commit"
echo two >> a.txt
echo untracked > b.txt
cd ..
cd "$root"
# Progress generators. These are the same programs the fixture capture runs, so a
# live eval measures exactly what the committed fixtures were taken from. They
# are copied in by the image rather than written here, because a reduction
# written against a remembered bar format is not evidence of anything.
#
# The set deliberately includes libraries that emit nothing into a pipe. A
# measurement that a library is silent is only worth having if it is re-checked,
# and the wrong assumption there is what leaves a real case unhandled.
if [ -d /usr/local/share/workcell-progress ]; then
  cp -r /usr/local/share/workcell-progress progressdemo
  (cd progressdemo && npm install --no-fund --no-audit --silent) || true
  (cd progressdemo/rust_indicatif && cargo build --quiet --release) || true
  (cd progressdemo/go_progressbar && go build -o generator .) || true
  (cd progressdemo/go_mpb && go build -o generator .) || true
fi

# Point the docker CLI at the daemon through a context rather than DOCKER_HOST.
# The shell tool gives its child a cleaned environment, and an inline
# `DOCKER_HOST=` assignment would make the command opaque and therefore exempt
# from filtering, so neither can be used to reach the daemon here.
if [ -n "${EVAL_DOCKER_ENDPOINT:-}" ]; then
  docker context create eval --docker "host=${EVAL_DOCKER_ENDPOINT}" >/dev/null 2>&1 || true
  docker context use eval >/dev/null 2>&1 || true
fi

if [ "$warm" = "--warm" ]; then
  # Resolve every dependency once so the captured output reflects the warm,
  # incremental case an agent normally sees rather than a first-run download.
  (cd mvndemo && mvn -q -B package >/dev/null)
  (cd gomulti && go build ./... >/dev/null)
  (cd godemo && go build ./... >/dev/null)
  echo "workspace warmed at $root"
fi

echo "workspace ready at $root"
