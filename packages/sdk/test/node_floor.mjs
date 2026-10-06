// The built package on the oldest Node that `engines` allows: start the server, which is the `initialize`
// handshake, and close it. CI runs this under Node 20. The test suite cannot run there, since it is TypeScript
// and Node 20 does not strip types, so this is plain JavaScript against `dist`.
//
//     node test/node_floor.mjs <fastbrowse executable>

import { Fastbrowse } from '../dist/index.js';

const [binaryPath] = process.argv.slice(2);
if (!binaryPath) throw new Error('usage: node test/node_floor.mjs <fastbrowse executable>');

const fb = await Fastbrowse.start({ binaryPath });
console.log(`node ${process.version}: fastbrowse ${fb.fastbrowseVersion} answered initialize`);
await fb.close();
