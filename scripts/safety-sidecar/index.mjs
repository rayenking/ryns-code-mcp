#!/usr/bin/env node
// ryns-code-mcp safety sidecar (plan §3): newline-delimited JSON bridge
// around cc-safety-net's synchronous checkCommand API.
//
// stdin:   {"v":1,"id":<u64>,"op":"checkCommand","command":"…","cwd":"…"}
// stdout:  {"v":1,"id":<n>,"kind":"allow"}
//          {"v":1,"id":<n>,"kind":"deny","reason":"…","ruleId":"…"}
//          {"v":1,"id":<n>,"kind":"error","message":"…"}        (protocol level)
//
// Frozen rules:
// - upstream checkCommand THROW (bad input / unreadable cwd / policy error)
//   ⇒ kind:"deny", ruleId:"custom.sidecar-error" (fail closed);
// - malformed request line ⇒ kind:"error","message":"malformed request";
// - one response line per request id, order meaningless (server matches id);
// - no audit writes here, no network.

import { createInterface } from 'node:readline';
import { checkCommand } from 'cc-safety-net/api';

const fail = (id, message) =>
  process.stdout.write(
    JSON.stringify({ v: 1, id, kind: 'error', message }) + '\n',
  );

function handle(req) {
  const id = req.id;
  if (typeof id !== 'number') {
    // No usable id — nothing to answer; drop silently (server-side budget
    // will fail the call).
    return;
  }
  if (
    req.op !== 'checkCommand' ||
    typeof req.command !== 'string' ||
    typeof req.cwd !== 'string'
  ) {
    fail(id, 'malformed request');
    return;
  }
  try {
    const out = checkCommand({ command: req.command, cwd: req.cwd });
    if (out && out.kind === 'allow') {
      process.stdout.write(JSON.stringify({ v: 1, id, kind: 'allow' }) + '\n');
      return;
    }
    if (out && out.kind === 'deny') {
      process.stdout.write(
        JSON.stringify({
          v: 1,
          id,
          kind: 'deny',
          reason: String(out.reason ?? 'denied by policy'),
          ruleId: String(out.ruleId ?? 'custom.rule'),
        }) + '\n',
      );
      return;
    }
    // Unknown engine shape — fail closed.
    process.stdout.write(
      JSON.stringify({
        v: 1,
        id,
        kind: 'deny',
        reason: 'unreadable policy result',
        ruleId: 'custom.sidecar-error',
      }) + '\n',
    );
  } catch (err) {
    // Frozen mapping: any throw ⇒ deny, ruleId custom.sidecar-error.
    process.stdout.write(
      JSON.stringify({
        v: 1,
        id,
        kind: 'deny',
        reason: `policy error: ${String(err && err.message ? err.message : err)}`,
        ruleId: 'custom.sidecar-error',
      }) + '\n',
    );
  }
}

const rl = createInterface({ input: process.stdin, crlfDelay: Infinity });
rl.on('line', (line) => {
  if (!line.trim()) return;
  try {
    handle(JSON.parse(line));
  } catch {
    // Unparseable JSON: no id to correlate — ignore the line.
  }
});
rl.on('close', () => process.exit(0));

// Never let a stray stdio error kill the loop silently: report on stderr.
process.on('uncaughtException', (err) => {
  process.stderr.write(`sidecar uncaught: ${String(err)}\n`);
  process.exit(1);
});
