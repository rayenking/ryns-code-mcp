import { McpServer } from '@modelcontextprotocol/server';
import { serveStdio } from '@modelcontextprotocol/server/stdio';
import * as z from 'zod/v4';
import { spawn, execFile } from 'node:child_process';
import { promises as fs } from 'node:fs';
import path from 'node:path';
import os from 'node:os';
import { promisify } from 'node:util';

const execFileAsync = promisify(execFile);
const ROOT = path.resolve(process.env.RYNS_WORKSPACE_ROOT ?? path.join(process.env.HOME ?? '.', 'RynsWorkspace'));
const MAX_OUTPUT = Number(process.env.RYNS_MCP_MAX_OUTPUT ?? 100_000);
const DEFAULT_TIMEOUT = Number(process.env.RYNS_MCP_COMMAND_TIMEOUT ?? 30_000);

type Proc = {
  id: string;
  child: ReturnType<typeof spawn>;
  command: string;
  cwd: string;
  startedAt: string;
  stdout: string;
  stderr: string;
  exitCode: number | null;
  signal: NodeJS.Signals | null;
  done: boolean;
};

const processes = new Map<string, Proc>();
let nextProcessId = 1;

function safePath(input: string): string {
  const p = path.resolve(ROOT, input);
  const rel = path.relative(ROOT, p);
  if (rel.startsWith('..') || path.isAbsolute(rel)) throw new Error(`Path is outside workspace: ${input}`);
  return p;
}

function clip(s: string): string {
  return s.length > MAX_OUTPUT ? `${s.slice(0, MAX_OUTPUT)}\n...[output truncated]` : s;
}

function result(text: string) {
  return { content: [{ type: 'text' as const, text: clip(text) }] };
}

function spawnProcess(command: string, cwd: string, interactive = false): Proc {
  const child = spawn('/bin/bash', ['-lc', command], {
    cwd,
    env: process.env,
    stdio: ['pipe', 'pipe', 'pipe'],
    detached: false,
  });
  const proc: Proc = {
    id: String(nextProcessId++), child, command, cwd,
    startedAt: new Date().toISOString(), stdout: '', stderr: '',
    exitCode: null, signal: null, done: false,
  };
  child.stdout.on('data', d => { proc.stdout += d.toString(); if (proc.stdout.length > MAX_OUTPUT * 2) proc.stdout = proc.stdout.slice(-MAX_OUTPUT * 2); });
  child.stderr.on('data', d => { proc.stderr += d.toString(); if (proc.stderr.length > MAX_OUTPUT * 2) proc.stderr = proc.stderr.slice(-MAX_OUTPUT * 2); });
  child.on('close', (code, signal) => { proc.exitCode = code; proc.signal = signal; proc.done = true; });
  child.on('error', err => { proc.stderr += `\nprocess error: ${err.message}`; });
  processes.set(proc.id, proc);
  return proc;
}

async function runOnce(command: string, cwd: string, timeout: number) {
  const child = spawn('/bin/bash', ['-lc', command], { cwd, env: process.env, stdio: ['ignore', 'pipe', 'pipe'] });
  let stdout = '', stderr = '';
  child.stdout.on('data', d => { stdout += d.toString(); });
  child.stderr.on('data', d => { stderr += d.toString(); });
  return await new Promise<string>((resolve) => {
    const timer = setTimeout(() => {
      child.kill('SIGTERM');
      setTimeout(() => child.kill('SIGKILL'), 1000).unref();
      resolve(`timed_out: true\nstdout:\n${clip(stdout)}\nstderr:\n${clip(stderr)}`);
    }, timeout);
    child.on('error', e => { clearTimeout(timer); resolve(`error: ${e.message}`); });
    child.on('close', (code, signal) => {
      clearTimeout(timer);
      resolve(`exit_code: ${code ?? 'null'}${signal ? ` signal: ${signal}` : ''}\nstdout:\n${clip(stdout)}${stderr ? `\nstderr:\n${clip(stderr)}` : ''}`);
    });
  });
}

const server = new McpServer({ name: 'ryns-code-mcp', version: '0.2.0' });

