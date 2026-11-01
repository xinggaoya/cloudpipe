'use strict';

const http = require('node:http');

/**
 * A minimal stand-in for the Cloudflare API, enough for one tunnel
 * lifecycle: create a tunnel, write its ingress, create a DNS record,
 * then delete both.
 *
 * @returns {Promise<{ base: string, state: object, close: () => void }>}
 */
function startMockApi() {
  const state = {
    tunnels: new Map(),
    records: new Map(),
    deleted: [],
  };
  let nextId = 1;

  const server = http.createServer((req, res) => {
    let body = '';
    req.on('data', (chunk) => {
      body += chunk;
    });
    req.on('end', () => {
      const json = (payload) => {
        res.writeHead(200, { 'Content-Type': 'application/json' });
        res.end(JSON.stringify(payload));
      };
      const url = req.url || '';
      const method = req.method || 'GET';
      const parsed = () => {
        try {
          return JSON.parse(body || '{}');
        } catch {
          return {};
        }
      };

      if (method === 'POST' && url.includes('/tunnels')) {
        const created = {
          id: `tunnel-${nextId++}`,
          name: parsed().name || 'demo',
          status: 'healthy',
          token: 'fake-tunnel-token',
        };
        state.tunnels.set(created.id, created);
        return json({ success: true, result: created });
      }
      if (method === 'PUT' && url.includes('/configurations')) {
        return json({ success: true, result: null });
      }
      if (method === 'POST' && url.includes('/dns_records')) {
        const record = {
          id: `record-${nextId++}`,
          name: parsed().name,
          content: 'x.cfargotunnel.com',
        };
        state.records.set(record.id, record);
        return json({ success: true, result: record });
      }
      if (method === 'DELETE' && url.includes('/dns_records')) {
        const id = url.split('/').pop();
        state.deleted.push(id);
        state.records.delete(id);
        return json({ success: true, result: null });
      }
      if (method === 'DELETE' && url.includes('/connections')) {
        return json({ success: true, result: null });
      }
      if (method === 'DELETE' && url.includes('/tunnels')) {
        const id = url.split('/').pop();
        state.deleted.push(id);
        state.tunnels.delete(id);
        return json({ success: true, result: null });
      }
      if (method === 'GET' && url.includes('/user/tokens/verify')) {
        return json({ success: true, result: { id: 'tok', status: 'active' } });
      }
      if (method === 'GET' && url.includes('/zones')) {
        return json({
          success: true,
          result: [{ id: 'zone-1', name: 'example.com', account: { id: 'account-1' } }],
          result_info: { page: 1, per_page: 50, total_pages: 1 },
        });
      }
      if (method === 'GET' && url.includes('/tunnels')) {
        return json({
          success: true,
          result: [],
          result_info: { page: 1, per_page: 100, total_pages: 1 },
        });
      }
      if (method === 'GET' && url.includes('/dns_records')) {
        return json({
          success: true,
          result: [],
          result_info: { page: 1, per_page: 100, total_pages: 1 },
        });
      }
      json({
        success: false,
        errors: [{ code: 7003, message: `unmocked: ${method} ${url}` }],
      });
    });
  });

  return new Promise((resolve) => {
    server.listen(0, '127.0.0.1', () => {
      const { port } = server.address();
      resolve({
        base: `http://127.0.0.1:${port}`,
        state,
        close: () => {
          // `close()` alone waits for the keep-alive connections to end,
          // and the SDK's client pool holds one open — the server handle
          // would keep the event loop alive and the process would never
          // exit.
          server.closeAllConnections?.();
          server.close();
        },
      });
    });
  });
}

module.exports = { startMockApi };
