'use strict';

// Subscribe to lifecycle events to render your own UI.
//
// Prereqs:
//   - CLOUDFLARE_API_TOKEN env var
//   - CFP_DOMAIN env var
//   - something listening on localhost:8080 (e.g. python -m http.server 8080)
//
// Run with:
//   node examples/with-events.js

const { connect } = require('cloudpipe');

function parse(json) {
 try {
 return JSON.parse(json);
 } catch {
 return json;
 }
}

(async () => {
 const token = process.env.CLOUDFLARE_API_TOKEN;
 const domain = process.env.CFP_DOMAIN;
 if (!token || !domain) {
 console.error(
 'set CLOUDFLARE_API_TOKEN and CFP_DOMAIN before running this example',
 );
 process.exit(1);
 }

 const listener = await connect({
 token,
 domain,
 protocol: 'http',
 port: 8080,
 autoRestart: true,
 });

 listener.on('creatingTunnel', (json) => {
 console.log('creating tunnel', parse(json).name);
 });
 listener.on('dnsCreated', (json) => {
 console.log('DNS created for', parse(json).fullName);
 });
 listener.on('cloudflaredStarted', () => {
 console.log('cloudflared spawned');
 });
 listener.on('edgeConnected', (json) => {
 const { connIndex, total } = parse(json);
 console.log(`edge connection ${connIndex}/${total}`);
 });
 listener.on('restarting', (json) => {
 const { reason, attempt } = parse(json);
 console.log(`restarting (attempt ${attempt}):`, reason);
 });
 listener.on('restartGivingUp', (json) => {
 const { attempts, lastError } = parse(json);
 console.log(`auto-restart giving up after ${attempts} attempts: ${lastError}`);
 });
 listener.on('shuttingDown', (json) => {
 console.log('shutting down:', parse(json).reason);
 });
 listener.on('cleaned', () => {
 console.log('all resources cleaned up');
 });

 console.log('live at', listener.url);

 process.on('SIGINT', async () => {
 try {
 await listener.close();
 } catch (err) {
 console.error('shutdown error:', err);
 }
 process.exit(0);
 });

 await listener.wait().catch((err) => {
 console.error('tunnel exited unexpectedly:', err);
 process.exit(1);
 });
})();