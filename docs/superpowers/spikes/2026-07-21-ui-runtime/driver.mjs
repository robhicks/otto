// Headless otto protocol driver for the 2026-07-21 UI runtime spike.
// Ground truth for what the server emits, independent of any UI client.
//
// Wire framing (see crates/protocol): ServerMessage is INTERNALLY tagged
// ({"type":"ready"|"event"|...}); Command is EXTERNALLY tagged ({"SendPrompt":{...}}).
import WebSocket from 'ws';

const arg = (name, def) => {
  const i = process.argv.indexOf(`--${name}`);
  return i === -1 ? def : process.argv[i + 1];
};

const url = arg('url', 'ws://127.0.0.1:8899');
const token = arg('token', 'spike-token');
const script = arg('script', 'turn');
const PROMPT = 'Add a doc comment to the add function in src/lib.rs';

const events = [];
const meta = { script, startedAt: Date.now() };

function connect(extra = '') {
  return new WebSocket(`${url}/ws?token=${encodeURIComponent(token)}${extra}`);
}

function send(ws, cmd) {
  ws.send(JSON.stringify(cmd));
}

const done = (code) => {
  meta.finishedAt = Date.now();
  process.stdout.write(JSON.stringify({ events, meta }, null, 2));
  process.exit(code);
};

const ws = connect();
let ready = null;

ws.on('message', (raw) => {
  const msg = JSON.parse(raw.toString());
  events.push({ at: Date.now(), msg });

  if (msg.type === 'ready') {
    ready = msg;
    meta.ready = msg;
    const session = msg.session;
    if (script === 'turn' || script === 'approve') send(ws, { SendPrompt: { session, text: PROMPT } });
    if (script === 'abort') send(ws, { SendPrompt: { session, text: PROMPT } });
    if (script === 'promote') send(ws, { SendPrompt: { session, text: PROMPT } });
    return;
  }

  if (msg.type === 'event') {
    const kind = Object.keys(msg.event?.kind ?? {})[0] ?? msg.event?.kind;
    meta.lastSeq = msg.event?.seq ?? meta.lastSeq;
    const session = ready?.session;

    if (script === 'abort' && events.length === 4) send(ws, { Abort: { session } });

    if (script === 'approve' && String(kind).includes('ApprovalRequest')) {
      const id = msg.event.kind.ApprovalRequest?.id;
      meta.approvalId = id;
      // Approve the first request, reject any second one.
      const approved = meta.approvedOnce ?? false;
      meta.approvedOnce = true;
      send(ws, { ApproveDiff: { session, id, approved: !approved } });
    }

    if (script === 'promote' && String(kind).includes('TurnComplete') && !meta.promoted) {
      meta.promoted = true;
      send(ws, { PromoteToRemote: { session } });
    }

    if (String(kind).includes('TurnComplete') && script === 'turn') done(0);
  }

  if (msg.type === 'promoted') {
    meta.promoted_frame = msg;
    done(0);
  }
});

ws.on('error', (e) => { meta.error = String(e); done(1); });
setTimeout(() => { meta.timeout = true; done(0); }, Number(arg('timeout', '120000')));
