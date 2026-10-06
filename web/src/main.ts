import { Client } from '@modelcontextprotocol/sdk/client/index.js';
import type { Tool } from '@modelcontextprotocol/sdk/types.js';
import { ApplesauceRelayPool } from '@contextvm/sdk/relay';
import { PrivateKeySigner } from '@contextvm/sdk/signer';
import { NostrClientTransport } from '@contextvm/sdk/transport';
import { generateSecretKey, nip19 } from 'nostr-tools';
import { bytesToHex } from 'nostr-tools/utils';

// Remembered per browser so the client identity is stable across reloads and
// can be listed in the server's ALLOWED_CLIENT_PUBKEYS.
const SK_STORAGE_KEY = 'contextbtc.clientSecretKey';
const FORM_STORAGE_KEY = 'contextbtc.form';

const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;

const relaysInput = $<HTMLInputElement>('relays');
const serverInput = $<HTMLInputElement>('server');
const skInput = $<HTMLInputElement>('sk');
const connectBtn = $<HTMLButtonElement>('connect');
const disconnectBtn = $<HTMLButtonElement>('disconnect');
const statusEl = $<HTMLParagraphElement>('status');
const toolsPanel = $<HTMLFieldSetElement>('tools-panel');
const toolSelect = $<HTMLSelectElement>('tool');
const toolDesc = $<HTMLParagraphElement>('tool-desc');
const argsInput = $<HTMLTextAreaElement>('args');
const callBtn = $<HTMLButtonElement>('call');
const resultEl = $<HTMLPreElement>('result');
const resultMeta = $<HTMLSpanElement>('result-meta');

let client: Client | undefined;
let tools: Tool[] = [];

function storageGet(key: string): string | null {
  try {
    return localStorage.getItem(key);
  } catch {
    return null;
  }
}

function storageSet(key: string, value: string) {
  try {
    localStorage.setItem(key, value);
  } catch {
    // Storage unavailable (private window etc.); the form still works.
  }
}

function setStatus(text: string, cls: 'muted' | 'ok' | 'error' = 'muted') {
  statusEl.textContent = text;
  statusEl.className = cls;
}

function showResult(text: string, isError: boolean, meta = '') {
  resultEl.textContent = text;
  resultEl.className = isError ? 'error' : '';
  resultMeta.textContent = meta;
}

/** Comma-separated relay URLs, falling back to the local relay like the Rust client. */
function relayUrls(): string[] {
  const urls = relaysInput.value
    .split(',')
    .map((s) => s.trim())
    .filter(Boolean);
  return urls.length ? urls : ['ws://localhost:10547'];
}

/** Resolve the client secret key (hex or nsec), generating and storing one if blank. */
function resolveSecretKey(): string {
  let sk = skInput.value.trim() || storageGet(SK_STORAGE_KEY) || '';
  if (!sk) {
    sk = bytesToHex(generateSecretKey());
  } else if (sk.startsWith('nsec')) {
    const decoded = nip19.decode(sk);
    if (decoded.type !== 'nsec') throw new Error('Invalid nsec key');
    sk = bytesToHex(decoded.data);
  }
  if (!/^[0-9a-f]{64}$/i.test(sk)) throw new Error('Secret key must be 64-char hex or nsec');
  storageSet(SK_STORAGE_KEY, sk);
  return sk.toLowerCase();
}

function restoreForm() {
  const saved = storageGet(FORM_STORAGE_KEY);
  if (!saved) return;
  try {
    const { relays, server } = JSON.parse(saved);
    if (relays) relaysInput.value = relays;
    if (server) serverInput.value = server;
  } catch {
    // Ignore malformed saved state.
  }
}

function setConnected(connected: boolean) {
  connectBtn.disabled = connected;
  disconnectBtn.disabled = !connected;
  toolsPanel.disabled = !connected;
  relaysInput.disabled = serverInput.disabled = skInput.disabled = connected;
}

/** Build an argument skeleton from a tool's JSON schema, e.g. `{"blockhash": ""}`. */
function argsSkeleton(tool: Tool): Record<string, unknown> {
  const props = (tool.inputSchema.properties ?? {}) as Record<string, { type?: string | string[] }>;
  const required = new Set(tool.inputSchema.required ?? []);
  const skeleton: Record<string, unknown> = {};
  for (const [name, schema] of Object.entries(props)) {
    const type = Array.isArray(schema.type) ? schema.type.find((t) => t !== 'null') : schema.type;
    if (!required.has(name)) {
      skeleton[name] = null;
    } else if (type === 'string') {
      skeleton[name] = '';
    } else if (type === 'integer' || type === 'number') {
      skeleton[name] = 0;
    } else if (type === 'boolean') {
      skeleton[name] = false;
    } else {
      skeleton[name] = null;
    }
  }
  return skeleton;
}

