'use strict';

// Minimal quickstart — expose localhost:8080 to the public internet.
//
// Prereqs:
//   - CLOUDFLARE_API_TOKEN env var
//   - CFP_DOMAIN env var (a zone this token can write DNS for)
//
// Run with:
//   node examples/quickstart.js

const { connect } = require('cloudpipe');

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
 });

 console.log('live at', listener.url);
 console.log('subdomain:', listener.subdomain);

 process.on('SIGINT', async () => {
 console.log('shutting down...');
 try {
 await listener.close();
 } catch (err) {
 console.error('shutdown error:', err);
 }
 process.exit(0);
 });

 // Wait for ctrl-c; the listener emits lifecycle events on the same
 // object if you also want to react to cloudflared crashes.
 await listener.wait().catch((err) => {
 console.error('tunnel exited unexpectedly:', err);
 });
})();