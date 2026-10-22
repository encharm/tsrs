// Watch mode end to end: `tsrs --watch` on a small project reports no errors, then the type error an edit makes,
// then none again once the edit is undone.
//   node tools/watch/smoke.mjs <path to tsrs>
import { spawn } from "node:child_process";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const bin = process.argv[2];
if (!bin) {
  console.error("usage: node tools/watch/smoke.mjs <path to tsrs>");
  process.exit(2);
}

const dir = mkdtempSync(join(tmpdir(), "tsrs-watch-smoke-"));
writeFileSync(join(dir, "tsconfig.json"), JSON.stringify({ compilerOptions: { strict: true, noEmit: true } }));
const file = join(dir, "a.ts");
const good = "export const a: number = 1;\n";
writeFileSync(file, good);

const child = spawn(bin, ["--watch", "--pretty", "false", "-p", dir], { stdio: ["ignore", "pipe", "pipe"] });
// the error count of each "Found N errors" line, in order
const reports = [];
let pending = "";
const onData = (d) => {
  const lines = (pending + d).split(/\r?\n/);
  pending = lines.pop();
  for (const line of lines) {
    const m = /Found (\d+) errors?/.exec(line);
    if (m) reports.push(Number(m[1]));
  }
};
child.stdout.on("data", onData);
child.stderr.on("data", onData);

function finish(code, message) {
  child.kill();
  rmSync(dir, { recursive: true, force: true });
  console.log(message);
  process.exit(code);
}

const steps = [
  // the first check, then an edit that adds a type error, then the edit undone
  { expect: 0, then: () => writeFileSync(file, 'export const a: number = "no";\n') },
  { expect: 1, then: () => writeFileSync(file, good) },
  { expect: 0, then: null },
];
let step = 0;
const deadline = Date.now() + 120_000;
const timer = setInterval(() => {
  while (reports.length > 0 && step < steps.length) {
    const errors = reports.shift();
    if (errors !== steps[step].expect) {
      clearInterval(timer);
      finish(1, `step ${step + 1}: expected ${steps[step].expect} errors, the check found ${errors}`);
    }
    console.log(`step ${step + 1}: ${errors} errors, as expected`);
    steps[step].then?.();
    step++;
  }
  if (step === steps.length) {
    clearInterval(timer);
    finish(0, "watch mode: ok");
  }
  if (Date.now() > deadline) {
    clearInterval(timer);
    finish(1, `timed out at step ${step + 1}`);
  }
}, 50);
child.on("exit", (code) => {
  if (step < steps.length) {
    clearInterval(timer);
    finish(1, `tsrs exited with ${code} at step ${step + 1}`);
  }
});