server.registerTool('execute_command', {
  title: 'Execute Command',
  description: 'Run a one-shot bash command inside RynsWorkspace. Good for git, npm, tests, builds and system inspection.',
  inputSchema: z.object({ command: z.string().min(1), cwd: z.string().default('.'), timeout_ms: z.number().int().min(100).max(300_000).default(DEFAULT_TIMEOUT) })
}, async ({ command, cwd, timeout_ms }) => {
  try { const dir = safePath(cwd); return result(`cwd: ${dir}\ncommand: ${command}\n${await runOnce(command, dir, timeout_ms)}`); }
  catch (e) { return result(`command_error: ${e instanceof Error ? e.message : String(e)}`); }
});

server.registerTool('start_process', {
  title: 'Start Process',
  description: 'Start a long-running shell command and keep its process/session alive. Use for dev servers, watchers and interactive CLIs.',
  inputSchema: z.object({ command: z.string().min(1), cwd: z.string().default('.') })
}, async ({ command, cwd }) => {
  try { const dir = safePath(cwd); const p = spawnProcess(command, dir, true); return result(JSON.stringify({ process_id: p.id, command, cwd: dir, started_at: p.startedAt }, null, 2)); }
  catch (e) { return result(`start_error: ${e instanceof Error ? e.message : String(e)}`); }
});

server.registerTool('process_output', {
  title: 'Process Output',
  description: 'Read accumulated stdout/stderr and current state from a long-running process.',
  inputSchema: z.object({ process_id: z.string(), clear: z.boolean().default(false) })
}, async ({ process_id, clear }) => {
  const p = processes.get(process_id); if (!p) return result(`process_not_found: ${process_id}`);
  const out = { process_id: p.id, command: p.command, cwd: p.cwd, done: p.done, exit_code: p.exitCode, signal: p.signal, stdout: p.stdout, stderr: p.stderr };
  if (clear) { p.stdout = ''; p.stderr = ''; }
  return result(JSON.stringify(out, null, 2));
});

server.registerTool('send_input', {
  title: 'Send Process Input',
  description: 'Send stdin to a running process. Append a newline when the process expects Enter.',
  inputSchema: z.object({ process_id: z.string(), input: z.string(), newline: z.boolean().default(false) })
}, async ({ process_id, input, newline }) => {
  const p = processes.get(process_id); if (!p) return result(`process_not_found: ${process_id}`);
  if (p.done || !p.child.stdin.writable) return result(`process_not_running: ${process_id}`);
  p.child.stdin.write(input + (newline ? '\n' : ''));
  return result(`sent ${input.length + (newline ? 1 : 0)} bytes to process ${process_id}`);
});

server.registerTool('list_processes', {
  title: 'List Processes',
  description: 'List processes started by this MCP server.',
  inputSchema: z.object({})
}, async () => result(JSON.stringify([...processes.values()].map(p => ({ process_id: p.id, command: p.command, cwd: p.cwd, started_at: p.startedAt, done: p.done, exit_code: p.exitCode, signal: p.signal })), null, 2)));

server.registerTool('kill_process', {
  title: 'Kill Process',
  description: 'Terminate a process started by this MCP server.',
  inputSchema: z.object({ process_id: z.string(), signal: z.enum(['SIGTERM', 'SIGKILL', 'SIGINT']).default('SIGTERM') })
}, async ({ process_id, signal }) => {
  const p = processes.get(process_id); if (!p) return result(`process_not_found: ${process_id}`);
  if (p.done) return result(`process_already_done: ${process_id}`);
  p.child.kill(signal); return result(`sent ${signal} to process ${process_id}`);
});

server.registerTool('read_file', {
  title: 'Read File',
  description: 'Read a UTF-8 file with optional line range.',
  inputSchema: z.object({ path: z.string(), start_line: z.number().int().positive().optional(), end_line: z.number().int().positive().optional() })
}, async ({ path: file, start_line, end_line }) => {
  try { const text = await fs.readFile(safePath(file), 'utf8'); const lines = text.split(/\r?\n/); return result(start_line || end_line ? lines.slice((start_line ?? 1) - 1, end_line ?? lines.length).map((x, i) => `${(start_line ?? 1) + i}: ${x}`).join('\n') : text); }
  catch (e) { return result(`read_error: ${e instanceof Error ? e.message : String(e)}`); }
});

server.registerTool('write_file', {
  title: 'Write File', description: 'Create or overwrite a UTF-8 text file.',
  inputSchema: z.object({ path: z.string(), content: z.string() })
}, async ({ path: file, content }) => {
  try { const p = safePath(file); await fs.mkdir(path.dirname(p), { recursive: true }); await fs.writeFile(p, content, 'utf8'); return result(`wrote ${p} (${Buffer.byteLength(content)} bytes)`); }
  catch (e) { return result(`write_error: ${e instanceof Error ? e.message : String(e)}`); }
});