function selectTool(name: string) {
  const tool = tools.find((t) => t.name === name);
  if (!tool) return;
  toolSelect.value = name;
  toolDesc.textContent = tool.description ?? '';
  argsInput.value = JSON.stringify(argsSkeleton(tool), null, 2);
}

/** Drop `null` values so optional params fall back to server defaults. */
function parseArgs(): Record<string, unknown> {
  const raw = argsInput.value.trim() || '{}';
  const parsed = JSON.parse(raw);
  if (typeof parsed !== 'object' || parsed === null || Array.isArray(parsed)) {
    throw new Error('Arguments must be a JSON object');
  }
  return Object.fromEntries(Object.entries(parsed).filter(([, v]) => v !== null));
}

function prettyText(text: string): string {
  try {
    return JSON.stringify(JSON.parse(text), null, 2);
  } catch {
    return text;
  }
}

async function callTool(name: string, args: Record<string, unknown>) {
  if (!client) return;
  callBtn.disabled = true;
  showResult('Calling…', false, name);
  const started = performance.now();
  try {
    // `onprogress` makes the MCP client attach a progressToken to the request.
    // The server only splits large responses (e.g. a full block) into CEP-22
    // chunks when one is present; without it, it tries a single oversized
    // Nostr event that never arrives and the call times out.
    const result = await client.callTool({ name, arguments: args }, undefined, {
      onprogress: ({ progress, total }) => {
        resultMeta.textContent = `${name} · receiving ${progress}${total ? `/${total}` : ''}…`;
      },
      resetTimeoutOnProgress: true,
      timeout: 5 * 60_000,
    });
    const elapsed = `${name} · ${Math.round(performance.now() - started)} ms`;
    const content = (result.content ?? []) as Array<{ type: string; text?: string }>;
    const text = content
      .map((c) => (c.type === 'text' ? prettyText(c.text ?? '') : JSON.stringify(c, null, 2)))
      .join('\n\n');
    showResult(text || JSON.stringify(result, null, 2), Boolean(result.isError), elapsed);
  } catch (err) {
    const elapsed = `${name} · ${Math.round(performance.now() - started)} ms`;
    showResult(String(err instanceof Error ? err.message : err), true, elapsed);
  } finally {
    callBtn.disabled = false;
  }
}

async function connect() {
  const serverPubkey = serverInput.value.trim();
  if (!serverPubkey) {
    setStatus('Server pubkey is required.', 'error');
    return;
  }
  connectBtn.disabled = true;
  setStatus('Connecting…');
  try {
    const signer = new PrivateKeySigner(resolveSecretKey());
    const clientPubkey = await signer.getPublicKey();
    storageSet(FORM_STORAGE_KEY, JSON.stringify({ relays: relaysInput.value, server: serverPubkey }));

    const transport = new NostrClientTransport({
      signer,
      relayHandler: new ApplesauceRelayPool(relayUrls()),
      serverPubkey,
    });
    client = new Client({ name: 'contextbtc-web', version: '0.1.0' });
    await client.connect(transport);

    tools = (await client.listTools()).tools;
    toolSelect.replaceChildren(
      ...tools.map((t) => new Option(t.name, t.name)),
    );
    if (tools.length) selectTool(tools[0].name);

    setConnected(true);
    setStatus(`Connected as ${clientPubkey}. Discovered ${tools.length} tool(s).`, 'ok');
  } catch (err) {
    await disconnect();
    setStatus(`Connection failed: ${err instanceof Error ? err.message : err}`, 'error');
  }
}

async function disconnect() {
  const current = client;
  client = undefined;
  tools = [];
  toolSelect.replaceChildren();
  toolDesc.textContent = '';
  setConnected(false);
  setStatus('Disconnected.');
  try {
    await current?.close();
  } catch {
    // Already closed.
  }
}

connectBtn.addEventListener('click', connect);
disconnectBtn.addEventListener('click', disconnect);
toolSelect.addEventListener('change', () => selectTool(toolSelect.value));
callBtn.addEventListener('click', () => {
  let args: Record<string, unknown>;
  try {
    args = parseArgs();
  } catch (err) {
    showResult(`Invalid arguments: ${err instanceof Error ? err.message : err}`, true);
    return;
  }
  callTool(toolSelect.value, args);
});
for (const btn of document.querySelectorAll<HTMLButtonElement>('[data-quick]')) {
  btn.addEventListener('click', () => callTool(btn.dataset.quick!, {}));
}

restoreForm();