server.registerTool('edit_file', {
  title: 'Edit File', description: 'Replace exact text in a file; refuses ambiguous matches unless replace_all is enabled.',
  inputSchema: z.object({ path: z.string(), old_text: z.string().min(1), new_text: z.string(), replace_all: z.boolean().default(false), dry_run: z.boolean().default(false) })
}, async ({ path: file, old_text, new_text, replace_all, dry_run }) => {
  try { const p = safePath(file); const old = await fs.readFile(p, 'utf8'); const count = old.split(old_text).length - 1; if (!count) return result('edit_error: old_text not found'); if (!replace_all && count !== 1) return result(`edit_error: ${count} matches; make old_text unique or set replace_all=true`); const updated = replace_all ? old.split(old_text).join(new_text) : old.replace(old_text, new_text); if (!dry_run) await fs.writeFile(p, updated, 'utf8'); return result(`${dry_run ? 'dry-run: ' : ''}replaced ${replace_all ? count : 1} occurrence(s) in ${p}`); }
  catch (e) { return result(`edit_error: ${e instanceof Error ? e.message : String(e)}`); }
});

server.registerTool('apply_patch', {
  title: 'Apply Patch', description: 'Apply a unified diff patch to workspace files using the system patch utility.',
  inputSchema: z.object({ patch: z.string().min(1), cwd: z.string().default('.'), dry_run: z.boolean().default(false) })
}, async ({ patch, cwd, dry_run }) => {
  try { const dir = safePath(cwd); const child = spawn('patch', ['-p1', ...(dry_run ? ['--dry-run'] : [])], { cwd: dir, stdio: ['pipe', 'pipe', 'pipe'] }); let out = '', err = ''; child.stdout.on('data', d => out += d); child.stderr.on('data', d => err += d); child.stdin.write(patch); child.stdin.end(); return result(await new Promise<string>(resolve => child.on('close', code => resolve(`exit_code: ${code}\n${out}${err}`)))); }
  catch (e) { return result(`patch_error: ${e instanceof Error ? e.message : String(e)}`); }
});

server.registerTool('list_directory', {
  title: 'List Directory', description: 'List directory entries with type and size.', inputSchema: z.object({ path: z.string().default('.') })
}, async ({ path: dir }) => {
  try { const p = safePath(dir); const entries = await fs.readdir(p, { withFileTypes: true }); const lines = await Promise.all(entries.sort((a,b)=>a.name.localeCompare(b.name)).map(async e => e.isDirectory() ? `[DIR]  ${e.name}` : `[FILE] ${e.name} (${(await fs.stat(path.join(p,e.name))).size} bytes)`)); return result(lines.join('\n') || '(empty)'); }
  catch (e) { return result(`list_error: ${e instanceof Error ? e.message : String(e)}`); }
});

server.registerTool('directory_tree', {
  title: 'Directory Tree', description: 'Return a recursive workspace tree.', inputSchema: z.object({ path: z.string().default('.'), max_depth: z.number().int().min(0).max(20).default(6) })
}, async ({ path: dir, max_depth }) => {
  try { const root = safePath(dir); async function walk(d:string, depth:number):Promise<string[]> { const rows:string[]=[]; for (const e of (await fs.readdir(d,{withFileTypes:true})).sort((a,b)=>a.name.localeCompare(b.name))) { rows.push(`${'  '.repeat(depth)}${e.isDirectory()?'[DIR] ':'[FILE]'}${e.name}`); if(e.isDirectory() && depth<max_depth) rows.push(...await walk(path.join(d,e.name),depth+1)); } return rows; } return result(`${root}\n${(await walk(root,0)).join('\n')}`); }
  catch(e){ return result(`tree_error: ${e instanceof Error ? e.message : String(e)}`); }
});

server.registerTool('search_files', {
  title: 'Search Files', description: 'Find files using ripgrep glob patterns.', inputSchema: z.object({ path:z.string().default('.'), pattern:z.string(), max_results:z.number().int().positive().max(5000).default(500) })
}, async ({path:dir,pattern,max_results}) => { try { const p=safePath(dir); const {stdout}=await execFileAsync('rg',['--files','--glob',pattern,p],{maxBuffer:5_000_000}); return result(stdout.split(/\r?\n/).filter(Boolean).slice(0,max_results).join('\n')||'(no matches)'); } catch(e:any){ return result(e?.code===1?'(no matches)':`search_error: ${e?.message??e}`); } });

server.registerTool('grep', {
  title:'Grep', description:'Search file contents recursively with ripgrep regex, glob, case and context support.', inputSchema:z.object({pattern:z.string(),path:z.string().default('.'),glob:z.string().optional(),ignore_case:z.boolean().default(false),context:z.number().int().min(0).max(20).default(0),max_results:z.number().int().positive().max(5000).default(500)})
}, async ({pattern,path:dir,glob,ignore_case,context,max_results})=>{ try { const p=safePath(dir); const args=['-n','--color=never',`--max-count=${max_results}`,...(ignore_case?['-i']:[]),...(context?[`-C${context}`]:[]),...(glob?['--glob',glob]:[]),pattern,p]; const {stdout}=await execFileAsync('rg',args,{maxBuffer:10_000_000}); return result(stdout||'(no matches)'); } catch(e:any){ return result(e?.code===1?'(no matches)':`grep_error: ${e?.message??e}`); } });

server.registerTool('create_directory', { title:'Create Directory', description:'Create a directory recursively.', inputSchema:z.object({path:z.string()}) }, async ({path:dir})=>{try{const p=safePath(dir);await fs.mkdir(p,{recursive:true});return result(`created/exists: ${p}`)}catch(e){return result(`mkdir_error: ${e instanceof Error?e.message:String(e)}`)}});
server.registerTool('delete_file', { title:'Delete File', description:'Delete a file or directory; recursive deletion must be explicit.', inputSchema:z.object({path:z.string(),recursive:z.boolean().default(false)}) }, async ({path:file,recursive})=>{try{const p=safePath(file);const s=await fs.stat(p);if(s.isDirectory()&&!recursive)return result('delete_error: directory requires recursive=true');await fs.rm(p,{recursive,force:false});return result(`deleted: ${p}`)}catch(e){return result(`delete_error: ${e instanceof Error?e.message:String(e)}`)}});
server.registerTool('move_file', { title:'Move File', description:'Move or rename a file/directory inside the workspace.', inputSchema:z.object({source:z.string(),destination:z.string()}) }, async ({source,destination})=>{try{const a=safePath(source),b=safePath(destination);await fs.rename(a,b);return result(`moved ${a} -> ${b}`)}catch(e){return result(`move_error: ${e instanceof Error?e.message:String(e)}`)}});
server.registerTool('get_file_info', { title:'Get File Info', description:'Return file/directory metadata.', inputSchema:z.object({path:z.string()}) }, async ({path:file})=>{try{const p=safePath(file),s=await fs.stat(p);return result(JSON.stringify({path:p,type:s.isDirectory()?'directory':'file',size:s.size,mode:s.mode.toString(8),created:s.birthtime.toISOString(),modified:s.mtime.toISOString()},null,2))}catch(e){return result(`stat_error: ${e instanceof Error?e.message:String(e)}`)}});

async function git(args:string[],cwd:string){ const {stdout,stderr}=await execFileAsync('git',args,{cwd,maxBuffer:10_000_000}); return stdout || stderr || '(no output)'; }
for (const [name, args, description] of [
  ['git_status',['status','--short','--branch'],'Show git status and branch.'],
  ['git_diff',['diff','--no-ext-diff'],'Show working-tree diff.'],
  ['git_log',['log','--oneline','--decorate','-20'],'Show recent commits.'],
  ['git_branch',['branch','-vv'],'Show branches and tracking info.']
] as const) {
  server.registerTool(name, { title:name.replaceAll('_',' '), description, inputSchema:z.object({cwd:z.string().default('.')}) }, async ({cwd})=>{try{return result(await git(args,safePath(cwd)))}catch(e){return result(`git_error: ${e instanceof Error?e.message:String(e)}`)}});
}

server.registerTool('workspace_info', { title:'Workspace Info', description:'Show workspace and runtime information.', inputSchema:z.object({}) }, async()=>result(JSON.stringify({workspace:ROOT,hostname:os.hostname(),platform:process.platform,arch:process.arch,node:process.version,cpu_count:os.cpus().length,active_processes:[...processes.keys()]},null,2)));

serveStdio(() => server);
console.error(`ryns-code-mcp v0.2.0 ready — ${ROOT}`);
